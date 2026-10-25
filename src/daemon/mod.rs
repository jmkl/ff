//! The indexer behind `ff-daemon.exe`.
//!   * one worker thread per volume: warm start from redb, USN catch-up / MFT scan, live updates
//!   * `server::serve` answers `ff.exe` clients over loopback TCP
pub mod db;
pub mod index;
pub mod mft;
pub mod search;
pub mod server;
pub mod usn;
pub mod win;
pub mod worker;

use crate::util::{home, log};
use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
use std::sync::Arc;

pub fn start(token: [u8; 16]) -> anyhow::Result<Arc<server::State>> {
    // keep the log small
    let logp = home().join("ff-daemon.log");
    if std::fs::metadata(&logp).map(|m| m.len() > 5 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&logp);
    }

    // database (recreate if corrupt / incompatible)
    let dbp = home().join("index.redb");
    let db = match db::Db::open(&dbp) {
        Ok(d) => d,
        Err(e) => {
            log(&format!("db open failed ({e}), recreating"));
            let _ = std::fs::remove_file(&dbp);
            db::Db::open(&dbp)?
        }
    };
    let db = Arc::new(db);

    let state = Arc::new(server::State {
        idx: RwLock::new(index::Index::new()),
        status: Mutex::new(BTreeMap::new()),
        token,
    });

    for letter in win::drive_letters() {
        let (st, db) = (state.clone(), db.clone());
        let _ = std::thread::Builder::new()
            .name(format!("vol-{letter}"))
            .spawn(move || worker::run(letter, st, db));
    }
    Ok(state)
}
