//! Search = scan the big lowercase buffer in parallel 1 MB chunks.
//!
//! * plain query  : memchr::memmem (SIMD) on the longest term, other terms verified on the line
//! * regex query  : regex::bytes (literal prefilters, lazy DFA) run over the whole chunk, each
//!                  candidate verified on its own line so matches can never span two names
use crate::daemon::index::{VolIndex, DEAD, DIR};
use crate::proto::SortKey;
use memchr::memmem;
use rayon::prelude::*;

pub enum Matcher {
    Lit { first: memmem::Finder<'static>, rest: Vec<memmem::Finder<'static>> },
    Re(regex::bytes::Regex),
}

pub fn compile(q: &str, regex: bool) -> Result<Option<Matcher>, String> {
    if regex {
        let p = q.trim();
        if p.is_empty() {
            return Ok(None);
        }
        // haystack is already lowercase: only need (?i) if the pattern itself has capitals
        let ci = p.bytes().any(|b| b.is_ascii_uppercase());
        let re = regex::bytes::RegexBuilder::new(p)
            .case_insensitive(ci)
            .multi_line(true)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Some(Matcher::Re(re)))
    } else {
        let lower = q.to_lowercase();
        let mut t: Vec<&str> = lower.split_whitespace().collect();
        if t.is_empty() {
            return Ok(None);
        }
        t.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let mk = |s: &str| memmem::Finder::new(s.as_bytes()).into_owned();
        Ok(Some(Matcher::Lit {
            first: mk(t[0]),
            rest: t[1..].iter().map(|s| mk(*s)).collect(),
        }))
    }
}

/// Split at line boundaries into ~1 MB pieces.
fn chunks(buf: &[u8]) -> Vec<(usize, usize)> {
    const TARGET: usize = 1 << 20;
    let n = (buf.len() / TARGET).max(1);
    let mut cuts = vec![0usize];
    for k in 1..n {
        let p = k * buf.len() / n;
        let p = match memchr::memchr(b'\n', &buf[p..]) {
            Some(x) => p + x + 1,
            None => buf.len(),
        };
        if p > *cuts.last().unwrap() {
            cuts.push(p);
        }
    }
    cuts.push(buf.len());
    cuts.windows(2).filter(|w| w[0] < w[1]).map(|w| (w[0], w[1])).collect()
}

fn scan(v: &VolIndex, m: &Matcher, s: usize, e: usize, cap: usize) -> Vec<u32> {
    let hay = &v.lc[s..e];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < hay.len() && out.len() < cap {
        let found = match m {
            Matcher::Lit { first, .. } => first.find(&hay[pos..]).map(|x| pos + x),
            Matcher::Re(re) => re.find_at(hay, pos).map(|x| x.start()),
        };
        let Some(rel) = found else { break };
        let abs = s + rel;
        // which entry (line) is this offset in?
        let i = v.lc_off.partition_point(|&o| (o as usize) <= abs) - 1;
        let ls = v.lc_off[i] as usize;
        let le = v.lc_off.get(i + 1).map(|&o| o as usize).unwrap_or(v.lc.len()) - 1; // excl '\n'
        let line = &v.lc[ls..le];
        let ok = v.flags[i] & DEAD == 0
            && match m {
                Matcher::Lit { rest, .. } => rest.iter().all(|f| f.find(line).is_some()),
                Matcher::Re(re) => re.is_match(line),
            };
        if ok {
            out.push(i as u32);
        }
        pos = le + 1 - s; // continue at the next line
    }
    out
}

/// Returns hits as (vol << 32 | entry) and whether the cap truncated the list.
pub fn run(vols: &[VolIndex], m: &Matcher, cap: usize) -> (Vec<u64>, bool) {
    let mut out: Vec<u64> = Vec::new();
    let mut trunc = false;
    'outer: for (vi, v) in vols.iter().enumerate() {
        let parts: Vec<Vec<u32>> = chunks(&v.lc)
            .par_iter()
            .map(|&(s, e)| scan(v, m, s, e, cap))
            .collect();
        for p in parts {
            for i in p {
                if out.len() >= cap {
                    trunc = true;
                    break 'outer;
                }
                out.push(((vi as u64) << 32) | i as u64);
            }
        }
    }
    (out, trunc)
}

pub fn split(h: u64) -> (usize, usize) {
    ((h >> 32) as usize, (h & 0xFFFF_FFFF) as usize)
}

fn ext_key(line: &[u8], dir: bool) -> u64 {
    if dir {
        return 0;
    }
    match memchr::memrchr(b'.', line) {
        Some(p) if p + 1 < line.len() => {
            let mut k = 0u64;
            for (j, &b) in line[p + 1..].iter().take(7).enumerate() {
                k |= (b as u64) << (56 - 8 * j);
            }
            k | 1
        }
        _ => 1,
    }
}

pub fn sort(vols: &[VolIndex], hits: &mut Vec<u64>, key: SortKey, desc: bool) {
    match key {
        SortKey::Name => hits.par_sort_unstable_by(|a, b| {
            let (av, ai) = split(*a);
            let (bv, bi) = split(*b);
            vols[av].lc_line(ai).cmp(vols[bv].lc_line(bi))
        }),
        SortKey::Size => hits.par_sort_unstable_by_key(|h| {
            let (v, i) = split(*h);
            vols[v].size[i]
        }),
        SortKey::Date => hits.par_sort_unstable_by_key(|h| {
            let (v, i) = split(*h);
            vols[v].mtime[i]
        }),
        SortKey::Kind => {
            let mut k: Vec<(u64, u64)> = hits
                .par_iter()
                .map(|&h| {
                    let (v, i) = split(h);
                    (ext_key(vols[v].lc_line(i), vols[v].flags[i] & DIR != 0), h)
                })
                .collect();
            k.par_sort_unstable();
            for (dst, src) in hits.iter_mut().zip(k) {
                *dst = src.1;
            }
        }
    }
    if desc {
        hits.reverse();
    }
}
