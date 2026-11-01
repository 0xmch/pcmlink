//! pcmlink-mixer: every stream reaching this machine, and what's playing in it.
//!
//! Nothing here is configured per client. Receivers are found by the sockets they
//! publish (pcmlink-rx, control.rs). A client is whatever address a stream's
//! packets come from. If that address also answers on the agent port, its
//! applications get strips too (pcmlink_agent.py describes the protocol).

mod model;
mod names;
mod ui;
mod workers;

use std::collections::HashSet;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde_json::{json, Value};

use model::{step_gain, to_db, to_linear, Model, Strip, FLOOR_DB};
use names::SshHosts;
use workers::{Discovery, Msg};

const FRAME: Duration = Duration::from_millis(33);
const SCAN: Duration = Duration::from_secs(1);
const NOTICE: Duration = Duration::from_secs(5);
const STEP_DB: f32 = 1.0;
const PAGE_DB: f32 = 6.0;
const DEFAULT_AGENT_PORT: u16 = 5003;

const USAGE: &str = "usage: pcmlink-mixer [--list | --json] [--dir DIR] [--agent-port 5003]

Shows every pcmlink stream reaching this machine, who sends it, and, where the
client runs `pcmlink agent`, each application playing there.

  --list         print what's there now and exit
  --json         the same as JSON
  --dir          where receivers publish their sockets (default: $PCMLINK_RX_DIR,
                 or ~/Library/Application Support/pcmlink/rx)
  --agent-port   the port agents listen on";

struct Opts {
    list: bool,
    json: bool,
    dir: PathBuf,
    agent_port: u16,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
    let mut opts = Opts { list: false, json: false, dir: workers::default_dir(), agent_port: DEFAULT_AGENT_PORT };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--list" => opts.list = true,
            "--json" => opts.json = true,
            "--dir" => opts.dir = args.next().ok_or("--dir needs a value")?.into(),
            "--agent-port" => {
                let value = args.next().ok_or("--agent-port needs a value")?;
                opts.agent_port = value.parse().map_err(|_| format!("bad port {value}"))?;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(opts)
}

pub struct Solo {
    pub strip: Strip,
    /// What this solo muted, and only that: undoing it leaves alone whatever
    /// was muted before.
    silenced: Vec<Strip>,
}

#[derive(Clone)]
enum Action {
    Reopen(PathBuf),
    RestartSender(IpAddr),
}

pub struct Confirm {
    pub text: String,
    action: Action,
}

pub struct Mixer {
    pub model: Model,
    pub selected: Option<Strip>,
    /// The user has chosen a strip. Until then the first one stays selected, in
    /// whatever order the receivers happen to answer at startup.
    chosen: bool,
    pub solo: Option<Solo>,
    pub scroll: u16,
    pub help: bool,
    pub confirm: Option<Confirm>,
    pub notice: Option<(String, Instant)>,
    pub hits: Vec<ui::Hit>,
    pub dir: PathBuf,
    discovery: Discovery,
    agents: HashSet<IpAddr>,
    resolving: HashSet<IpAddr>,
    ssh_hosts: SshHosts,
    agent_port: u16,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    last_scan: Option<Instant>,
}

impl Mixer {
    fn new(opts: &Opts) -> Mixer {
        let (tx, rx) = mpsc::channel();
        Mixer {
            model: Model::default(),
            selected: None,
            chosen: false,
            solo: None,
            scroll: 0,
            help: false,
            confirm: None,
            notice: None,
            hits: Vec::new(),
            dir: opts.dir.clone(),
            discovery: Discovery::new(opts.dir.clone(), tx.clone()),
            agents: HashSet::new(),
            resolving: HashSet::new(),
            ssh_hosts: SshHosts::load(),
            agent_port: opts.agent_port,
            tx,
            rx,
            last_scan: None,
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// Everything that happened since the last frame.
    fn pump(&mut self) {
        if self.last_scan.is_none_or(|t| t.elapsed() >= SCAN) {
            self.last_scan = Some(Instant::now());
            self.discovery.scan();
        }
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::StreamUp(path, writer) => self.model.add_stream(path, writer),
                Msg::StreamStatus(path, status) => {
                    self.model.stream_status(&path, status);
                    if let Some(ip) = self.model.stream(&path).and_then(|s| s.peer) {
                        self.met_client(ip);
                    }
                }
                Msg::StreamGone(path) => {
                    self.model.remove_stream(&path);
                    self.discovery.forget(&path);
                }
                Msg::AgentUp(ip, writer) => self.model.agent_up(ip, writer),
                Msg::AgentLine(ip, value) => {
                    if let Some(text) = self.model.agent_message(ip, value) {
                        self.say(text);
                    }
                }
                Msg::AgentGone(ip) => self.model.agent_gone(ip),
                Msg::Resolved(ip, name) => {
                    let alias = self.ssh_hosts.alias_of(&name);
                    let client = self.model.clients.entry(ip).or_default();
                    // An alias can point at the name rather than at the address.
                    client.alias = client.alias.take().or(alias);
                    client.resolved = Some(names::short_name(&name));
                }
            }
        }
        // The selection follows its strip; if that went away, take a neighbour.
        let strips = self.model.strips();
        if !self.chosen || !self.selected.as_ref().is_some_and(|s| strips.contains(s)) {
            self.selected = strips.first().cloned();
        }
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTICE) {
            self.notice = None;
        }
    }

