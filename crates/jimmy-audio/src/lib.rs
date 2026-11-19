//! Native audio and WAV I/O. CPAL callbacks only convert samples and touch SPSC rings.
use anyhow::{Context, Result, bail, ensure};
use cpal::{
    BufferSize, Error as StreamError, ErrorKind, FromSample, Sample, SampleFormat, SizedSample,
    Stream, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use jimmy_core::Resampler;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    io::{Read, Seek, Write},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
};

/// Seconds of speech the playback queue can hold. Synthesis runs ahead into it,
/// so a slow stretch, or a session thread that is busy transcribing, is covered
/// by audio that is already queued.
const QUEUE_SECONDS: usize = 8;
/// Quiet for this long is a pause between phrases, not the closure of a consonant.
const PAUSE_MS: usize = 120;
/// Mean level below which synthesized speech is silent.
const QUIET_LEVEL: f32 = 100.0;
/// The longest one pause is held open while waiting for more speech.
const MAX_STRETCH_MS: usize = 3_000;

/// The longest period `Playback::longer_period` will ask for, in seconds.
const MAX_PERIOD_SECONDS: f32 = 0.35;
/// Seconds of microphone audio kept while the session is busy. A transcription
/// holds its thread for a few hundred milliseconds, and longer on a busy
/// machine. What was said meanwhile must still be there afterwards.
const CAPTURE_SECONDS: usize = 8;

/// Silences the output at once from any thread. What is queued stays queued
/// and plays on when the hold is lifted.
#[derive(Clone, Default)]
pub struct Hold(Arc<AtomicBool>);

impl Hold {
    pub fn set(&self, held: bool) {
        self.0.store(held, Ordering::Release);
    }
}

pub struct Capture {
    _stream: Stream,
    ring: Consumer<i16>,
    pub rate: u32,
    failure: Arc<OnceLock<StreamError>>,
    lost: Arc<AtomicUsize>,
    xruns: Arc<AtomicU32>,
}

pub struct Playback {
    _stream: Stream,
    ring: Producer<(u32, i16)>,
    shared: Arc<Shared>,
    failure: Arc<OnceLock<StreamError>>,
    pub rate: u32,
    period: Option<u32>,
}

/// Frames to ask the device for at a time: about 43 ms, with a buffer of twice that.
///
/// CPAL's own default on Linux follows the sound server's suggestion, which
/// through PipeWire is 512 frames with a 1024 frame buffer. That is 21 ms of
/// audio between the device and silence. A thread that is scheduled 20 ms late
/// on a busy machine empties it, and a sink that takes more than 1024 frames
/// at a time, as some high-latency sinks do, empties it on every cycle.
/// `JIMMY_AUDIO_PERIOD` overrides the frames; 0 leaves the device's default.
pub fn preferred_period(rate: u32) -> Option<u32> {
    if let Some(frames) = std::env::var("JIMMY_AUDIO_PERIOD")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        return (frames > 0).then_some(frames);
    }
    cfg!(target_os = "linux").then(|| (rate / 24).next_power_of_two())
}

/// The stream configurations to try: the wanted period when the device can do
/// it, then the device's own default.
fn configs(
    supported: &SupportedStreamConfig,
    period: Option<u32>,
) -> Vec<(StreamConfig, Option<u32>)> {
    let mut all = Vec::new();
    if let Some(frames) = period {
        let fits = match supported.buffer_size() {
            SupportedBufferSize::Range { min, max } => (*min..=*max).contains(&frames),
            SupportedBufferSize::Unknown => true,
        };
        if fits {
            let mut config = supported.config();
            config.buffer_size = BufferSize::Fixed(frames);
            all.push((config, Some(frames)));
        }
    }
    all.push((supported.config(), None));
    all
}

