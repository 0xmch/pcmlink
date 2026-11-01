//! What the mixer knows: streams (from receivers), clients (where the streams come
//! from) and their applications (from agents). Nothing here names a client: they
//! appear when packets arrive from a new address.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::{IpAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{json, Value};

/// The fader's and the meters' range.
pub const FLOOR_DB: f32 = -60.0;
/// A stream older than this is not arriving any more.
pub const STALE_MS: u64 = 2000;
const METER_FALL_DB_PER_S: f32 = 30.0;
const HOLD_S: f32 = 1.5;
const HISTORY: usize = 240;

pub fn to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}

pub fn to_linear(db: f32) -> f32 {
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// The gain one step from `gain`, on a whole-dB grid. It stops at unity going up:
/// a mixer key must never make anything louder than the source. Below the floor
/// is silence, and the first step up from silence is the floor.
pub fn step_gain(gain: f32, delta_db: f32) -> f32 {
    let db = to_db(gain);
    if db == f32::NEG_INFINITY {
        return if delta_db > 0.0 { to_linear(FLOOR_DB) } else { 0.0 };
    }
    // Already above unity (--volume 2): up does nothing, down starts from there.
    if db > 0.0 && delta_db > 0.0 {
        return gain;
    }
    let next = db.round() + delta_db;
    if next < FLOOR_DB {
        0.0
    } else {
        to_linear(next.min(db.max(0.0)))
    }
}

/// One channel's meter: falls smoothly, and remembers its recent peak.
#[derive(Clone, Copy)]
pub struct Meter {
    pub peak_db: f32,
    pub rms_db: f32,
    pub hold_db: f32,
    hold_age: f32,
}

impl Default for Meter {
    fn default() -> Self {
        Meter { peak_db: FLOOR_DB, rms_db: FLOOR_DB, hold_db: FLOOR_DB, hold_age: 0.0 }
    }
}

impl Meter {
    pub fn feed(&mut self, peak: f32, rms: Option<f32>) {
        let db = to_db(peak).max(FLOOR_DB);
        self.peak_db = self.peak_db.max(db);
        if db >= self.hold_db {
            self.hold_db = db;
            self.hold_age = 0.0;
        }
        // With no average to show (applications report only a peak), the whole
        // bar is drawn as the body.
        let body = rms.map_or(db, |rms| to_db(rms).max(FLOOR_DB));
        self.rms_db = self.rms_db.max(body);
    }

    pub fn tick(&mut self, dt: f32) {
        let fall = METER_FALL_DB_PER_S * dt;
        self.peak_db = (self.peak_db - fall).max(FLOOR_DB);
        self.rms_db = (self.rms_db - fall).max(FLOOR_DB);
        self.hold_age += dt;
        if self.hold_age > HOLD_S {
            self.hold_db = (self.hold_db - fall).max(self.peak_db);
        }
    }
}

pub struct Stream {
    pub path: PathBuf,
    writer: UnixStream,
    pub status: Value,
    pub meters: [Meter; 2],
    pub peer: Option<IpAddr>,
    pub buffer_history: VecDeque<f32>,
    pub packet_rate: f64,
    pub under: u64,
    pub last_dropout: Option<Instant>,
    rate_from: Option<(Instant, u64)>,
    history_at: Option<Instant>,
}

impl Stream {
    pub fn port(&self) -> u64 {
        self.status["port"].as_u64().unwrap_or(0)
    }
    pub fn gain(&self) -> f32 {
        self.status["gain"].as_f64().unwrap_or(1.0) as f32
    }
    pub fn muted(&self) -> bool {
        self.status["mute"].as_bool().unwrap_or(false)
    }
    pub fn num(&self, key: &str) -> f64 {
        self.status[key].as_f64().unwrap_or(0.0)
    }
    /// Packets are arriving now.
    pub fn flowing(&self) -> bool {
        self.status["age_ms"].as_u64().is_some_and(|age| age < STALE_MS) && self.num("packets") > 0.0
    }
    pub fn send(&mut self, line: &str) -> bool {
        self.writer.write_all(format!("{line}\n").as_bytes()).is_ok()
    }

    fn update(&mut self, status: Value) {
        let now = Instant::now();
        for c in 0..2 {
            let peak = status["peak"][c].as_f64().unwrap_or(0.0) as f32;
            let rms = status["rms"][c].as_f64().unwrap_or(0.0) as f32;
            self.meters[c].feed(peak, Some(rms));
        }
        self.peer = status["peer"].as_str().and_then(|p| p.parse().ok()).or(self.peer);

        let packets = status["packets"].as_u64().unwrap_or(0);
        match self.rate_from {
            Some((at, from)) if now.duration_since(at).as_secs_f64() >= 1.0 => {
                let seconds = now.duration_since(at).as_secs_f64();
                self.packet_rate = packets.saturating_sub(from) as f64 / seconds;
                self.rate_from = Some((now, packets));
            }
            None => self.rate_from = Some((now, packets)),
            _ => {}
        }
        if self.history_at.is_none_or(|at| now.duration_since(at).as_secs_f64() >= 1.0) {
            self.history_at = Some(now);
            self.buffer_history.push_back(status["buf_ms"].as_f64().unwrap_or(0.0) as f32);
            if self.buffer_history.len() > HISTORY {
                self.buffer_history.pop_front();
            }
        }
        let under = status["under"].as_u64().unwrap_or(0);
        if under > self.under && self.status != Value::Null {
            self.last_dropout = Some(now);
        }
        self.under = under;
        self.status = status;
    }
}

pub struct App {
    pub id: String,
    pub info: Value,
    pub meters: [Meter; 2],
    /// A peak is not available on every platform; then there is no meter to show.
    pub metered: bool,
}

impl App {
    pub fn name(&self) -> &str {
        self.info["name"].as_str().unwrap_or("?")
    }
    pub fn gain(&self) -> f32 {
        self.info["volume"].as_f64().unwrap_or(1.0) as f32
    }
    pub fn muted(&self) -> bool {
        self.info["mute"].as_bool().unwrap_or(false)
    }
    pub fn active(&self) -> bool {
        self.info["active"].as_bool().unwrap_or(false)
    }
}

#[derive(Default)]
pub struct Client {
    /// What you call it: its alias in ~/.ssh/config.
    pub alias: Option<String>,
    /// What it calls itself, through its agent.
    pub host: Option<String>,
    /// What the network calls it: multicast DNS or reverse DNS.
    pub resolved: Option<String>,
    pub os: Option<String>,
    pub apps: Vec<App>,
    agent: Option<TcpStream>,
}

impl Client {
    /// An agent has said hello. A connection alone proves nothing: an agent that
    /// doesn't allow this machine accepts and then closes it.
    pub fn has_agent(&self) -> bool {
        self.agent.is_some() && self.os.is_some()
    }
    pub fn send(&mut self, message: &Value) -> bool {
        match self.agent.as_mut() {
            Some(agent) => agent.write_all(format!("{message}\n").as_bytes()).is_ok(),
            None => false,
        }
    }
}

/// A strip, by identity: selection and solo survive streams and apps coming and going.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Strip {
    Stream(PathBuf),
    App(IpAddr, String),
}