    /// The first time an address sends a stream: look for its agent and its name.
    fn met_client(&mut self, ip: IpAddr) {
        if self.agents.insert(ip) {
            workers::spawn_agent(ip, self.agent_port, self.tx.clone());
        }
        if self.resolving.insert(ip) {
            self.model.clients.entry(ip).or_default().alias = self.ssh_hosts.alias_of(&ip.to_string());
            workers::spawn_resolve(ip, self.tx.clone());
        }
    }

    fn move_selection(&mut self, by: i32) {
        let strips = self.model.strips();
        if strips.is_empty() {
            return;
        }
        let at = self.selected.as_ref().and_then(|s| strips.iter().position(|x| x == s)).unwrap_or(0) as i32;
        let next = (at + by).clamp(0, strips.len() as i32 - 1) as usize;
        self.selected = Some(strips[next].clone());
        self.chosen = true;
    }

    fn next_group(&mut self) {
        let groups = self.model.groups();
        if groups.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| groups.iter().position(|g| g.strips.contains(s)))
            .unwrap_or(0);
        let next = &groups[(at + 1) % groups.len()];
        self.selected = next.strips.first().cloned();
        self.chosen = true;
    }

    fn nudge(&mut self, strip: &Strip, delta_db: f32) {
        if let Some(gain) = self.model.gain(strip) {
            self.model.set_gain(strip, step_gain(gain, delta_db));
        }
    }

    fn toggle_mute(&mut self, strip: &Strip) {
        if let Some(muted) = self.model.muted(strip) {
            self.model.set_mute(strip, !muted);
        }
    }

    fn clear_solo(&mut self) {
        if let Some(solo) = self.solo.take() {
            for strip in &solo.silenced {
                self.model.set_mute(strip, false);
            }
        }
    }

    fn toggle_solo(&mut self) {
        let Some(strip) = self.selected.clone() else { return };
        let same = self.solo.as_ref().is_some_and(|s| s.strip == strip);
        self.clear_solo();
        if same {
            return;
        }
        let mut silenced = Vec::new();
        for sibling in self.model.siblings(&strip) {
            if self.model.muted(&sibling) == Some(false) {
                self.model.set_mute(&sibling, true);
                silenced.push(sibling);
            }
        }
        self.solo = Some(Solo { strip, silenced });
    }

    fn ask_reopen(&mut self) {
        match &self.selected {
            Some(Strip::Stream(path)) => {
                let port = self.model.stream(path).map_or(0, |s| s.port());
                self.confirm = Some(Confirm {
                    text: format!("Reopen the output of stream :{port}? Its audio drops for a moment."),
                    action: Action::Reopen(path.clone()),
                });
            }
            _ => self.say("reopen works on a stream: select one"),
        }
    }

    fn selected_client(&self) -> Option<IpAddr> {
        match &self.selected {
            Some(Strip::Stream(path)) => self.model.stream(path)?.peer,
            Some(Strip::App(ip, _)) => Some(*ip),
            None => None,
        }
    }

    fn ask_restart_sender(&mut self) {
        let Some(ip) = self.selected_client() else {
            self.say("nothing is sending to this stream");
            return;
        };
        if !self.model.clients.get(&ip).is_some_and(|c| c.has_agent()) {
            self.say(format!("{} runs no agent, so it can't be asked", self.model.client_name(ip)));
            return;
        }
        self.confirm = Some(Confirm {
            text: format!("Restart the sender on {}? Its audio drops for a moment.", self.model.client_name(ip)),
            action: Action::RestartSender(ip),
        });
    }

    fn carry_out(&mut self, action: Action) {
        match action {
            Action::Reopen(path) => {
                if let Some(stream) = self.model.streams.iter_mut().find(|s| s.path == path) {
                    stream.send("reopen");
                }
            }
            Action::RestartSender(ip) => {
                if let Some(client) = self.model.clients.get_mut(&ip) {
                    client.send(&json!({"cmd": "restart-sender"}));
                }
            }
        }
    }

    /// False to quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return false;
        }
        if let Some(question) = self.confirm.take() {
            if key.code == KeyCode::Char('y') {
                self.carry_out(question.action);
            }
            return true;
        }
        if self.help {
            self.help = false;
            return true;
        }
        let selected = self.selected.clone();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Left | KeyCode::Char('h') => self.move_selection(-1),
            KeyCode::Right | KeyCode::Char('l') => self.move_selection(1),
            KeyCode::Tab => self.next_group(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(s) = &selected {
                    self.nudge(s, STEP_DB)
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(s) = &selected {
                    self.nudge(s, -STEP_DB)
                }
            }
            KeyCode::PageUp => {
                if let Some(s) = &selected {
                    self.nudge(s, PAGE_DB)
                }
            }
            KeyCode::PageDown => {
                if let Some(s) = &selected {
                    self.nudge(s, -PAGE_DB)
                }
            }
            KeyCode::Char('m') => {
                if let Some(s) = &selected {
                    self.toggle_mute(s)
                }
            }
            KeyCode::Char('s') => self.toggle_solo(),
            KeyCode::Char('o') => self.ask_reopen(),
            KeyCode::Char('R') => self.ask_restart_sender(),
            KeyCode::Char('a') => self.model.show_idle = !self.model.show_idle,
            KeyCode::Char('?') => self.help = true,
            _ => {}
        }
        true
    }

    fn mouse(&mut self, event: MouseEvent) {
        let (x, y) = (event.column, event.row);
        let Some(hit) = self
            .hits
            .iter()
            .find(|h| x >= h.area.x && x < h.area.right() && y >= h.area.y && y < h.area.bottom())
            .cloned()
        else {
            return;
        };
        match event.kind {
            MouseEventKind::ScrollUp => self.nudge(&hit.strip, STEP_DB),
            MouseEventKind::ScrollDown => self.nudge(&hit.strip, -STEP_DB),
            MouseEventKind::Down(MouseButton::Left) => {
                self.selected = Some(hit.strip.clone());
                self.chosen = true;
                if y == hit.mute_row {
                    self.toggle_mute(&hit.strip);
                } else if x >= hit.fader.x && x < hit.fader.right() && y >= hit.fader.y && y < hit.fader.bottom() {
                    self.click_fader(&hit, y);
                }
            }
            _ => {}
        }
    }

    /// A click moves the fader down at once, but up by one page at most: a stray
    /// click near the top of the rail must not take a quiet strip to full level.
    fn click_fader(&mut self, hit: &ui::Hit, row: u16) {
        let Some(gain) = self.model.gain(&hit.strip) else { return };
        let wanted = ui::db_at_row(hit.fader, row);
        let current = to_db(gain).max(FLOOR_DB);
        let target = if wanted > current { wanted.min(current + PAGE_DB) } else { wanted };
        self.model.set_gain(&hit.strip, to_linear(target.min(0.0)));
    }
}

