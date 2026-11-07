//! Audio monitor: capture-card input played straight to the default output.
//!
//! Latency rule: the ring buffer between the two streams is kept just deep
//! enough to absorb one callback from each side. If clock drift lets it grow
//! past that, the oldest samples are dropped instead of building delay.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, DeviceId, FromSample, Sample, SampleFormat, SizedSample, Stream,
    StreamConfig, SupportedBufferSize, SupportedStreamConfig,
};
use ringbuf::{HeapProd, HeapRb};
use ringbuf::traits::{Consumer, Observer, Producer, Split};

/// Requested callback size in frames (~5 ms at 48 kHz). Falls back to the
/// device default when unsupported.
const BUFFER_FRAMES: u32 = 256;

#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub id: DeviceId,
    pub name: String,
}

pub fn list_inputs() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| {
            Some(AudioDevice {
                id: d.id().ok()?,
                name: d.description().ok()?.name().to_string(),
            })
        })
        .collect()
}

/// Volume as f32 bits, shared with the output callback.
#[derive(Clone)]
pub struct Volume(Arc<AtomicU32>);

impl Volume {
    pub fn new(v: f32) -> Self {
        Self(Arc::new(AtomicU32::new(v.to_bits())))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// Channels in recorded audio. Input is remapped to stereo for the recorder.
pub const RECORD_CHANNELS: usize = 2;

/// A timestamped copy of captured audio for the recorder: interleaved stereo f32.
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// When the first sample was captured, on the same clock as video frames.
    pub at: Instant,
}

/// Where the input callback sends recording copies. Attach or detach any time;
/// the callback never blocks on it.
#[derive(Clone, Default)]
pub struct AudioTap(Arc<Mutex<Option<SyncSender<AudioChunk>>>>);

impl AudioTap {
    pub fn set(&self, tx: Option<SyncSender<AudioChunk>>) {
        *self.0.lock().unwrap() = tx;
    }

    fn is_attached(&self) -> bool {
        self.0.try_lock().is_ok_and(|tx| tx.is_some())
    }

    fn send(&self, chunk: AudioChunk) {
        if let Ok(tx) = self.0.try_lock()
            && let Some(tx) = tx.as_ref()
        {
            let _ = tx.try_send(chunk);
        }
    }
}

/// DVR playback audio. While active, the output plays this instead of the live input.
pub struct Playback {
    active: AtomicBool,
    flush: AtomicBool,
    producer: Mutex<HeapProd<f32>>,
    pub channels: usize,
}

impl Playback {
    pub fn set_active(&self, active: bool) {
        self.flush.store(true, Ordering::Release);
        self.active.store(active, Ordering::Release);
    }

    /// Drops whatever is queued (on seek or pause).
    pub fn flush(&self) {
        self.flush.store(true, Ordering::Release);
    }