/// What the output callback and the thread that feeds it tell each other.
#[derive(Default)]
struct Shared {
    turn: AtomicU32,
    /// More of the current speech is still on its way.
    more: AtomicBool,
    /// Frames to queue before playback starts.
    start: AtomicUsize,
    /// Frames of queued speech below which the next pause is held open.
    reserve: AtomicUsize,
    underruns: AtomicU32,
    stretched: AtomicUsize,
    xruns: AtomicU32,
    period: AtomicUsize,
}

/// Counters since the stream opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStats {
    /// Times the queue ran dry in the middle of speech.
    pub underruns: u32,
    /// Frames of silence added to pauses while waiting for more speech.
    pub stretched: usize,
    /// Underruns reported by the device itself.
    pub xruns: u32,
    /// The most frames the device asked for at once.
    pub period: usize,
}

impl Capture {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_input_device()
            .context("No default input device")?;
        let supported = device
            .default_input_config()
            .context("Open the default input configuration")?;
        let mut failed = None;
        for (config, _) in configs(&supported, preferred_period(supported.sample_rate())) {
            match Self::start(&device, &supported, config) {
                Ok(capture) => return Ok(capture),
                Err(error) => failed = Some(error),
            }
        }
        Err(failed.expect("at least the default configuration was tried"))
    }

    fn start(
        device: &cpal::Device,
        supported: &SupportedStreamConfig,
        config: StreamConfig,
    ) -> Result<Self> {
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);
        let failure = Arc::new(OnceLock::new());
        let lost = Arc::new(AtomicUsize::new(0));
        let xruns = Arc::new(AtomicU32::new(0));
        let (mut producer, ring) = RingBuffer::new(rate as usize * CAPTURE_SECONDS);
        macro_rules! capture {
            ($sample:ty) => {{
                let failure = failure.clone();
                let lost = lost.clone();
                let xruns = xruns.clone();
                device.build_input_stream(
                    config,
                    move |samples: &[$sample], _| {
                        capture_samples(samples, channels, &mut producer, &lost)
                    },
                    move |error| {
                        if error.kind() == ErrorKind::Xrun {
                            xruns.fetch_add(1, Ordering::Relaxed);
                        }
                        record_stream_error(&failure, error)
                    },
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
            lost,
            xruns,
        })
    }

    /// Times the device itself overran, each losing some audio.
    pub fn xruns(&self) -> u32 {
        self.xruns.load(Ordering::Relaxed)
    }

    /// Frames dropped because nothing read them in time.
    pub fn lost(&self) -> usize {
        self.lost.load(Ordering::Relaxed)
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

fn capture_samples<T>(
    samples: &[T],
    channels: usize,
    producer: &mut Producer<i16>,
    lost: &AtomicUsize,
) where
    T: Sample,
    f32: FromSample<T>,
{
    let mut dropped = 0;
    for frame in samples.chunks_exact(channels) {
        let mono = frame
            .iter()
            .map(|&sample| f32::from_sample(sample))
            .sum::<f32>()
            / channels as f32;
        if producer.push((mono * 32768.0).round() as i16).is_err() {
            dropped += 1;
        }
    }
    if dropped > 0 {
        lost.fetch_add(dropped, Ordering::Relaxed);
    }
}

enum Playout {
    /// Nothing is playing. `dry` when the queue emptied in the middle of speech.
    Waiting {
        dry: bool,
    },
    Playing,
    /// A pause between phrases has been held open for this many frames.
    Stretching(usize),
}

/// The output callback's side of the playback queue.
///
/// Speech is synthesized at about the speed it is spoken, and slower on a busy
/// machine. Playing each sample the moment it exists then breaks words apart
/// whenever synthesis falls behind. So speech starts once a little is queued,
/// and while the queue is short the pauses the speech already has are held
/// open until it has caught up. If the queue still runs dry, playback waits
/// for the reserve again instead of stuttering through every late frame.
struct Player {
    ring: Consumer<(u32, i16)>,
    shared: Arc<Shared>,
    hold: Hold,
    turn: u32,
    state: Playout,
    level: f32,
    smoothing: f32,
    quiet: usize,
    pause: usize,
    max_stretch: usize,
    stretched: bool,
}

impl Player {
    fn new(ring: Consumer<(u32, i16)>, shared: Arc<Shared>, hold: Hold, rate: u32) -> Self {
        let rate = rate as usize;
        Self {
            ring,
            shared,
            hold,
            turn: 0,
            state: Playout::Waiting { dry: false },
            level: 0.0,
            smoothing: 100.0 / rate as f32,
            quiet: 0,
            pause: rate * PAUSE_MS / 1_000,
            max_stretch: rate * MAX_STRETCH_MS / 1_000,
            stretched: false,
        }
    }

    fn fill<T: Sample + FromSample<i16>>(&mut self, output: &mut [T], channels: usize) {
        let frames = output.len() / channels;
        self.shared.period.fetch_max(frames, Ordering::Relaxed);
        if self.hold.0.load(Ordering::Acquire) {
            output.fill(T::from_sample(0i16));
            return;
        }
        let more = self.shared.more.load(Ordering::Acquire);
        // The device takes a whole period at once, so less than two of them
        // queued cannot play smoothly whatever was asked for.
        let start = self.shared.start.load(Ordering::Relaxed).max(2 * frames);
        let reserve = self.shared.reserve.load(Ordering::Relaxed).max(start);
        self.turn = self.shared.turn.load(Ordering::Acquire);
        let (mut underruns, mut stretched) = (0, 0);
        for frame in output.chunks_exact_mut(channels) {
            let sample = self.next(more, start, reserve, &mut underruns, &mut stretched);
            frame.fill(T::from_sample(sample));
        }
        if underruns > 0 {
            self.shared
                .underruns
                .fetch_add(underruns, Ordering::Relaxed);
        }
        if stretched > 0 {
            self.shared
                .stretched
                .fetch_add(stretched, Ordering::Relaxed);
        }
    }

    fn next(
        &mut self,
        more: bool,
        start: usize,
        reserve: usize,
        underruns: &mut u32,
        stretched: &mut usize,
    ) -> i16 {
        // Drop what a stop or a new turn left queued. The turn is read again
        // before a sample is dropped, because the feeding thread may have moved
        // on and queued this very sample since the last read.
        while let Ok(&(tag, _)) = self.ring.peek() {
            if tag == self.turn {
                break;
            }
            self.turn = self.shared.turn.load(Ordering::Acquire);
            if tag == self.turn {
                break;
            }
            let _ = self.ring.pop();
            self.state = Playout::Waiting { dry: false };
            self.level = 0.0;
            self.quiet = 0;
            self.stretched = false;
        }
        match self.state {
            Playout::Waiting { dry } => {
                let queued = self.ring.slots();
                let wanted = if dry { reserve } else { start };
                if queued == 0 || (more && queued < wanted) {
                    return 0;
                }
            }
            Playout::Stretching(held) => {
                if more && held < self.max_stretch && self.ring.slots() < reserve {
                    self.state = Playout::Stretching(held + 1);
                    *stretched += 1;
                    return 0;
                }
            }
            Playout::Playing => {}
        }
        self.state = Playout::Playing;
        let Ok((_, sample)) = self.ring.pop() else {
            *underruns += u32::from(more);
            self.state = Playout::Waiting { dry: more };
            self.level = 0.0;
            self.quiet = 0;
            self.stretched = false;
            return 0;
        };
        self.level += (f32::from(sample).abs() - self.level) * self.smoothing;
        if self.level < QUIET_LEVEL {
            self.quiet += 1;
            if more && !self.stretched && self.quiet >= self.pause && self.ring.slots() < reserve {
                self.stretched = true;
                self.state = Playout::Stretching(0);
            }
        } else {
            self.quiet = 0;
            self.stretched = false;
        }
        sample
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
    ring: Consumer<(u32, i16)>,
    shared: Arc<Shared>,
    hold: Hold,
    failure: Arc<OnceLock<StreamError>>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<i16>,
{
    let channels = usize::from(config.channels);
    let xruns = shared.clone();
    let mut player = Player::new(ring, shared, hold, config.sample_rate);
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| player.fill(out, channels),
        move |error| {
            if error.kind() == ErrorKind::Xrun {
                xruns.xruns.fetch_add(1, Ordering::Relaxed);
            }
            record_stream_error(&failure, error)
        },
        None,
    )?)
}

impl Playback {
    pub fn open() -> Result<Self> {
        Self::open_with(None, &Hold::default())
    }

    /// Opens the default output asking for `period` frames at a time, or for
    /// `preferred_period` when `None`. A device that cannot do it is opened
    /// with its own default. `hold` silences it from other threads.
    pub fn open_with(period: Option<u32>, hold: &Hold) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("No default output device")?;
        let supported = device
            .default_output_config()
            .context("Open the default output configuration")?;
        let period = period.or_else(|| preferred_period(supported.sample_rate()));
        let mut failed = None;
        for (config, period) in configs(&supported, period) {
            match Self::start(&device, &supported, config, period, hold) {
                Ok(playback) => return Ok(playback),
                Err(error) => failed = Some(error),
            }
        }
        Err(failed.expect("at least the default configuration was tried"))
    }

    fn start(
        device: &cpal::Device,
        supported: &SupportedStreamConfig,
        config: StreamConfig,
        period: Option<u32>,
        hold: &Hold,
    ) -> Result<Self> {
        let rate = config.sample_rate;
        let shared = Arc::new(Shared::default());
        let failure = Arc::new(OnceLock::new());
        let (ring, consumer) = RingBuffer::new(rate as usize * QUEUE_SECONDS);
        macro_rules! play {
            ($sample:ty) => {
                output_stream::<$sample>(
                    device,
                    config,
                    consumer,
                    shared.clone(),
                    hold.clone(),
                    failure.clone(),
                )?
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
        let playback = Self {
            _stream: stream,
            ring,
            shared,
            failure,
            rate,
            period,
        };
        playback.set_reserve(0.0);
        Ok(playback)
    }

    /// The next period to try for a device that underruns with this one, or
    /// `None` when it is already as long as is useful.
    pub fn longer_period(&self) -> Option<u32> {
        let now = self.period.unwrap_or(self.stats().period as u32).max(256);
        let longer = now.checked_mul(2)?;
        (longer as f32 <= self.rate as f32 * MAX_PERIOD_SECONDS).then_some(longer)
    }

    pub fn set_turn(&self, turn: u32) {
        self.shared.turn.store(turn, Ordering::Release);
    }

    /// Says whether more of the current speech is still to be written. While
    /// it is, playback waits for a reserve before it starts and holds pauses
    /// open to keep one. Once it is not, everything queued plays out.
    pub fn expect_more(&self, more: bool) {
        self.shared.more.store(more, Ordering::Release);
    }

    /// Sets the seconds of queued speech below which pauses are held open.
    /// Playback starts at half of it. Both have the same floor, a quarter of
    /// a second, which suits speech synthesized as fast as it is spoken.
    pub fn set_reserve(&self, seconds: f32) {
        let frames = |seconds: f32| (seconds * self.rate as f32) as usize;
        let reserve = seconds.clamp(0.25, 3.0);
        self.shared
            .reserve
            .store(frames(reserve), Ordering::Relaxed);
        self.shared
            .start
            .store(frames((reserve / 2.0).clamp(0.25, 1.0)), Ordering::Relaxed);
    }

    pub fn stats(&self) -> PlaybackStats {
        PlaybackStats {
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            stretched: self.shared.stretched.load(Ordering::Relaxed),
            xruns: self.shared.xruns.load(Ordering::Relaxed),
            period: self.shared.period.load(Ordering::Relaxed),
        }
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
        let (mut playback, queued) = RingBuffer::new(4);
        playback.push((1, 5)).unwrap();
        playback.push((2, -7)).unwrap();
        let shared = Arc::new(Shared::default());
        shared.turn.store(2, Ordering::Release);
        let mut player = Player::new(queued, shared, Hold::default(), RATE);
        let mut out = [0i16; 6];
        COUNT.set(Some(0));
        capture_samples(
            &[0.25f32, 0.75, -0.25, -0.75],
            2,
            &mut capture,
            &AtomicUsize::new(0),
        );
        player.fill(&mut out, 2);
        let count = COUNT.replace(None);
        assert_eq!(count, Some(0));
        assert_eq!(captured.pop(), Ok(16384));
        assert_eq!(captured.pop(), Ok(-16384));
        assert_eq!(out, [-7, -7, 0, 0, 0, 0]);
    }

    const RATE: u32 = 48_000;
    const PERIOD: usize = 480;
    const LOUD: i16 = 4_000;
    const HUSH: i16 = 5;

    /// A playback queue driven by hand: `write` is the feeding thread, `play`
    /// is the device asking for one 10 ms period.
    struct Rig {
        queue: Producer<(u32, i16)>,
        player: Player,
        shared: Arc<Shared>,
        hold: Hold,
        heard: Vec<i16>,
    }

    impl Rig {
        fn new(reserve_ms: usize, start_ms: usize) -> Self {
            let (queue, consumer) = RingBuffer::new(RATE as usize * QUEUE_SECONDS);
            let shared = Arc::new(Shared::default());
            let frames = |ms: usize| RATE as usize * ms / 1_000;
            shared.reserve.store(frames(reserve_ms), Ordering::Relaxed);
            shared.start.store(frames(start_ms), Ordering::Relaxed);
            shared.more.store(true, Ordering::Release);
            let hold = Hold::default();
            Self {
                queue,
                player: Player::new(consumer, shared.clone(), hold.clone(), RATE),
                shared,
                hold,
                heard: Vec::new(),
            }
        }

        fn write(&mut self, sample: i16, ms: usize) {
            let turn = self.shared.turn.load(Ordering::Acquire);
            for _ in 0..RATE as usize * ms / 1_000 {
                self.queue.push((turn, sample)).unwrap();
            }
        }

        fn play(&mut self, ms: usize) -> &[i16] {
            let before = self.heard.len();
            for _ in 0..ms / 10 {
                let mut out = [0i16; PERIOD];
                self.player.fill(&mut out, 1);
                self.heard.extend(out);
            }
            &self.heard[before..]
        }

        fn underruns(&self) -> u32 {
            self.shared.underruns.load(Ordering::Relaxed)
        }
    }

    fn silent(samples: &[i16]) -> bool {
        samples.iter().all(|&sample| sample == 0)
    }

    /// The longest run of exact zeros between two loud samples, in ms.
    fn longest_break_inside_sound(samples: &[i16]) -> usize {
        let mut longest = 0;
        let mut run = 0;
        let mut after_sound = false;
        for &sample in samples {
            if sample == 0 {
                run += 1;
            } else {
                if sample == LOUD && after_sound {
                    longest = longest.max(run);
                }
                after_sound = sample == LOUD;
                run = 0;
            }
        }
        longest * 1_000 / RATE as usize
    }

    #[test]
    fn speech_starts_once_a_reserve_is_queued_and_short_speech_plays_out() {
        let mut rig = Rig::new(300, 250);
        rig.write(LOUD, 100);
        assert!(silent(rig.play(50)), "100 ms queued must not start yet");
        rig.write(LOUD, 200);
        assert_eq!(rig.play(10)[0], LOUD, "300 ms queued starts playback");

        let mut rig = Rig::new(300, 250);
        rig.write(LOUD, 100);
        rig.shared.more.store(false, Ordering::Release);
        assert_eq!(rig.play(10)[0], LOUD, "a short reply plays at once");
        rig.play(200);
        assert_eq!(
            rig.underruns(),
            0,
            "running out at the end is not an underrun"
        );
    }

    #[test]
    fn slow_synthesis_stretches_pauses_instead_of_breaking_words() {
        // Every second of speech takes 1.25 s to synthesize: 800 ms of sound
        // and a 200 ms pause arrive over 1250 ms.
        let mut rig = Rig::new(1_300, 250);
        let phrase: Vec<i16> = [vec![LOUD; 80], vec![HUSH; 20]].concat();
        let mut written = 0;
        for tick in 0..2_000 {
            // 10 ms of playback per tick, 8 ms of new speech.
            if tick % 5 != 4 {
                let sample = phrase[(written / 10) % phrase.len()];
                rig.write(sample, 10);
                written += 10;
            }
            rig.play(10);
        }
        assert_eq!(rig.underruns(), 0, "the queue must never run dry");
        assert_eq!(longest_break_inside_sound(&rig.heard), 0);
        let stretched = rig.shared.stretched.load(Ordering::Relaxed) * 1_000 / RATE as usize;
        // 16 s of speech took 20 s to arrive, so about 4 s of waiting went into pauses.
        assert!((2_500..=5_000).contains(&stretched), "{stretched} ms");
    }

    #[test]
    fn fast_synthesis_is_played_untouched() {
        let mut rig = Rig::new(300, 250);
        let phrase: Vec<i16> = [vec![LOUD; 80], vec![HUSH; 20]].concat();
        for tick in 0..500 {
            rig.write(phrase[tick % phrase.len()], 20);
            rig.play(10);
        }
        assert_eq!(rig.underruns(), 0);
        assert_eq!(rig.shared.stretched.load(Ordering::Relaxed), 0);
        let played: Vec<i16> = rig.heard.iter().copied().filter(|&s| s != 0).collect();
        assert!(played.len() > RATE as usize * 4);
    }

    #[test]
    fn an_underrun_waits_for_the_reserve_before_resuming() {
        let mut rig = Rig::new(600, 250);
        rig.write(LOUD, 300);
        rig.play(400);
        assert_eq!(rig.underruns(), 1);
        rig.write(LOUD, 300);
        assert!(silent(rig.play(50)), "300 ms is below the reserve");
        rig.write(LOUD, 300);
        assert_eq!(rig.play(10)[0], LOUD);
        assert_eq!(rig.underruns(), 1);
    }

    #[test]
    fn a_stop_silences_the_queue_at_once_and_new_speech_starts_clean() {
        let mut rig = Rig::new(300, 250);
        rig.write(LOUD, 5_000);
        assert_eq!(rig.play(10)[0], LOUD);
        rig.shared.turn.store(1, Ordering::Release);
        assert!(
            silent(rig.play(100)),
            "nothing of the stopped speech may play"
        );
        assert_eq!(rig.queue.slots(), rig.queue.buffer().capacity());
        rig.write(LOUD / 2, 300);
        assert_eq!(rig.play(10)[0], LOUD / 2);
        assert_eq!(rig.underruns(), 0);
    }

    #[test]
    fn a_hold_silences_at_once_and_loses_nothing() {
        let mut rig = Rig::new(300, 250);
        rig.write(LOUD, 1_000);
        assert_eq!(rig.play(100)[0], LOUD);
        rig.hold.set(true);
        assert!(silent(rig.play(200)));
        rig.hold.set(false);
        // The stop was not for this speech after all: it carries on to its end.
        rig.shared.more.store(false, Ordering::Release);
        let rest = rig.play(1_000).to_vec();
        assert_eq!(
            rest.iter().filter(|&&sample| sample == LOUD).count(),
            RATE as usize * 9 / 10
        );
        assert_eq!(rig.underruns(), 0);
    }

    #[test]
    fn a_long_period_raises_the_start_to_two_periods() {
        // A sink that takes 8192 frames at a time (171 ms) needs more than the
        // 100 ms asked for here, or every period would end in silence.
        let mut rig = Rig::new(100, 100);
        rig.write(LOUD, 200);
        let mut out = [0i16; 8_192];
        rig.player.fill(&mut out, 1);
        assert!(silent(&out));
        rig.write(LOUD, 200);
        rig.player.fill(&mut out, 1);
        assert!(out.iter().all(|&sample| sample == LOUD));
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
