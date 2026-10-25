//! Raw $MFT reader: extent-aware chunked reads, reader thread overlapped with
//! parallel record parsing (approach adapted from UFFS).
use crate::daemon::index::VolIndex;
use crate::daemon::win;
use crate::util::{u16_, u32_, u64_};
use rayon::prelude::*;
use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;
use std::sync::mpsc;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const FRS_MASK: u64 = 0xFFFF_FFFF_FFFF;
const CHUNK: usize = 4 << 20;

struct Parsed {
    frs: u64,
    parent: u64,
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

fn read_full(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// Undo the Update Sequence Array protection on a FILE record.
fn fixup(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE".as_slice()) {
        return false;
    }
    let uo = u16_(rec, 4) as usize;
    let uc = u16_(rec, 6) as usize;
    if uc < 2 || uo + uc * 2 > rec.len() {
        return false;
    }
    let usn = [rec[uo], rec[uo + 1]];
    for i in 1..uc {
        let end = i * 512;
        if end > rec.len() {
            break;
        }
        if rec[end - 2..end] != usn {
            return false;
        }
        let (a, b) = (rec[uo + i * 2], rec[uo + i * 2 + 1]);
        rec[end - 2] = a;
        rec[end - 1] = b;
    }
    true
}

/// Decode NTFS data runs -> (absolute LCN, length in clusters)
fn data_runs(attr: &[u8], mut p: usize) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    while let Some(&hd) = attr.get(p) {
        if hd == 0 {
            break;
        }
        let ls = (hd & 0x0F) as usize;
        let os = (hd >> 4) as usize;
        p += 1;
        if ls == 0 || ls > 8 || os > 8 {
            break;
        }
        let mut len = 0u64;
        for i in 0..ls {
            len |= (attr.get(p + i).copied().unwrap_or(0) as u64) << (8 * i);
        }
        p += ls;
        let mut off = 0i64;
        for i in 0..os {
            off |= (attr.get(p + i).copied().unwrap_or(0) as i64) << (8 * i);
        }
        if os > 0 && os < 8 && attr.get(p + os - 1).copied().unwrap_or(0) & 0x80 != 0 {
            off |= (-1i64) << (8 * os);
        }
        p += os;
        if os == 0 {
            continue; // sparse
        }
        lcn += off;
        out.push((lcn as u64, len));
    }
    out
}

fn parse_record(rec: &mut [u8], frs: u64) -> Option<Parsed> {
    if frs < 24 || !fixup(rec) {
        return None;
    }
    let flags = u16_(rec, 22);
    if flags & 1 == 0 {
        return None; // not in use
    }
    if u64_(rec, 32) & FRS_MASK != 0 {
        return None; // extension record (limitation)
    }
    let used = (u32_(rec, 24) as usize).min(rec.len());
    let mut off = u16_(rec, 20) as usize;

    let mut name: Option<String> = None;
    let mut parent = 0u64;
    let mut best = 0u8;
    let mut size = 0u64;
    let mut have_size = false;
    let mut mtime = 0i64;

    while off + 16 <= used {
        let ty = u32_(rec, off);
        if ty == 0xFFFF_FFFF {
            break;
        }
        let len = u32_(rec, off + 4) as usize;
        if len < 16 || off + len > used {
            break;
        }
        let nonres = rec[off + 8] != 0;
        let nlen = rec[off + 9];
        match ty {
            0x10 if !nonres => {
                let c = off + u16_(rec, off + 20) as usize;
                mtime = u64_(rec, c + 8) as i64;
            }
            0x30 if !nonres => {
                let c = off + u16_(rec, off + 20) as usize;
                let ns = rec.get(c + 65).copied().unwrap_or(0);
                let rank = match ns {
                    1 | 3 => 3, // Win32 / Win32+DOS
                    0 => 2,     // POSIX
                    2 => 1,     // DOS 8.3
                    _ => 0,
                };
                if rank > best {
                    let nl = rec.get(c + 64).copied().unwrap_or(0) as usize;
                    let bytes = rec.get(c + 66..c + 66 + nl * 2)?;
                    let u: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|x| u16::from_le_bytes([x[0], x[1]]))
                        .collect();
                    name = Some(String::from_utf16_lossy(&u));
                    parent = u64_(rec, c) & FRS_MASK;
                    best = rank;
                }
            }
            0x80 if nlen == 0 && !have_size => {
                if nonres {
                    if u64_(rec, off + 16) == 0 {
                        size = u64_(rec, off + 48);
                        have_size = true;
                    }
                } else {
                    size = u32_(rec, off + 16) as u64;
                    have_size = true;
                }
            }
            _ => {}
        }
        off += len;
    }

    Some(Parsed { frs, parent, name: name?, dir: flags & 2 != 0, size, mtime })
}

