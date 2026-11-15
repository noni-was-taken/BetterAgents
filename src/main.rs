use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame,
    crossterm::{execute, terminal::supports_keyboard_enhancement},
    crossterm::event::{DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    crossterm::event::{self, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    process::Command,
    sync::mpsc::{self, Sender},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tui_term::{vt100::{self, MouseProtocolEncoding, MouseProtocolMode}, widget::PseudoTerminal};

// [accent, muted, highlight bg, text]; follows the wallpaper via Noctalia's generated theme file
const AUTHOR: &str = "noni-was-taken";

/// bumped whenever background threads produce something worth redrawing
static TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn bump() { TICK.fetch_add(1, Relaxed); }

static THEME: Mutex<[Color; 4]> = Mutex::new([Color::Rgb(137, 180, 250), Color::Rgb(110, 110, 125), Color::Rgb(49, 50, 68), Color::Rgb(205, 214, 244)]);

fn accent() -> Color { THEME.lock().unwrap()[0] }
fn gray() -> Color { THEME.lock().unwrap()[1] }
fn hl() -> Color { THEME.lock().unwrap()[2] }
fn text() -> Color { THEME.lock().unwrap()[3] }

fn load_theme() {
    let Ok(t) = std::fs::read_to_string(std::env::var("HOME").unwrap_or_default() + "/.config/hypr/config/theme.lua") else { return };
    let get = |k: &str| -> Option<(u8, u8, u8)> {
        let l = t.lines().find(|l| l.starts_with(k))?;
        let h = &l[l.find("rgba(")? + 5..][..6];
        Some((u8::from_str_radix(&h[0..2], 16).ok()?, u8::from_str_radix(&h[2..4], 16).ok()?, u8::from_str_radix(&h[4..6], 16).ok()?))
    };
    let (Some(p), Some(on), Some(out)) = (get("THEME_SINGLE"), get("THEME_STACK"), get("THEME_UNFOCUSED")) else { return };
    let mix = |a: (u8, u8, u8), b: (u8, u8, u8), t: f32| Color::Rgb((a.0 as f32 * (1. - t) + b.0 as f32 * t) as u8, (a.1 as f32 * (1. - t) + b.1 as f32 * t) as u8, (a.2 as f32 * (1. - t) + b.2 as f32 * t) as u8);
    *THEME.lock().unwrap() = [Color::Rgb(p.0, p.1, p.2), mix(out, on, 0.35), mix((16, 16, 20), p, 0.22), Color::Rgb(on.0, on.1, on.2)];
}

struct Agent {
    id: String,
    name: String,
    cwd: String,
    state: String,
    started: u64,
    detail: String,
    result: String,
    tokens: u64,
    color: Color,
}

struct Sess {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    dead: Arc<AtomicBool>,
    size: (u16, u16),
}

enum Mode {
    Nav,
    Focus,
    Prompt(String),
    Search,
    Confirm,
}

enum Row {
    Head(String, Color),
    Agent(usize),
}

struct App {
    agents: Vec<Agent>,
    cats: HashMap<String, String>,
    filter: String, // search text; empty = show everything
    by_category: bool,
    sel: String,
    mode: Mode,
    sess: HashMap<String, Sess>,
    msg: String,
    accounts: Vec<(u32, String)>,
    acct: Option<u32>, // None = default profile (your normal sessions)
    full: bool, // chat takes the whole window
    win_focused: bool, // is this terminal window focused (focus-change reporting)
    chat: Option<Rect>, // where the embedded chat is drawn, for mouse forwarding
}

/// `claude <args>` for the default profile, or `cswap run N -- <args>` for account N's isolated profile.
fn cl(acct: Option<u32>, args: &[&str]) -> Command {
    let mut c;
    match acct {
        Some(n) => { c = Command::new("cswap"); c.args(["run", &n.to_string(), "--"]); }
        None => c = Command::new("claude"),
    }
    c.args(args);
    c
}

/// Managed cswap accounts as (number, label). Empty when cswap isn't installed or has none.
fn load_accounts() -> Vec<(u32, String)> {
    let Ok(o) = Command::new("cswap").args(["list", "--json"]).stdin(std::process::Stdio::null()).output() else { return vec![] };
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
    v["accounts"].as_array().map_or(vec![], |a| {
        a.iter().filter_map(|x| {
            let n = x["number"].as_u64()? as u32;
            Some((n, x["alias"].as_str().filter(|s| !s.is_empty()).map_or(format!("Account {n}"), String::from)))
        }).collect()
    })
}

/// Where account N's isolated profile keeps its job state (cswap: <data>/claude-swap/sessions/<N>-<slug>/).
fn jobs_dir(acct: Option<u32>) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let Some(n) = acct else { return format!("{home}/.claude/jobs/") };
    let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share")) + "/claude-swap/sessions/";
    let pre = format!("{n}-");
    std::fs::read_dir(&base).ok().and_then(|d| d.flatten().find(|e| e.file_name().to_string_lossy().starts_with(&pre)))
        .map_or(String::new(), |e| format!("{}/jobs/", e.path().display()))
}

fn load(acct: Option<u32>) -> Vec<Agent> {
    let Ok(out) = cl(acct, &["agents", "--json", "--all"]).stdin(std::process::Stdio::null()).output() else {
        return vec![];
    };
    let list: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    // `--json` without --all lists only live sessions; everything else is offline
    let live: Vec<String> = cl(acct, &["agents", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Vec<Value>>(&o.stdout).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v["id"].as_str().map(String::from))
        .collect();
    let jobs = jobs_dir(acct);
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut agents: Vec<Agent> = list
        .iter()
        .map(|a| {
            let id = s(a, "id");
            let st: Value = std::fs::read_to_string(format!("{jobs}{id}/state.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            Agent {
                name: Some(s(a, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| s(a, "cwd").rsplit('/').next().unwrap_or("session").to_string()),
                cwd: s(a, "cwd"),
                state: if live.contains(&id) { s(a, "state") } else { "stopped".into() },
                started: a["startedAt"].as_u64().unwrap_or(0),
                detail: s(&st, "detail"),
                result: st["output"]["result"].as_str().unwrap_or("").to_string(),
                tokens: st["tokens"].as_u64().unwrap_or(0),
                color: chip(st["color"].as_str().unwrap_or("")),
                id,
            }
        })
        .collect();
    agents.sort_by_key(|a| std::cmp::Reverse(a.started));
    agents
}

fn chip(c: &str) -> Color {
    match c {
        "blue" => Color::Rgb(137, 180, 250),
        "cyan" => Color::Rgb(137, 220, 235),
        "green" => Color::Rgb(166, 227, 161),
        "orange" => Color::Rgb(250, 179, 135),
        "pink" => Color::Rgb(245, 194, 231),
        "purple" => Color::Rgb(203, 166, 247),
        "yellow" => Color::Rgb(249, 226, 175),
        "red" => Color::Rgb(243, 139, 168),
        _ => Color::Rgb(147, 153, 178),
    }
}

/// glyph: needs input *, working = animated flower, idle = still flower, offline ·
fn glyph(state: &str) -> &'static str {
    const FLOWER: [&str; 6] = ["✻", "✽", "✶", "✳", "✢", "✱"];
    match state {
        "blocked" => "*",
        "working" => FLOWER[(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() / 150 % 6) as usize],
        "done" => "✱",
        _ => "·",
    }
}

fn cat_list(c: &HashMap<String, String>) -> Vec<String> {
    let mut l: Vec<String> = c.values().filter(|v| !v.is_empty()).cloned().collect();
    l.sort();
    l.dedup();
    l
}

/// Run an optional `claude` subcommand then reload agents, off the UI thread.
fn kick(tx: &Sender<(Option<u32>, Vec<Agent>, String)>, acct: Option<u32>, args: Option<Vec<String>>) {
    let tx = tx.clone();
    thread::spawn(move || {
        let msg = match args.map(|a| cl(acct, &a.iter().map(String::as_str).collect::<Vec<_>>()).stdin(std::process::Stdio::null()).output()) {
            Some(Ok(o)) if !o.status.success() => String::from_utf8_lossy(&o.stderr).lines().chain(String::from_utf8_lossy(&o.stdout).lines()).next().unwrap_or("command failed").to_string(),
            Some(Err(e)) => e.to_string(),
            _ => String::new(),
        };
        let _ = tx.send((acct, load(acct), msg));
        bump();
    });
}

/// tui-term paints its own block cursor; hide it so only the real terminal cursor (with its trail) shows
fn no_cursor() -> tui_term::widget::Cursor {
    let mut c = tui_term::widget::Cursor::default();
    c.hide();
    c
}

fn cat_path() -> String {
    std::env::var("HOME").unwrap_or_default() + "/.config/betteragents/categories.json"
}

fn save_cats(c: &HashMap<String, String>) {
    let p = cat_path();
    let _ = std::fs::create_dir_all(std::path::Path::new(&p).parent().unwrap());
    let _ = std::fs::write(p, serde_json::to_string_pretty(c).unwrap_or_default());
}

/// (group key, label, color) for the status view
fn status_of(state: &str) -> (&'static str, &'static str, Color) {
    match state {
        "blocked" => ("blocked", "NEEDS INPUT", Color::Rgb(243, 139, 168)),
        "working" => ("working", "WORKING", Color::Rgb(249, 226, 175)),
        "done" => ("done", "IDLE", Color::Rgb(166, 227, 161)),
        _ => ("stopped", "OFFLINE", gray()),
    }
}

fn rows(a: &App) -> Vec<Row> {
    let mut groups: Vec<(String, Color, Vec<usize>)> = vec![];
    if a.by_category {
        let mut m: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, ag) in a.agents.iter().enumerate() {
            let c = a.cats.get(&ag.id).cloned().unwrap_or_default();
            m.entry(if c.is_empty() { "~".into() } else { c }).or_default().push(i);
        }
        for (k, v) in m {
            if k == "~" {
                continue;
            }
            groups.push((k.to_uppercase(), accent(), v));
        }
        let none: Vec<usize> = (0..a.agents.len()).filter(|&i| !a.cats.get(&a.agents[i].id).is_some_and(|c| !c.is_empty())).collect();
        if !none.is_empty() {
            groups.push(("UNCATEGORIZED".into(), gray(), none));
        }
    } else {
        for key in ["blocked", "working", "done", "stopped"] {
            let v: Vec<usize> = (0..a.agents.len()).filter(|&i| status_of(&a.agents[i].state).0 == key).collect();
            if !v.is_empty() {
                let (_, label, col) = status_of(key);
                groups.push((label.into(), col, v));
            }
        }
    }
    let q = a.filter.to_lowercase();
    let keep = |i: usize| {
        let ag = &a.agents[i];
        q.is_empty() || format!("{} {} {}", ag.name, ag.cwd, a.cats.get(&ag.id).map_or("", |c| c.as_str())).to_lowercase().contains(&q)
    };
    let mut out = vec![];
    for (label, col, v) in groups {
        let v: Vec<usize> = v.into_iter().filter(|&i| keep(i)).collect();
        if v.is_empty() {
            continue;
        }
        out.push(Row::Head(format!("{label} · {}", v.len()), col));
        out.extend(v.into_iter().map(Row::Agent));
    }
    out
}

fn order(a: &App) -> Vec<usize> {
    rows(a).iter().filter_map(|r| if let Row::Agent(i) = r { Some(*i) } else { None }).collect()
}

fn sel_idx(a: &App) -> Option<usize> {
    a.agents.iter().position(|x| x.id == a.sel)
}

fn fix_sel(a: &mut App) {
    if sel_idx(a).is_none_or(|i| !order(a).contains(&i)) {
        if let Some(&i) = order(a).first() {
            a.sel = a.agents[i].id.clone();
        }
    }
}

fn age(ms: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let m = now.saturating_sub(ms) / 60_000;
    match m {
        0..=59 => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        _ => format!("{}d", m / 1440),
    }
}

fn spawn(acct: Option<u32>, id: &str, rows: u16, cols: u16) -> Option<Sess> {
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .ok()?;
    let mut cmd = CommandBuilder::new(if acct.is_some() { "cswap" } else { "claude" });
    if let Some(n) = acct { cmd.args(["run", &n.to_string(), "--"]); }
    cmd.args(["attach", id]);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let child = pair.slave.spawn_command(cmd).ok()?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().ok()?;
    let writer = pair.master.take_writer().ok()?;
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let dead = Arc::new(AtomicBool::new(false));
    let (p, d) = (parser.clone(), dead.clone());
    thread::spawn(move || {
        let mut b = [0u8; 8192];
        while let Ok(n) = reader.read(&mut b) {
            if n == 0 {
                break;
            }
            p.lock().unwrap().process(&b[..n]);
            bump();
        }
        d.store(true, Relaxed);
        bump();
    });
    Some(Sess { parser, writer, master: pair.master, child, dead, size: (rows, cols) })
}

/// Translate a mouse event into the SGR sequence the attached session asked for (it enables mouse
/// reporting itself, which is how scrolling and link clicks work inside it).
fn mouse_bytes(m: &MouseEvent, r: Rect, s: &Sess) -> Option<Vec<u8>> {
    let p = s.parser.lock().unwrap();
    let mode = p.screen().mouse_protocol_mode();
    if mode == MouseProtocolMode::None || p.screen().mouse_protocol_encoding() != MouseProtocolEncoding::Sgr {
        return None;
    }
    let btn = |b: MouseButton| match b { MouseButton::Left => 0, MouseButton::Middle => 1, MouseButton::Right => 2 };
    let moving = matches!(mode, MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion);
    let (mut code, up) = match m.kind {
        MouseEventKind::Down(b) => (btn(b), false),
        MouseEventKind::Up(b) if mode != MouseProtocolMode::Press => (btn(b), true),
        MouseEventKind::Drag(b) if moving => (btn(b) + 32, false),
        MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    if m.modifiers.contains(KeyModifiers::SHIFT) { code += 4; }
    if m.modifiers.contains(KeyModifiers::ALT) { code += 8; }
    if m.modifiers.contains(KeyModifiers::CONTROL) { code += 16; }
    Some(format!("\x1b[<{};{};{}{}", code, m.column - r.x + 1, m.row - r.y + 1, if up { 'm' } else { 'M' }).into_bytes())
}

/// True when the attached session's prompt box (`❯ …`, where the cursor is) holds no typed text.
/// Dim text is the placeholder, so it doesn't count. ponytail: relies on Claude's `❯` prompt glyph.
fn prompt_empty(s: &Sess) -> bool {
    let p = s.parser.lock().unwrap();
    let scr = p.screen();
    let (row, _) = scr.cursor_position();
    let (_, cols) = scr.size();
    if scr.cell(row, 0).map(|c| c.contents()) != Some("❯".into()) {
        return false;
    }
    (2..cols).all(|c| scr.cell(row, c).is_none_or(|x| x.contents().trim().is_empty() || x.dim()))
}

/// Grayscale equivalent of a terminal color (same brightness, no hue).
fn rgb_of(c: Color) -> Option<(u8, u8, u8)> {
    const X16: [(u8, u8, u8); 16] = [(0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0), (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229), (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0), (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255)];
    let idx = |n: u8| -> (u8, u8, u8) {
        match n {
            0..=15 => X16[n as usize],
            16..=231 => {
                let l = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                let n = n - 16;
                (l(n / 36), l(n / 6 % 6), l(n % 6))
            }
            _ => (8 + 10 * (n - 232), 8 + 10 * (n - 232), 8 + 10 * (n - 232)),
        }
    };
    Some(match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(n) => idx(n),
        Color::Black => X16[0], Color::Red => X16[1], Color::Green => X16[2], Color::Yellow => X16[3],
        Color::Blue => X16[4], Color::Magenta => X16[5], Color::Cyan => X16[6], Color::Gray => X16[7],
        Color::DarkGray => X16[8], Color::LightRed => X16[9], Color::LightGreen => X16[10], Color::LightYellow => X16[11],
        Color::LightBlue => X16[12], Color::LightMagenta => X16[13], Color::LightCyan => X16[14], Color::White => X16[15],
        _ => return None, // Reset: the terminal's own default, can't be recolored
    })
}

fn mono(c: Color) -> Color {
    rgb_of(c).map_or(c, |(r, g, b)| {
        let l = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
        Color::Rgb(l, l, l)
    })
}

fn darken(c: Color) -> Color {
    rgb_of(c).map_or(c, |(r, g, b)| Color::Rgb((r as f32 * 0.45) as u8, (g as f32 * 0.45) as u8, (b as f32 * 0.45) as u8))
}

fn key_bytes(k: KeyEvent) -> Vec<u8> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let mut v = match k.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => b"\x1b\r".to_vec(), // newline in claude
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => vec![],
    };
    if !v.is_empty() && k.modifiers.contains(KeyModifiers::ALT) {
        v.insert(0, 0x1b);
    }
    v
}