/// Clicks and the wheel. crossterm's EnableMouseCapture also asks for every movement
/// of the mouse, which nothing here reads.
fn mouse_capture(on: bool) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(if on { b"\x1b[?1000h\x1b[?1006h" } else { b"\x1b[?1006l\x1b[?1000l" })?;
    out.flush()
}

/// Wait for input, for at most `wait`, and handle all that has come, so that a key
/// reaches the screen without waiting for the next frame. False to quit.
fn take_input(mixer: &mut Mixer, wait: Duration) -> io::Result<bool> {
    if !event::poll(wait)? {
        return Ok(true);
    }
    loop {
        match event::read()? {
            Event::Key(key) => {
                if !mixer.key(key) {
                    return Ok(false);
                }
            }
            Event::Mouse(mouse) => mixer.mouse(mouse),
            _ => {}
        }
        if !event::poll(Duration::ZERO)? {
            return Ok(true);
        }
    }
}

fn run_tui(opts: &Opts) -> io::Result<()> {
    let mut terminal = ratatui::init();
    mouse_capture(true)?;
    let mut mixer = Mixer::new(opts);
    let mut last = Instant::now();
    let result = loop {
        let now = Instant::now();
        mixer.model.tick(now.duration_since(last).as_secs_f32());
        last = now;
        mixer.pump();
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut mixer)) {
            break Err(e);
        }
        match take_input(&mut mixer, (now + FRAME).saturating_duration_since(Instant::now())) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    mixer.clear_solo();
    let _ = mouse_capture(false);
    ratatui::restore();
    result
}

