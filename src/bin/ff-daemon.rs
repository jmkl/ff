//! ff-daemon: indexes NTFS volumes, persists to redb, serves queries on 127.0.0.1.
//! Needs Administrator (raw volume access). `ff.exe` starts it for you via a UAC prompt.
use ff::daemon;
use ff::proto::{token_path, PORT};
use ff::util::{hex, log};
use std::net::TcpListener;

fn main() {
    // bind first: if the port is taken another daemon is already running (and owns the token)
    let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("ff-daemon: port {PORT} busy - already running?");
            return;
        }
    };

    // auth token: clients read it from the user's profile dir
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).expect("rng");
    std::fs::write(token_path(), hex(&token)).expect("write token");

    let state = match daemon::start(token) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("start failed: {e}"));
            eprintln!("ff-daemon: start failed: {e}");
            return;
        }
    };

    log(&format!("ff-daemon listening on 127.0.0.1:{PORT}"));
    daemon::server::serve(listener, state);
}