fn ui(f: &mut Frame, a: &mut App) {
    ui_inner(f, a);
    if !a.win_focused {
        let area = f.area();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let c = &mut f.buffer_mut()[(x, y)];
                (c.fg, c.bg) = (mono(c.fg), mono(c.bg));
            }
        }
    }
}

fn ui_inner(f: &mut Frame, a: &mut App) {
    a.chat = None;
    let [hd, body, ft] = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());

    // header: title + view tabs
    let tab = |on: bool, t: &str| {
        if on { Span::styled(format!(" {t} "), Style::new().fg(Color::Black).bg(accent()).bold()) } else { Span::styled(format!(" {t} "), Style::new().fg(gray())) }
    };
    let count = |s: &str| a.agents.iter().filter(|x| status_of(&x.state).0 == s).count();
    let mut head = vec![
        Span::styled(" BetterAgents ", Style::new().bold().fg(accent())),
        tab(!a.by_category, "Status"),
        tab(a.by_category, "Category"),
    ];
    if !a.accounts.is_empty() {
        head.push(Span::styled("  │ ", Style::new().fg(gray())));
        head.push(tab(a.acct.is_none(), "Default"));
        head.extend(a.accounts.iter().map(|(n, label)| tab(a.acct == Some(*n), label)));
    }
    head.push(Span::styled(
        format!("   {} needs input · {} working · {} idle", count("blocked"), count("working"), count("done")),
        Style::new().fg(gray()),
    ));
    f.render_widget(Line::from(head), hd);

    let [l, r] = if a.full { [Rect::default(), body] } else { Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)]).areas(body) };
    let focused = matches!(a.mode, Mode::Focus);
    // pane title: the focused pane gets a filled accent chip with ▶, the other a dim label
    let tag = |on: bool, t: String| {
        if on { Span::styled(format!(" ▶ {t} "), Style::new().fg(Color::Black).bg(accent()).bold()) } else { Span::styled(format!(" {t} "), Style::new().fg(gray())) }
    };

    // list
    let rs = rows(a);
    let mut items = vec![];
    let mut disp = None;
    for row in &rs {
        match row {
            Row::Head(t, c) => items.push(ListItem::new(Line::from(Span::styled(format!("▍{t}"), Style::new().fg(*c).bold())))),
            Row::Agent(i) => {
                let ag = &a.agents[*i];
                if ag.id == a.sel {
                    disp = Some(items.len());
                }
                let (_, _, col) = status_of(&ag.state);
                let mut sp = vec![
                    Span::styled(format!("  {} ", glyph(&ag.state)), Style::new().fg(col)),
                    Span::styled(format!(" {} ", ag.name), if ag.id == a.sel { Style::new().fg(ag.color).bg(hl()).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Black).bg(ag.color).add_modifier(Modifier::BOLD) }),
                ];
                if a.sess.contains_key(&ag.id) {
                    sp.push(Span::styled(" ▸chat", Style::new().fg(accent())));
                }
                if !a.by_category {
                    if let Some(c) = a.cats.get(&ag.id).filter(|c| !c.is_empty()) {
                        sp.push(Span::styled(format!(" #{c}"), Style::new().fg(accent()).dim()));
                    }
                }
                sp.push(Span::styled(format!("  {}", age(ag.started)), Style::new().fg(gray())));
                items.push(ListItem::new(Line::from(sp)));
            }
        }
    }
    let mut st = ListState::default().with_selected(disp);
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(if focused { gray() } else { accent() })).title(tag(!focused, "SESSIONS".into())))
            .highlight_style(Style::new().bg(hl()).add_modifier(Modifier::BOLD)),
        l,
        &mut st,
    );
    // chat has focus: darken the session picker (keeps hue, unlike the chat's monochrome)
    if focused {
        for y in l.top()..l.bottom() {
            for x in l.left()..l.right() {
                let c = &mut f.buffer_mut()[(x, y)];
                (c.fg, c.bg) = (darken(c.fg), darken(c.bg));
                c.modifier |= Modifier::DIM; // also dims the terminal's default-colored text
            }
        }
    }

    // right pane: live chat if a session exists, else details
    let Some(ai) = sel_idx(a) else {
        f.render_widget(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(gray())), r);
        return footer(f, a, ft);
    };
    let (name, sid) = (a.agents[ai].name.clone(), a.agents[ai].id.clone());
    if let Some(s) = a.sess.get_mut(&sid) {
        let blk = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { accent() } else { gray() }))
            .title(tag(focused, format!("CHAT · {name}")));
        let inner = blk.inner(r);
        a.chat = Some(inner);
        f.render_widget(blk, r);
        let size = (inner.height.max(1), inner.width.max(1));
        if size != s.size {
            s.size = size;
            let _ = s.master.resize(PtySize { rows: size.0, cols: size.1, pixel_width: 0, pixel_height: 0 });
            s.parser.lock().unwrap().screen_mut().set_size(size.0, size.1);
        }
        let p = s.parser.lock().unwrap();
        f.render_widget(PseudoTerminal::new(p.screen()).cursor(no_cursor()), inner);
        if !focused {
            for y in inner.top()..inner.bottom() {
                for x in inner.left()..inner.right() {
                    let c = &mut f.buffer_mut()[(x, y)];
                    (c.fg, c.bg) = (mono(c.fg), mono(c.bg));
                }
            }
        }
        if focused && !p.screen().hide_cursor() {
            let (r, c) = p.screen().cursor_position();
            f.set_cursor_position((inner.x + c.min(inner.width.saturating_sub(1)), inner.y + r.min(inner.height.saturating_sub(1))));
        }
    } else {
        let ag = &a.agents[ai];
        let (_, label, col) = status_of(&ag.state);
        let cat = a.cats.get(&ag.id).cloned().unwrap_or_default();
        let lines = vec![
            Line::from(vec![Span::styled(ag.name.clone(), Style::new().bold().fg(Color::White)), Span::styled(format!("  {} {label}", glyph(&ag.state)), Style::new().fg(col))]),
            Line::from(Span::styled(ag.cwd.clone(), Style::new().fg(gray()))),
            Line::from(Span::styled(
                format!("category: {}   tokens: {}   started {} ago", if cat.is_empty() { "—" } else { &cat }, ag.tokens, age(ag.started)),
                Style::new().fg(gray()),
            )),
            Line::raw(""),
            Line::from(Span::styled(ag.detail.clone(), Style::new().fg(text()))),
            Line::raw(""),
            Line::raw(ag.result.clone()),
            Line::raw(""),
            Line::from(Span::styled("Enter → open chat here", Style::new().fg(accent()))),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(gray())).title(" details ")),
            r,
        );
    }
    footer(f, a, ft);
}

