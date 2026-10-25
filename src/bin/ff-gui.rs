//! ff-gui - GUI client for ff-daemon, ported from the TUI client (src/main.rs).
//!
//! Same anti-lag design as the TUI:
//!   * the egui thread never touches the network (a separate IO thread does)
//!   * only the LATEST query is sent (stale ones are coalesced away in the IO thread)
//!   * the daemon sorts once and we fetch just a window of rows (PAGE) around the viewport
//!   * strings are formatted when a page arrives, not per frame
//!   * only the visible band of rows is built
//!
//! Layout (borderless launcher): drag strip / search bar / results list / details panel.
//! Keys: ↑/↓/PgUp/PgDn/Home/End browse · Enter open · Ctrl+R regex · Ctrl+U clear · F5 reconnect · Esc quit.

use anyhow::Result;
use eframe::egui;
use eframe::egui::{
    pos2, vec2, Align, Align2, Color32, Context, CornerRadius, FontFamily, FontId, Frame, Key,
    Label, Layout, Margin, Panel, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle,
    Ui,
};
use ff::proto::*;
use ff::util::{fmt_n, fmt_time, hsize};
use std::io::{self, BufReader, BufWriter};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PAGE: u32 = 400; // rows fetched per request (same as the TUI)
const ROW_H: f32 = 22.0; // results list row pitch

// ── palette: near-black, thin gray borders, muted text ──────────────────────
const BG: Color32 = Color32::from_rgb(0x11, 0x14, 0x18);
const BOX: Color32 = Color32::from_rgb(0x15, 0x18, 0x1d);
const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x34);
const TEXT: Color32 = Color32::from_rgb(0xcc, 0xd2, 0xda);
const MUTED: Color32 = Color32::from_rgb(0x76, 0x7e, 0x88);
const DIR: Color32 = Color32::from_rgb(0x84, 0x9b, 0xc2); // muted slate blue (TUI colors dirs blue)
const HOVER: Color32 = Color32::from_rgb(0x1b, 0x20, 0x27);
const SELECTED: Color32 = Color32::from_rgb(0x25, 0x2b, 0x34);
const ERROR: Color32 = Color32::from_rgb(0xb3, 0x5a, 0x50); // dim red, errors only

// ───────────────────────── connection (ported from main.rs) ─────────────────────────

type Conn = (BufReader<TcpStream>, BufWriter<TcpStream>);

fn connect() -> io::Result<Conn> {
    let token = read_token().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no token"))?;
    let s = TcpStream::connect_timeout(&([127, 0, 0, 1], PORT).into(), Duration::from_millis(500))?;
    s.set_nodelay(true)?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut w = BufWriter::new(s);
    send(&mut w, &Req::Hello { token })?;
    match recv::<_, Resp>(&mut r)? {
        Resp::Ok => Ok((r, w)),
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "daemon refused")),
    }
}

/// Launch ff-daemon.exe (next to this exe) elevated and hidden.
fn start_daemon() -> io::Result<()> {
    let exe = std::env::current_exe()?.with_file_name("ff-daemon.exe");
    if !exe.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found (keep ff-gui.exe and ff-daemon.exe together)", exe.display()),
        ));
    }
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Start-Process -FilePath '{}' -Verb RunAs -WindowStyle Hidden", exe.display()),
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()?;
    Ok(())
}

fn connect_or_start() -> io::Result<Conn> {
    if let Ok(c) = connect() {
        return Ok(c);
    }
    start_daemon()?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(40) {
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(c) = connect() {
            return Ok(c);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "could not reach ff-daemon (UAC declined? try starting ff-daemon.exe as Administrator)",
    ))
}

/// Connect on a background thread so the window opens instantly (UAC prompt may block).
fn connect_async() -> mpsc::Receiver<io::Result<Conn>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(connect_or_start());
    });
    rx
}

// ───────────────────────── io thread (ported from main.rs) ─────────────────────────

struct RowView {
    name: String,
    kind: String,
    size: String,
    date: String,
    path: String,
    dir: bool,
}

fn view(r: ff::proto::Row) -> RowView {
    let kind = if r.dir {
        "dir".to_string()
    } else {
        r.name.rsplit_once('.').map(|x| x.1.to_lowercase()).unwrap_or_default()
    };
    RowView {
        name: if r.dir { format!("{}\\", r.name) } else { r.name },
        kind,
        size: if r.dir { String::new() } else { hsize(r.size) },
        date: fmt_time(r.mtime),
        path: r.path,
        dir: r.dir,
    }
}

