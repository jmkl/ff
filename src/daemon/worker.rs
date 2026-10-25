//! One thread per volume:
//!   1. warm start from the db (queries work immediately)
//!   2. USN journal catch-up if the saved cursor is still valid, else full MFT scan
//!   3. poll the journal every 500 ms and apply changes to memory + db
use crate::daemon::db::Db;
use crate::daemon::index::DEAD;
use crate::daemon::mft;
use crate::daemon::server::State;
use crate::daemon::usn;
use crate::daemon::win;
use crate::util::{log, st_to_ft};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

type Pending = HashMap<u64, Option<Vec<u8>>>;

pub fn run(letter: char, st: Arc<State>, db: Arc<Db>) {
    // 1. warm start (works without admin: search the cached index)
    st.set(letter, "loading db");
    let t = Instant::now();
    let loaded = match db.load_volume(letter) {
        Ok(Some(v)) if v.len() > 0 => {
            log(&format!("{letter}: loaded {} entries from db in {:.2?}", v.len(), t.elapsed()));
            st.idx.write().install(v);
            true
        }
        Ok(_) => false,
        Err(e) => {
            log(&format!("{letter}: db load failed: {e}"));
            false
        }
    };

    let f = match win::open_volume(letter) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("{letter}: open failed: {e} (need admin for live index)"));
            st.set(letter, if loaded { "cached only (not admin)" } else { "no index (run as admin)" });
            return;
        }
    };

    // 2. catch up or rescan
    let mut cursor: Option<(u64, i64)> = None;
    if loaded {
        if let (Some((id, usn_pos)), Ok(j)) = (db.load_meta(letter), usn::query(&f)) {
            if id == j.id && usn_pos >= j.lowest && usn_pos <= j.next {
                cursor = Some((id, usn_pos));
                log(&format!("{letter}: journal cursor valid, replaying from {usn_pos}"));
            }
        }
    }
    let mut last_scan = Instant::now();
    if cursor.is_none() {
        cursor = full_scan(&f, letter, &st, &db);
        last_scan = Instant::now();
        if !st.has(letter) {
            return; // scan failed (e.g. not an NTFS volume): nothing to keep updating
        }
    }
    st.set(letter, "ready");

    // 3. live updates
    let mut last_meta = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(mut c) = cursor {
            match poll(&f, letter, &mut c, &st, &db) {
                Ok(changed) => {
                    cursor = Some(c);
                    if !changed && last_meta.elapsed() > Duration::from_secs(30) {
                        let _ = db.save_meta(letter, c);
                        last_meta = Instant::now();
                    }
                }
                Err(e) => {
                    log(&format!("{letter}: journal read failed ({e}), rescanning"));
                    cursor = full_scan(&f, letter, &st, &db);
                    last_scan = Instant::now();
                    st.set(letter, "ready");
                }
            }
        } else if last_scan.elapsed() > Duration::from_secs(600) {
            // no journal available: periodic rescan fallback
            cursor = full_scan(&f, letter, &st, &db);
            last_scan = Instant::now();
            st.set(letter, "ready");
        }
    }
}

fn full_scan(f: &File, letter: char, st: &State, db: &Db) -> Option<(u64, i64)> {
    st.set(letter, "scanning MFT");
    // grab the cursor BEFORE scanning so changes made during the scan are replayed afterwards
    let j = usn::query(f).ok();
    let t = Instant::now();
    let v = match mft::scan(f, letter) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{letter}: scan failed: {e}"));
            st.clear(letter); // e.g. not NTFS
            return None;
        }
    };
    log(&format!("{letter}: scanned {} entries in {:.2?}", v.len(), t.elapsed()));
    st.idx.write().install(v);

    st.set(letter, "saving to db");
    let meta = j.as_ref().map(|j| (j.id, j.next)).unwrap_or((0, 0));
    rewrite_db(letter, st, db, meta);
    j.map(|j| (j.id, j.next))
}

