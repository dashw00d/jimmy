//! Native audio and WAV I/O. CPAL callbacks only convert samples and touch SPSC rings.
use anyhow::{Context, Result, bail, ensure};
use cpal::{
    Error as StreamError, ErrorKind, FromSample, Sample, SampleFormat, SizedSample, Stream,
    StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use jimmy_core::Resampler;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    io::{Read, Seek, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

pub struct Capture {
    _stream: Stream,
    ring: Consumer<i16>,
    pub rate: u32,
    failure: Arc<OnceLock<StreamError>>,
}

pub struct Playback {
    _stream: Stream,
    ring: Producer<(u32, i16)>,
    turn: Arc<AtomicU32>,
    failure: Arc<OnceLock<StreamError>>,
    pub rate: u32,
}

impl Capture {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("No default input device")?;
        let supported = device
            .default_input_config()
            .context("Open the default input configuration")?;
        let config = supported.config();
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let failure = Arc::new(OnceLock::new());
        let (mut producer, ring) = RingBuffer::new(rate as usize / 2);
        macro_rules! capture {
            ($sample:ty) => {{
                let failure = failure.clone();
                device.build_input_stream(
                    config,
                    move |samples: &[$sample], _| capture_samples(samples, channels, &mut producer),
                    move |error| record_stream_error(&failure, error),
                    None,
                )?
            }};
        }
        let stream = match supported.sample_format() {
            SampleFormat::F32 => capture!(f32),
            SampleFormat::F64 => capture!(f64),
            SampleFormat::I16 => capture!(i16),
            SampleFormat::I32 => capture!(i32),
            SampleFormat::U16 => capture!(u16),
            SampleFormat::I8 => capture!(i8),
            SampleFormat::U8 => capture!(u8),
            format => bail!("Unsupported input sample format {format}"),
        };
        stream.play()?;
        Ok(Self {
            _stream: stream,
            ring,
            rate,
            failure,
        })
    }

    pub fn read(&mut self, out: &mut [i16]) -> Result<usize> {
        if let Some(error) = self.failure.get() {
            bail!("Input device stopped: {error}");
        }
        let count = self.ring.slots().min(out.len());
        for sample in &mut out[..count] {
            *sample = self.ring.pop().expect("SPSC readable sample");
        }
        Ok(count)
    }
}

fn capture_samples<T>(samples: &[T], channels: usize, producer: &mut Producer<i16>)
where
    T: Sample,
    f32: FromSample<T>,
{
    for frame in samples.chunks_exact(channels) {
        let mono = frame
            .iter()
            .map(|&sample| f32::from_sample(sample))
            .sum::<f32>()
            / channels as f32;
        let _ = producer.push((mono * 32768.0).round() as i16);
    }
}

fn playback_samples<T: Sample + FromSample<i16>>(
    output: &mut [T],
    channels: usize,
    turn: &AtomicU32,
    ring: &mut Consumer<(u32, i16)>,
) {
    for frame in output.chunks_exact_mut(channels) {
        let current = turn.load(Ordering::Acquire);
        let sample = loop {
            match ring.pop() {
                Ok((tag, sample)) if tag == current => break sample,
                Ok(_) => {}
                Err(_) => break 0,
            }
        };
        frame.fill(T::from_sample(sample));
    }
}

fn record_stream_error(failure: &OnceLock<StreamError>, error: StreamError) {
    // CPAL reports an underrun before attempting recovery. A failed recovery
    // is delivered separately as a backend or device error.
    if !matches!(
        error.kind(),
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    ) {
        let _ = failure.set(error);
    }
}

fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut ring: Consumer<(u32, i16)>,
    turn: Arc<AtomicU32>,
    failure: Arc<OnceLock<StreamError>>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<i16>,
{
    let channels = usize::from(config.channels);
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| playback_samples(out, channels, &turn, &mut ring),
        move |error| record_stream_error(&failure, error),
        None,
    )?)
}

