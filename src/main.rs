//! ff - TUI client. Talks to ff-daemon.exe (separate process, runs as Administrator).
//! If the daemon is not running, ff starts it for you (one UAC prompt).
//!
//! Anti-lag design:
//!   * the UI thread never touches the network (a separate IO thread does)
//!   * only the LATEST query is sent (stale ones are coalesced away)
//!   * the daemon sorts once and we fetch just a window of rows (PAGE) around the viewport
//!   * strings are formatted when a page arrives, not per frame
//!   * only visible rows are built; redraw only when something changed
//!   * all pending key events are drained before a single draw

use ff::proto::*;
use ff::util::{fmt_n, fmt_time, hsize};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};
use ratatui::widgets::Row; // explicit: wins over the glob imports (ff::proto also has a `Row`)
use std::io::{self, BufReader, BufWriter};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PAGE: u32 = 400;

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
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, "daemon refused")),
    }
}

/// Launch ff-daemon.exe (next to ff.exe) elevated and hidden.
fn start_daemon() -> io::Result<()> {
    let exe = std::env::current_exe()?.with_file_name("ff-daemon.exe");
    if !exe.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found (keep ff.exe and ff-daemon.exe together)", exe.display()),
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
    eprintln!("ff-daemon is not running - starting it as Administrator (accept the UAC prompt)...");
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

// ───────────────────────── io thread ─────────────────────────

#[derive(Clone)]
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

// ───────────────────────── ui state ─────────────────────────

struct Ui {
    q: String,
    cur: usize, // cursor position inside the query, in chars
    regex: bool,
    list: bool,
    sort: SortKey,
    desc: bool,
    generation: u64,
    total: u32,
    truncated: bool,
    took_us: u64,
    win_start: u32,
    rows: Vec<RowView>,
    pending: Option<(u64, u32)>,

    sel: usize,
    scroll: usize,
    h: usize,

    status: Status,
    last_ver: u64,
    last_refresh: Instant,
    msg: String,
    dirty: bool,
    cmd: mpsc::Sender<Cmd>,
}

impl Ui {
    fn new(cmd: mpsc::Sender<Cmd>) -> Self {
        Ui {
            q: String::new(),
            cur: 0,
            regex: false,
            list: false,
            sort: SortKey::Name,
            desc: false,
            generation: 0,
            total: 0,
            truncated: false,
            took_us: 0,
            win_start: 0,
            rows: Vec::new(),
            pending: None,
            sel: 0,
            scroll: 0,
            h: 20,
            status: Status::default(),
            last_ver: 0,
            last_refresh: Instant::now(),
            msg: String::new(),
            dirty: true,
            cmd,
        }
    }

    fn request(&mut self, offset: u32) {
        self.pending = Some((self.generation, offset));
        let _ = self.cmd.send(Cmd::Search(SearchReq {
            generation: self.generation,
            q: self.q.clone(),
            regex: self.regex,
            sort: self.sort,
            desc: self.desc,
            offset,
            limit: PAGE,
        }));
    }

    /// query / sort / mode changed: restart from the top
    fn query_changed(&mut self) {
        self.generation += 1;
        self.sel = 0;
        self.scroll = 0;
        self.msg.clear();
        if self.q.trim().is_empty() {
            self.total = 0;
            self.rows.clear();
            self.win_start = 0;
            self.pending = None;
        } else {
            self.request(0); // old rows stay on screen until the new page lands (no flicker)
        }
        self.dirty = true;
    }

    /// index changed under us: re-run, keep position
    fn refresh(&mut self) {
        self.generation += 1;
        self.last_refresh = Instant::now();
        let off = (self.scroll as u32).saturating_sub(PAGE / 4);
        self.request(off);
    }

    fn adjust(&mut self) {
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + self.h {
            self.scroll = self.sel + 1 - self.h;
        }
    }

    /// make sure the rows under the viewport are cached; fetch a window if not
    fn ensure_window(&mut self) {
        if self.total == 0 || self.q.trim().is_empty() {
            return;
        }
        let vis_start = self.scroll as u32;
        let vis_end = ((self.scroll + self.h) as u32).min(self.total);
        let have_end = self.win_start + self.rows.len() as u32;
        let covered = vis_start >= self.win_start && vis_end <= have_end;
        if !covered {
            let want = vis_start.saturating_sub(PAGE / 4);
            if self.pending != Some((self.generation, want)) {
                self.request(want);
            }
        }
    }

    fn row_at(&self, i: usize) -> Option<&RowView> {
        let i = i as u32;
        if i >= self.win_start && i < self.win_start + self.rows.len() as u32 {
            self.rows.get((i - self.win_start) as usize)
        } else {
            None
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
                self.adjust();
                self.dirty = true;
            }
            Ev::Status(s) => {
                let changed = s.version != self.last_ver;
                self.last_ver = s.version;
                if self.status.state != s.state || self.status.entries != s.entries {
                    self.dirty = true;
                }
                self.status = s;
                if changed && !self.q.trim().is_empty() && self.last_refresh.elapsed() > Duration::from_millis(300) {
                    self.refresh();
                }
            }
            Ev::Err(e) => {
                self.msg = e;
                self.dirty = true;
            }
            Ev::Down(e) => {
                self.msg = format!("daemon connection lost: {e}");
                self.dirty = true;
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

    fn act(&mut self, which: char) {
        let Some(row) = self.row_at(self.sel) else { return };
        let p = row.path.clone();
        self.msg = match which {
            'c' => match arboard::Clipboard::new().and_then(|mut c| c.set_text(p.clone())) {
                Ok(_) => format!("copied: {p}"),
                Err(e) => format!("copy failed: {e}"),
            },
            'o' => match open::that(&p) {
                Ok(_) => format!("opened: {p}"),
                Err(e) => format!("open failed: {e}"),
            },
            _ => match Command::new("explorer").raw_arg(format!("/select,\"{p}\"")).spawn() {
                Ok(_) => format!("revealed: {p}"),
                Err(e) => format!("reveal failed: {e}"),
            },
        };
    }

    fn qlen(&self) -> usize {
        self.q.chars().count()
    }

    /// byte offset of the char index `ci`
    fn byte_at(&self, ci: usize) -> usize {
        self.q.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.q.len())
    }

    fn word_left(&self) -> usize {
        let ch: Vec<char> = self.q.chars().collect();
        let mut i = self.cur.min(ch.len());
        while i > 0 && ch[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !ch[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let ch: Vec<char> = self.q.chars().collect();
        let mut i = self.cur.min(ch.len());
        while i < ch.len() && ch[i].is_whitespace() {
            i += 1;
        }
        while i < ch.len() && !ch[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// returns true to quit
    fn on_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT);
        if ctrl {
            match k.code {
                KeyCode::Char('c') => return true,
                KeyCode::Char('r') => {
                    self.regex = !self.regex;
                    self.query_changed();
                    return false;
                }
                KeyCode::Char('u') if !self.list => {
                    self.q.clear();
                    self.cur = 0;
                    self.query_changed();
                    return false;
                }
                KeyCode::Char('w') if !self.list => {
                    // delete the word before the cursor
                    let a = self.word_left();
                    if a < self.cur {
                        let (x, y) = (self.byte_at(a), self.byte_at(self.cur));
                        self.q.replace_range(x..y, "");
                        self.cur = a;
                        self.query_changed();
                    }
                    return false;
                }
                KeyCode::Left if !self.list => {
                    self.cur = self.word_left();
                    return false;
                }
                KeyCode::Right if !self.list => {
                    self.cur = self.word_right();
                    return false;
                }
                _ => {}
            }
        }

        if !self.list {
            match k.code {
                KeyCode::Esc => return true,
                KeyCode::Tab | KeyCode::Down | KeyCode::Enter => {
                    if self.total > 0 {
                        self.list = true;
                    }
                }
                KeyCode::Backspace => {
                    if self.cur > 0 {
                        let b = self.byte_at(self.cur - 1);
                        self.q.remove(b);
                        self.cur -= 1;
                        self.query_changed();
                    }
                }
                KeyCode::Delete => {
                    if self.cur < self.qlen() {
                        let b = self.byte_at(self.cur);
                        self.q.remove(b);
                        self.query_changed();
                    }
                }
                KeyCode::Left => self.cur = self.cur.saturating_sub(1),
                KeyCode::Right => self.cur = (self.cur + 1).min(self.qlen()),
                KeyCode::Home => self.cur = 0,
                KeyCode::End => self.cur = self.qlen(),
                KeyCode::Char(c) if !ctrl => {
                    let b = self.byte_at(self.cur);
                    self.q.insert(b, c);
                    self.cur += 1;
                    self.query_changed();
                }
                _ => {}
            }
            return false;
        }

        let last = (self.total as usize).saturating_sub(1);
        let page = self.h.max(1);
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('/') => self.list = false,
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page),
            KeyCode::PageDown => self.sel = (self.sel + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = last,
            KeyCode::Char('1') => self.set_sort(SortKey::Name),
            KeyCode::Char('2') => self.set_sort(SortKey::Date),
            KeyCode::Char('3') => self.set_sort(SortKey::Size),
            KeyCode::Char('4') => self.set_sort(SortKey::Kind),
            KeyCode::Char('c') => self.act('c'),
            KeyCode::Char('o') | KeyCode::Enter => self.act('o'),
            KeyCode::Char('e') => self.act('e'),
            _ => {}
        }
        self.adjust();
        false
    }
}

// ───────────────────────── drawing ─────────────────────────

fn lab(u: &Ui, s: SortKey, txt: &str) -> String {
    if u.sort == s {
        format!("{txt} {}", if u.desc { "▼" } else { "▲" })
    } else {
        txt.to_string()
    }
}

fn draw(f: &mut Frame, u: &mut Ui) {
    let [top, mid, bot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    u.h = (mid.height.saturating_sub(3) as usize).max(1);
    u.adjust();

    let focus = |on: bool| if on { Style::new().cyan() } else { Style::new().dark_gray() };

    // horizontal scroll so the cursor always stays inside the box
    let inner_w = top.width.saturating_sub(2) as usize;
    let hscroll = if inner_w > 0 && u.cur >= inner_w { u.cur + 1 - inner_w } else { 0 };
    f.render_widget(
        Paragraph::new(u.q.as_str()).scroll((0, hscroll as u16)).block(
            Block::bordered()
                .title(if u.regex { " Search (regex) " } else { " Search " })
                .border_style(focus(!u.list)),
        ),
        top,
    );
    if !u.list {
        f.set_cursor_position((top.x + 1 + (u.cur - hscroll) as u16, top.y + 1));
    }

    // only the visible rows are built, and they borrow the cached strings (no per-frame copies)
    let end = (u.scroll + u.h).min(u.total as usize);
    let rows: Vec<Row> = (u.scroll..end)
        .map(|i| match u.row_at(i) {
            Some(r) => Row::new([
                Cell::from(r.name.as_str()),
                Cell::from(r.kind.as_str()),
                Cell::from(r.size.as_str()),
                Cell::from(r.date.as_str()),
                Cell::from(r.path.as_str()),
            ])
            .style(if r.dir { Style::new().blue() } else { Style::new() }),
            None => Row::new(["…"]).style(Style::new().dark_gray()),
        })
        .collect();

    let hdr = Row::new([
        lab(u, SortKey::Name, "1 Name"),
        lab(u, SortKey::Kind, "4 Kind"),
        lab(u, SortKey::Size, "3 Size"),
        lab(u, SortKey::Date, "2 Modified"),
        "Path".to_string(),
    ])
    .style(Style::new().bold());

    let title = format!(
        " {}{} results · {:.1} ms ",
        fmt_n(u.total as u64),
        if u.truncated { "+" } else { "" },
        u.took_us as f64 / 1000.0
    );

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(28),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(17),
            Constraint::Fill(1),
        ],
    )
    .header(hdr)
    .block(Block::bordered().title(title).border_style(focus(u.list)))
    .row_highlight_style(Style::new().reversed());

    let mut st = TableState::default();
    if u.total > 0 && u.sel >= u.scroll {
        st.select(Some(u.sel - u.scroll));
    }
    f.render_stateful_widget(table, mid, &mut st);

    let hint = if u.list {
        "c copy · o open · e reveal · 1-4 sort (again = reverse) · Ctrl+R regex · / search · q quit"
    } else {
        "type to search (space = AND) · ←/→ move · Ctrl+R regex · Tab/↓ browse · Ctrl+U clear · Esc quit"
    };
    let daemon = format!("daemon: {} · {} entries", u.status.state, fmt_n(u.status.entries));
    let line = if u.msg.is_empty() {
        format!("{daemon}  |  {hint}")
    } else {
        format!("{}  |  {daemon}", u.msg)
    };
    f.render_widget(Paragraph::new(line).style(Style::new().dark_gray()), bot);
}

// ───────────────────────── main ─────────────────────────

fn run(term: &mut DefaultTerminal, mut u: Ui, ev_rx: mpsc::Receiver<Ev>) -> io::Result<()> {
    loop {
        // 1. drain ALL pending input, then draw once
        if event::poll(Duration::from_millis(16))? {
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if u.on_key(k) {
                            return Ok(());
                        }
                        u.dirty = true;
                    }
                    Event::Resize(..) => u.dirty = true,
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        // 2. results from the io thread
        while let Ok(ev) = ev_rx.try_recv() {
            u.on_event(ev);
        }
        u.ensure_window();
        // 3. draw only if something changed
        if u.dirty {
            term.draw(|f| draw(f, &mut u))?;
            u.dirty = false;
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // ff --stop : shut the daemon down
    if args.iter().any(|a| a == "--stop") {
        match connect() {
            Ok((_, mut w)) => {
                let _ = send(&mut w, &Req::Shutdown);
                eprintln!("ff-daemon: shutdown sent");
            }
            Err(_) => eprintln!("ff-daemon is not running"),
        }
        return Ok(());
    }

    let (r, w) = match connect_or_start() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ff: {e}");
            return Ok(());
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::spawn(move || io_thread(r, w, cmd_rx, ev_tx));

    let mut ui = Ui::new(cmd_tx.clone());
    let init: String = args.iter().filter(|a| !a.starts_with("--")).cloned().collect::<Vec<_>>().join(" ");
    if !init.is_empty() {
        ui.q = init;
        ui.cur = ui.q.chars().count();
        ui.query_changed();
    }

    let mut term = ratatui::init();
    let res = run(&mut term, ui, ev_rx);
    ratatui::restore();
    let _ = cmd_tx.send(Cmd::Stop);
    res
}
