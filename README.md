# ff - fast NTFS file finder (TUI)

Two programs:

- `ff.exe` - the TUI. Runs as a normal user. Starts the daemon for you if needed.
- `ff-daemon.exe` - indexer + database + local server. Runs as Administrator (raw MFT / USN journal access).

## Build

    rustup update stable
    cargo build --release

Both exes end up in `target\release\`. Keep them in the same folder.

## Use

    ff                 open the TUI (starts ff-daemon via a UAC prompt if it is not running)
    ff some words      open with a query already filled in
    ff --stop          shut the daemon down

Typing mode: type to search (space = AND), Ctrl+R toggles regex, Ctrl+U clears,
Tab / Down / Enter go to the list, Esc quits.
List mode: c copy path, o / Enter open, e reveal in Explorer,
1 name, 2 date, 3 size, 4 kind (press again to reverse),
/ back to search, q quit.

## Files

Data lives in `%LOCALAPPDATA%\ff\` : index.redb (database), daemon.token, ff-daemon.log.

## Layout

    src/main.rs            TUI client (IO thread + redraw-on-change)
    src/bin/ff-daemon.rs   daemon entry point (TCP listener on 127.0.0.1:47615)
    src/daemon/            mft.rs (raw $MFT reader), usn.rs (change journal), index.rs (columnar index),
                           search.rs (SIMD memmem / regex scan + sorting), db.rs (redb), worker.rs, server.rs
    src/proto.rs           request/response types + postcard framing
    src/util.rs            helpers