    /// Queues interleaved samples (in `channels` channels). Returns how many fit.
    pub fn push(&self, samples: &[f32]) -> usize {
        self.producer.lock().unwrap().push_slice(samples)
    }
}

pub struct Monitor {
    _input: Stream,
    _output: Stream,
    pub sample_rate: u32,
    pub playback: Arc<Playback>,
}

impl Monitor {
    pub fn start(input: &AudioDevice, volume: Volume, tap: AudioTap) -> Result<Monitor> {
        let host = cpal::default_host();
        let in_dev = host
            .device_by_id(&input.id)
            .ok_or_else(|| anyhow!("audio input {} disappeared", input.name))?;
        let out_dev = host
            .default_output_device()
            .context("no audio output device")?;

        let in_cfg = in_dev.default_input_config().context("input config")?;
        let rate = in_cfg.sample_rate();
        let out_cfg = output_config_at(&out_dev, rate)?;
        let in_ch = in_cfg.channels() as usize;
        let out_ch = out_cfg.channels() as usize;

        // 100 ms of headroom; the output callback trims it back down.
        let ring = HeapRb::<f32>::new(rate as usize * out_ch / 10);
        let (producer, consumer) = ring.split();
        let input_chunk = Arc::new(AtomicUsize::new(0));

        // One second of playback audio; the DVR player keeps it topped up.
        let (playback_prod, playback_cons) = HeapRb::<f32>::new(rate as usize * out_ch).split();
        let playback = Arc::new(Playback {
            active: AtomicBool::new(false),
            flush: AtomicBool::new(false),
            producer: Mutex::new(playback_prod),
            channels: out_ch,
        });

        let _input =
            build_input(&in_dev, &in_cfg, in_ch, out_ch, producer, input_chunk.clone(), tap)?;
        let _output = build_output(
            &out_dev,
            &out_cfg,
            consumer,
            input_chunk,
            volume,
            playback_cons,
            playback.clone(),
        )?;
        _input.play().context("starting input")?;
        _output.play().context("starting output")?;
        Ok(Monitor { _input, _output, sample_rate: rate, playback })
    }
}

fn output_config_at(device: &Device, rate: u32) -> Result<SupportedStreamConfig> {
    let default = device.default_output_config().context("output config")?;
    if default.sample_rate() == rate {
        return Ok(default);
    }
    device
        .supported_output_configs()
        .context("output configs")?
        .filter(|c| c.channels() == default.channels())
        .find_map(|c| c.try_with_sample_rate(rate))
        .ok_or_else(|| anyhow!("output device can't run at the input's {rate} Hz"))
}

fn stream_config(supported: &SupportedStreamConfig) -> StreamConfig {
    let mut config = supported.config();
    if let SupportedBufferSize::Range { min, max } = supported.buffer_size()
        && (*min..=*max).contains(&BUFFER_FRAMES)
    {
        config.buffer_size = BufferSize::Fixed(BUFFER_FRAMES);
    }
    config
}

fn err_fn(err: cpal::Error) {
    eprintln!("audio stream error: {err}");
}

macro_rules! dispatch {
    ($format:expr, $func:ident, ($($arg:expr),*)) => {
        match $format {
            SampleFormat::I16 => $func::<i16>($($arg),*),
            SampleFormat::I32 => $func::<i32>($($arg),*),
            SampleFormat::U16 => $func::<u16>($($arg),*),
            SampleFormat::U8 => $func::<u8>($($arg),*),
            SampleFormat::F32 => $func::<f32>($($arg),*),
            SampleFormat::F64 => $func::<f64>($($arg),*),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
    };
}

fn build_input(
    device: &Device,
    supported: &SupportedStreamConfig,
    in_ch: usize,
    out_ch: usize,
    producer: impl Producer<Item = f32> + Send + 'static,
    input_chunk: Arc<AtomicUsize>,
    tap: AudioTap,
) -> Result<Stream> {
    fn build<T: SizedSample>(
        device: &Device,
        config: StreamConfig,
        in_ch: usize,
        out_ch: usize,
        mut producer: impl Producer<Item = f32> + Send + 'static,
        input_chunk: Arc<AtomicUsize>,
        tap: AudioTap,
    ) -> Result<Stream>
    where
        f32: FromSample<T>,
    {
        let stream = device.build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_ch;
                input_chunk.store(frames * out_ch, Ordering::Relaxed);
                // Remap channels: mono is duplicated, extra input channels are dropped.
                let remap = |channels: usize| {
                    data.chunks_exact(in_ch).flat_map(move |frame| {
                        (0..channels).map(move |c| f32::from_sample(frame[c.min(in_ch - 1)]))
                    })
                };
                producer.push_iter(remap(out_ch));

                if tap.is_attached() {
                    let ts = info.timestamp();
                    let age = ts.callback.duration_since(ts.capture);
                    let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
                    tap.send(AudioChunk { samples: remap(RECORD_CHANNELS).collect(), at });
                }
            },
            err_fn,
            None,
        )?;
        Ok(stream)
    }
    let config = stream_config(supported);
    dispatch!(supported.sample_format(), build, (device, config, in_ch, out_ch, producer, input_chunk, tap))
        .context("building input stream")
}

fn build_output(
    device: &Device,
    supported: &SupportedStreamConfig,
    consumer: impl Consumer<Item = f32> + Observer + Send + 'static,
    input_chunk: Arc<AtomicUsize>,
    volume: Volume,
    playback_cons: impl Consumer<Item = f32> + Observer + Send + 'static,
    playback: Arc<Playback>,
) -> Result<Stream> {
    fn build<T: SizedSample + FromSample<f32>>(
        device: &Device,
        config: StreamConfig,
        mut consumer: impl Consumer<Item = f32> + Observer + Send + 'static,
        input_chunk: Arc<AtomicUsize>,
        volume: Volume,
        mut playback_cons: impl Consumer<Item = f32> + Observer + Send + 'static,
        playback: Arc<Playback>,
    ) -> Result<Stream> {
        let mut scratch = Vec::new();
        let stream = device.build_output_stream(
            config,
            move |data: &mut [T], _: &_| {
                let keep = data.len() + input_chunk.load(Ordering::Relaxed) * 2;
                let backlog = consumer.occupied_len();
                if backlog > keep + data.len() {
                    consumer.skip(backlog - keep);
                }
                if playback.flush.swap(false, Ordering::AcqRel) {
                    playback_cons.clear();
                }
                scratch.resize(data.len(), 0.0);
                let read = if playback.active.load(Ordering::Acquire) {
                    // Keep the live ring drained so returning to live isn't delayed.
                    consumer.skip(consumer.occupied_len());
                    playback_cons.pop_slice(&mut scratch)
                } else {
                    consumer.pop_slice(&mut scratch)
                };
                scratch[read..].fill(0.0);
                let gain = volume.get();
                for (out, s) in data.iter_mut().zip(&scratch) {
                    *out = T::from_sample(s * gain);
                }
            },
            err_fn,
            None,
        )?;
        Ok(stream)
    }
    dispatch!(
        supported.sample_format(),
        build,
        (device, stream_config(supported), consumer, input_chunk, volume, playback_cons, playback)
    )
        .context("building output stream")
}