enum Cmd {
    Search(SearchReq),
    Stop,
}

enum Ev {
    Page { generation: u64, offset: u32, total: u32, truncated: bool, took_us: u64, rows: Vec<RowView> },
    Status(Status),
    Err(String),
    Down(String),
}

fn io_thread(mut r: BufReader<TcpStream>, mut w: BufWriter<TcpStream>, rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Ev>) {
    loop {
        let req = match rx.recv_timeout(Duration::from_millis(1000)) {
            Err(mpsc::RecvTimeoutError::Timeout) => Req::Status, // idle heartbeat
            Err(_) => return,
            Ok(mut c) => {
                // coalesce: only the newest command matters
                while let Ok(n) = rx.try_recv() {
                    c = n;
                }
                match c {
                    Cmd::Stop => return,
                    Cmd::Search(s) => Req::Search(s),
                }
            }
        };
        if let Err(e) = send(&mut w, &req) {
            let _ = tx.send(Ev::Down(e.to_string()));
            return;
        }
        match recv::<_, Resp>(&mut r) {
            Ok(Resp::Page { generation, offset, total, truncated, took_us, rows }) => {
                let rows = rows.into_iter().map(view).collect();
                let _ = tx.send(Ev::Page { generation, offset, total, truncated, took_us, rows });
            }
            Ok(Resp::Status(s)) => {
                let _ = tx.send(Ev::Status(s));
            }
            Ok(Resp::Err(e)) => {
                let _ = tx.send(Ev::Err(e));
            }
            Ok(Resp::Ok) => {}
            Err(e) => {
                let _ = tx.send(Ev::Down(e.to_string()));
                return;
            }
        }
    }
}

// ───────────────────────── app state (ported from the TUI's Ui) ─────────────────────────

enum Link {
    Connecting(mpsc::Receiver<io::Result<Conn>>),
    Up { cmd: mpsc::Sender<Cmd>, ev: mpsc::Receiver<Ev> },
    Offline(String),
}

struct FFGui {
    // query
    q: String,
    applied: String, // query text already reflected in a request
    regex: bool,
    sort: SortKey,
    desc: bool,

    // results cache (one daemon-sorted window of PAGE rows)
    generation: u64,
    total: u32,
    truncated: bool,
    took_us: u64,
    win_start: u32,
    rows: Vec<RowView>,
    pending: Option<(u64, u32)>,
    vis: (usize, usize), // absolute row index range visible last frame
    sel: usize,
    follow: bool, // scroll the list to `sel` next frame

    // daemon
    link: Link,
    status: Status,
    last_ver: u64,
    last_refresh: Instant,

    msg: String,
    msg_err: bool,
    focus_search: bool,
}

impl FFGui {
    fn new() -> Self {
        Self {
            q: String::new(),
            applied: String::new(),
            regex: false,
            sort: SortKey::Name,
            desc: false,
            generation: 0,
            total: 0,
            truncated: false,
            took_us: 0,
            win_start: 0,
            rows: Vec::new(),
            pending: None,
            vis: (0, 0),
            sel: 0,
            follow: false,
            link: Link::Connecting(connect_async()),
            status: Status::default(),
            last_ver: 0,
            last_refresh: Instant::now(),
            msg: String::new(),
            msg_err: false,
            focus_search: true,
        }
    }

    fn up(&self) -> bool {
        matches!(self.link, Link::Up { .. })
    }

    // ── search flow (TUi::request / query_changed / refresh / ensure_window) ──

    fn request(&mut self, offset: u32) {
        self.pending = Some((self.generation, offset));
        if let Link::Up { cmd, .. } = &self.link {
            let _ = cmd.send(Cmd::Search(SearchReq {
                generation: self.generation,
                q: self.q.clone(),
                regex: self.regex,
                sort: self.sort,
                desc: self.desc,
                offset,
                limit: PAGE,
            }));
        }
    }

