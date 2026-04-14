use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use audiopus::coder::Decoder;
use audiopus::{Channels, MutSignals, SampleRate};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

/// Opus frame size: 20ms at 48kHz = 960 samples.
const OPUS_FRAME_SIZE: usize = 960;

/// Ring buffer capacity in samples. Sized to absorb DRM's bursty audio
/// output without dropping samples: DREAM emits audio in ~400–500 ms
/// frame-aligned chunks (one per OFDM transmission frame) rather than
/// at strict 20 ms cadence, and the earlier 200 ms buffer was smaller
/// than a single burst, so the drain-oldest logic below threw away
/// half of every burst and played silence between them.
///
/// 1.5 s at 48 kHz = 72 000 f32 samples ≈ 280 KB. Cheap, and DRM's
/// inherent ~2 s interleaver delay dwarfs the added latency anyway.
/// For SSB/AM where low latency matters, the drain-oldest logic will
/// trim down to near-empty in normal operation since arrivals match
/// output rate.
const RING_CAPACITY: usize = 48000 * 3 / 2;

/// Audio player: decodes Opus chunks and plays through the default output device.
pub struct AudioPlayer {
    ring: Arc<Mutex<VecDeque<f32>>>,
    _stream: Stream,
    decoder: Mutex<(Decoder, Vec<f32>)>, // decoder + reusable PCM buffer
    chunks_received: AtomicU64,
    volume: Arc<AtomicU32>,  // volume as fixed-point: 0..1000 → 0.0..1.0
    muted: Arc<AtomicBool>,
}

impl AudioPlayer {
    /// Create and start the audio output stream.
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no audio output device")?;

        // Find a f32 mono/stereo config at 48kHz
        let config = find_config(&device)?;
        let channels = config.channels as usize;

        let ring: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY)));

        let volume: Arc<AtomicU32> = Arc::new(AtomicU32::new(700)); // default 70%
        let muted: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

        let ring2 = ring.clone();
        let vol2 = volume.clone();
        let mute2 = muted.clone();
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    let is_muted = mute2.load(Ordering::Relaxed);
                    let gain = vol2.load(Ordering::Relaxed) as f32 / 1000.0;
                    let mut rb = ring2.lock().unwrap_or_else(|e| e.into_inner());
                    for frame in data.chunks_mut(channels) {
                        let sample = if is_muted {
                            let _ = rb.pop_front(); // drain buffer even when muted
                            0.0
                        } else {
                            rb.pop_front().unwrap_or(0.0) * gain
                        };
                        for ch in frame.iter_mut() {
                            *ch = sample;
                        }
                    }
                },
                move |err| {
                    eprintln!("audio output error: {err}");
                },
                None,
            )
            .map_err(|e| format!("build_output_stream: {e}"))?;

        stream.play().map_err(|e| format!("stream play: {e}"))?;

        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| format!("opus decoder: {e}"))?;

        eprintln!(
            "audio: device={}, config={:?}",
            device.name().unwrap_or_default(),
            config,
        );

        let pcm_buf = vec![0.0f32; OPUS_FRAME_SIZE];
        Ok(Self {
            ring,
            _stream: stream,
            decoder: Mutex::new((decoder, pcm_buf)),
            chunks_received: AtomicU64::new(0),
            volume,
            muted,
        })
    }

    /// Set volume (0.0 to 1.0).
    pub fn set_volume(&self, vol: f32) {
        self.volume.store((vol.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    /// Toggle mute. Returns new mute state.
    pub fn toggle_mute(&self) -> bool {
        let was = self.muted.load(Ordering::Relaxed);
        self.muted.store(!was, Ordering::Relaxed);
        !was
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Decode an Opus chunk and push PCM samples into the ring buffer.
    pub fn push_audio(&self, opus_data: &[u8]) {
        let count = self.chunks_received.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 {
            eprintln!("audio: first chunk received ({} bytes)", opus_data.len());
        } else if count % 500 == 0 {
            let rb = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!("audio: {} chunks received, ring buffer: {} samples", count, rb.len());
        }

        let mut guard = self.decoder.lock().unwrap_or_else(|e| e.into_inner());
        let (ref mut dec, ref mut pcm) = *guard;

        pcm.iter_mut().for_each(|s| *s = 0.0);
        let packet: audiopus::packet::Packet<'_> = match opus_data.try_into() {
            Ok(p) => p,
            Err(_) => return,
        };
        let signals: MutSignals<'_, f32> = match (&mut pcm[..]).try_into() {
            Ok(s) => s,
            Err(_) => return,
        };
        let n = match dec.decode_float(Some(packet), signals, false) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("opus decode error: {e}");
                return;
            }
        };

        if count <= 5 || count % 500 == 0 {
            let peak = pcm[..n].iter().map(|s| s.abs()).fold(0.0f32, f32::max);
            eprintln!("audio: chunk #{count} decoded {n} samples, peak={peak:.6}");
        }

        let mut rb = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        // Drop oldest samples if buffer is getting too large (prevents latency buildup)
        let max = RING_CAPACITY;
        let incoming = n;
        if rb.len() + incoming > max {
            let drain = rb.len() + incoming - max;
            rb.drain(..drain);
        }
        rb.extend(&pcm[..n]);
    }
}

/// Find a suitable output config: prefer 48kHz f32.
fn find_config(device: &cpal::Device) -> Result<StreamConfig, String> {
    let supported = device
        .supported_output_configs()
        .map_err(|e| format!("supported configs: {e}"))?;

    // Try to find 48kHz f32
    for cfg in supported {
        if cfg.sample_format() == SampleFormat::F32 {
            let rate = cpal::SampleRate(48000);
            if cfg.min_sample_rate() <= rate && rate <= cfg.max_sample_rate() {
                return Ok(cfg.with_sample_rate(rate).into());
            }
        }
    }

    // Fallback: default config — but only if it's 48kHz
    let default = device
        .default_output_config()
        .map_err(|e| format!("default config: {e}"))?;
    if default.sample_rate() == cpal::SampleRate(48000) {
        Ok(default.into())
    } else {
        Err(format!(
            "no 48kHz output config found (default is {}Hz)",
            default.sample_rate().0
        ))
    }
}