/// Strips grouped under where they come from, for drawing.
pub struct Group {
    pub ip: Option<IpAddr>,
    pub strips: Vec<Strip>,
}

#[derive(Default)]
pub struct Model {
    pub streams: Vec<Stream>,
    pub clients: HashMap<IpAddr, Client>,
    pub show_idle: bool,
}

impl Model {
    pub fn add_stream(&mut self, path: PathBuf, writer: UnixStream) {
        self.streams.push(Stream {
            path,
            writer,
            status: Value::Null,
            meters: Default::default(),
            peer: None,
            buffer_history: VecDeque::new(),
            packet_rate: 0.0,
            under: 0,
            last_dropout: None,
            rate_from: None,
            history_at: None,
        });
    }

    pub fn remove_stream(&mut self, path: &PathBuf) {
        self.streams.retain(|s| &s.path != path);
    }

    pub fn stream_status(&mut self, path: &PathBuf, status: Value) {
        if let Some(stream) = self.streams.iter_mut().find(|s| &s.path == path) {
            stream.update(status);
        }
    }

    pub fn agent_up(&mut self, ip: IpAddr, writer: TcpStream) {
        self.clients.entry(ip).or_default().agent = Some(writer);
    }

    pub fn agent_gone(&mut self, ip: IpAddr) {
        if let Some(client) = self.clients.get_mut(&ip) {
            client.agent = None;
            client.apps.clear();
            client.host = None;
            client.os = None;
        }
    }