    /// query / mode changed: restart from the top (TUI query_changed)
    fn query_changed(&mut self) {
        self.generation += 1;
        self.sel = 0;
        self.follow = true; // scroll the list back to the top
        self.msg.clear();
        self.msg_err = false;
        if self.q.trim().is_empty() {
            self.total = 0;
            self.truncated = false;
            self.rows.clear();
            self.win_start = 0;
            self.pending = None;
        } else {
            self.request(0);
        }
        self.applied = self.q.clone();
    }

    /// index changed under us: re-run, keep position (TUI refresh)
    fn refresh(&mut self) {
        self.generation += 1;
        self.last_refresh = Instant::now();
        let off = (self.vis.0.saturating_sub(PAGE as usize / 4)) as u32;
        self.request(off);
    }

    fn row_at(&self, i: usize) -> Option<&RowView> {
        let i = i as u32;
        if i >= self.win_start && i < self.win_start + self.rows.len() as u32 {
            self.rows.get((i - self.win_start) as usize)
        } else {
            None
        }
    }

    /// make sure the rows under the viewport (and the selection) are cached (TUI ensure_window)
    fn ensure_window(&mut self) {
        if self.total == 0 || self.q.trim().is_empty() {
            return;
        }
        let (vf, vl) = self.vis;
        let sf = vf.min(self.sel);
        let sl = vl.max(self.sel + 1);
        let have_end = (self.win_start + self.rows.len() as u32) as usize;
        let covered = sf >= self.win_start as usize && sl <= have_end;
        if !covered {
            let want = sf.saturating_sub(PAGE as usize / 4) as u32;
            if self.pending != Some((self.generation, want)) {
                self.request(want);
            }
        }
    }

    fn set_sort(&mut self, s: SortKey) {
        if self.sort == s {
            self.desc = !self.desc;
        } else {
            self.sort = s;
            self.desc = matches!(s, SortKey::Date | SortKey::Size);
        }
        self.query_changed();
    }

    // ── daemon events (TUI on_event) ──

    fn poll_link(&mut self) {
        let outcome = if let Link::Connecting(rx) = &self.link {
            match rx.try_recv() {
                Ok(res) => Some(res.map_err(|e| e.to_string())),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("connect thread died".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            }
        } else {
            None
        };
        match outcome {
            Some(Ok((r, w))) => {
                let (cmd_tx, cmd_rx) = mpsc::channel();
                let (ev_tx, ev_rx) = mpsc::channel();
                std::thread::spawn(move || io_thread(r, w, cmd_rx, ev_tx));
                self.link = Link::Up { cmd: cmd_tx, ev: ev_rx };
                if !self.q.trim().is_empty() {
                    self.query_changed(); // replay whatever was typed while connecting
                }
            }
            Some(Err(e)) => {
                self.link = Link::Offline(e);
            }
            None => {}
        }
    }

    fn drain_events(&mut self) {
        let drained: Vec<Ev> = match &self.link {
            Link::Up { ev, .. } => {
                let mut v = Vec::new();
                while let Ok(e) = ev.try_recv() {
                    v.push(e);
                }
                v
            }
            _ => Vec::new(),
        };
        for e in drained {
            self.on_event(e);
        }
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Page { generation, offset, total, truncated, took_us, rows } => {
                if generation != self.generation {
                    return; // stale
                }
                self.win_start = offset;
                self.rows = rows;
                self.total = total;
                self.truncated = truncated;
                if took_us > 0 {
                    self.took_us = took_us;
                }
                self.pending = None;
                let last = (total as usize).saturating_sub(1);
                if self.sel > last {
                    self.sel = last;
                }
            }
            Ev::Status(s) => {
                let changed = s.version != self.last_ver;
                self.last_ver = s.version;
                self.status = s;
                // index changed under us: re-run, throttled (TUI logic)
                if changed && !self.q.trim().is_empty() && self.last_refresh.elapsed() > Duration::from_millis(300) {
                    self.refresh();
                }
            }
            Ev::Err(e) => {
                self.msg = e;
                self.msg_err = true;
            }
            Ev::Down(e) => {
                self.msg = format!("daemon connection lost: {e}");
                self.msg_err = true;
                self.link = Link::Offline(e);
            }
        }
    }

