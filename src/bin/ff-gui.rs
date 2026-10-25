//! ff-gui - GUI client for ff-daemon (compact launcher-style UI, solid #191919).
//!
//! Anti-lag design (same as the TUI) plus an event-driven repaint loop:
//!   * the egui thread never touches the network (a separate IO thread does)
//!   * the IO thread wakes the UI (`request_repaint`) only when something changed,
//!     so an idle window costs ~0 CPU
//!   * only the LATEST query is sent (stale ones are coalesced away in the IO thread)
//!   * the daemon sorts once and we fetch just a window of rows (PAGE) around the viewport
//!   * strings are formatted when a page arrives, not per frame
//!   * only the visible band of rows is built
//!
//! Layout: search row / results list | details panel / footer with key hints.
//! Keys: ↑/↓/PgUp/PgDn/Home/End browse · Enter open · Ctrl+Enter reveal · Ctrl+K copy path ·
//!       Ctrl+R regex · Ctrl+U clear · F5 reconnect · Esc quit.
//!
//! Compact by design: small window, tight rows, details panel auto-hides when the window is narrow.
//!

use anyhow::Result;
use eframe::egui;
use eframe::egui::{
    Align, Align2, Color32, Context, CornerRadius, FontFamily, FontId, Frame, Key, Label, Layout,
    Margin, Modifiers, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, pos2,
    vec2,
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
const ROW_H: f32 = 26.0; // results list row pitch
const TOP_H: f32 = 44.0; // search row height
const FOOT_H: f32 = 28.0; // footer height
const DETAILS_MIN_W: f32 = 560.0; // below this window width the details panel is hidden

// ── palette: everything derives from the window color #191919 ───────────────
const BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19); // window
const BOX: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f); // dropdown button
const POPUP: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f); // dropdown menu
const BORDER: Color32 = Color32::from_rgb(0x2e, 0x2e, 0x2e);
const HOVER: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);
const SELECTED: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2c);
const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xe6);
const MUTED: Color32 = Color32::from_rgb(0x8c, 0x8c, 0x8c);
const DIR: Color32 = Color32::from_rgb(0x84, 0x9b, 0xc2); // muted slate blue
const ACCENT: Color32 = Color32::from_rgb(0xe8, 0x86, 0x62); // paths (orange)
const ERROR: Color32 = Color32::from_rgb(0xc2, 0x64, 0x5a);

const FONT_ROW: FontId = FontId::new(12.5, FontFamily::Proportional);
const FONT_PATH: FontId = FontId::new(11.0, FontFamily::Proportional);
const FONT_SMALL: FontId = FontId::new(10.5, FontFamily::Proportional);
const FONT_UI: FontId = FontId::new(11.5, FontFamily::Proportional);

const SORTS: [(SortKey, &str); 4] = [
    (SortKey::Name, "Name"),
    (SortKey::Date, "Modified"),
    (SortKey::Size, "Size"),
    (SortKey::Kind, "Kind"),
];

// ───────────────────────── connection ─────────────────────────

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
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon refused",
        )),
    }
}

/// Launch ff-daemon.exe (next to this exe) elevated and hidden.
fn start_daemon() -> io::Result<()> {
    let exe = std::env::current_exe()?.with_file_name("ff-daemon.exe");
    if !exe.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} not found (keep ff-gui.exe and ff-daemon.exe together)",
                exe.display()
            ),
        ));
    }
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Start-Process -FilePath '{}' -Verb RunAs -WindowStyle Hidden",
                exe.display()
            ),
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
fn connect_async(ctx: Context) -> mpsc::Receiver<io::Result<Conn>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(connect_or_start());
        ctx.request_repaint();
    });
    rx
}

// ───────────────────────── io thread ─────────────────────────

struct RowView {
    name: String,
    kind: String,
    size: String,
    date: String,
    path: String,
    parent_len: u32, // byte length of the parent directory prefix of `path`
    dir: bool,
}