pub fn scan(f: &File, letter: char) -> io::Result<VolIndex> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

    // 1. geometry
    let mut vd = [0u8; 128];
    win::ioctl(f, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut vd)?;
    let bpc = u32_(&vd, 44) as u64;
    let rs = u32_(&vd, 48) as usize;
    let mft_lcn = u64_(&vd, 64);
    if bpc == 0 || rs < 512 || rs > (1 << 16) {
        return Err(bad("odd NTFS geometry"));
    }

    // 2. record 0 -> $MFT data runs
    let mut r0 = vec![0u8; (bpc as usize).max(rs)];
    read_full(f, &mut r0, mft_lcn * bpc)?;
    let rec0 = &mut r0[..rs];
    if !fixup(rec0) {
        return Err(bad("bad $MFT record"));
    }
    let mut mft_runs = Vec::new();
    let mut valid = 0u64;
    let mut off = u16_(rec0, 20) as usize;
    while off + 8 <= rec0.len() {
        let ty = u32_(rec0, off);
        if ty == 0xFFFF_FFFF {
            break;
        }
        let len = u32_(rec0, off + 4) as usize;
        if len == 0 || off + len > rec0.len() {
            break;
        }
        if ty == 0x80 && rec0[off + 8] == 1 && rec0[off + 9] == 0 {
            valid = u64_(rec0, off + 56);
            mft_runs = data_runs(&rec0[off..off + len], u16_(rec0, off + 32) as usize);
            break;
        }
        off += len;
    }
    if mft_runs.is_empty() {
        return Err(bad("no $MFT data runs"));
    }
    let total = valid / rs as u64;

    // 3. plan chunked reads along the extents
    let chunk = CHUNK / rs * rs;
    let mut plan: Vec<(u64, usize, u64)> = Vec::new(); // (byte offset, bytes, first FRS)
    let mut frs = 0u64;
    for &(lcn, clusters) in &mft_runs {
        let mut o = lcn * bpc;
        let mut remain = clusters * bpc;
        while remain > 0 && frs < total {
            let n = remain.min(chunk as u64) as usize;
            let recs = ((n / rs) as u64).min(total - frs);
            if recs == 0 {
                break;
            }
            let n = recs as usize * rs;
            plan.push((o, n, frs));
            o += n as u64;
            remain -= n as u64;
            frs += recs;
        }
    }

    // 4. reader thread || parallel parse
    let mut v = VolIndex::new(letter);
    v.reserve(total as usize / 2);
    v.idx.resize(total as usize, crate::daemon::index::NO);
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(4);
        s.spawn(move || {
            for (o, n, first) in plan {
                let mut b = vec![0u8; n];
                if read_full(f, &mut b, o).is_err() {
                    b.clear();
                }
                if tx.send((first, b)).is_err() {
                    break;
                }
            }
        });
        for (first, mut buf) in rx {
            let ents: Vec<Parsed> = buf
                .par_chunks_mut(rs)
                .enumerate()
                .filter_map(|(i, r)| parse_record(r, first + i as u64))
                .collect();
            for p in ents {
                v.push(p.frs, p.parent, &p.name, p.dir, p.size, p.mtime);
            }
        }
    });
    Ok(v)
}