    /// c copy · o open · e reveal (TUI act)
    fn act(&mut self, which: char) {
        let Some(row) = self.row_at(self.sel) else { return };
        let p = row.path.clone();
        let (msg, err) = match which {
            'c' => match arboard::Clipboard::new().and_then(|mut c| c.set_text(p.clone())) {
                Ok(_) => (format!("copied: {p}"), false),
                Err(e) => (format!("copy failed: {e}"), true),
            },
            'o' => match open::that(&p) {
                Ok(_) => (format!("opened: {p}"), false),
                Err(e) => (format!("open failed: {e}"), true),
            },
            _ => match Command::new("explorer").raw_arg(format!("/select,\"{p}\"")).spawn() {
                Ok(_) => (format!("revealed: {p}"), false),
                Err(e) => (format!("reveal failed: {e}"), true),
            },
        };
        self.msg = msg;
        self.msg_err = err;
    }

    // ── keys ──

    fn keys(&mut self, ctx: &Context) {
        let (esc, up, down, pgup, pgdn, home, end, enter, ctrl_r, ctrl_u, f5) = ctx.input(|i| {
            (
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::Enter),
                i.modifiers.ctrl && i.key_pressed(Key::R),
                i.modifiers.ctrl && i.key_pressed(Key::U),
                i.key_pressed(Key::F5),
            )
        });

        if esc {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if f5 && matches!(self.link, Link::Offline(_)) {
            self.link = Link::Connecting(connect_async());
            self.msg.clear();
            return;
        }
        if ctrl_r {
            self.regex = !self.regex;
            self.query_changed();
        }
        if ctrl_u {
            self.q.clear();
            self.query_changed();
        }

        if self.total == 0 {
            return;
        }
        let last = (self.total as usize).saturating_sub(1);
        let page = self.vis.1.saturating_sub(self.vis.0).max(1);
        let mut moved = false;
        if up {
            self.sel = self.sel.saturating_sub(1);
            moved = true;
        }
        if down {
            self.sel = (self.sel + 1).min(last);
            moved = true;
        }
        if pgup {
            self.sel = self.sel.saturating_sub(page);
            moved = true;
        }
        if pgdn {
            self.sel = (self.sel + page).min(last);
            moved = true;
        }
        if home {
            self.sel = 0;
            moved = true;
        }
        if end {
            self.sel = last;
            moved = true;
        }
        if moved {
            self.follow = true; // keep the selection in view next frame
        }
        if enter {
            self.act('o');
        }
    }

    // ── layout ──

    fn frame(&mut self, ui: &mut Ui) {
        self.poll_link();
        self.drain_events();
        self.keys(ui.ctx());
        if self.q != self.applied {
            self.query_changed();
        }
        self.ensure_window();
        ui.ctx().request_repaint_after(Duration::from_millis(100)); // keep draining daemon events

        self.title_bar(ui);
        ui.add_space(10.0);
        self.search_bar(ui);
        ui.add_space(8.0);
        self.info_line(ui);
        ui.add_space(6.0);

        // details on the right, results fill the rest
        Panel::right("details")
            .resizable(false)
            .exact_size(250.0)
            .show_separator_line(false)
            .frame(Frame::NONE.inner_margin(Margin { left: 10, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| self.details(ui));
        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ui, |ui| self.results(ui));
    }

    /// borderless-window drag strip: "ff" · daemon status · close
    fn title_bar(&mut self, ui: &mut Ui) {
        let resp = ui.allocate_response(vec2(ui.available_width(), 28.0), Sense::drag());
        let rect = resp.rect;
        if resp.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        ui.painter().text(
            rect.left_top() + vec2(2.0, rect.height() * 0.5),
            Align2::LEFT_CENTER,
            "ff",
            FontId::new(12.0, FontFamily::Proportional),
            MUTED,
        );

        let (txt, err) = match &self.link {
            Link::Connecting(_) => ("daemon: connecting…".to_string(), false),
            Link::Up { .. } => (
                format!("daemon: {} · {} entries", self.status.state, fmt_n(self.status.entries)),
                false,
            ),
            Link::Offline(e) => (format!("daemon: {e} · press F5 to retry"), true),
        };
        let close = Rect::from_center_size(rect.right_center() - vec2(14.0, 0.0), vec2(24.0, 28.0));
        let cx = ui.interact(close, egui::Id::unique("ff-gui-close"), Sense::click());
        if cx.clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.painter().text(
            close.left_center() - vec2(10.0, 0.0),
            Align2::RIGHT_CENTER,
            txt,
            FontId::new(11.0, FontFamily::Proportional),
            if err { ERROR } else { MUTED },
        );
        ui.painter().text(
            close.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::new(14.0, FontFamily::Proportional),
            if cx.hovered() { TEXT } else { MUTED },
        );
    }

    fn search_bar(&mut self, ui: &mut Ui) {
        Frame::NONE
            .fill(BOX)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::symmetric(8, 7))
            .show(ui, |ui| {
                // NOTE: do NOT use `with_layout(right_to_left(Align::Center))` here —
                // in egui 0.37 it centers widgets against the FULL remaining height and
                // expands min_rect to match, so this frame swallowed the whole window.
                ui.horizontal(|ui| {
                    // text edit first; reserve room on the right for the toggle + icon
                    let te = TextEdit::singleline(&mut self.q)
                        .hint_text(if self.regex { "Search files (regex)..." } else { "Search files..." })
                        .desired_width(ui.available_width() - 64.0)
                        .show(ui);
                    if self.focus_search {
                        te.response.request_focus();
                        self.focus_search = false;
                    }

                    // regex toggle (Ctrl+R)
                    let re_on = self.regex;
                    let re = ui.allocate_response(vec2(28.0, 16.0), Sense::click());
                    if re_on {
                        ui.painter().rect_filled(re.rect, CornerRadius::same(4), SELECTED);
                    }
                    ui.painter().text(
                        re.rect.center(),
                        Align2::CENTER_CENTER,
                        ".*",
                        FontId::new(12.0, FontFamily::Proportional),
                        if re_on { TEXT } else { MUTED },
                    );
                    if re.clicked() {
                        self.regex = !self.regex;
                        self.query_changed();
                    }

                    ui.label(RichText::new("🔍").size(13.0).color(MUTED));
                });
            });
    }

