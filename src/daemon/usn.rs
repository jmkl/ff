//! NTFS change journal: query + incremental read.
use crate::daemon::win;
use crate::util::{i64_, u16_, u32_, u64_};
use std::fs::File;
use std::io;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;

pub const REASON_DELETE: u32 = 0x0000_0200;
const FRS_MASK: u64 = 0xFFFF_FFFF_FFFF;

pub struct Journal {
    pub id: u64,
    pub next: i64,
    pub lowest: i64,
}

pub fn query(f: &File) -> io::Result<Journal> {
    let mut o = [0u8; 64];
    win::ioctl(f, FSCTL_QUERY_USN_JOURNAL, &[], &mut o)?;
    Ok(Journal { id: u64_(&o, 0), next: i64_(&o, 16), lowest: i64_(&o, 24) })
}

pub struct Rec {
    pub frs: u64,
    pub parent: u64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

/// Returns (next USN to read from, records). Only "close" records are returned,
/// with all reasons accumulated, which keeps the volume of events low.
pub fn read(f: &File, start: i64, id: u64, buf: &mut [u8]) -> io::Result<(i64, Vec<Rec>)> {
    let mut inp = [0u8; 40];
    inp[0..8].copy_from_slice(&start.to_le_bytes());
    inp[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // reason mask
    inp[12..16].copy_from_slice(&1u32.to_le_bytes()); // return only on close
    // timeout = 0, bytes_to_wait_for = 0 -> return immediately
    inp[32..40].copy_from_slice(&id.to_le_bytes());

    let n = win::ioctl(f, FSCTL_READ_USN_JOURNAL, &inp, buf)?;
    if n < 8 {
        return Ok((start, Vec::new()));
    }
    let next = i64_(buf, 0);
    let mut out = Vec::new();
    let mut off = 8;
    while off + 60 <= n {
        let len = u32_(buf, off) as usize;
        if len < 60 || off + len > n {
            break;
        }
        let major = u16_(buf, off + 4);
        if major == 2 {
            let nlen = u16_(buf, off + 56) as usize;
            let noff = u16_(buf, off + 58) as usize;
            if let Some(nb) = buf.get(off + noff..off + noff + nlen) {
                let u: Vec<u16> = nb
                    .chunks_exact(2)
                    .map(|x| u16::from_le_bytes([x[0], x[1]]))
                    .collect();
                out.push(Rec {
                    frs: u64_(buf, off + 8) & FRS_MASK,
                    parent: u64_(buf, off + 16) & FRS_MASK,
                    reason: u32_(buf, off + 40),
                    attrs: u32_(buf, off + 52),
                    name: String::from_utf16_lossy(&u),
                });
            }
        }
        off += len;
    }
    Ok((next, out))
}