impl RowView {
    fn parent(&self) -> &str {
        &self.path[..self.parent_len as usize]
    }
}

fn view(r: ff::proto::Row) -> RowView {
    let kind = if r.dir {
        "dir".to_string()
    } else {
        r.name
            .rsplit_once('.')
            .map(|x| x.1.to_lowercase())
            .unwrap_or_default()
    };
    let parent_len = match r.path.rfind('\\') {
        Some(i) if i <= 2 => i + 1, // "C:\"
        Some(i) => i,
        None => 0,
    } as u32;
    RowView {
        name: if r.dir {
            format!("{}\\", r.name)
        } else {
            r.name
        },
        kind,
        size: if r.dir { String::new() } else { hsize(r.size) },
        date: fmt_time(r.mtime),
        path: r.path,
        parent_len,
        dir: r.dir,
    }
}

enum Cmd {
    Search(SearchReq),
    Stop,
}

enum Ev {
    Page {
        generation: u64,
        offset: u32,
        total: u32,
        truncated: bool,
        took_us: u64,
        rows: Vec<RowView>,
    },
    Status(Status),
    Err(String),
    Down(String),
}

fn same_status(a: &Status, b: &Status) -> bool {
    a.version == b.version && a.entries == b.entries && a.state == b.state
}

fn io_thread(
    mut r: BufReader<TcpStream>,
    mut w: BufWriter<TcpStream>,
    rx: mpsc::Receiver<Cmd>,
    tx: mpsc::Sender<Ev>,
    ctx: Context,
) {
    let mut last_status = Status::default();
    // send an event and wake the UI; returns false if the UI is gone
    let emit = |ev: Ev| -> bool {
        let ok = tx.send(ev).is_ok();
        ctx.request_repaint();
        ok
    };
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
            emit(Ev::Down(e.to_string()));
            return;
        }
        let ok = match recv::<_, Resp>(&mut r) {
            Ok(Resp::Page {
                generation,
                offset,
                total,
                truncated,
                took_us,
                rows,
            }) => {
                let rows = rows.into_iter().map(view).collect();
                emit(Ev::Page {
                    generation,
                    offset,
                    total,
                    truncated,
                    took_us,
                    rows,
                })
            }
            Ok(Resp::Status(s)) => {
                // the heartbeat only wakes the UI when something actually changed
                if same_status(&s, &last_status) {
                    true
                } else {
                    last_status = s.clone();
                    emit(Ev::Status(s))
                }
            }
            Ok(Resp::Err(e)) => emit(Ev::Err(e)),
            Ok(Resp::Ok) => true,
            Err(e) => {
                emit(Ev::Down(e.to_string()));
                return;
            }
        };
        if !ok {
            return;
        }
    }
}

// ───────────────────────── app state ─────────────────────────

enum Link {
    Connecting(mpsc::Receiver<io::Result<Conn>>),
    Up {
        cmd: mpsc::Sender<Cmd>,
        ev: mpsc::Receiver<Ev>,
    },
    Offline(String),
}