impl Playback {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("No default output device")?;
        let supported = device
            .default_output_config()
            .context("Open the default output configuration")?;
        let config = supported.config();
        let rate = config.sample_rate;
        let turn = Arc::new(AtomicU32::new(0));
        let failure = Arc::new(OnceLock::new());
        let (ring, consumer) = RingBuffer::new(rate as usize / 25);
        macro_rules! play {
            ($sample:ty) => {
                output_stream::<$sample>(&device, config, consumer, turn.clone(), failure.clone())?
            };
        }
        let stream = match supported.sample_format() {
            SampleFormat::F32 => play!(f32),
            SampleFormat::F64 => play!(f64),
            SampleFormat::I16 => play!(i16),
            SampleFormat::I32 => play!(i32),
            SampleFormat::U16 => play!(u16),
            SampleFormat::I8 => play!(i8),
            SampleFormat::U8 => play!(u8),
            format => bail!("Unsupported output sample format {format}"),
        };
        stream.play()?;
        Ok(Self {
            _stream: stream,
            ring,
            turn,
            failure,
            rate,
        })
    }

    pub fn set_turn(&self, turn: u32) {
        self.turn.store(turn, Ordering::Release);
    }

    pub fn available(&self) -> usize {
        self.ring.slots()
    }

    pub fn write(&mut self, turn: u32, samples: &[i16]) -> Result<usize> {
        self.check()?;
        let count = samples.len().min(self.ring.slots());
        for &sample in &samples[..count] {
            self.ring
                .push((turn, sample))
                .expect("SPSC writable sample");
        }
        Ok(count)
    }

    pub fn check(&self) -> Result<()> {
        if let Some(error) = self.failure.get() {
            bail!("Output device stopped: {error}");
        }
        Ok(())
    }

    pub fn idle(&self) -> bool {
        self.ring.slots() == self.ring.buffer().capacity()
    }
}

pub fn devices() -> Result<Vec<(String, String)>> {
    let host = cpal::default_host();
    let mut rows = Vec::new();
    for device in host.input_devices()? {
        rows.push(("input".into(), device.description()?.name().into()));
    }
    for device in host.output_devices()? {
        rows.push(("output".into(), device.description()?.name().into()));
    }
    Ok(rows)
}

pub fn read_wav(source: impl Read, rate: u32) -> Result<Vec<i16>> {
    read_wav_inner(source, rate, None)
}

pub fn read_wav_limited(source: impl Read, rate: u32, max_seconds: u32) -> Result<Vec<i16>> {
    read_wav_inner(source, rate, Some(max_seconds))
}

fn read_wav_inner(source: impl Read, rate: u32, max_seconds: Option<u32>) -> Result<Vec<i16>> {
    let mut wav = hound::WavReader::new(source)?;
    let spec = wav.spec();
    ensure!(
        (1..=64).contains(&spec.channels) && (1_000..=384_000).contains(&spec.sample_rate),
        "Unsupported WAV channel count or sampling rate"
    );
    ensure!(
        max_seconds.is_none_or(|limit| wav.duration() <= spec.sample_rate.saturating_mul(limit)),
        "WAV exceeds the duration limit"
    );
    let channels = usize::from(spec.channels);
    let mut out = Vec::new();
    let mut resampler = Resampler::new(spec.sample_rate, rate);
    match spec.sample_format {
        hound::SampleFormat::Float => {
            downmix(
                wav.samples::<f32>()
                    .map(|sample| sample.map(|s| f64::from(s) * 32768.0)),
                channels,
                &mut resampler,
                &mut out,
            )?;
        }
        hound::SampleFormat::Int => {
            let scale = 32768.0 / 2.0f64.powi(i32::from(spec.bits_per_sample) - 1);
            downmix(
                wav.samples::<i32>()
                    .map(|sample| sample.map(|s| f64::from(s) * scale)),
                channels,
                &mut resampler,
                &mut out,
            )?;
        }
    }
    resampler.finish(&mut out);
    Ok(out)
}

fn downmix(
    samples: impl Iterator<Item = hound::Result<f64>>,
    channels: usize,
    resampler: &mut Resampler,
    out: &mut Vec<i16>,
) -> Result<()> {
    let mut sum = 0.0;
    let mut count = 0;
    let mut block = [0i16; 1_024];
    let mut used = 0;
    for sample in samples {
        sum += sample?;
        count += 1;
        if count == channels {
            block[used] = (sum / channels as f64).round() as i16;
            used += 1;
            if used == block.len() {
                resampler.push(&block, out);
                used = 0;
            }
            sum = 0.0;
            count = 0;
        }
    }
    ensure!(count == 0, "WAV ends in the middle of a sample frame");
    resampler.push(&block[..used], out);
    Ok(())
}

