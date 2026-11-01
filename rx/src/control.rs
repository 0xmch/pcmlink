//! The control socket: how a mixer watches and steers this receiver.
//!
//! Each receiver listens on a Unix socket named after its UDP port, in one
//! directory. A mixer finds receivers by listing that directory, so nothing has
//! to know in advance which links exist. The protocol is plain text, one message
//! per line: while a client is connected it is sent a JSON status about 25 times
//! a second, and it can send
//!
//!     volume 0.5     linear gain, 1.0 = unity (0 to 8, like --volume)
//!     mute 1         1 or 0
//!     reopen         close the output and open it again
//!
//! This thread cannot delay the audio. It touches the network thread only through
//! atomics, and it never takes a lock the network thread waits on (`set_peer`
//! gives up instead of blocking). It takes no time while no mixer is connected:
//! it sleeps in accept(), and the meters just accumulate.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::rtp::Levels;
use crate::Shared;

const TICK: Duration = Duration::from_millis(40);
const MAX_GAIN: f32 = 8.0;

/// What outlives one run of the output: a device change keeps the fader.
pub struct Live {
    gain: AtomicU32, // f32 bits
    muted: AtomicBool,
    reopen: AtomicBool,
    peak: [AtomicU32; 2],
    energy: [AtomicU64; 2],
    frames: AtomicU64,
    peer: Mutex<Option<SocketAddr>>,
    current: Mutex<Option<Current>>,
    error: Mutex<Option<String>>,
    started: Instant,
}

struct Current {
    shared: Arc<Shared>,
    device: String,
    rate: f64,
}

impl Live {
    pub fn new(volume: f32) -> Live {
        Live {
            gain: AtomicU32::new(volume.to_bits()),
            muted: AtomicBool::new(false),
            reopen: AtomicBool::new(false),
            peak: Default::default(),
            energy: Default::default(),
            frames: AtomicU64::new(0),
            peer: Mutex::new(None),
            current: Mutex::new(None),
            error: Mutex::new(None),
            started: Instant::now(),
        }
    }

    /// The gain the network thread should be at.
    pub fn target_gain(&self) -> f32 {
        if self.muted.load(Relaxed) {
            0.0
        } else {
            f32::from_bits(self.gain.load(Relaxed))
        }
    }

    /// Network thread: add one packet to the meters, before the gain, so a muted
    /// stream still shows what it is carrying.
    pub fn meter(&self, levels: &Levels, frames: u32) {
        for c in 0..2 {
            self.peak[c].fetch_max(levels.peak[c] as u32, Relaxed);
            self.energy[c].fetch_add(levels.energy[c], Relaxed);
        }
        self.frames.fetch_add(frames as u64, Relaxed);
    }

    /// Network thread: note who is sending. False if the lock was busy, and the
    /// caller tries again on the next packet rather than wait for it.
    pub fn set_peer(&self, peer: SocketAddr) -> bool {
        match self.peer.try_lock() {
            Ok(mut guard) => {
                *guard = Some(peer);
                true
            }
            Err(_) => false,
        }
    }

    pub fn set_current(&self, shared: Arc<Shared>, device: String, rate: f64) {
        *self.current.lock().unwrap() = Some(Current { shared, device, rate });
    }

    pub fn set_error(&self, error: Option<String>) {
        *self.error.lock().unwrap() = error;
    }

    pub fn take_reopen(&self) -> bool {
        self.reopen.swap(false, Relaxed)
    }

    /// Read and reset the meters: they cover the time since the last call.
    fn take_meters(&self) -> ([f32; 2], [f32; 2]) {
        let frames = self.frames.swap(0, Relaxed);
        let mut peak = [0.0; 2];
        let mut rms = [0.0; 2];
        for c in 0..2 {
            peak[c] = self.peak[c].swap(0, Relaxed) as f32 / 32768.0;
            let energy = self.energy[c].swap(0, Relaxed);
            if frames > 0 {
                rms[c] = ((energy as f64 / frames as f64).sqrt() / 32768.0) as f32;
            }
        }
        (peak, rms)
    }

    fn apply(&self, line: &str) {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("volume"), Some(v)) => {
                if let Ok(v) = v.parse::<f32>() {
                    if v.is_finite() {
                        self.gain.store(v.clamp(0.0, MAX_GAIN).to_bits(), Relaxed);
                    }
                }
            }
            (Some("mute"), Some(v)) => self.muted.store(v == "1", Relaxed),
            (Some("reopen"), None) => self.reopen.store(true, Relaxed),
            _ => {}
        }
    }

    fn status(&self, port: u16, margin_ms: f64, stream_rate: u32) -> String {
        let (peak, rms) = self.take_meters();
        let peer = self.peer.lock().unwrap().map(|p| p.ip().to_string());
        let error = self.error.lock().unwrap().clone();
        let mut s = format!(
            "{{\"port\":{port},\"pid\":{},\"up_s\":{},\"gain\":{},\"mute\":{},\"peak\":[{:.5},{:.5}],\"rms\":[{:.5},{:.5}],\"margin_ms\":{margin_ms},\"peer\":{},\"error\":{}",
            std::process::id(),
            self.started.elapsed().as_secs(),
            f32::from_bits(self.gain.load(Relaxed)),
            self.muted.load(Relaxed),
            peak[0],
            peak[1],
            rms[0],
            rms[1],
            json_string(peer.as_deref()),
            json_string(error.as_deref()),
        );
        if let Some(c) = self.current.lock().unwrap().as_ref() {
            let d = &c.shared;
            let ms = |frames: u32| frames as f64 / c.rate * 1000.0;
            let last = d.last_packet_ms.load(Relaxed);
            s += &format!(
                ",\"device\":{},\"rate\":{},\"on\":{},\"playing\":{},\"buf_ms\":{:.1},\"low_ms\":{:.1},\"packets\":{},\"age_ms\":{},\"lost\":{},\"late\":{},\"dup\":{},\"bad\":{},\"resets\":{},\"gap_ms\":{},\"under\":{},\"over\":{},\"drift_ppm\":{}",
                json_string(Some(&c.device)),
                c.rate,
                d.on.load(Relaxed),
                d.playing.load(Relaxed),
                ms(d.mean_frames.load(Relaxed)),
                ms(d.low_frames.load(Relaxed)),
                d.packets.load(Relaxed),
                crate::now_ms().saturating_sub(last),
                d.lost.load(Relaxed),
                d.late.load(Relaxed),
                d.dup.load(Relaxed),
                d.bad.load(Relaxed),
                d.resets.load(Relaxed),
                d.gap_frames.load(Relaxed) * 1000 / stream_rate as u64,
                d.underruns.load(Relaxed),
                d.overflows.load(Relaxed),
                d.drift_ppm.load(Relaxed),
            );
        }
        s += "}\n";
        s
    }
}

