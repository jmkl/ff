//! Length-prefixed postcard frames over loopback TCP between `ff` and `ff-daemon`.
use crate::util;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const PORT: u16 = 47615;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Date,
    Size,
    Kind,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SearchReq {
    pub generation: u64, // client query generation, echoed back
    pub q: String,
    pub regex: bool,
    pub sort: SortKey,
    pub desc: bool,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Req {
    Hello { token: [u8; 16] },
    Search(SearchReq),
    Status,
    Shutdown,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Row {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64, // FILETIME
    pub dir: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Status {
    pub state: String,
    pub entries: u64,
    pub version: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Resp {
    Ok,
    Err(String),
    Page {
        generation: u64,
        offset: u32,
        total: u32,
        truncated: bool,
        took_us: u64,
        rows: Vec<Row>,
    },
    Status(Status),
}

fn ioerr<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub fn send<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let b = postcard::to_stdvec(v).map_err(ioerr)?;
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(&b)?;
    w.flush()
}

pub fn recv<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut l = [0u8; 4];
    r.read_exact(&mut l)?;
    let n = u32::from_le_bytes(l) as usize;
    if n > (64 << 20) {
        return Err(ioerr("frame too large"));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    postcard::from_bytes(&b).map_err(ioerr)
}

pub fn token_path() -> PathBuf {
    util::home().join("daemon.token")
}

pub fn read_token() -> Option<[u8; 16]> {
    util::unhex16(&std::fs::read_to_string(token_path()).ok()?)
}