fn footer(f: &mut Frame, a: &App, area: ratatui::layout::Rect) {
    let t = match &a.mode {
        Mode::Search => format!(" search › {}   Enter keep · Esc clear", a.filter),
        Mode::Nav if !a.filter.is_empty() => format!(" filter: {} · / edit · Esc clear", a.filter),
        Mode::Prompt(s) => format!(" category › {s}   Tab/↑↓ pick existing [{}] · Enter save · empty clears · Esc cancel", cat_list(&a.cats).join(", ")),
        Mode::Confirm => format!(" delete session '{}' permanently? y / n", a.agents.iter().find(|x| x.id == a.sel).map_or("", |x| &x.name)),
        Mode::Nav if !a.msg.is_empty() => format!(" {}", a.msg),
        Mode::Focus => " chat focused — keys go to the agent · F3 fullscreen · ← (empty input) or F2 back to list".into(),
        Mode::Nav => " j/k move · / search · Enter/→ chat · a account · f fullscreen · Tab status/category view · c categorize · x close chat · o online · s offline · d delete · r refresh · q quit".into(),
    };
    if matches!(a.mode, Mode::Search) {
        f.set_cursor_position((area.x + AUTHOR.len() as u16 + 2 + 10 + a.filter.chars().count() as u16, area.y));
    }
    if let Mode::Prompt(s) = &a.mode {
        f.set_cursor_position((area.x + AUTHOR.len() as u16 + 2 + 12 + s.chars().count() as u16, area.y));
    }
    f.render_widget(
        Line::from(vec![
            Span::styled(format!(" {AUTHOR} "), Style::new().fg(Color::Black).bg(accent()).bold()),
            Span::styled(t, Style::new().fg(gray())),
        ]),
        area,
    );
}

