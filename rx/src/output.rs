//! The output device: finding it, and a CoreAudio output unit bound to it whose
//! render callback only copies samples out of the ring buffer.

use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::ptr::{null, NonNull};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::{get_audio_device_ids, get_device_name};
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use objc2_audio_toolbox::{
    kAudioOutputUnitProperty_ChannelMap, kAudioOutputUnitProperty_CurrentDevice,
    AudioUnitSetProperty,
};
use objc2_core_audio::{
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyPreferredChannelsForStereo,
    kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, AudioDeviceID,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
};
use objc2_core_audio_types::AudioBufferList;

use crate::Shared;

pub struct Device {
    pub id: AudioDeviceID,
    pub name: String,
    pub rate: f64,
    pub channels: u32,
    /// The pair macOS plays stereo on (Audio MIDI Setup > Configure Speakers), 1-based.
    pub stereo: [u32; 2],
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a fixed-size device property.
fn get<T: Copy>(id: AudioDeviceID, selector: u32, scope: u32) -> Option<T> {
    let addr = address(selector, scope);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(value.as_mut_ptr().cast::<c_void>()),
        )
    };
    (status == 0 && size as usize == size_of::<T>()).then(|| unsafe { value.assume_init() })
}

/// Total output channels across the device's output streams.
fn output_channels(id: AudioDeviceID) -> u32 {
    let addr = address(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeOutput);
    let mut size = 0u32;
    if unsafe { AudioObjectGetPropertyDataSize(id, NonNull::from(&addr), 0, null(), NonNull::from(&mut size)) } != 0 {
        return 0;
    }
    // u64 storage keeps the AudioBufferList's pointers aligned.
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(storage.as_mut_ptr().cast::<c_void>()),
        )
    };
    if status != 0 {
        return 0;
    }
    let list = storage.as_ptr().cast::<AudioBufferList>();
    unsafe {
        let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), (*list).mNumberBuffers as usize);
        buffers.iter().map(|b| b.mNumberChannels).sum()
    }
}

pub fn nominal_rate(id: AudioDeviceID) -> Option<f64> {
    get::<f64>(id, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)
}

pub fn is_alive(id: AudioDeviceID) -> bool {
    get::<u32>(id, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal) == Some(1)
}

/// The one output device whose name contains `pattern` (case-insensitive).
pub fn find(pattern: &str) -> Result<Device, String> {
    let ids = get_audio_device_ids().map_err(|e| format!("cannot list audio devices: {e:?}"))?;
    let wanted = pattern.to_lowercase();
    let mut names = Vec::new();
    let mut hits = Vec::new();
    for id in ids {
        let channels = output_channels(id);
        if channels == 0 {
            continue;
        }
        let name = get_device_name(id).unwrap_or_default();
        if name.to_lowercase().contains(&wanted) {
            hits.push((id, name.clone(), channels));
        }
        names.push(name);
    }
    match hits.as_slice() {
        [(id, name, channels)] => {
            let rate = nominal_rate(*id).ok_or("cannot read the device's sample rate")?;
            let stereo = get::<[u32; 2]>(*id, kAudioDevicePropertyPreferredChannelsForStereo, kAudioObjectPropertyScopeOutput)
                .filter(|p| p.iter().all(|&c| (1..=*channels).contains(&c)))
                .unwrap_or([1, 2]);
            Ok(Device { id: *id, name: name.clone(), rate, channels: *channels, stereo })
        }
        [] => Err(format!("no output device matching \"{pattern}\". Available: {}", names.join(", "))),
        _ => Err(format!("\"{pattern}\" is ambiguous: {}", hits.iter().map(|h| h.1.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

/// An output unit bound to `dev` (not to whatever the default output is), playing
/// stereo on the device's stereo pair, fed from `ring`. It starts stopped.
///
/// The callback runs on CoreAudio's real-time thread, so it only copies: no
/// locks, no allocation, no logging. It waits until `prime` samples are
/// buffered before playing, and again after running dry (an underrun).
pub fn open(
    dev: &Device,
    io_frames: Option<u32>,
    prime: usize,
    mut ring: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
) -> Result<AudioUnit, String> {
    fn fail(what: &'static str) -> impl Fn(coreaudio::Error) -> String {
        move |e| format!("{what}: {e:?}")
    }
    let mut au = AudioUnit::new_uninitialized(IOType::HalOutput).map_err(fail("cannot create output unit"))?;
    au.set_property(kAudioOutputUnitProperty_CurrentDevice, Scope::Global, Element::Output, Some(&dev.id))
        .map_err(fail("cannot select the device"))?;
    let format = StreamFormat {
        sample_rate: dev.rate,
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels: 2,
    };
    au.set_stream_format(format, Scope::Input, Element::Output).map_err(fail("cannot set the stream format"))?;

    // One entry per device output: which of our two channels it plays, or -1.
    let map: Vec<i32> = (1..=dev.channels)
        .map(|c| if c == dev.stereo[0] { 0 } else if c == dev.stereo[1] { 1 } else { -1 })
        .collect();
    let status = unsafe {
        AudioUnitSetProperty(
            *au.as_ref(),
            kAudioOutputUnitProperty_ChannelMap,
            Scope::Output as u32,
            Element::Output as u32,
            map.as_ptr().cast(),
            (map.len() * size_of::<i32>()) as u32,
        )
    };
    if status != 0 {
        return Err(format!("cannot set the channel map (OSStatus {status})"));
    }
    // This client's I/O period; the device runs at the smallest any client asks
    // for, so asking small costs every client. Left to CoreAudio unless given.
    if let Some(frames) = io_frames {
        au.set_property(kAudioDevicePropertyBufferFrameSize, Scope::Global, Element::Output, Some(&frames))
            .map_err(fail("cannot set the I/O buffer size"))?;
    }

    let mut priming = true;
    type Args = render_callback::Args<data::Interleaved<f32>>;
    au.set_render_callback(move |args: Args| {
        let out = args.data.buffer;
        let available = ring.slots();
        if priming && available < prime {
            out.fill(0.0);
            return Ok(());
        }
        priming = false;
        let n = out.len().min(available);
        if let Ok(chunk) = ring.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
        }
        if n < out.len() {
            out[n..].fill(0.0);
            shared.dry.store(true, Relaxed);
            priming = true;
        }
        shared.playing.store(!priming, Relaxed);
        Ok(())
    })
    .map_err(fail("cannot install the render callback"))?;
    au.initialize().map_err(fail("cannot initialize the output unit"))?;
    Ok(au)
}