/// Writes the volume to the db in chunks, holding the read lock only while packing each chunk.
fn rewrite_db(letter: char, st: &State, db: &Db, meta: (u64, i64)) {
    const CH: usize = 200_000;
    let t = Instant::now();
    // a half-written table must never be trusted for journal replay (e.g. if ff is closed mid-save)
    let _ = db.save_meta(letter, (0, 0));
    let mut start = 0usize;
    let mut first = true;
    loop {
        let (rows, done) = {
            let g = st.idx.read();
            let Some(vi) = g.vols.iter().find(|v| v.letter == letter) else { return };
            let end = (start + CH).min(vi.len());
            let mut rows = Vec::with_capacity(end.saturating_sub(start));
            for i in start..end {
                if vi.flags[i] & DEAD != 0 {
                    continue;
                }
                let mut b = Vec::new();
                vi.pack_into(i, &mut b);
                rows.push((vi.frs[i], b));
            }
            (rows, end >= vi.len())
        };
        if let Err(e) = db.put_chunk(letter, first, &rows) {
            log(&format!("{letter}: db write failed: {e}"));
            return;
        }
        first = false;
        start += CH;
        if done {
            break;
        }
    }
    let _ = db.save_meta(letter, meta);
    log(&format!("{letter}: db rewritten in {:.2?}", t.elapsed()));
}

fn poll(f: &File, letter: char, cur: &mut (u64, i64), st: &State, db: &Db) -> io::Result<bool> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut pending: Pending = HashMap::new();
    loop {
        let (next, recs) = usn::read(f, cur.1, cur.0, &mut buf)?;
        let had = !recs.is_empty();
        if had {
            apply_batch(letter, st, recs, &mut pending);
        }
        cur.1 = next;
        if !had {
            break;
        }
    }
    if pending.is_empty() {
        return Ok(false);
    }
    if let Err(e) = db.apply(letter, &pending, *cur) {
        log(&format!("{letter}: db apply failed: {e}"));
    }
    Ok(true)
}

enum Ch {
    Del,
    Up { parent: u64, name: String, dir: bool },
}

fn apply_batch(letter: char, st: &State, recs: Vec<usn::Rec>, pending: &mut Pending) {
    // collapse to the last state per file, keeping first-seen order (parents before children)
    let mut order: Vec<(u64, Ch)> = Vec::new();
    let mut pos: HashMap<u64, usize> = HashMap::new();
    for r in recs {
        if r.frs < 24 {
            continue;
        }
        let ch = if r.reason & usn::REASON_DELETE != 0 {
            Ch::Del
        } else {
            Ch::Up { parent: r.parent, name: r.name, dir: r.attrs & 0x10 != 0 }
        };
        match pos.get(&r.frs) {
            Some(&k) => order[k].1 = ch,
            None => {
                pos.insert(r.frs, order.len());
                order.push((r.frs, ch));
            }
        }
    }

    let mut g = st.idx.write();
    let compacted = {
        let Some(vi) = g.vol_mut(letter) else { return };
        for (frs, ch) in order {
            match ch {
                Ch::Del => {
                    vi.delete(frs);
                    pending.insert(frs, None);
                }
                Ch::Up { parent, name, dir } => {
                    let i = vi.upsert(frs, parent, &name, dir);
                    if !dir {
                        match std::fs::metadata(vi.path(i as u32)) {
                            Ok(m) => vi.set_meta(i, m.len(), st_to_ft(m.modified().unwrap_or(UNIX_EPOCH))),
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                                vi.delete(frs);
                                pending.insert(frs, None);
                                continue;
                            }
                            Err(_) => {}
                        }
                    }
                    let mut b = Vec::new();
                    vi.pack_into(i, &mut b);
                    pending.insert(frs, Some(b));
                }
            }
        }
        if vi.dead > 100_000 && vi.dead * 4 > vi.len() {
            vi.compact();
            true
        } else {
            false
        }
    };
    if compacted {
        g.epoch += 1;
    }
    g.version += 1;
}
