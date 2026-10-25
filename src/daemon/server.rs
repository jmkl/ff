//! Shared state, per-connection query handler and the loopback TCP server.
use crate::daemon::index::{Index, DIR};
use crate::daemon::search;
use crate::proto::*;
use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
use std::io::{self, BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// max hits kept per query (sorted once, then paged)
const CAP: usize = 200_000;

pub struct State {
    pub idx: RwLock<Index>,
    pub status: Mutex<BTreeMap<char, String>>,
    pub token: [u8; 16],
}

impl State {
    pub fn set(&self, c: char, s: &str) {
        self.status.lock().insert(c, s.to_string());
    }
    pub fn clear(&self, c: char) {
        self.status.lock().remove(&c);
    }
    pub fn has(&self, c: char) -> bool {
        self.status.lock().contains_key(&c)
    }
    pub fn text(&self) -> String {
        let g = self.status.lock();
        let busy: Vec<String> = g
            .iter()
            .filter(|(_, s)| s.as_str() != "ready")
            .map(|(c, s)| format!("{c}: {s}"))
            .collect();
        if busy.is_empty() {
            "ready".into()
        } else {
            busy.join(", ")
        }
    }
}

/// Last sorted hit list, so scrolling = slicing, not searching.
struct Cache {
    key: (String, bool, SortKey, bool),
    version: u64,
    epoch: u64,
    at: Instant,
    hits: Vec<u64>,
    truncated: bool,
}

pub struct Session {
    cache: Option<Cache>,
}

impl Session {
    pub fn new() -> Self {
        Session { cache: None }
    }

    pub fn handle(&mut self, st: &State, req: Req) -> Resp {
        match req {
            Req::Status => {
                let (entries, version) = {
                    let g = st.idx.read();
                    (g.entries(), g.version)
                };
                Resp::Status(Status { state: st.text(), entries, version })
            }
            Req::Search(s) => self.search(st, s),
            Req::Hello { .. } | Req::Shutdown => Resp::Ok, // handled at the connection layer
        }
    }

    fn search(&mut self, st: &State, s: SearchReq) -> Resp {
        let t = Instant::now();
        let key = (s.q.clone(), s.regex, s.sort, s.desc);
        let g = st.idx.read();

        // reuse when same query; tolerate a live-updating index for 750 ms so scrolling stays stable
        let reuse = self
            .cache
            .as_ref()
            .map(|c| {
                c.key == key
                    && c.epoch == g.epoch
                    && (c.version == g.version || c.at.elapsed() < Duration::from_millis(750))
            })
            .unwrap_or(false);

        let mut took = 0u64;
        if !reuse {
            let m = match search::compile(&s.q, s.regex) {
                Ok(m) => m,
                Err(e) => return Resp::Err(format!("regex: {e}")),
            };
            let (mut hits, truncated) = match m {
                Some(m) => search::run(&g.vols, &m, CAP),
                None => (Vec::new(), false),
            };
            search::sort(&g.vols, &mut hits, s.sort, s.desc);
            took = t.elapsed().as_micros() as u64;
            self.cache = Some(Cache { key, version: g.version, epoch: g.epoch, at: Instant::now(), hits, truncated });
        }

        let c = self.cache.as_ref().unwrap();
        let total = c.hits.len();
        let off = (s.offset as usize).min(total);
        let end = (off + (s.limit as usize).min(2000)).min(total);
        let rows: Vec<Row> = c.hits[off..end]
            .iter()
            .map(|&h| {
                let (v, i) = search::split(h);
                match g.vols.get(v) {
                    Some(vi) if i < vi.len() => Row {
                        name: vi.name(i).to_string(),
                        path: vi.path(i as u32),
                        size: vi.size[i],
                        mtime: vi.mtime[i],
                        dir: vi.flags[i] & DIR != 0,
                    },
                    _ => Row { name: "?".into(), path: String::new(), size: 0, mtime: 0, dir: false },
                }
            })
            .collect();

        Resp::Page {
            generation: s.generation,
            offset: off as u32,
            total: total as u32,
            truncated: c.truncated,
            took_us: took,
            rows,
        }
    }
}

// ───────────────────────── tcp ─────────────────────────

pub fn serve(l: TcpListener, st: Arc<State>) {
    for c in l.incoming() {
        let Ok(s) = c else { continue };
        let st = st.clone();
        std::thread::spawn(move || {
            let _ = handle(s, st);
        });
    }
}

fn handle(stream: TcpStream, st: Arc<State>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut r = BufReader::new(stream.try_clone()?);
    let mut w = BufWriter::new(stream);

    match recv::<_, Req>(&mut r)? {
        Req::Hello { token } if token == st.token => send(&mut w, &Resp::Ok)?,
        _ => return Ok(()), // wrong token: drop
    }

    let mut session = Session::new(); // per-connection result cache
    loop {
        let req: Req = recv(&mut r)?;
        if let Req::Shutdown = req {
            send(&mut w, &Resp::Ok)?;
            std::process::exit(0);
        }
        let resp = session.handle(&st, req);
        send(&mut w, &resp)?;
    }
}
