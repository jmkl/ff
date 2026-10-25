//! Persistence in redb (pure-Rust embedded ACID key/value store).
//!   table "vol_C": frs (u64) -> parent | size | mtime | dir | name
//!   table "meta" : drive letter -> journal id | next usn
use crate::daemon::index::VolIndex;
use anyhow::Result;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::collections::HashMap;
use std::path::Path;

const META: TableDefinition<u64, &[u8]> = TableDefinition::new("meta");

pub struct Db {
    db: Database,
}

fn tname(c: char) -> String {
    format!("vol_{c}")
}

fn meta_bytes(m: (u64, i64)) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&m.0.to_le_bytes());
    b[8..].copy_from_slice(&m.1.to_le_bytes());
    b
}

impl Db {
    pub fn open(p: &Path) -> Result<Db> {
        Ok(Db { db: Database::create(p)? })
    }

    /// Fast warm start: rebuild the in-memory index from the stored rows.
    pub fn load_volume(&self, letter: char) -> Result<Option<VolIndex>> {
        let name = tname(letter);
        let def: TableDefinition<u64, &[u8]> = TableDefinition::new(&name);
        let rx = self.db.begin_read()?;
        let t = match rx.open_table(def) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut v = VolIndex::new(letter);
        for r in t.iter()? {
            let (k, val) = r?;
            let b = val.value();
            if b.len() < 25 {
                continue;
            }
            let parent = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let size = u64::from_le_bytes(b[8..16].try_into().unwrap());
            let mtime = i64::from_le_bytes(b[16..24].try_into().unwrap());
            let dir = b[24] & 1 != 0;
            let nm = std::str::from_utf8(&b[25..]).unwrap_or("?");
            v.push(k.value(), parent, nm, dir, size, mtime);
        }
        Ok(Some(v))
    }

    pub fn load_meta(&self, letter: char) -> Option<(u64, i64)> {
        let rx = self.db.begin_read().ok()?;
        let t = rx.open_table(META).ok()?;
        let g = t.get(letter as u64).ok()??;
        let b = g.value();
        if b.len() < 16 {
            return None;
        }
        Some((
            u64::from_le_bytes(b[0..8].try_into().ok()?),
            i64::from_le_bytes(b[8..16].try_into().ok()?),
        ))
    }

    pub fn save_meta(&self, letter: char, m: (u64, i64)) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(META)?;
            t.insert(letter as u64, meta_bytes(m).as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Bulk (re)write of a volume in chunks. `reset` drops the old table first.
    pub fn put_chunk(&self, letter: char, reset: bool, rows: &[(u64, Vec<u8>)]) -> Result<()> {
        let name = tname(letter);
        let mk = || TableDefinition::<u64, &[u8]>::new(&name);
        let tx = self.db.begin_write()?;
        if reset {
            tx.delete_table(mk())?;
        }
        {
            let mut t = tx.open_table(mk())?;
            for (k, v) in rows {
                t.insert(*k, v.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Incremental update: Some(bytes) = upsert, None = delete. Also stores the journal cursor.
    pub fn apply(&self, letter: char, pending: &HashMap<u64, Option<Vec<u8>>>, m: (u64, i64)) -> Result<()> {
        let name = tname(letter);
        let def: TableDefinition<u64, &[u8]> = TableDefinition::new(&name);
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(def)?;
            for (k, v) in pending {
                match v {
                    Some(b) => {
                        t.insert(*k, b.as_slice())?;
                    }
                    None => {
                        t.remove(*k)?;
                    }
                }
            }
        }
        {
            let mut mt = tx.open_table(META)?;
            mt.insert(letter as u64, meta_bytes(m).as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }
}
