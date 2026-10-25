use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FT_DIFF: i64 = 116_444_736_000_000_000; // FILETIME of 1970-01-01

/// %LOCALAPPDATA%\ff  (db, token, log live here)
pub fn home() -> PathBuf {
    let p = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ff");
    let _ = std::fs::create_dir_all(&p);
    p
}

// ---- safe little-endian readers (return 0 when out of range) ----
pub fn rd<const N: usize>(b: &[u8], o: usize) -> [u8; N] {
    let mut a = [0u8; N];
    if let Some(s) = b.get(o..o + N) {
        a.copy_from_slice(s);
    }
    a
}
pub fn u16_(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(rd(b, o))
}
pub fn u32_(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(rd(b, o))
}
pub fn u64_(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(rd(b, o))
}
pub fn i64_(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(rd(b, o))
}

// ---- time ----
pub fn ft_to_st(ft: i64) -> SystemTime {
    if ft <= FT_DIFF {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_nanos((ft - FT_DIFF) as u64 * 100)
    }
}
pub fn st_to_ft(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as i64 + FT_DIFF)
        .unwrap_or(0)
}
pub fn fmt_time(ft: i64) -> String {
    let t = ft_to_st(ft);
    if t == UNIX_EPOCH {
        return String::new();
    }
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn hsize(n: u64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB"];
    let (mut f, mut i) = (n as f64, 0);
    while f >= 1024.0 && i < 4 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.1} {}", u[i])
    }
}

pub fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            o.push(',');
        }
        o.push(c);
    }
    o
}

// ---- token helpers ----
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
pub fn unhex16(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() != 32 {
        return None;
    }
    let mut o = [0u8; 16];
    for (i, slot) in o.iter_mut().enumerate() {
        *slot = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(o)
}

/// tiny append-only log used by the (hidden) daemon
pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home().join("ff-daemon.log"))
    {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{t}] {msg}");
    }
}