fn json_string(value: Option<&str>) -> String {
    let Some(value) = value else { return "null".into() };
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn socket_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PCMLINK_RX_DIR") {
        return Some(dir.into());
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/pcmlink/rx"))
}

fn listen(port: u16) -> Result<(UnixListener, PathBuf), String> {
    let dir = socket_dir().ok_or("HOME is not set")?;
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let path = dir.join(format!("{port}.sock"));
    // The UDP port is already ours, so a socket left here belongs to a receiver
    // that is gone.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((listener, path))
}

/// Start the control thread. Audio matters more than the mixer: if the socket
/// cannot be set up, say so and carry on without one.
pub fn spawn(port: u16, margin_ms: f64, stream_rate: u32, live: Arc<Live>) {
    let (listener, path) = match listen(port) {
        Ok(found) => found,
        Err(e) => {
            println!("pcmlink: no control socket: {e}");
            return;
        }
    };
    println!("pcmlink receive: control socket {}", path.display());
    let body = move || serve(listener, port, margin_ms, stream_rate, &live);
    if let Err(e) = thread::Builder::new().name("control".into()).spawn(body) {
        println!("pcmlink: no control thread: {e}");
    }
}

struct Client {
    stream: UnixStream,
    pending: Vec<u8>,
}

fn serve(listener: UnixListener, port: u16, margin_ms: f64, stream_rate: u32, live: &Live) {
    let mut clients: Vec<Client> = Vec::new();
    let add = |stream: UnixStream, clients: &mut Vec<Client>| {
        if stream.set_nonblocking(true).is_ok() {
            clients.push(Client { stream, pending: Vec::new() });
        }
    };
    loop {
        if clients.is_empty() {
            // Nobody is watching: wait without a timer. What the meters gathered
            // meanwhile is stale, so the first status does not report it.
            let _ = listener.set_nonblocking(false);
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_secs(1));
                continue;
            };
            add(stream, &mut clients);
            let _ = listener.set_nonblocking(true);
            live.take_meters();
        }
        while let Ok((stream, _)) = listener.accept() {
            add(stream, &mut clients);
        }

        let status = live.status(port, margin_ms, stream_rate);
        clients.retain_mut(|client| read_commands(client, live) && send(client, status.as_bytes()));
        thread::sleep(TICK);
    }
}

/// False if the client has gone.
fn read_commands(client: &mut Client, live: &Live) -> bool {
    let mut buf = [0u8; 512];
    loop {
        match client.stream.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => client.pending.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    while let Some(end) = client.pending.iter().position(|&b| b == b'\n') {
        if let Ok(line) = std::str::from_utf8(&client.pending[..end]) {
            live.apply(line);
        }
        client.pending.drain(..=end);
    }
    client.pending.len() < 4096 // a client that never ends a line is not a mixer
}

/// A client that cannot keep up is dropped, and can connect again: this thread
/// never waits for one, and a half-written line could not be finished anyway.
fn send(client: &mut Client, bytes: &[u8]) -> bool {
    matches!(client.stream.write(bytes), Ok(n) if n == bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_steer_the_target_gain() {
        let live = Live::new(1.0);
        live.apply("volume 0.5");
        assert_eq!(live.target_gain(), 0.5);
        live.apply("mute 1");
        assert_eq!(live.target_gain(), 0.0);
        live.apply("volume 0.25");
        assert_eq!(live.target_gain(), 0.0); // the fader moves, still muted
        live.apply("mute 0");
        assert_eq!(live.target_gain(), 0.25);
    }

    #[test]
    fn bad_commands_change_nothing() {
        let live = Live::new(1.0);
        for line in ["volume", "volume loud", "volume NaN", "volume inf", "mystery 3", ""] {
            live.apply(line);
        }
        assert_eq!(live.target_gain(), 1.0);
        live.apply("volume 100");
        assert_eq!(live.target_gain(), MAX_GAIN);
    }

    #[test]
    fn meters_cover_the_time_since_the_last_read() {
        let live = Live::new(1.0);
        live.meter(&Levels { peak: [16384, 8192], energy: [16384 * 16384 * 4, 8192 * 8192 * 4] }, 4);
        let (peak, rms) = live.take_meters();
        assert_eq!(peak, [0.5, 0.25]);
        assert_eq!(rms, [0.5, 0.25]);
        let (peak, rms) = live.take_meters();
        assert_eq!((peak, rms), ([0.0, 0.0], [0.0, 0.0]));
    }

    #[test]
    fn status_is_json_with_awkward_names() {
        assert_eq!(json_string(Some("a\"b\\c\n")), "\"a\\\"b\\\\c\\u000a\"");
        assert_eq!(json_string(None), "null");
    }
}