fn run(term: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    load_theme();
    let cats = std::fs::read_to_string(cat_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut a = App { accounts: load_accounts(), acct: None, agents: load(None), cats, filter: String::new(), by_category: false, sel: String::new(), mode: Mode::Nav, sess: HashMap::new(), msg: String::new(), win_focused: true, full: false, chat: None };
    let (tx, rx) = mpsc::channel::<(Option<u32>, Vec<Agent>, String)>();
    let mut loading = false;
    let mut last = Instant::now();
    let (mut dirty, mut last_sig) = (true, (0u64, 0u128));
    loop {
        while let Ok((acct, ag, msg)) = rx.try_recv() {
            if acct != a.acct { loading = false; continue; } // result for a tab we've left
            a.agents = ag;
            if !msg.is_empty() { a.msg = msg; }
            loading = false;
        }
        a.sess.retain(|_, s| !s.dead.load(Relaxed));
        if last.elapsed() > Duration::from_secs(5) && !loading {
            loading = true;
            kick(&tx, a.acct, None);
            load_theme();
            dirty = true;
            last = Instant::now();
        }
        fix_sel(&mut a);
        if matches!(a.mode, Mode::Focus) && !a.sess.contains_key(&a.sel) {
            a.mode = Mode::Nav;
        }
        if !matches!(a.mode, Mode::Focus) {
            a.full = false;
        }
        // redraw only on input, new output/data, the 5s tick, or the working-flower animation frame
        let anim = if a.agents.iter().any(|x| x.state == "working") { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() / 150 } else { 0 };
        let sig = (TICK.load(Relaxed), anim);
        if dirty || sig != last_sig {
            term.draw(|f| ui(f, &mut a))?;
            (last_sig, dirty) = (sig, false);
        }
        if !event::poll(Duration::from_millis(if a.sess.is_empty() { 100 } else { 16 }))? {
            continue;
        }
        dirty = true;
        let k = match event::read()? {
            Event::Key(k) => k,
            Event::FocusGained => { a.win_focused = true; continue }
            Event::FocusLost => { a.win_focused = false; continue }
            Event::Mouse(m) => {
                if let (Some(r), Some(s)) = (a.chat, a.sess.get_mut(&a.sel)) {
                    if m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height {
                        if matches!(m.kind, MouseEventKind::Down(_)) && matches!(a.mode, Mode::Nav) {
                            a.mode = Mode::Focus;
                        }
                        if let Some(b) = mouse_bytes(&m, r, s) {
                            let _ = s.writer.write_all(&b);
                            let _ = s.writer.flush();
                        }
                    }
                }
                continue;
            }
            _ => continue,
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match &mut a.mode {
            Mode::Focus => {
                if k.code == KeyCode::F(3) {
                    a.full = !a.full;
                } else if k.code == KeyCode::F(2) || (k.code == KeyCode::Left && a.sess.get(&a.sel).is_some_and(prompt_empty)) {
                    a.mode = Mode::Nav;
                } else if let Some(s) = a.sess.get_mut(&a.sel) {
                    let _ = s.writer.write_all(&key_bytes(k));
                    let _ = s.writer.flush();
                }
            }
            Mode::Search => match k.code {
                KeyCode::Enter => a.mode = Mode::Nav,
                KeyCode::Esc => { a.filter.clear(); a.mode = Mode::Nav; }
                KeyCode::Backspace => { a.filter.pop(); }
                KeyCode::Char(c) => a.filter.push(c),
                _ => {}
            },
            Mode::Prompt(buf) => match k.code {
                KeyCode::Enter => {
                    let v = buf.trim().to_string();
                    if v.is_empty() { a.cats.remove(&a.sel); } else { a.cats.insert(a.sel.clone(), v); }
                    save_cats(&a.cats);
                    a.mode = Mode::Nav;
                }
                KeyCode::Esc => a.mode = Mode::Nav,
                KeyCode::Backspace => { buf.pop(); }
                KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                    let l = cat_list(&a.cats);
                    if !l.is_empty() {
                        let i = l.iter().position(|c| c == buf);
                        let n = if k.code == KeyCode::Up { i.map_or(l.len() - 1, |i| (i + l.len() - 1) % l.len()) } else { i.map_or(0, |i| (i + 1) % l.len()) };
                        *buf = l[n].clone();
                    }
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Mode::Confirm => {
                if k.code == KeyCode::Char('y') {
                    if let Some(mut s) = a.sess.remove(&a.sel) { let _ = s.child.kill(); }
                    kick(&tx, a.acct, Some(vec!["rm".into(), a.sel.clone()]));
                }
                a.mode = Mode::Nav;
            }
            Mode::Nav => {
                a.msg.clear();
                let ord = order(&a);
                let pos = sel_idx(&a).and_then(|i| ord.iter().position(|&x| x == i)).unwrap_or(0);
                match k.code {
                    KeyCode::Esc if !a.filter.is_empty() => a.filter.clear(),
                    KeyCode::Char('/') => a.mode = Mode::Search,
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Some(&i) = ord.get((pos + 1).min(ord.len().saturating_sub(1))) { a.sel = a.agents[i].id.clone(); }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Some(&i) = ord.get(pos.saturating_sub(1)) { a.sel = a.agents[i].id.clone(); }
                    }
                    KeyCode::Tab => a.by_category = !a.by_category,
                    KeyCode::Char('r') => kick(&tx, a.acct, None),
                    KeyCode::Char('c') if !a.sel.is_empty() => a.mode = Mode::Prompt(a.cats.get(&a.sel).cloned().unwrap_or_default()),
                    KeyCode::Char('d') if !a.sel.is_empty() => a.mode = Mode::Confirm,
                    KeyCode::Char('o') if !a.sel.is_empty() => {
                        kick(&tx, a.acct, Some(vec!["respawn".into(), a.sel.clone()]));
                    }
                    KeyCode::Char('s') if !a.sel.is_empty() => {
                        if let Some(mut s) = a.sess.remove(&a.sel) { let _ = s.child.kill(); }
                        kick(&tx, a.acct, Some(vec!["stop".into(), a.sel.clone()]));
                    }
                    KeyCode::Char('a') if !a.accounts.is_empty() => {
                        // Default -> Account 1 -> Account 2 -> Default
                        let i = a.acct.and_then(|n| a.accounts.iter().position(|x| x.0 == n));
                        a.acct = match i { None if a.acct.is_none() => Some(a.accounts[0].0), Some(i) if i + 1 < a.accounts.len() => Some(a.accounts[i + 1].0), _ => None };
                        a.agents.clear();
                        a.sel.clear();
                        kick(&tx, a.acct, None);
                    }
                    KeyCode::Char('f') if a.sess.contains_key(&a.sel) => (a.mode, a.full) = (Mode::Focus, true),
                    KeyCode::Char('x') => {
                        if let Some(mut s) = a.sess.remove(&a.sel) { let _ = s.child.kill(); }
                    }
                    KeyCode::Enter | KeyCode::Right if !a.sel.is_empty() => {
                        if !a.sess.contains_key(&a.sel) {
                            let sz = term.size()?;
                            let (rows, cols) = (sz.height.saturating_sub(4).max(5), (sz.width * 64 / 100).saturating_sub(2).max(20));
                            if let Some(s) = spawn(a.acct, &a.sel, rows, cols) { a.sess.insert(a.sel.clone(), s); }
                        }
                        if a.sess.contains_key(&a.sel) { a.mode = Mode::Focus; }
                    }
                    _ => {}
                }
            }
        }
    }
    for s in a.sess.values_mut() {
        let _ = s.child.kill();
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut term = ratatui::init();
    // lets the terminal report Shift+Enter (kitty keyboard protocol)
    let kbd = supports_keyboard_enhancement().unwrap_or(false);
    if kbd {
        let _ = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange);
    let res = run(&mut term);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableFocusChange);
    if kbd {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    res
}