    fn info_line(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (line, err) = if !self.msg.is_empty() {
                (self.msg.clone(), self.msg_err)
            } else if self.total == 0 && self.q.trim().is_empty() {
                (
                    "↑↓ browse · Enter open · Ctrl+R regex · Ctrl+U clear · Esc quit".to_string(),
                    false,
                )
            } else {
                (
                    format!(
                        "{}{} results · {:.1} ms",
                        fmt_n(self.total as u64),
                        if self.truncated { "+" } else { "" },
                        self.took_us as f64 / 1000.0
                    ),
                    false,
                )
            };
            ui.label(RichText::new(line).size(11.0).color(if err { ERROR } else { MUTED }));
            self.sort_bar(ui);
        });
    }

    /// right-aligned sort switcher (TUI: keys 1-4, clicking again reverses)
    fn sort_bar(&mut self, ui: &mut Ui) {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (key, label) in [
                (SortKey::Kind, "Kind"),
                (SortKey::Size, "Size"),
                (SortKey::Date, "Modified"),
                (SortKey::Name, "Name"),
            ] {
                let on = self.sort == key;
                let txt = if on {
                    format!("{label} {}", if self.desc { "▼" } else { "▲" })
                } else {
                    label.to_string()
                };
                let font = FontId::new(11.0, FontFamily::Proportional);
                let w = ui.painter().layout_no_wrap(txt.clone(), font.clone(), TEXT).size().x + 10.0;
                let resp = ui.allocate_response(vec2(w, 14.0), Sense::click());
                ui.painter().text(
                    pos2(resp.rect.left() + 5.0, resp.rect.center().y),
                    Align2::LEFT_CENTER,
                    &txt,
                    font,
                    if on || resp.hovered() { TEXT } else { MUTED },
                );
                if resp.clicked() {
                    self.set_sort(key);
                }
            }
        });
    }

    fn details(&mut self, ui: &mut Ui) {
        Frame::NONE
            .fill(BOX)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size()); // fill the panel
                ui.label(RichText::new("File Details").size(14.0).strong().color(TEXT));
                hline(ui, 10.0);

                let Some(r) = self.row_at(self.sel) else {
                    ui.add_space(8.0);
                    let t = if !self.up() {
                        "waiting for daemon…"
                    } else if self.total > 0 {
                        "fetching…"
                    } else {
                        "no file selected"
                    };
                    ui.label(RichText::new(t).size(12.0).color(MUTED));
                    return;
                };

                // clone out of the cache so the action buttons can borrow `self`
                let (name, path, size, kind, date) =
                    (r.name.clone(), r.path.clone(), r.size.clone(), r.kind.clone(), r.date.clone());

                detail(ui, "Name", RichText::new(name).size(13.0).color(TEXT));
                detail(ui, "Location", RichText::new(path).monospace().size(11.0).color(MUTED));
                detail(ui, "Size", RichText::new(if size.is_empty() { "—".to_string() } else { size }).size(13.0).color(TEXT));
                detail(ui, "Type", RichText::new(if kind.is_empty() { "file".to_string() } else { kind }).size(13.0).color(TEXT));
                detail(ui, "Modified", RichText::new(if date.is_empty() { "—".to_string() } else { date }).size(13.0).color(TEXT));

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if text_button(ui, "Open", 52.0) {
                        self.act('o');
                    }
                    if text_button(ui, "Reveal", 56.0) {
                        self.act('e');
                    }
                    if text_button(ui, "Copy path", 66.0) {
                        self.act('c');
                    }
                });
            });
    }

    fn results(&mut self, ui: &mut Ui) {
        Frame::NONE
            .fill(BOX)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(6))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size()); // fill the panel

                if self.total == 0 {
                    ui.add_space(12.0);
                    let t = match &self.link {
                        Link::Offline(e) => format!("daemon offline: {e}"),
                        Link::Connecting(_) => "connecting to ff-daemon…".to_string(),
                        Link::Up { .. } => {
                            if self.q.trim().is_empty() {
                                "type to search".to_string()
                            } else if self.pending.is_some() {
                                "searching…".to_string()
                            } else {
                                "no results".to_string()
                            }
                        }
                    };
                    ui.label(RichText::new(t).size(12.0).color(MUTED));
                    return;
                }

                // virtualized list: content height = total rows, but only the visible
                // band is built (like ScrollArea::show_rows, kept in sync with ensure_window)
                ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
                    ui.style_mut().spacing.item_spacing.y = 0.0; // exact ROW_H pitch
                    let x = ui.max_rect().x_range();
                    let top = ui.max_rect().top();
                    ui.set_height(self.total as f32 * ROW_H);

                    let first = ((viewport.min.y / ROW_H).floor().max(0.0)) as usize;
                    let last = (((viewport.max.y / ROW_H).ceil() + 1.0).min(self.total as f32)) as usize;
                    let first = first.min(last);
                    let last = last.max(first);
                    self.vis = (first, last);

                    // keep the selection in view (register a widget at its row and scroll to it,
                    // even when it is outside the visible band)
                    if self.follow {
                        self.follow = false;
                        let y = top + self.sel as f32 * ROW_H;
                        let r = Rect::from_x_y_ranges(x, y..=y + ROW_H);
                        ui.allocate_rect(r, Sense::hover()).scroll_to_me(Some(Align::Center));
                    }

                    // the visible band (scoped for stable widget ids, like show_rows does)
                    let band = Rect::from_x_y_ranges(
                        x,
                        top + first as f32 * ROW_H..=top + last as f32 * ROW_H,
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(band), |vui| {
                        vui.skip_ahead_auto_ids(first);
                        for i in first..last {
                            self.row(vui, i);
                        }
                    });
                });
            });
    }

    fn row(&mut self, ui: &mut Ui, i: usize) {
        let resp = ui.allocate_response(vec2(ui.available_width(), ROW_H), Sense::click());
        let rect = resp.rect;

        let is_sel = i == self.sel;
        let bg = if is_sel {
            SELECTED
        } else if resp.hovered() {
            HOVER
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, CornerRadius::same(4), bg);
        }

        let (label, color) = match self.row_at(i) {
            Some(r) => (r.name.as_str(), if r.dir { DIR } else { TEXT }),
            None => ("…", MUTED), // outside the cached window (TUI shows "…")
        };
        ui.painter().text(
            pos2(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::new(13.0, FontFamily::Proportional),
            color,
        );

        if resp.clicked() {
            self.sel = i;
        }
        if resp.double_clicked() {
            self.sel = i;
            self.act('o');
        }
    }
}

