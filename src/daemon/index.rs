//! In-memory index, column oriented.
//!
//! `lc` is ONE big buffer of lowercase names, each terminated by '\n'. Searching is a
//! SIMD scan over that buffer (memmem / regex) instead of a per-entry loop.
use std::borrow::Cow;

pub const NO: u32 = u32::MAX;
pub const DIR: u8 = 1;
pub const DEAD: u8 = 2;

pub struct VolIndex {
    pub letter: char,
    pub lc: Vec<u8>,     // "name\nname\n..." lowercase, searched
    pub lc_off: Vec<u32>, // start of each line in `lc`
    pub nm: Vec<u8>,     // same layout, original case, displayed
    pub nm_off: Vec<u32>,
    pub frs: Vec<u64>,
    pub parent: Vec<u64>,
    pub size: Vec<u64>,
    pub mtime: Vec<i64>,
    pub flags: Vec<u8>,
    pub idx: Vec<u32>, // sparse: FRS -> entry index
    pub dead: usize,
}

impl VolIndex {
    pub fn new(letter: char) -> Self {
        VolIndex {
            letter,
            lc: Vec::new(),
            lc_off: Vec::new(),
            nm: Vec::new(),
            nm_off: Vec::new(),
            frs: Vec::new(),
            parent: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            flags: Vec::new(),
            idx: Vec::new(),
            dead: 0,
        }
    }

    pub fn reserve(&mut self, n: usize) {
        self.lc.reserve(n * 24);
        self.nm.reserve(n * 24);
        self.lc_off.reserve(n);
        self.nm_off.reserve(n);
        self.frs.reserve(n);
        self.parent.reserve(n);
        self.size.reserve(n);
        self.mtime.reserve(n);
        self.flags.reserve(n);
    }

    pub fn len(&self) -> usize {
        self.lc_off.len()
    }

    pub fn live(&self) -> usize {
        self.len() - self.dead
    }

    fn clean(name: &str) -> Cow<'_, str> {
        if name.contains('\n') {
            Cow::Owned(name.replace('\n', "?"))
        } else {
            Cow::Borrowed(name)
        }
    }

    pub fn push(&mut self, frs: u64, parent: u64, name: &str, dir: bool, size: u64, mtime: i64) -> usize {
        let name = Self::clean(name);
        let i = self.lc_off.len();
        self.lc_off.push(self.lc.len() as u32);
        if name.is_ascii() {
            self.lc.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        } else {
            self.lc.extend_from_slice(name.to_lowercase().as_bytes());
        }
        self.lc.push(b'\n');
        self.nm_off.push(self.nm.len() as u32);
        self.nm.extend_from_slice(name.as_bytes());
        self.nm.push(b'\n');
        self.frs.push(frs);
        self.parent.push(parent);
        self.size.push(size);
        self.mtime.push(mtime);
        self.flags.push(if dir { DIR } else { 0 });
        let f = frs as usize;
        if f >= self.idx.len() {
            self.idx.resize(f + 1, NO);
        }
        self.idx[f] = i as u32;
        i
    }

    pub fn name(&self, i: usize) -> &str {
        let s = self.nm_off[i] as usize;
        let e = self.nm_off.get(i + 1).map(|&o| o as usize).unwrap_or(self.nm.len()) - 1;
        std::str::from_utf8(&self.nm[s..e]).unwrap_or("?")
    }

    pub fn lc_line(&self, i: usize) -> &[u8] {
        let s = self.lc_off[i] as usize;
        let e = self.lc_off.get(i + 1).map(|&o| o as usize).unwrap_or(self.lc.len()) - 1;
        &self.lc[s..e]
    }

    pub fn path(&self, i: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = i as usize;
        loop {
            parts.push(self.name(cur));
            match self.idx.get(self.parent[cur] as usize) {
                Some(&p) if p != NO && parts.len() < 512 => cur = p as usize,
                _ => break,
            }
        }
        let mut s = format!("{}:\\", self.letter);
        for (k, p) in parts.iter().rev().enumerate() {
            if k > 0 {
                s.push('\\');
            }
            s.push_str(p);
        }
        s
    }

    /// Insert or update structure (name/parent). Keeps size/mtime of an unchanged entry.
    pub fn upsert(&mut self, frs: u64, parent: u64, name: &str, dir: bool) -> usize {
        if let Some(&i) = self.idx.get(frs as usize) {
            if i != NO {
                let i = i as usize;
                if self.parent[i] == parent && self.name(i) == Self::clean(name).as_ref() {
                    self.flags[i] = (self.flags[i] & !DIR) | if dir { DIR } else { 0 };
                    return i;
                }
                self.flags[i] |= DEAD;
                self.dead += 1;
            }
        }
        self.push(frs, parent, name, dir, 0, 0)
    }

    pub fn set_meta(&mut self, i: usize, size: u64, mtime: i64) {
        self.size[i] = size;
        self.mtime[i] = mtime;
    }

    pub fn delete(&mut self, frs: u64) {
        if let Some(&i) = self.idx.get(frs as usize) {
            if i != NO {
                self.flags[i as usize] |= DEAD;
                self.dead += 1;
                self.idx[frs as usize] = NO;
            }
        }
    }

    /// Drop tombstones. Entry indices change -> caller must bump the epoch.
    pub fn compact(&mut self) {
        let mut n = VolIndex::new(self.letter);
        n.reserve(self.live());
        for i in 0..self.len() {
            if self.flags[i] & DEAD == 0 {
                n.push(self.frs[i], self.parent[i], self.name(i), self.flags[i] & DIR != 0, self.size[i], self.mtime[i]);
            }
        }
        *self = n;
    }

    /// db value: parent u64 | size u64 | mtime i64 | dir u8 | name utf8
    pub fn pack_into(&self, i: usize, out: &mut Vec<u8>) {
        out.clear();
        out.extend(self.parent[i].to_le_bytes());
        out.extend(self.size[i].to_le_bytes());
        out.extend(self.mtime[i].to_le_bytes());
        out.push(self.flags[i] & DIR);
        out.extend_from_slice(self.name(i).as_bytes());
    }
}

pub struct Index {
    pub vols: Vec<VolIndex>,
    pub version: u64, // bumps on any change
    pub epoch: u64,   // bumps when entry indices are invalidated
}

impl Index {
    pub fn new() -> Self {
        Index { vols: Vec::new(), version: 0, epoch: 0 }
    }

    pub fn install(&mut self, v: VolIndex) {
        match self.vols.iter().position(|x| x.letter == v.letter) {
            Some(p) => self.vols[p] = v,
            None => self.vols.push(v),
        }
        self.version += 1;
        self.epoch += 1;
    }

    pub fn vol_mut(&mut self, letter: char) -> Option<&mut VolIndex> {
        self.vols.iter_mut().find(|v| v.letter == letter)
    }

    pub fn entries(&self) -> u64 {
        self.vols.iter().map(|v| v.live() as u64).sum()
    }
}