    /// One line from an agent. Returns an error or notice for the status bar.
    pub fn agent_message(&mut self, ip: IpAddr, message: Value) -> Option<String> {
        let client = self.clients.entry(ip).or_default();
        if let Some(hello) = message.get("hello") {
            client.host = hello["host"].as_str().map(str::to_string);
            client.os = hello["os"].as_str().map(str::to_string);
        } else if let Some(apps) = message.get("apps").and_then(Value::as_array) {
            let mut next = Vec::with_capacity(apps.len());
            for info in apps {
                let Some(id) = info["id"].as_str().map(str::to_string) else { continue };
                let meters = client.apps.iter().find(|a| a.id == id).map(|a| a.meters).unwrap_or_default();
                let mut app = App { id, metered: info["peak"].is_array(), info: info.clone(), meters };
                for c in 0..2 {
                    let peak = info["peak"][c].as_f64().unwrap_or(0.0) as f32;
                    app.meters[c].feed(peak, None);
                }
                next.push(app);
            }
            // By name, not by when they appeared, so the strips don't jump around.
            next.sort_by_key(|a| a.name().to_lowercase());
            client.apps = next;
        } else if let Some(text) = message.get("error").and_then(Value::as_str) {
            return Some(format!("error: {text}"));
        } else if let Some(text) = message.get("notice").and_then(Value::as_str) {
            return Some(text.to_string());
        }
        None
    }

    pub fn tick(&mut self, dt: f32) {
        for stream in &mut self.streams {
            for meter in &mut stream.meters {
                meter.tick(dt);
            }
        }
        for client in self.clients.values_mut() {
            for app in &mut client.apps {
                for meter in &mut app.meters {
                    meter.tick(dt);
                }
            }
        }
    }

    /// Idle applications are clutter (a game's launcher, a chat client), unless
    /// they are muted: that is state somebody may want to find and undo.
    fn app_shown(&self, app: &App) -> bool {
        self.show_idle || app.active() || app.muted()
    }

    /// Strips in display order: clients by their lowest port, then a client's
    /// streams, then its applications.
    pub fn groups(&self) -> Vec<Group> {
        let mut streams: Vec<&Stream> = self.streams.iter().filter(|s| !s.status.is_null()).collect();
        streams.sort_by_key(|s| s.port());
        let mut groups: Vec<Group> = Vec::new();
        for stream in streams {
            let id = Strip::Stream(stream.path.clone());
            // A receiver nobody has sent to yet stands alone.
            match groups.iter_mut().find(|g| g.ip.is_some() && g.ip == stream.peer) {
                Some(group) => group.strips.push(id),
                None => groups.push(Group { ip: stream.peer, strips: vec![id] }),
            }
        }
        for group in &mut groups {
            let Some(ip) = group.ip else { continue };
            let Some(client) = self.clients.get(&ip) else { continue };
            for app in &client.apps {
                if self.app_shown(app) {
                    group.strips.push(Strip::App(ip, app.id.clone()));
                }
            }
        }
        groups
    }

    pub fn strips(&self) -> Vec<Strip> {
        self.groups().into_iter().flat_map(|g| g.strips).collect()
    }

    pub fn stream(&self, path: &PathBuf) -> Option<&Stream> {
        self.streams.iter().find(|s| &s.path == path)
    }

    pub fn app(&self, ip: IpAddr, id: &str) -> Option<&App> {
        self.clients.get(&ip)?.apps.iter().find(|a| a.id == id)
    }

    pub fn gain(&self, strip: &Strip) -> Option<f32> {
        match strip {
            Strip::Stream(path) => self.stream(path).map(Stream::gain),
            Strip::App(ip, id) => self.app(*ip, id).map(App::gain),
        }
    }

    pub fn muted(&self, strip: &Strip) -> Option<bool> {
        match strip {
            Strip::Stream(path) => self.stream(path).map(Stream::muted),
            Strip::App(ip, id) => self.app(*ip, id).map(App::muted),
        }
    }

    /// Set a strip's gain, at the receiver or agent, and on screen at once.
    pub fn set_gain(&mut self, strip: &Strip, gain: f32) {
        match strip {
            Strip::Stream(path) => {
                if let Some(stream) = self.streams.iter_mut().find(|s| &s.path == path) {
                    stream.send(&format!("volume {gain}"));
                    stream.status["gain"] = json!(gain);
                }
            }
            Strip::App(ip, id) => {
                if let Some(client) = self.clients.get_mut(ip) {
                    client.send(&json!({"cmd": "volume", "id": id, "value": gain}));
                    if let Some(app) = client.apps.iter_mut().find(|a| &a.id == id) {
                        app.info["volume"] = json!(gain);
                    }
                }
            }
        }
    }

