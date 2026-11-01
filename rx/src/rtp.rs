//! RTP L16 packets: header parsing, the silence test, sample decoding, and
//! following one sender's stream (losses, late packets, restarts).

/// Largest 16-bit sample still counted as silence: -66 dBFS, above dither.
pub const SILENCE_PEAK: u16 = 16;

pub struct Packet<'a> {
    pub payload_type: u8,
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    pub payload: &'a [u8],
}

/// Parse an RTP packet (RFC 3550), skipping CSRCs, header extension and padding.
pub fn parse(b: &[u8]) -> Option<Packet<'_>> {
    if b.len() < 12 || b[0] >> 6 != 2 {
        return None;
    }
    let mut start = 12 + 4 * (b[0] & 0x0F) as usize;
    if b[0] & 0x10 != 0 {
        let words = u16::from_be_bytes([*b.get(start + 2)?, *b.get(start + 3)?]) as usize;
        start += 4 + 4 * words;
    }
    let mut end = b.len();
    if b[0] & 0x20 != 0 {
        end = end.checked_sub(*b.last()? as usize)?;
    }
    if start > end {
        return None;
    }
    Some(Packet {
        payload_type: b[1] & 0x7F,
        seq: u16::from_be_bytes([b[2], b[3]]),
        ts: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        payload: &b[start..end],
    })
}

/// Largest sample magnitude in an L16 payload (16-bit big-endian samples).
#[cfg(test)]
pub fn peak(payload: &[u8]) -> u16 {
    let levels = levels(payload);
    levels.peak[0].max(levels.peak[1])
}

/// One packet's level per channel of a stereo stream: the same pass decides the
/// silence gate and feeds the meters.
#[derive(Default)]
pub struct Levels {
    pub peak: [u16; 2],
    /// Sum of the squared samples.
    pub energy: [u64; 2],
}

pub fn levels(payload: &[u8]) -> Levels {
    let mut out = Levels::default();
    for (i, s) in payload.as_chunks::<2>().0.iter().enumerate() {
        let v = i16::from_be_bytes([s[0], s[1]]).unsigned_abs();
        let c = i & 1;
        out.peak[c] = out.peak[c].max(v);
        out.energy[c] += (v as u64) * (v as u64);
    }
    out
}

/// Append the payload's samples as f32, moving the gain linearly from `from` to
/// `to` across the packet, so a fader or mute change does not click.
pub fn decode_into(payload: &[u8], from: f32, to: f32, channels: usize, out: &mut Vec<f32>) {
    let samples = payload.as_chunks::<2>().0;
    if from == to {
        let scale = to / 32768.0;
        out.extend(samples.iter().map(|s| i16::from_be_bytes([s[0], s[1]]) as f32 * scale));
        return;
    }
    let frames = (samples.len() / channels).max(1) as f32;
    out.extend(samples.iter().enumerate().map(|(i, s)| {
        let gain = from + (to - from) * ((i / channels) as f32 + 1.0) / frames;
        i16::from_be_bytes([s[0], s[1]]) as f32 / 32768.0 * gain
    }));
}

#[derive(Debug, PartialEq, Eq)]
pub enum Arrival {
    /// The next packet, exactly where it was expected.
    Next,
    /// Audio is missing before this packet: `packets` never arrived, and/or the
    /// sender's timestamps jumped (it dropped samples it could not capture).
    /// `frames` of silence keep the timeline.
    Gap { packets: u16, frames: u32 },
    /// Older than what was already played: too late to use.
    Late,
    /// The packet just played, again.
    Duplicate,
    /// A new stream: first packet, a restarted sender (new SSRC), or a jump too
    /// large to be loss. Nothing is counted as lost.
    Reset,
}

/// Follows one sender's packets.
#[derive(Default)]
pub struct Tracker {
    ssrc: Option<u32>,
    next_seq: u16,
    next_ts: u32,
    last_seq: u16,
}