pub fn write_wav(to: impl Write + Seek, samples: &[i16], rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::new(to, spec)?;
    for &sample in samples {
        wav.write_sample(sample)?;
    }
    wav.finalize()?;
    Ok(())
}

#[cfg(feature = "opus")]
pub struct Opus {
    encoder: opus::Encoder,
    frame: [i16; 480],
    used: usize,
}

#[cfg(feature = "opus")]
impl Opus {
    pub fn new() -> Result<Self> {
        Ok(Self {
            encoder: opus::Encoder::new(24_000, opus::Channels::Mono, opus::Application::Voip)?,
            frame: [0; 480],
            used: 0,
        })
    }

    pub fn push(&mut self, pcm: &[i16]) -> Result<Option<Vec<u8>>> {
        ensure!(
            pcm.len() <= self.frame.len(),
            "Opus input exceeds one frame"
        );
        let count = pcm.len().min(self.frame.len() - self.used);
        self.frame[self.used..self.used + count].copy_from_slice(&pcm[..count]);
        self.used += count;
        if self.used < self.frame.len() {
            return Ok(None);
        }
        let packet = self.encoder.encode_vec(&self.frame, 1276)?;
        let left = &pcm[count..];
        self.frame[..left.len()].copy_from_slice(left);
        self.used = left.len();
        Ok(Some(packet))
    }

    pub fn finish(&mut self) -> Result<Option<Vec<u8>>> {
        if self.used == 0 {
            return Ok(None);
        }
        self.frame[self.used..].fill(0);
        self.used = 0;
        Ok(Some(self.encoder.encode_vec(&self.frame, 1276)?))
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    thread_local! {
        static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    #[global_allocator]
    static ALLOCATOR: Probe = Probe;

    struct Probe;

    unsafe impl GlobalAlloc for Probe {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = COUNT.try_with(|count| {
                if let Some(n) = count.get() {
                    count.set(Some(n + 1));
                }
            });
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe {
                System.dealloc(ptr, layout);
            }
        }
    }

    #[test]
    fn recoverable_underrun_leaves_stream_available() {
        let failure = OnceLock::new();
        record_stream_error(&failure, ErrorKind::Xrun.into());
        assert!(
            failure.get().is_none(),
            "an underrun must not stop the stream"
        );
        record_stream_error(&failure, ErrorKind::DeviceNotAvailable.into());
        record_stream_error(&failure, ErrorKind::Xrun.into());
        assert!(
            failure.get().map(StreamError::kind) == Some(ErrorKind::DeviceNotAvailable),
            "a disconnected device must be reported"
        );
    }

    #[test]
    fn callback_conversion_is_allocation_free_and_drops_cancelled_samples() {
        let (mut capture, mut captured) = RingBuffer::new(4);
        let (mut playback, mut queued) = RingBuffer::new(4);
        playback.push((1, 5)).unwrap();
        playback.push((2, -7)).unwrap();
        let current = AtomicU32::new(2);
        let mut out = [0i16; 6];
        COUNT.set(Some(0));
        capture_samples(&[0.25f32, 0.75, -0.25, -0.75], 2, &mut capture);
        playback_samples(&mut out, 2, &current, &mut queued);
        let count = COUNT.replace(None);
        assert_eq!(count, Some(0));
        assert_eq!(captured.pop(), Ok(16384));
        assert_eq!(captured.pop(), Ok(-16384));
        assert_eq!(out, [-7, -7, 0, 0, 0, 0]);
    }
}

#[cfg(all(test, feature = "opus"))]
#[test]
fn opus_keeps_partial_frames_until_the_tail() {
    let mut encoder = Opus::new().unwrap();
    assert!(encoder.push(&[17; 200]).unwrap().is_none());
    let head = encoder.push(&[31; 400]).unwrap().unwrap();
    let tail = encoder.finish().unwrap().unwrap();
    let mut decoder = opus::Decoder::new(24_000, opus::Channels::Mono).unwrap();
    let mut output = [0; 480];
    assert_eq!(decoder.decode(&head, &mut output, false).unwrap(), 480);
    assert_eq!(decoder.decode(&tail, &mut output, false).unwrap(), 480);
    assert!(encoder.finish().unwrap().is_none());
}