    pub fn set_mute(&mut self, strip: &Strip, mute: bool) {
        match strip {
            Strip::Stream(path) => {
                if let Some(stream) = self.streams.iter_mut().find(|s| &s.path == path) {
                    stream.send(if mute { "mute 1" } else { "mute 0" });
                    stream.status["mute"] = json!(mute);
                }
            }
            Strip::App(ip, id) => {
                if let Some(client) = self.clients.get_mut(ip) {
                    client.send(&json!({"cmd": "mute", "id": id, "value": mute}));
                    if let Some(app) = client.apps.iter_mut().find(|a| &a.id == id) {
                        app.info["mute"] = json!(mute);
                    }
                }
            }
        }
    }

    /// The strips a solo silences: other streams for a stream, the same client's
    /// other applications for an application.
    pub fn siblings(&self, strip: &Strip) -> Vec<Strip> {
        match strip {
            Strip::Stream(path) => self
                .streams
                .iter()
                .filter(|s| &s.path != path)
                .map(|s| Strip::Stream(s.path.clone()))
                .collect(),
            Strip::App(ip, id) => self.clients.get(ip).map_or(Vec::new(), |client| {
                client
                    .apps
                    .iter()
                    .filter(|a| &a.id != id)
                    .map(|a| Strip::App(*ip, a.id.clone()))
                    .collect()
            }),
        }
    }

    /// The name a client goes by: see names.rs for the order.
    pub fn client_name(&self, ip: IpAddr) -> String {
        let client = self.clients.get(&ip);
        client
            .and_then(|c| c.alias.clone().or_else(|| c.host.clone()).or_else(|| c.resolved.clone()))
            .unwrap_or_else(|| ip.to_string())
    }

    /// Where a client's name came from aside, how else to recognise it: its own
    /// name, if that isn't what it's already called, and its address.
    pub fn client_full_name(&self, ip: IpAddr) -> String {
        let name = self.client_name(ip);
        let own = self.clients.get(&ip).and_then(|c| c.host.clone().or_else(|| c.resolved.clone()));
        match own.filter(|own| !own.eq_ignore_ascii_case(&name)) {
            Some(own) => format!("{name} ({own}, {ip})"),
            None if name == ip.to_string() => name,
            None => format!("{name} ({ip})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> UnixStream {
        UnixStream::pair().unwrap().0
    }

    fn status(port: u64, peer: &str) -> Value {
        json!({"port": port, "peer": peer, "gain": 1.0, "mute": false, "packets": 1, "age_ms": 1})
    }

    fn model_with(streams: &[(u64, &str)]) -> Model {
        let mut model = Model::default();
        for (port, peer) in streams {
            let path = PathBuf::from(format!("{port}.sock"));
            model.add_stream(path.clone(), pair());
            model.stream_status(&path, status(*port, peer));
        }
        model
    }

    #[test]
    fn gain_steps_stop_at_unity_and_fall_to_silence() {
        assert_eq!(step_gain(1.0, 1.0), 1.0);
        assert!((to_db(step_gain(1.0, -1.0)) + 1.0).abs() < 0.01);
        assert!((to_db(step_gain(0.5, 6.0)) - 0.0).abs() < 0.01); // -6 dB + 6
        assert_eq!(step_gain(to_linear(-60.0), -1.0), 0.0);
        assert!((to_db(step_gain(0.0, 1.0)) - FLOOR_DB).abs() < 0.01);
        assert_eq!(step_gain(0.0, -1.0), 0.0);
    }

    #[test]
    fn a_boost_set_at_startup_never_goes_higher() {
        assert_eq!(step_gain(2.0, 1.0), 2.0);
        assert!(step_gain(2.0, -1.0) < 2.0);
    }

    #[test]
    fn meter_jumps_up_falls_slowly_and_holds() {
        let mut m = Meter::default();
        m.feed(0.5, None);
        assert!((m.peak_db + 6.02).abs() < 0.01);
        m.tick(0.1);
        assert!((m.peak_db + 9.02).abs() < 0.01);
        assert!((m.hold_db + 6.02).abs() < 0.01); // still held
        m.tick(2.0);
        assert!(m.hold_db < -6.1);
    }

    #[test]
    fn streams_group_by_where_their_packets_come_from() {
        let model = model_with(&[(5004, "10.0.0.2"), (5006, "10.0.0.3"), (5010, "10.0.0.2")]);
        let groups = model.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].ip, "10.0.0.2".parse().ok());
        assert_eq!(groups[0].strips.len(), 2);
        assert_eq!(groups[1].strips.len(), 1);
    }