struct FFGui {
    ctx: Context,

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
    fn new(ctx: Context) -> Self {
        Self {
            link: Link::Connecting(connect_async(ctx.clone())),
            ctx,
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

    fn query_empty(&self) -> bool {
        self.q.trim().is_empty()
    }

    fn set_msg(&mut self, msg: String, err: bool) {
        self.msg = msg;
        self.msg_err = err;
    }

    // ── search flow ──

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

    /// query / mode changed: restart from the top
    fn query_changed(&mut self) {
        self.generation += 1;
        self.sel = 0;
        self.follow = true; // scroll the list back to the top
        self.set_msg(String::new(), false);
        if self.query_empty() {
            self.total = 0;
            self.truncated = false;
            self.rows.clear();
            self.win_start = 0;
            self.pending = None;
        } else {
            self.request(0);
        }
        self.applied.clone_from(&self.q);
    }

    /// index changed under us: re-run, keep position
    fn refresh(&mut self) {
        self.generation += 1;
        self.last_refresh = Instant::now();
        let off = self.vis.0.saturating_sub(PAGE as usize / 4) as u32;
        self.request(off);
    }

    fn row_at(&self, i: usize) -> Option<&RowView> {
        let rel = (i as u32).checked_sub(self.win_start)?;
        self.rows.get(rel as usize)
    }

    /// make sure the rows under the viewport (and the selection) are cached
    fn ensure_window(&mut self) {
        if self.total == 0 || self.query_empty() {
            return;
        }
        let (vf, vl) = self.vis;
        let sf = vf.min(self.sel);
        let sl = vl.max(self.sel + 1);
        let have_end = self.win_start as usize + self.rows.len();
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

    // ── daemon events ──

    fn poll_link(&mut self) {
        let outcome = match &self.link {
            Link::Connecting(rx) => match rx.try_recv() {
                Ok(res) => Some(res.map_err(|e| e.to_string())),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("connect thread died".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            _ => None,
        };
        match outcome {
            Some(Ok((r, w))) => {
                let (cmd_tx, cmd_rx) = mpsc::channel();
                let (ev_tx, ev_rx) = mpsc::channel();
                let ctx = self.ctx.clone();
                std::thread::spawn(move || io_thread(r, w, cmd_rx, ev_tx, ctx));
                self.link = Link::Up {
                    cmd: cmd_tx,
                    ev: ev_rx,
                };
                if !self.query_empty() {
                    self.query_changed(); // replay whatever was typed while connecting
                }
            }
            Some(Err(e)) => self.link = Link::Offline(e),
            None => {}
        }
    }

    fn drain_events(&mut self) {
        let Link::Up { ev, .. } = &self.link else {
            return;
        };
        let drained: Vec<Ev> = ev.try_iter().collect();
        for e in drained {
            self.on_event(e);
        }
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Page {
                generation,
                offset,
                total,
                truncated,
                took_us,
                rows,
            } => {
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
                self.sel = self.sel.min((total as usize).saturating_sub(1));
            }
            Ev::Status(s) => {
                let changed = s.version != self.last_ver;
                self.last_ver = s.version;
                self.status = s;
                // index changed under us: re-run, throttled
                if changed
                    && !self.query_empty()
                    && self.last_refresh.elapsed() > Duration::from_millis(300)
                {
                    self.refresh();
                }
            }
            Ev::Err(e) => self.set_msg(e, true),
            Ev::Down(e) => {
                self.set_msg(format!("daemon connection lost: {e}"), true);
                self.link = Link::Offline(e);
            }
        }
    }

    /// c copy path · o open · e reveal in explorer
    fn act(&mut self, which: char) {
        let Some(row) = self.row_at(self.sel) else {
            return;
        };
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
            _ => match Command::new("explorer")
                .raw_arg(format!("/select,\"{p}\""))
                .spawn()
            {
                Ok(_) => (format!("revealed: {p}"), false),
                Err(e) => (format!("reveal failed: {e}"), true),
            },
        };
        self.set_msg(msg, err);
    }

    // ── keys ──

    fn keys(&mut self, ctx: &Context) {
        // consume_key so the search box doesn't also see ↑/↓/Enter/Home/End
        let k = |i: &mut egui::InputState, m: Modifiers, key: Key| i.consume_key(m, key);
        let (esc, f5, ctrl_r, ctrl_u, ctrl_k, ctrl_enter) = ctx.input_mut(|i| {
            (
                k(i, Modifiers::NONE, Key::Escape),
                k(i, Modifiers::NONE, Key::F5),
                k(i, Modifiers::CTRL, Key::R),
                k(i, Modifiers::CTRL, Key::U),
                k(i, Modifiers::CTRL, Key::K),
                k(i, Modifiers::CTRL, Key::Enter), // before plain Enter
            )
        });
        let (up, down, pgup, pgdn, home, end, enter) = ctx.input_mut(|i| {
            (
                k(i, Modifiers::NONE, Key::ArrowUp),
                k(i, Modifiers::NONE, Key::ArrowDown),
                k(i, Modifiers::NONE, Key::PageUp),
                k(i, Modifiers::NONE, Key::PageDown),
                k(i, Modifiers::NONE, Key::Home),
                k(i, Modifiers::NONE, Key::End),
                k(i, Modifiers::NONE, Key::Enter),
            )
        });

        if esc {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if f5 && matches!(self.link, Link::Offline(_)) {
            self.link = Link::Connecting(connect_async(self.ctx.clone()));
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
        let old = self.sel;
        if up {
            self.sel = self.sel.saturating_sub(1);
        }
        if down {
            self.sel = (self.sel + 1).min(last);
        }
        if pgup {
            self.sel = self.sel.saturating_sub(page);
        }
        if pgdn {
            self.sel = (self.sel + page).min(last);
        }
        if home {
            self.sel = 0;
        }
        if end {
            self.sel = last;
        }
        if self.sel != old {
            self.follow = true; // keep the selection in view next frame
        }
        if ctrl_enter {
            self.act('e');
        } else if enter {
            self.act('o');
        }
        if ctrl_k {
            self.act('c');
        }
    }

    // ── layout ──

    fn frame(&mut self, ui: &mut Ui) {
        self.poll_link();
        self.drain_events();
        self.keys(&ui.ctx().clone());
        if self.q != self.applied {
            self.query_changed();
        }
        self.ensure_window();

        // carve the window into explicit rects (no panels: cheap and predictable)
        let full = ui.max_rect();
        let top = Rect::from_min_size(full.min, vec2(full.width(), TOP_H));
        let foot = Rect::from_min_max(pos2(full.left(), full.bottom() - FOOT_H), full.max);
        let body = Rect::from_min_max(
            pos2(full.left(), top.bottom()),
            pos2(full.right(), foot.top()),
        );
        let show_det = full.width() >= DETAILS_MIN_W;
        let dw = if show_det {
            (full.width() * 0.36).clamp(190.0, 250.0)
        } else {
            0.0
        };
        let list = Rect::from_min_max(body.min, pos2(body.right() - dw, body.bottom()));
        let det = Rect::from_min_max(pos2(list.right(), body.top()), body.max);

        // separators
        let line = Stroke::new(1.0, BORDER);
        let p = ui.painter();
        p.line_segment(
            [
                pos2(full.left(), top.bottom()),
                pos2(full.right(), top.bottom()),
            ],
            line,
        );
        p.line_segment(
            [
                pos2(full.left(), foot.top()),
                pos2(full.right(), foot.top()),
            ],
            line,
        );
        if show_det {
            p.line_segment(
                [
                    pos2(list.right(), body.top()),
                    pos2(list.right(), body.bottom()),
                ],
                line,
            );
        }

        self.search_row(ui, top);
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(list.shrink2(vec2(5.0, 4.0))),
            |ui| self.results(ui),
        );
        if show_det {
            ui.scope_builder(egui::UiBuilder::new().max_rect(det.shrink(12.0)), |ui| {
                self.details(ui)
            });
        }
        if let Some(a) = self.footer(ui, foot) {
            self.act(a);
        }
    }

    /// back · search icon · query · clear · regex toggle · sort dropdown (whole row drags the window)
    fn search_row(&mut self, ui: &mut Ui, rect: Rect) {
        // drag handle underneath everything (widgets added later win hit-testing)
        let drag = ui.interact(rect, egui::Id::new("ff-gui-drag"), Sense::drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        let mut clear = false;
        let mut toggle_re = false;
        let mut pick: Option<SortKey> = None;

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(12.0, 0.0)))
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                // magnifier drawn with the painter (no emoji font, consistent color)
                let (ic, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                let c = ic.center() - vec2(1.5, 1.5);
                let pen = Stroke::new(1.5, MUTED);
                ui.painter().circle_stroke(c, 5.0, pen);
                ui.painter()
                    .line_segment([c + vec2(3.6, 3.6), c + vec2(7.5, 7.5)], pen);

                let hint = if self.regex {
                    "Search files (regex)..."
                } else {
                    "Search files..."
                };
                let w = (ui.available_width() - 150.0).max(80.0);
                let te = TextEdit::singleline(&mut self.q)
                    .font(TextStyle::Heading)
                    .hint_text(hint)
                    .desired_width(w)
                    .show(ui);
                if self.focus_search {
                    te.response.request_focus();
                    self.focus_search = false;
                }

                // clear
                if !self.q.is_empty() && icon_button(ui, "×", 20.0, 24.0, 15.0) {
                    clear = true;
                }

                // regex toggle (Ctrl+R)
                let re = ui.allocate_response(vec2(26.0, 20.0), Sense::click());
                if self.regex {
                    ui.painter()
                        .rect_filled(re.rect, CornerRadius::same(5), SELECTED);
                } else if re.hovered() {
                    ui.painter()
                        .rect_filled(re.rect, CornerRadius::same(5), HOVER);
                }
                ui.painter().text(
                    re.rect.center(),
                    Align2::CENTER_CENTER,
                    ".*",
                    FONT_UI,
                    if self.regex { TEXT } else { MUTED },
                );
                toggle_re = re.clicked();

                // sort dropdown
                ui.scope(|ui| {
                    let w = &mut ui.visuals_mut().widgets;
                    for s in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                        s.bg_fill = BOX;
                        s.weak_bg_fill = BOX;
                        s.bg_stroke = Stroke::new(1.0, BORDER);
                        s.corner_radius = CornerRadius::same(5);
                        s.expansion = 0.0;
                    }
                    ui.spacing_mut().button_padding = vec2(6.0, 2.0);
                    ui.spacing_mut().interact_size.y = 20.0;
                    let name = SORTS
                        .iter()
                        .find(|s| s.0 == self.sort)
                        .map(|s| s.1)
                        .unwrap_or("Name");
                    let arrow = if self.desc { "↓" } else { "↑" };
                    egui::ComboBox::from_id_salt("ff-sort")
                        .width(64.0)
                        .selected_text(RichText::new(format!("{name} {arrow}")).size(11.5))
                        .show_ui(ui, |ui| {
                            for (key, label) in SORTS {
                                let on = self.sort == key;
                                let txt = if on {
                                    format!("{label} {arrow}")
                                } else {
                                    label.to_string()
                                };
                                if ui
                                    .selectable_label(on, RichText::new(txt).size(11.5))
                                    .clicked()
                                {
                                    pick = Some(key); // clicking the active one flips the direction
                                }
                            }
                        });
                });
            },
        );

        if clear {
            self.q.clear();
            self.focus_search = true;
        }
        if toggle_re {
            self.regex = !self.regex;
            self.query_changed();
        }
        if let Some(k) = pick {
            self.set_sort(k);
        }
    }

    fn results(&mut self, ui: &mut Ui) {
        if self.total == 0 {
            ui.add_space(14.0);
            let t = match &self.link {
                Link::Offline(e) => format!("daemon offline: {e}"),
                Link::Connecting(_) => "connecting to ff-daemon…".to_string(),
                Link::Up { .. } if self.query_empty() => "type to search".to_string(),
                Link::Up { .. } if self.pending.is_some() => "searching…".to_string(),
                Link::Up { .. } => "no results".to_string(),
            };
            ui.vertical_centered(|ui| ui.label(RichText::new(t).size(11.5).color(MUTED)));
            return;
        }

        // virtualized list: content height = total rows, but only the visible
        // band is built (like ScrollArea::show_rows, kept in sync with ensure_window)
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                ui.style_mut().spacing.item_spacing.y = 0.0; // exact ROW_H pitch
                let x = ui.max_rect().x_range();
                let top = ui.max_rect().top();
                ui.set_height(self.total as f32 * ROW_H);

                let total = self.total as usize;
                let last = ((viewport.max.y / ROW_H).ceil() as usize + 1).min(total);
                let first = ((viewport.min.y / ROW_H).floor().max(0.0) as usize).min(last);
                self.vis = (first, last);

                // keep the selection in view (register a widget at its row and scroll to it,
                // even when it is outside the visible band)
                if self.follow {
                    self.follow = false;
                    let y = top + self.sel as f32 * ROW_H;
                    let r = Rect::from_x_y_ranges(x, y..=y + ROW_H);
                    ui.allocate_rect(r, Sense::hover())
                        .scroll_to_me(Some(Align::Center));
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
    }

    fn row(&mut self, ui: &mut Ui, i: usize) {
        let resp = ui.allocate_response(vec2(ui.available_width(), ROW_H), Sense::click());
        let rect = resp.rect;
        let painter = ui.painter().with_clip_rect(rect);
        let cy = rect.center().y;

        if i == self.sel {
            painter.rect_filled(
                rect.shrink2(vec2(0.0, 1.0)),
                CornerRadius::same(5),
                SELECTED,
            );
        } else if resp.hovered() {
            painter.rect_filled(rect.shrink2(vec2(0.0, 1.0)), CornerRadius::same(5), HOVER);
        }

        match self.row_at(i) {
            Some(r) => {
                painter.text(
                    pos2(rect.left() + 9.0, cy),
                    Align2::LEFT_CENTER,
                    if r.dir { "📁" } else { "📄" },
                    FONT_ROW,
                    if r.dir { DIR } else { MUTED },
                );
                let name = painter.text(
                    pos2(rect.left() + 30.0, cy),
                    Align2::LEFT_CENTER,
                    &r.name,
                    FONT_ROW,
                    if r.dir { DIR } else { TEXT },
                );
                // parent folder, muted, right after the name (clipped by the row)
                painter.text(
                    pos2(name.right() + 10.0, cy),
                    Align2::LEFT_CENTER,
                    r.parent(),
                    FONT_PATH,
                    MUTED,
                );
            }
            None => {
                // outside the cached window
                painter.text(
                    pos2(rect.left() + 30.0, cy),
                    Align2::LEFT_CENTER,
                    "…",
                    FONT_ROW,
                    MUTED,
                );
            }
        }

        if resp.clicked() || resp.double_clicked() {
            self.sel = i;
        }
        if resp.double_clicked() {
            self.act('o');
        }
    }

    /// preview tile · name · folder (accent) · size / type / modified
    fn details(&self, ui: &mut Ui) {
        let Some(r) = self.row_at(self.sel) else {
            let t = if !self.up() {
                "waiting for daemon…"
            } else if self.total > 0 {
                "fetching…"
            } else {
                "no file selected"
            };
            ui.add_space(4.0);
            ui.label(RichText::new(t).size(11.5).color(MUTED));
            return;
        };

        // preview tile: folder glyph or the extension
        let (tile, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(tile, CornerRadius::same(7), SELECTED);
        if r.dir {
            p.text(
                tile.center(),
                Align2::CENTER_CENTER,
                "📁",
                FontId::new(22.0, FontFamily::Proportional),
                DIR,
            );
        } else if r.kind.is_empty() {
            p.text(
                tile.center(),
                Align2::CENTER_CENTER,
                "📄",
                FontId::new(22.0, FontFamily::Proportional),
                MUTED,
            );
        } else {
            let ext: String = r.kind.chars().take(5).collect::<String>().to_uppercase();
            p.text(
                tile.center(),
                Align2::CENTER_CENTER,
                ext,
                FontId::new(13.0, FontFamily::Proportional),
                ACCENT,
            );
        }

        ui.add_space(8.0);
        ui.add(Label::new(RichText::new(&r.name).size(12.0).color(TEXT)).wrap());
        ui.add_space(2.0);
        ui.add(Label::new(RichText::new(r.parent()).size(11.5).color(ACCENT)).wrap());

        hline(ui, 6.0);
        let or = |s: &str, d: &str| {
            if s.is_empty() {
                d.to_owned()
            } else {
                s.to_owned()
            }
        };
        field(ui, "Size", &or(&r.size, "—"));
        let kind = if r.dir {
            "Folder".to_owned()
        } else if r.kind.is_empty() {
            "File".to_owned()
        } else {
            format!("{} file", r.kind.to_uppercase())
        };
        field(ui, "Type", &kind);
        field(ui, "Modified", &or(&r.date, "—"));
    }

    /// left: status / message · right: clickable key hints. Returns the clicked action.
    fn footer(&mut self, ui: &mut Ui, rect: Rect) -> Option<char> {
        let mut action = None;
        let mut left_edge = rect.right();
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(vec2(12.0, 0.0)))
                .layout(Layout::right_to_left(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                // right_to_left: the first item added is the right-most
                if hint(ui, "Copy", &["Ctrl", "K"]) {
                    action = Some('c');
                }
                if hint(ui, "Reveal", &["Ctrl", "⏎"]) {
                    action = Some('e');
                }
                if hint(ui, "Open", &["⏎"]) {
                    action = Some('o');
                }
                left_edge = ui.min_rect().left();
            },
        );

        let owned: String;
        let (txt, color): (&str, Color32) = if !self.msg.is_empty() {
            (&self.msg, if self.msg_err { ERROR } else { MUTED })
        } else {
            match &self.link {
                Link::Connecting(_) => ("daemon: connecting…", MUTED),
                Link::Offline(e) => {
                    owned = format!("daemon: {e} · press F5 to retry");
                    (&owned, ERROR)
                }
                Link::Up { .. } if !self.query_empty() && self.total > 0 => {
                    owned = format!(
                        "{}{} results · {:.1} ms",
                        fmt_n(self.total as u64),
                        if self.truncated { "+" } else { "" },
                        self.took_us as f64 / 1000.0
                    );
                    (&owned, MUTED)
                }
                Link::Up { .. } => {
                    owned = format!(
                        "ff · {} · {} entries",
                        self.status.state,
                        fmt_n(self.status.entries)
                    );
                    (&owned, MUTED)
                }
            }
        };
        let clip = Rect::from_min_max(
            rect.min,
            pos2((left_edge - 14.0).max(rect.left()), rect.bottom()),
        );
        ui.painter().with_clip_rect(clip).text(
            pos2(rect.left() + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            txt,
            FONT_UI,
            color,
        );
        action
    }
}

// ───────────────────────── small paint helpers ─────────────────────────

fn field(ui: &mut Ui, label: &str, value: &str) {
    ui.add_space(5.0);
    ui.label(RichText::new(label).size(10.5).color(MUTED));
    ui.label(RichText::new(value).size(12.0).color(TEXT));
}

fn icon_button(ui: &mut Ui, glyph: &str, w: f32, h: f32, size: f32) -> bool {
    let resp = ui.allocate_response(vec2(w, h), Sense::click());
    ui.painter().text(
        resp.rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::new(size, FontFamily::Proportional),
        if resp.hovered() { TEXT } else { MUTED },
    );
    resp.clicked()
}

/// "Open file [⏎]" style footer hint; the whole thing is clickable
fn hint(ui: &mut Ui, label: &str, keys: &[&str]) -> bool {
    let p = ui.painter().clone();
    let lw = p.layout_no_wrap(label.to_string(), FONT_UI, TEXT).size().x;
    let kw: Vec<f32> = keys
        .iter()
        .map(|k| (p.layout_no_wrap(k.to_string(), FONT_SMALL, TEXT).size().x + 10.0).max(18.0))
        .collect();
    let total = lw + 6.0 + kw.iter().sum::<f32>() + 3.0 * (keys.len() as f32 - 1.0);
    let resp = ui.allocate_response(vec2(total, 20.0), Sense::click());
    let hot = resp.hovered();
    let cy = resp.rect.center().y;
    p.text(
        pos2(resp.rect.left(), cy),
        Align2::LEFT_CENTER,
        label,
        FONT_UI,
        if hot { TEXT } else { MUTED },
    );
    let mut x = resp.rect.left() + lw + 6.0;
    for (k, w) in keys.iter().zip(kw) {
        let chip = Rect::from_min_size(pos2(x, cy - 8.0), vec2(w, 16.0));
        p.rect_filled(chip, CornerRadius::same(4), SELECTED);
        p.text(chip.center(), Align2::CENTER_CENTER, *k, FONT_SMALL, TEXT);
        x += w + 3.0;
    }
    resp.clicked()
}

fn hline(ui: &mut Ui, pad_before: f32) {
    ui.add_space(pad_before);
    let resp = ui.allocate_response(vec2(ui.available_width(), 4.0), Sense::hover());
    let y = resp.rect.center().y;
    ui.painter().line_segment(
        [pos2(resp.rect.left(), y), pos2(resp.rect.right(), y)],
        Stroke::new(1.0, BORDER),
    );
}

// ───────────────────────── style / main ─────────────────────────

fn style(ctx: &Context) {
    let mut s = egui::Style::default();
    s.text_styles = [
        (TextStyle::Body, FontId::new(12.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (TextStyle::Small, FONT_SMALL),
        (
            TextStyle::Heading,
            FontId::new(15.0, FontFamily::Proportional),
        ), // search box
        (
            TextStyle::Monospace,
            FontId::new(11.0, FontFamily::Monospace),
        ),
    ]
    .into_iter()
    .collect();

    let v = &mut s.visuals;
    v.panel_fill = BG;
    v.window_fill = POPUP; // dropdown menus
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.extreme_bg_color = BG; // text edit background (same as the window)
    v.override_text_color = Some(TEXT);
    v.weak_text_color = Some(MUTED); // hint text
    v.selection.bg_fill = SELECTED;
    v.selection.stroke = Stroke::NONE; // no focus ring around the search box
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;

    for (w, bg, fg) in [
        (&mut v.widgets.noninteractive, Color32::TRANSPARENT, MUTED),
        (&mut v.widgets.inactive, Color32::TRANSPARENT, TEXT),
        (&mut v.widgets.hovered, HOVER, TEXT),
        (&mut v.widgets.active, SELECTED, TEXT),
    ] {
        w.bg_fill = bg;
        w.weak_bg_fill = bg;
        w.bg_stroke = Stroke::NONE; // no borders on widgets (surfaces draw their own)
        w.fg_stroke = Stroke::new(1.0, fg);
        w.corner_radius = CornerRadius::same(5);
    }

    ctx.set_global_style(s);
}

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ff")
            .with_inner_size([600.0, 340.0])
            .with_min_inner_size([400.0, 240.0])
            .with_decorations(false) // borderless: no native title bar
            .with_active(true),
        ..Default::default()
    };

    Ok(eframe::run_native(
        "ff",
        options,
        Box::new(|cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(FFGui::new(cc.egui_ctx.clone())))
        }),
    )?)
}

impl eframe::App for FFGui {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(
                Frame::NONE
                    .fill(BG)
                    .stroke(Stroke::new(1.0, BORDER))
                    .inner_margin(Margin::ZERO),
            )
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