impl Tracker {
    /// Classify a packet holding `frames` frames; `max_gap` is the largest gap,
    /// in frames, still treated as loss rather than a new stream.
    pub fn arrive(&mut self, p: &Packet, frames: u32, max_gap: u32) -> Arrival {
        let behind = p.seq.wrapping_sub(self.next_seq) as i16;
        let arrival = match self.ssrc {
            Some(ssrc) if ssrc == p.ssrc => {
                if behind < 0 {
                    return if p.seq == self.last_seq { Arrival::Duplicate } else { Arrival::Late };
                }
                let gap = p.ts.wrapping_sub(self.next_ts);
                if behind as u32 > 1000 || gap > max_gap {
                    Arrival::Reset
                } else if behind == 0 && gap == 0 {
                    Arrival::Next
                } else {
                    Arrival::Gap { packets: behind as u16, frames: gap }
                }
            }
            _ => Arrival::Reset,
        };
        self.ssrc = Some(p.ssrc);
        self.last_seq = p.seq;
        self.next_seq = p.seq.wrapping_add(1);
        self.next_ts = p.ts.wrapping_add(frames);
        arrival
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(seq: u16, ts: u32, ssrc: u32, samples: &[i16]) -> Vec<u8> {
        let mut b = vec![0x80, 96];
        b.extend(seq.to_be_bytes());
        b.extend(ts.to_be_bytes());
        b.extend(ssrc.to_be_bytes());
        for s in samples {
            b.extend(s.to_be_bytes());
        }
        b
    }

    #[test]
    fn parses_header_and_payload() {
        let b = rtp(7, 960, 42, &[1, -300]);
        let p = parse(&b).unwrap();
        assert_eq!((p.payload_type, p.seq, p.ts, p.ssrc), (96, 7, 960, 42));
        assert_eq!(peak(p.payload), 300);
    }

    #[test]
    fn skips_csrcs_extension_and_padding() {
        // Every non-audio byte is 0xff, which would read as a loud sample.
        let mut b = vec![0x80 | 0x20 | 0x10 | 1, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        b.extend([0xff; 4]); // one CSRC
        b.extend([0xbe, 0xde, 0, 1, 0xff, 0xff, 0xff, 0xff]); // extension, one word
        b.extend(5i16.to_be_bytes());
        b.extend([0xff, 0xff, 3]); // three bytes of padding
        assert_eq!(peak(parse(&b).unwrap().payload), 5);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[0x80; 5]).is_none());
        assert!(parse(&[0x40; 20]).is_none()); // RTP version 1
    }

    #[test]
    fn silence_threshold() {
        assert!(peak(&rtp(0, 0, 0, &[1, -1, 0, 16])[12..]) <= SILENCE_PEAK);
        assert!(peak(&rtp(0, 0, 0, &[0, 104])[12..]) > SILENCE_PEAK); // -50 dBFS
        assert_eq!(peak(&rtp(0, 0, 0, &[i16::MIN])[12..]), 32768);
    }

    #[test]
    fn decodes_big_endian_with_gain() {
        let mut out = Vec::new();
        decode_into(&rtp(0, 0, 0, &[16384, -32768])[12..], 0.5, 0.5, 2, &mut out);
        assert_eq!(out, vec![0.25, -0.5]);
    }

    #[test]
    fn a_gain_change_ramps_across_the_packet_and_ends_on_target() {
        let mut out = Vec::new();
        // Four stereo frames at -6 dBFS, fading from unity to silence.
        decode_into(&rtp(0, 0, 0, &[16384; 8])[12..], 1.0, 0.0, 2, &mut out);
        assert_eq!(out, vec![0.375, 0.375, 0.25, 0.25, 0.125, 0.125, 0.0, 0.0]);
    }

    #[test]
    fn levels_are_per_channel() {
        let l = levels(&rtp(0, 0, 0, &[100, -200, -50, 20])[12..]);
        assert_eq!(l.peak, [100, 200]);
        assert_eq!(l.energy, [100 * 100 + 50 * 50, 200 * 200 + 20 * 20]);
    }

    #[test]
    fn follows_a_stream() {
        let mut t = Tracker::default();
        let p = |seq, ts, ssrc| rtp(seq, ts, ssrc, &[0; 4]);
        let arrive = |t: &mut Tracker, b: &Vec<u8>| t.arrive(&parse(b).unwrap(), 240, 48_000);
        assert_eq!(arrive(&mut t, &p(10, 1000, 1)), Arrival::Reset);
        assert_eq!(arrive(&mut t, &p(11, 1240, 1)), Arrival::Next);
        assert_eq!(arrive(&mut t, &p(11, 1240, 1)), Arrival::Duplicate);
        assert_eq!(arrive(&mut t, &p(14, 1960, 1)), Arrival::Gap { packets: 2, frames: 480 });
        assert_eq!(arrive(&mut t, &p(12, 1480, 1)), Arrival::Late);
        // Sender dropped samples: contiguous sequence, timestamps jump.
        assert_eq!(arrive(&mut t, &p(15, 2300, 1)), Arrival::Gap { packets: 0, frames: 100 });
        // A restarted sender is a new stream, not a loss of thousands of packets.
        assert_eq!(arrive(&mut t, &p(40_000, 7, 2)), Arrival::Reset);
        assert_eq!(arrive(&mut t, &p(40_001, 247, 2)), Arrival::Next);
        // Sequence numbers wrap.
        let mut t = Tracker::default();
        arrive(&mut t, &p(u16::MAX, 0, 3));
        assert_eq!(arrive(&mut t, &p(0, 240, 3)), Arrival::Next);
    }
}