/// Collect for a moment: receivers report at once, an agent a little later.
fn gather(opts: &Opts) -> Mixer {
    let mut mixer = Mixer::new(opts);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1600) {
        mixer.pump();
        std::thread::sleep(Duration::from_millis(40));
        // Enough for levels to mean something, and every agent has answered.
        if start.elapsed() > Duration::from_millis(1000) {
            let waiting = mixer.agents.iter().any(|ip| {
                mixer.model.clients.get(ip).is_some_and(|c| c.has_agent() && c.apps.is_empty())
            });
            if !waiting {
                break;
            }
        }
    }
    mixer
}

fn peak_db(peak: &Value) -> Option<f32> {
    let channels = peak.as_array()?;
    let top = channels.iter().filter_map(Value::as_f64).fold(0.0, f64::max) as f32;
    Some(to_db(top))
}

fn level(db: Option<f32>) -> String {
    match db {
        None => "no meter".into(),
        Some(db) if db < FLOOR_DB => "silent".into(),
        Some(db) => format!("{db:.0} dB"),
    }
}

fn print_list(mixer: &Mixer) {
    let groups = mixer.model.groups();
    if groups.is_empty() {
        println!("no receivers in {}", mixer.dir.display());
    }
    for group in groups {
        let title = match group.ip {
            Some(ip) => {
                let os = mixer.model.clients.get(&ip).and_then(|c| c.os.clone()).unwrap_or_default();
                format!("{} {os}", mixer.model.client_full_name(ip))
            }
            None => "waiting for a sender".to_string(),
        };
        println!("{}", title.trim_end());
        for strip in &group.strips {
            match strip {
                Strip::Stream(path) => {
                    let Some(s) = mixer.model.stream(path) else { continue };
                    let state = if s.flowing() { "flowing" } else { "no packets" };
                    println!(
                        "  stream :{:<5} {:<10} level {:<8} gain {:<8}{}  buffer {:.1} ms  lost {}  dropouts {}",
                        s.port(),
                        state,
                        level(peak_db(&s.status["peak"])),
                        ui::format_db(s.gain()),
                        if s.muted() { " MUTED" } else { "" },
                        s.num("buf_ms"),
                        s.num("lost"),
                        s.num("under"),
                    );
                }
                Strip::App(ip, id) => {
                    let Some(a) = mixer.model.app(*ip, id) else { continue };
                    let state = if a.active() { "playing" } else { "idle" };
                    let exe = a.info["exe"].as_str().unwrap_or("-");
                    let pid = a.info["pid"].as_u64().map_or("-".to_string(), |p| p.to_string());
                    let media = a.info["media"].as_str().or(a.info["title"].as_str()).unwrap_or("");
                    println!(
                        "  app {:<18} {:<8} level {:<8} gain {:<8}{}  {exe} pid {pid}  {media}",
                        a.name(),
                        state,
                        level(peak_db(&a.info["peak"])),
                        ui::format_db(a.gain()),
                        if a.muted() { " MUTED" } else { "" },
                    );
                }
            }
        }
    }
}

fn print_json(mixer: &Mixer) {
    let groups: Vec<Value> = mixer
        .model
        .groups()
        .into_iter()
        .map(|group| {
            let client = group.ip.and_then(|ip| mixer.model.clients.get(&ip));
            let streams: Vec<&Value> = group
                .strips
                .iter()
                .filter_map(|s| match s {
                    Strip::Stream(path) => mixer.model.stream(path).map(|s| &s.status),
                    _ => None,
                })
                .collect();
            let apps: Vec<&Value> = client.map_or(Vec::new(), |c| c.apps.iter().map(|a| &a.info).collect());
            json!({
                "address": group.ip.map(|ip| ip.to_string()),
                "name": group.ip.map(|ip| mixer.model.client_name(ip)),
                "host": client.and_then(|c| c.host.clone().or_else(|| c.resolved.clone())),
                "os": client.and_then(|c| c.os.clone()),
                "agent": client.is_some_and(|c| c.has_agent()),
                "streams": streams,
                "apps": apps,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&groups).unwrap_or_default());
}

fn main() {
    let opts = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        if !e.is_empty() {
            eprintln!("pcmlink-mixer: {e}");
        }
        eprintln!("{USAGE}");
        std::process::exit(2);
    });
    if opts.list || opts.json {
        let mixer = gather(&opts);
        if opts.json {
            print_json(&mixer);
        } else {
            print_list(&mixer);
        }
        return;
    }
    if let Err(e) = run_tui(&opts) {
        eprintln!("pcmlink-mixer: {e}");
        std::process::exit(1);
    }
}