// ───────────────────────── small paint helpers ─────────────────────────

fn detail(ui: &mut Ui, label: &str, value: RichText) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let lab = ui.allocate_response(vec2(64.0, 14.0), Sense::hover());
        ui.painter().text(
            pos2(lab.rect.left(), lab.rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::new(12.0, FontFamily::Proportional),
            MUTED,
        );
        ui.add(Label::new(value).wrap());
    });
}

fn text_button(ui: &mut Ui, label: &str, w: f32) -> bool {
    let resp = ui.allocate_response(vec2(w, 18.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(resp.rect, CornerRadius::same(4), HOVER);
    }
    ui.painter().text(
        resp.rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::new(12.0, FontFamily::Proportional),
        if resp.hovered() { TEXT } else { MUTED },
    );
    resp.clicked()
}

fn hline(ui: &mut Ui, pad_before: f32) {
    ui.add_space(pad_before);
    let resp = ui.allocate_response(vec2(ui.available_width(), 8.0), Sense::hover());
    let y = resp.rect.center().y;
    ui.painter().line_segment(
        [resp.rect.left_top(), pos2(resp.rect.right(), y)],
        Stroke::new(1.0, BORDER),
    );
}

// ───────────────────────── style / main ─────────────────────────

fn style(ctx: &Context) {
    let mut s = egui::Style::default();
    s.text_styles = [
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into_iter()
    .collect();

    let v = &mut s.visuals;
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = BOX; // text edit interior
    v.override_text_color = Some(TEXT);
    v.weak_text_color = Some(MUTED); // hint text
    v.selection.bg_fill = SELECTED;
    v.selection.stroke = Stroke::new(1.0, TEXT);

    for (w, bg, fg) in [
        (&mut v.widgets.noninteractive, Color32::TRANSPARENT, MUTED),
        (&mut v.widgets.inactive, Color32::TRANSPARENT, TEXT),
        (&mut v.widgets.hovered, HOVER, TEXT),
        (&mut v.widgets.active, SELECTED, TEXT),
    ] {
        w.bg_fill = bg;
        w.bg_stroke = Stroke::NONE; // no borders on widgets (the boxes draw their own)
        w.fg_stroke = Stroke::new(1.0, fg);
        w.corner_radius = CornerRadius::same(4);
    }

    ctx.set_global_style(s);
}

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ff")
            .with_inner_size([760.0, 500.0])
            .with_min_inner_size([620.0, 400.0])
            .with_decorations(false) // borderless: no native title bar
            .with_active(true),
        ..Default::default()
    };

    Ok(eframe::run_native(
        "ff",
        options,
        Box::new(|cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(FFGui::new()))
        }),
    )?)
}

