//! pcmlink-rx: plays a pcmlink stream (RTP L16 over UDP) on a CoreAudio device.
//!
//! Three threads, each with one job:
//! - network: receive, decode, convert to the device rate with drift control,
//!   and fill a lock-free ring buffer (net.rs);
//! - CoreAudio's real-time callback: copy from the ring to the device (output.rs);
//! - main: start and stop the output for the silence gate, print statistics,
//!   and rebuild everything if the device goes away or changes rate.

mod net;
mod output;
mod rtp;

use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: pcmlink-rx [--port 5004] [--bind 0.0.0.0] [--device NAME] [--volume 1.0]
                  [--gate SECONDS] [--idle 10] [--margin-ms 10] [--io-frames N]
                  [--rate 48000] [--payload 96] [--no-stats]

  --device     output device whose name contains NAME; empty = the current default output
  --volume     gain, 1.0 = unity (0 to 8)
  --gate       stop the output after this many seconds of silence and restart it
               on the next sound; 0 = never
  --idle       stop the output after this many seconds without any packet (the
               sender is off) and restart it on the next packet; 0 = never
  --margin-ms  audio the buffer keeps in reserve at its lowest point; latency adapts
               to the sender's bursts on top of this
  --io-frames  ask the device for this I/O block size (default: leave it to CoreAudio;
               a small value makes every client of the device cycle faster)";

/// Counters and state shared by the three threads.
#[derive(Default)]
pub struct Shared {
    pub packets: AtomicU64,
    pub lost: AtomicU64,
    pub late: AtomicU64,
    pub dup: AtomicU64,
    pub bad: AtomicU64,
    pub resets: AtomicU64,
    /// Frames of silence inserted for audio that never arrived.
    pub gap_frames: AtomicU64,
    /// Times the output ran dry while the stream kept coming. Running dry
    /// because the sender stopped is not counted.
    pub underruns: AtomicU64,
    /// Set by the callback when it runs dry; the network thread decides, on the
    /// next packet, whether that was a dropout (the stream kept coming) or the
    /// end of a stream (it paused first).
    pub dry: AtomicBool,
    /// When the last packet arrived, in ms since START.
    pub last_packet_ms: AtomicU64,
    /// Times the ring was full (the output stalled).
    pub overflows: AtomicU64,
    /// Buffer level over the last second, in frames: average and low point.
    pub mean_frames: AtomicU32,
    pub low_frames: AtomicU32,
    pub drift_ppm: AtomicI32,
    /// The output is running and past its priming.
    pub playing: AtomicBool,
    pub stop: AtomicBool,
}

/// Time origin for timestamps shared between threads.
pub static START: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

/// Network thread to main thread: the silence gate's decisions.
pub enum Ctl {
    Wake,
    Idle,
}

struct Opts {
    port: u16,
    bind: String,
    device: String,
    volume: f32,
    gate: u64,
    idle: u64,
    margin_ms: f64,
    io_frames: Option<u32>,
    rate: u32,
    payload: u8,
    stats: bool,
}