    #[test]
    fn a_new_client_needs_nothing_but_its_packets() {
        let mut model = model_with(&[(5004, "10.0.0.2")]);
        assert_eq!(model.groups().len(), 1);
        let path = PathBuf::from("5004.sock");
        model.stream_status(&path, status(5004, "10.0.0.9"));
        assert_eq!(model.groups()[0].ip, "10.0.0.9".parse().ok());
    }

    #[test]
    fn idle_apps_are_hidden_unless_muted_or_asked_for() {
        let mut model = model_with(&[(5004, "10.0.0.2")]);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        model.agent_message(ip, json!({"apps": [
            {"id": "1", "name": "Playing", "active": true, "mute": false, "volume": 1.0, "peak": [0.1, 0.1]},
            {"id": "2", "name": "Idle", "active": false, "mute": false, "volume": 1.0, "peak": [0.0, 0.0]},
            {"id": "3", "name": "Muted", "active": false, "mute": true, "volume": 1.0, "peak": null},
        ]}));
        assert_eq!(model.strips().len(), 3); // stream + Muted + Playing
        model.show_idle = true;
        assert_eq!(model.strips().len(), 4);
        assert!(!model.app(ip, "3").unwrap().metered);
    }

    #[test]
    fn apps_keep_their_meters_across_updates() {
        let mut model = model_with(&[(5004, "10.0.0.2")]);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        model.agent_message(ip, json!({"apps": [{"id": "1", "name": "a", "peak": [1.0, 1.0]}]}));
        model.agent_message(ip, json!({"apps": [{"id": "1", "name": "a", "peak": [0.0, 0.0]}]}));
        assert!(model.app(ip, "1").unwrap().meters[0].peak_db > -1.0);
    }

    #[test]
    fn solo_scope() {
        let mut model = model_with(&[(5004, "10.0.0.2"), (5006, "10.0.0.3")]);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        model.agent_message(ip, json!({"apps": [{"id": "1", "name": "a"}, {"id": "2", "name": "b"}]}));
        assert_eq!(model.siblings(&Strip::Stream("5004.sock".into())), vec![Strip::Stream("5006.sock".into())]);
        assert_eq!(model.siblings(&Strip::App(ip, "1".into())), vec![Strip::App(ip, "2".into())]);
    }

    #[test]
    fn a_connection_is_not_an_agent_until_it_says_hello() {
        let mut model = model_with(&[(5004, "127.0.0.1")]);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        model.agent_up(ip, writer);
        assert!(!model.clients[&ip].has_agent());
        model.agent_message(ip, json!({"hello": {"host": "a", "os": "Linux"}}));
        assert!(model.clients[&ip].has_agent());
        model.agent_gone(ip);
        assert!(!model.clients[&ip].has_agent());
    }

    #[test]
    fn a_client_is_named_by_alias_then_agent_then_network_then_address() {
        let mut model = model_with(&[(5004, "10.0.0.2")]);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(model.client_name(ip), "10.0.0.2");
        assert_eq!(model.client_full_name(ip), "10.0.0.2");
        model.clients.entry(ip).or_default().resolved = Some("from-dns".into());
        assert_eq!(model.client_name(ip), "from-dns");
        assert_eq!(model.client_full_name(ip), "from-dns (10.0.0.2)");
        model.agent_message(ip, json!({"hello": {"host": "from-agent", "os": "Linux"}}));
        assert_eq!(model.client_name(ip), "from-agent");
        model.clients.entry(ip).or_default().alias = Some("mine".into());
        assert_eq!(model.client_name(ip), "mine");
        assert_eq!(model.client_full_name(ip), "mine (from-agent, 10.0.0.2)");
    }

    #[test]
    fn an_agent_going_away_keeps_the_names_it_did_not_give() {
        let mut model = model_with(&[(5004, "10.0.0.2")]);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        model.clients.entry(ip).or_default().alias = Some("mine".into());
        model.agent_message(ip, json!({"hello": {"host": "from-agent", "os": "Linux"}}));
        model.agent_gone(ip);
        assert_eq!(model.client_name(ip), "mine");
        assert_eq!(model.clients[&ip].host, None);
    }
}