impl eframe::App for FFGui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(BG).inner_margin(Margin::same(12)))
            .show(ui, |ui| self.frame(ui));
    }
}

/// window closed: stop the IO thread like the TUI does on quit
impl Drop for FFGui {
    fn drop(&mut self) {
        if let Link::Up { cmd, .. } = &self.link {
            let _ = cmd.send(Cmd::Stop);
        }
    }
}

// ───────────────────────── headless layout regression test ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn offline_gui() -> FFGui {
        FFGui {
            q: String::new(),
            applied: String::new(),
            regex: false,
            sort: SortKey::Name,
            desc: false,
            generation: 0,
            total: 0,
            truncated: false,
            took_us: 0,
            win_start: 0,
            rows: Vec::new(),
            pending: None,
            vis: (0, 0),
            sel: 0,
            follow: false,
            link: Link::Offline("probe".to_string()),
            status: Status::default(),
            last_ver: 0,
            last_refresh: Instant::now(),
            msg: String::new(),
            msg_err: false,
            focus_search: true,
        }
    }

    /// egui 0.37: a `right_to_left(Align::Center)` scope placed directly against a
    /// full-height ui vertically centers its widgets against ALL remaining space and
    /// expands `min_rect` to match — the search box then swallowed the whole window.
    /// Regression: the search bar must stay a single row tall.
    #[test]
    fn search_bar_stays_compact() {
        let mut app = offline_gui();
        egui::__run_test_ui(|ui| {
            let y0 = ui.cursor().top();
            app.search_bar(ui);
            let consumed = ui.cursor().top() - y0;
            assert!(consumed <= 60.0, "search bar consumed {consumed}px - the layout blew up again");
        });
    }

}
