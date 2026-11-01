//! The network thread: receive packets, follow the stream, decode, convert to
//! the device rate, and keep the playout buffer at its target.
//!
//! One adaptive resampler does both jobs: the nominal conversion (48 kHz on the
//! wire to the device's rate) and the clock drift between the sender and the
//! device. The drift controller nudges its ratio so that the lowest point the
//! buffer reaches each second sits at a small safety margin. Senders deliver in
//! bursts (Windows: 10 ms of audio at a time) and the device reads in blocks,
//! so the level is a sawtooth; holding its low point, not its average, gives
//! the lowest latency that never runs dry, whatever the sender's burst size.
//! Audio is never skipped or padded to do this.

use std::net::UdpSocket;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::rtp::{self, Arrival, Tracker, SILENCE_PEAK};
use crate::control::Live;
use crate::{Ctl, Shared};

/// Frames per resampler call: 2.5 ms at 48 kHz.
const CHUNK: usize = 120;
/// Drift controller gain, per second: a 5 ms error moves the ratio by 1000 ppm.
const GAIN: f64 = 0.2;
/// Largest ratio correction: far beyond any real clock error (0.3 %, 5 cents).
const MAX_ADJUST: f64 = 0.003;
/// Larger correction while the buffer holds much more than it needs, as after a
/// device that took a few hundred ms to start: 0.5 % (8.6 cents) sheds that
/// excess in tens of seconds instead of minutes, without skipping any audio.
const CATCH_UP: f64 = 0.005;
/// Excess over the margin, in seconds, beyond which CATCH_UP applies.
const CATCH_UP_ABOVE: f64 = 0.040;
/// The controller looks at the buffer's low point over windows this long.
const WINDOW: Duration = Duration::from_secs(1);

pub struct Config {
    pub stream_rate: u32,
    pub channels: usize,
    pub payload_type: u8,
    pub device_rate: f64,
    /// Frames (device rate) the buffer's low point should keep in reserve, and
    /// the ring's capacity in samples.
    pub margin_frames: f64,
    pub ring_capacity: usize,
    /// Stop the output after this much silence; None = never.
    pub gate: Option<Duration>,
    /// Stop the output after this long without any packet (the sender is off);
    /// None = never.
    pub idle: Option<Duration>,
}