impl Opts {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
        let mut o = Opts {
            port: 5004,
            bind: "0.0.0.0".into(),
            device: String::new(),
            volume: 1.0,
            gate: 0,
            idle: 10,
            margin_ms: 10.0,
            io_frames: None,
            rate: 48_000,
            payload: 96,
            stats: true,
        };
        while let Some(flag) = args.next() {
            if flag == "--no-stats" {
                o.stats = false;
                continue;
            }
            if flag == "-h" || flag == "--help" {
                return Err(String::new());
            }
            let value = args.next().ok_or(format!("{flag} needs a value"))?;
            fn num<T: std::str::FromStr>(flag: &str, value: &str) -> Result<T, String> {
                value.parse().map_err(|_| format!("bad value for {flag}: {value}"))
            }
            match flag.as_str() {
                "--port" => o.port = num(&flag, &value)?,
                "--bind" => o.bind = value,
                "--device" => o.device = value,
                "--volume" => o.volume = num(&flag, &value)?,
                "--gate" => o.gate = num(&flag, &value)?,
                "--idle" => o.idle = num(&flag, &value)?,
                "--margin-ms" => o.margin_ms = num(&flag, &value)?,
                "--io-frames" => o.io_frames = Some(num(&flag, &value)?),
                "--rate" => o.rate = num(&flag, &value)?,
                "--payload" => o.payload = num(&flag, &value)?,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if !(0.0..=8.0).contains(&o.volume) {
            return Err(format!("volume {} out of range (0 to 8)", o.volume));
        }
        if !(1.0..=500.0).contains(&o.margin_ms) {
            return Err(format!("margin {} ms out of range (1 to 500)", o.margin_ms));
        }
        Ok(o)
    }
}

/// Local wall-clock time, HH:MM:SS, for log lines.
pub fn clock() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn main() {
    let opts = Opts::parse(std::env::args().skip(1)).unwrap_or_else(|e| {
        if !e.is_empty() {
            eprintln!("pcmlink-rx: {e}");
        }
        eprintln!("{USAGE}");
        std::process::exit(2);
    });
    // Bound once for the life of the process, so a second receiver on the same
    // port fails here instead of splitting the stream with this one.
    let socket = UdpSocket::bind((opts.bind.as_str(), opts.port)).unwrap_or_else(|e| {
        eprintln!("pcmlink-rx: cannot bind {}:{} ({e}); already running?", opts.bind, opts.port);
        std::process::exit(2);
    });
    let buffer: libc::c_int = 1 << 20; // room for a second of bursts
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&buffer as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    socket.set_read_timeout(Some(Duration::from_millis(100))).expect("socket timeout");

    // Every failure (device unplugged or not there yet, rate changed, a broken
    // output unit) tears everything down and starts again. Only a signal ends it.
    loop {
        if let Err(e) = run(&opts, &socket) {
            println!("pcmlink: {e}; restarting in 2s");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn run(opts: &Opts, socket: &UdpSocket) -> Result<(), String> {
    let dev = output::find(&opts.device)?;
    let channels = 2;
    let margin_frames = opts.margin_ms / 1000.0 * dev.rate;
    let ring_capacity = (dev.rate / 2.0) as usize * channels; // half a second
    let (producer, consumer) = rtrb::RingBuffer::new(ring_capacity);
    let shared = Arc::new(Shared::default());
    // Before playing, wait for three times the margin, enough to ride out one
    // sender burst plus one device block; the controller then
    // settles the level to what this sender's bursts need.
    let prime = (3.0 * margin_frames) as usize * channels;
    let mut au = output::open(&dev, opts.io_frames, prime, consumer, shared.clone())?;

    let (ctl, events) = mpsc::channel();
    let config = net::Config {
        stream_rate: opts.rate,
        channels,
        payload_type: opts.payload,
        gain: opts.volume,
        device_rate: dev.rate,
        margin_frames,
        ring_capacity,
        gate: (opts.gate > 0).then(|| Duration::from_secs(opts.gate)),
        idle: (opts.idle > 0).then(|| Duration::from_secs(opts.idle)),
    };
    // Start the device before reading the network, so no audio waits on it.
    au.start().map_err(|e| format!("cannot start the output: {e:?}"))?;
    let socket = socket.try_clone().map_err(|e| e.to_string())?;
    let net_shared = shared.clone();
    let worker = thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
            net::run(config, socket, producer, net_shared, ctl)
        })
        .map_err(|e| e.to_string())?;

    println!(
        "pcmlink receive: running ({} at {} Hz, outputs {}+{}, margin {} ms{})",
        dev.name,
        dev.rate,
        dev.stereo[0],
        dev.stereo[1],
        opts.margin_ms,
        if opts.gate > 0 { format!(", gate {} s", opts.gate) } else { String::new() }
    );
    let result = supervise(opts, &dev, &mut au, &shared, &events);
    shared.stop.store(true, Relaxed);
    let _ = au.stop();
    drop(au);
    let worker_result = worker.join().unwrap_or_else(|_| Err("network thread panicked".into()));
    result.and(worker_result)
}

fn supervise(
    opts: &Opts,
    dev: &output::Device,
    au: &mut coreaudio::audio_unit::AudioUnit,
    shared: &Shared,
    events: &mpsc::Receiver<Ctl>,
) -> Result<(), String> {
    let mut on = true;
    let mut next = Instant::now() + Duration::from_secs(1);
    let mut last = (Instant::now(), 0u64);
    let mut first: Option<(Instant, u64)> = None;
    loop {
        match events.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Ctl::Wake) if !on => {
                // Fast while another client keeps the device running; a device
                // that had stopped completely can take a few hundred ms. Audio
                // arriving meanwhile is kept and played, not skipped.
                let t = Instant::now();
                au.start().map_err(|e| format!("cannot restart the output: {e:?}"))?;
                on = true;
                println!("{}  output on (started in {:.0} ms)", clock(), t.elapsed().as_secs_f64() * 1e3);
            }
            Ok(Ctl::Idle) if on => {
                au.stop().map_err(|e| format!("cannot stop the output: {e:?}"))?;
                on = false;
                shared.playing.store(false, Relaxed);
                println!("{}  output idle", clock());
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()), // the network thread ended; run() reports why
            Err(RecvTimeoutError::Timeout) => {
                next += Duration::from_secs(1);
                if !output::is_alive(dev.id) {
                    return Err(format!("{} went away", dev.name));
                }
                if output::nominal_rate(dev.id) != Some(dev.rate) {
                    return Err(format!("{} changed its sample rate", dev.name));
                }
                if !opts.stats {
                    continue;
                }
                let now = Instant::now();
                let packets = shared.packets.load(Relaxed);
                let rate = (packets - last.1) as f64 / now.duration_since(last.0).as_secs_f64();
                last = (now, packets);
                if first.is_none() && packets > 0 {
                    first = Some((now, packets));
                }
                let avg = match first {
                    Some((t, p)) if now.duration_since(t).as_secs_f64() > 1.0 => {
                        (packets - p) as f64 / now.duration_since(t).as_secs_f64()
                    }
                    _ => 0.0,
                };
                println!(
                    "{}  buf={:4.1}/{:4.1}ms  pkt/s={:4.0}  avg={:7.2}  lost={} late={} dup={} gap={}ms under={} drift={:+}ppm out={}",
                    clock(),
                    shared.mean_frames.load(Relaxed) as f64 / dev.rate * 1000.0,
                    shared.low_frames.load(Relaxed) as f64 / dev.rate * 1000.0,
                    rate,
                    avg,
                    shared.lost.load(Relaxed),
                    shared.late.load(Relaxed),
                    shared.dup.load(Relaxed),
                    shared.gap_frames.load(Relaxed) * 1000 / opts.rate as u64,
                    shared.underruns.load(Relaxed),
                    shared.drift_ppm.load(Relaxed),
                    if on { "on" } else { "idle" },
                );
            }
        }
    }
}