pub fn run(
    cfg: Config,
    socket: UdpSocket,
    mut ring: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    live: Arc<Live>,
    ctl: Sender<Ctl>,
) -> Result<(), String> {
    let ch = cfg.channels;
    let mut gain = live.target_gain();
    let mut peer = None;
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: None,
        oversampling_factor: 256,
        interpolation: SincInterpolationType::Linear,
        window: WindowFunction::BlackmanHarris2,
    };
    let ratio = cfg.device_rate / cfg.stream_rate as f64;
    let mut resampler = Async::<f32>::new_sinc(ratio, 1.01, &params, CHUNK, ch, FixedAsync::Input)
        .map_err(|e| format!("cannot create the resampler: {e}"))?;
    let out_max = resampler.output_frames_max();
    let mut out = vec![0.0f32; out_max * ch];
    let mut staging: Vec<f32> = Vec::with_capacity(16 * 1024);
    let mut packet = [0u8; 2048];

    // Packets queued while the output was being set up are stale: nothing has
    // been played yet, so start from the newest instead of buffering them.
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    while socket.recv(&mut packet).is_ok() {}
    socket.set_nonblocking(false).map_err(|e| e.to_string())?;

    let mut tracker = Tracker::default();
    let mut window = (Instant::now(), usize::MAX, 0usize, 0usize); // start, low, sum, count
    let mut low = cfg.margin_frames; // smoothed low point
    let mut was_playing = false;
    let mut last_sound = Instant::now();
    let mut last_packet = Instant::now();
    let mut idle = false;
    // Whether the output should stop now: after `gate` of silence, or after
    // `idle` without packets.
    let quiet = |last_sound: Instant, last_packet: Instant| {
        cfg.gate.is_some_and(|g| last_sound.elapsed() >= g)
            || cfg.idle.is_some_and(|i| last_packet.elapsed() >= i)
    };

    while !shared.stop.load(Relaxed) {
        let (len, from) = match socket.recv_from(&mut packet) {
            Ok(received) => received,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                // No packets at all is silence too.
                if !idle && quiet(last_sound, last_packet) {
                    idle = true;
                    let _ = ctl.send(Ctl::Idle);
                }
                continue;
            }
            Err(e) => return Err(format!("receive failed: {e}")),
        };
        let Some(p) = rtp::parse(&packet[..len]) else {
            shared.bad.fetch_add(1, Relaxed);
            continue;
        };
        if p.payload_type != cfg.payload_type || p.payload.len() % (2 * ch) != 0 {
            shared.bad.fetch_add(1, Relaxed);
            continue;
        }
        let frames = (p.payload.len() / (2 * ch)) as u32;
        shared.packets.fetch_add(1, Relaxed);
        if peer != Some(from) && live.set_peer(from) {
            peer = Some(from);
        }
        let now = crate::now_ms();
        let previous = shared.last_packet_ms.swap(now, Relaxed);
        // The output ran dry since the last packet. If the stream kept coming
        // (no pause), that was a dropout; if it had paused, the stream ended.
        if shared.dry.swap(false, Relaxed) && now - previous < 100 {
            shared.underruns.fetch_add(1, Relaxed);
        }

        let gap = match tracker.arrive(&p, frames, cfg.stream_rate) {
            Arrival::Next => 0,
            Arrival::Gap { packets, frames } => {
                shared.lost.fetch_add(packets as u64, Relaxed);
                shared.gap_frames.fetch_add(frames as u64, Relaxed);
                frames as usize
            }
            Arrival::Late => {
                shared.late.fetch_add(1, Relaxed);
                continue;
            }
            Arrival::Duplicate => {
                shared.dup.fetch_add(1, Relaxed);
                continue;
            }
            Arrival::Reset => {
                shared.resets.fetch_add(1, Relaxed);
                0
            }
        };

        // Restart the output on the first packet with sound (any packet, if
        // there is no silence gate). It is buffered below before the device
        // starts reading, so it is played, not skipped.
        last_packet = Instant::now();
        let levels = rtp::levels(p.payload);
        live.meter(&levels, frames);
        let loud = levels.peak[0].max(levels.peak[1]) > SILENCE_PEAK;
        if loud {
            last_sound = last_packet;
        }
        if idle && (loud || cfg.gate.is_none()) {
            idle = false;
            let _ = ctl.send(Ctl::Wake);
        } else if !idle && quiet(last_sound, last_packet) {
            idle = true;
            let _ = ctl.send(Ctl::Idle);
        }
        if idle {
            continue;
        }
        // Silence arriving while the buffer holds far more than it needs (after
        // a slow device start, say) is dropped instead of buffered: inaudible,
        // and the extra latency goes at once instead of over many seconds.
        if !loud && ((cfg.ring_capacity - ring.slots()) / ch) as f64 > 4.0 * cfg.margin_frames {
            continue;
        }

        staging.resize(staging.len() + gap * ch, 0.0); // silence where audio was lost
        let target = live.target_gain();
        rtp::decode_into(p.payload, gain, target, ch, &mut staging);
        gain = target;

        let mut used = 0;
        while staging.len() - used >= CHUNK * ch {
            let input = InterleavedSlice::new(&staging[used..used + CHUNK * ch], ch, CHUNK)
                .map_err(|e| format!("{e:?}"))?;
            let mut output = InterleavedSlice::new_mut(&mut out, ch, out_max).map_err(|e| format!("{e:?}"))?;
            let (_, produced) = resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| format!("resampling failed: {e}"))?;
            used += CHUNK * ch;

            // The level just before adding audio is the sawtooth's low point.
            let before = (cfg.ring_capacity - ring.slots()) / ch;
            let samples = produced * ch;
            match ring.write_chunk_uninit(samples) {
                Ok(chunk) => {
                    chunk.fill_from_iter(out[..samples].iter().copied());
                }
                Err(_) => {
                    shared.overflows.fetch_add(1, Relaxed);
                }
            }

            // Drift control, once per window, only while the output plays.
            // Each time playback (re)starts, the controller starts fresh: what it
            // learned before (a catch-up, say) says nothing about the new start.
            let playing = shared.playing.load(Relaxed);
            if playing != was_playing {
                was_playing = playing;
                low = cfg.margin_frames;
                if let Some(adjustable) = resampler.as_adjustable() {
                    let _ = adjustable.set_resample_ratio_relative(1.0, false);
                }
                shared.drift_ppm.store(0, Relaxed);
                window = (Instant::now(), usize::MAX, 0, 0);
            }
            if !playing {
                continue;
            }
            window.1 = window.1.min(before);
            window.2 += before;
            window.3 += 1;
            if window.0.elapsed() >= WINDOW {
                low += (window.1 as f64 - low) * 0.5;
                let error = (low - cfg.margin_frames) / cfg.device_rate;
                let limit = if error > CATCH_UP_ABOVE { CATCH_UP } else { MAX_ADJUST };
                let relative = (1.0 - GAIN * error).clamp(1.0 - limit, 1.0 + limit);
                if let Some(adjustable) = resampler.as_adjustable() {
                    let _ = adjustable.set_resample_ratio_relative(relative, true);
                }
                shared.drift_ppm.store(((relative - 1.0) * 1e6).round() as i32, Relaxed);
                shared.low_frames.store(window.1 as u32, Relaxed);
                shared.mean_frames.store((window.2 / window.3) as u32, Relaxed);
                window = (Instant::now(), usize::MAX, 0, 0);
            }
        }
        staging.drain(..used);
    }
    Ok(())
}
