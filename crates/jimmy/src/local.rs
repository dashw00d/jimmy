use crate::print_json;
use anyhow::{Result, ensure};
use jimmy_audio::{Capture, Hold, Playback};
use jimmy_core::{AudioStep, AudioStream, Resampler, Session, Settings, Speech, Stt, Tts};
use jimmy_phonon::Phonon;
use jimmy_proto::{ClientEvent, OUTPUT_RATE, ServerEvent};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, Read},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(4);
const OUTPUT_RETRY: Duration = Duration::from_secs(1);
/// Speech between two pauses that the playback reserve has to carry when
/// synthesis is slower than speaking.
const PHRASE_SECONDS: f32 = 5.0;

/// Measures how fast speech is synthesized against the speed it is spoken, and
/// sizes the playback reserve to match.
///
/// At full speed or faster a small reserve is enough. Slower than that, each
/// phrase drains the queue by the shortfall, so the reserve has to cover one
/// phrase of it. The pauses between phrases are then held open to refill it.
/// The reserve also has to outlast the time this thread spends elsewhere.
struct Pace {
    since: Option<Instant>,
    samples: usize,
    speed: f32,
    passed: Instant,
    stall: f32,
}

impl Pace {
    fn new() -> Self {
        Self {
            since: None,
            samples: 0,
            speed: 1.0,
            passed: Instant::now(),
            stall: 0.0,
        }
    }

    /// Notes how long the thread that feeds the speaker was away. A
    /// transcription keeps it for a few hundred milliseconds or more, and the
    /// queue has to last through that. The longest recent absence fades over
    /// about ten seconds.
    fn pass(&mut self) {
        let away = self.passed.elapsed().as_secs_f32();
        self.passed = Instant::now();
        self.stall = if away > self.stall {
            away
        } else {
            self.stall - (self.stall - away) * (away / 10.0).min(1.0)
        };
    }

    /// Counts synthesized samples. The first ones only start the clock: they
    /// were made before it.
    fn produced(&mut self, count: usize) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        } else {
            self.samples += count;
        }
    }

    /// Call on every pass. `synthesizing` while speech is being made, `ahead`
    /// when the playback queue is full and synthesis has to wait for it.
    fn update(&mut self, synthesizing: bool, ahead: bool) {
        if !synthesizing || ahead {
            if ahead {
                self.speed = self.speed.max(1.0);
            }
            self.since = None;
            self.samples = 0;
            return;
        }
        let Some(since) = self.since else {
            return;
        };
        let elapsed = since.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            self.measured(self.samples as f32 / OUTPUT_RATE as f32 / elapsed);
            self.since = Some(Instant::now());
            self.samples = 0;
        }
    }

    fn measured(&mut self, speed: f32) {
        self.speed = (self.speed + speed) / 2.0;
    }

    /// Seconds of queued speech to keep.
    fn reserve(&self) -> f32 {
        0.25 + PHRASE_SECONDS * (1.0 - self.speed).max(0.0) + self.stall
    }
}

/// Spots an output device whose buffer is too short for its sink.
///
/// Jimmy plays silence between replies, so the device underruns then too. A
/// busy moment causes the odd underrun. A sink that takes more frames at a
/// time than the buffer holds causes one on every cycle.
struct Storm {
    since: Instant,
    xruns: u32,
    seconds: u8,
}

impl Storm {
    fn new(xruns: u32) -> Self {
        Self {
            since: Instant::now(),
            xruns,
            seconds: 0,
        }
    }

    /// Takes the device's underrun count. True once it has underrun at least
    /// four times a second for two seconds running.
    fn seen(&mut self, xruns: u32) -> bool {
        if self.since.elapsed() >= Duration::from_secs(1) {
            let steady = xruns.wrapping_sub(self.xruns) >= 4;
            *self = Self {
                since: Instant::now(),
                xruns,
                seconds: if steady {
                    self.seconds.saturating_add(1)
                } else {
                    0
                },
            };
        }
        self.seconds >= 2
    }
}

/// Speech at the device's rate that is waiting for room in the playback queue.
#[derive(Default)]
struct Staged {
    pcm: Vec<i16>,
    sent: usize,
}

impl Staged {
    fn is_empty(&self) -> bool {
        self.sent == self.pcm.len()
    }

    fn clear(&mut self) {
        self.pcm.clear();
        self.sent = 0;
    }

    /// Queues as much as fits. Returns whether everything was queued.
    fn send(&mut self, device: &mut Playback, turn: u32) -> Result<bool> {
        self.sent += device.write(turn, &self.pcm[self.sent..])?;
        if self.is_empty() {
            self.clear();
        }
        Ok(self.is_empty())
    }
}

struct LocalTts(Option<Phonon>);

impl Tts for LocalTts {
    fn available(&self) -> bool {
        self.0.is_some()
    }
    fn start(&mut self, text: &str, options: &Speech) -> Result<Box<dyn AudioStream>, String> {
        self.0
            .as_mut()
            .ok_or_else(|| "Playback is unavailable in listen mode".to_owned())?
            .start(text, options)
    }

    fn voices(&self) -> Vec<String> {
        self.0.as_ref().map(Tts::voices).unwrap_or_default()
    }
}

enum Input {
    Event(ClientEvent),
    Toggle,
    Error(String),
}

pub fn run(
    stt: impl Stt,
    tts: Option<Phonon>,
    mut settings: Settings,
    pipe: bool,
    json: bool,
    cue_dir: Option<&Path>,
    agent: Option<crate::talk::Agent>,
) -> Result<()> {
    let mut capture = Capture::open()?;
    let mut retry_output = None;
    let hold = Hold::default();
    let mut playback = match tts
        .is_some()
        .then(|| Playback::open_with(None, &hold))
        .transpose()
    {
        Ok(playback) => playback,
        Err(error) => {
            retry_output = Some(retry_playback(&error));
            None
        }
    };
    let cues = read_cues(cue_dir)?;
    settings.input_rate = capture.rate;
    let mode = settings.mode;
    let ptt = mode == jimmy_proto::Mode::Ptt;
    let mut session = Session::new(stt, LocalTts(tts), settings);
    let mut resampler = playback
        .as_ref()
        .map(|playback| Resampler::new(OUTPUT_RATE, playback.rate));
    let mut staged = Staged::default();
    let mut output_period = None;
    let playback_log = std::env::var_os("JIMMY_PLAYBACK_LOG").is_some();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let (tx, rx) = mpsc::sync_channel(32);
    let link = agent.map(|agent| {
        let tx = tx.clone();
        agent.start(
            mode,
            move |event| {
                let _ = tx.send(Input::Event(event));
            },
            crate::talk::Terminal,
        )
    });
    if pipe || ptt {
        let hold = hold.clone();
        thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                match (&mut stdin).take(96_001).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() > 96_000 => {
                        let _ = tx.send(Input::Error("JSONL line exceeds 96000 bytes".into()));
                        break;
                    }
                    _ => {}
                }
                let event = if pipe {
                    match serde_json::from_slice::<ClientEvent>(&line) {
                        Ok(event) => {
                            // Go quiet now. The session may be in the middle of
                            // a transcription and only see this when it is done.
                            if stops_speech(&event) {
                                hold.set(true);
                            }
                            Input::Event(event)
                        }
                        Err(error) => Input::Error(error.to_string()),
                    }
                } else {
                    Input::Toggle
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
    }
    if json {
        print_json(&session.poll().expect("session ready"))?;
    }
    let mut holding = false;
    let mut eof = false;
    let mut mic = [0; 1920];
    let mut out = [0; 480];
    let mut playback_tag = 0u32;
    let mut playback_key = (session.turn(), session.playback_revision());
    // Started here, so that setting up is not taken for a stall.
    let mut pace = Pace::new();
    let mut storm = Storm::new(0);
    while !stop.load(Ordering::Relaxed) {
        let mut stopped = false;
        for _ in 0..32 {
            match rx.try_recv() {
                Ok(Input::Event(mut event)) => {
                    if let ClientEvent::Configure(config) = &mut event {
                        config.input_rate = Some(capture.rate);
                    }
                    stopped |= stops_speech(&event);
                    session.push_event(event);
                }
                Ok(Input::Toggle) => {
                    holding = !holding;
                    session.push_event(if holding {
                        ClientEvent::ListenStart
                    } else {
                        ClientEvent::ListenStop
                    });
                }
                Ok(Input::Error(error)) => print_json(&ServerEvent::error(
                    Some(session.turn()),
                    "invalid_event",
                    error,
                ))?,
                Err(TryRecvError::Disconnected) if pipe && !eof => {
                    eof = true;
                    session.push_event(ClientEvent::ListenStop);
                    session.push_event(ClientEvent::SpeakFlush { turn: None });
                    break;
                }
                Err(_) => break,
            }
        }
        if playback.is_none() && !eof && retry_output.is_some_and(|at| Instant::now() >= at) {
            if let Ok(device) = Playback::open_with(output_period, &hold) {
                device.set_turn(playback_tag);
                resampler = Some(Resampler::new(OUTPUT_RATE, device.rate));
                storm = Storm::new(0);
                playback = Some(device);
            }
            // Give the reopened stream time to report an immediate device error
            // before announcing that the outage is over.
            retry_output = Some(Instant::now() + OUTPUT_RETRY);
        }
        if let Some(device) = &mut playback {
            let key = (session.turn(), session.playback_revision());
            if playback_key != key {
                playback_key = key;
                playback_tag = playback_tag.wrapping_add(1);
                device.set_turn(playback_tag);
                staged.clear();
                resampler = Some(Resampler::new(OUTPUT_RATE, device.rate));
            }
            // Synthesis runs ahead of the speaker: take everything it has made
            // until the playback queue is full.
            let queued = loop {
                match staged.send(device, playback_tag) {
                    Ok(true) => {}
                    Ok(false) => break Ok(true),
                    Err(error) => break Err(error),
                }
                // The end of a reply stays in the session until the queue has
                // played out, so `speak.end` and the follow-up window still
                // begin when the speaker goes quiet.
                if !session.speech_ready() && !device.idle() {
                    break Ok(false);
                }
                let count = session.pull_audio(&mut out);
                if count == 0 {
                    break Ok(false);
                }
                pace.produced(count);
                resampler
                    .as_mut()
                    .unwrap()
                    .push(&out[..count], &mut staged.pcm);
            };
            match queued {
                Ok(full) => {
                    let synthesizing = session.speech_ready();
                    pace.pass();
                    pace.update(synthesizing, full);
                    device.set_reserve(pace.reserve());
                    device.expect_more(synthesizing || !staged.is_empty());
                    if retry_output.is_some_and(|at| Instant::now() >= at) {
                        retry_output = None;
                        eprintln!("jimmy: Output device restored.");
                    }
                }
                Err(error) => {
                    playback = None;
                    staged.clear();
                    retry_output = Some(if retry_output.is_none() {
                        retry_playback(&error)
                    } else {
                        Instant::now() + OUTPUT_RETRY
                    });
                }
            }
        }
        if playback.is_none() && retry_output.is_some() {
            session.playback_failed();
        }
        if stopped {
            // The session has seen the stop. If it took it, the queue was
            // dropped above. If the stop was for an older turn, speech goes on.
            hold.set(false);
        }
        // Between replies, give a device that keeps running dry a longer buffer.
        if let Some(device) = &playback
            && storm.seen(device.stats().xruns)
            && !session.is_speaking()
            && staged.is_empty()
            && device.idle()
            && let Some(period) = device.longer_period()
            && let Ok(longer) = Playback::open_with(Some(period), &hold)
        {
            eprintln!("jimmy: The output device kept running dry. Using a {period} frame period.");
            longer.set_turn(playback_tag);
            resampler = Some(Resampler::new(OUTPUT_RATE, longer.rate));
            storm = Storm::new(0);
            output_period = Some(period);
            playback = Some(longer);
        }
        while let Some(event) = session.poll() {
            match &event {
                ServerEvent::Cue { id, .. } => {
                    if let Some(pcm) = cues.get(id) {
                        let Some(device) = playback.as_ref() else {
                            print_json(&ServerEvent::error(
                                Some(session.turn()),
                                "audio_output",
                                "Cannot play cue while the output device is unavailable",
                            ))?;
                            continue;
                        };
                        let mut cue_resampler = Resampler::new(OUTPUT_RATE, device.rate);
                        cue_resampler.push(pcm, &mut staged.pcm);
                        cue_resampler.finish(&mut staged.pcm);
                    } else if pipe {
                        print_json(&ServerEvent::error(
                            Some(session.turn()),
                            "unknown_cue",
                            format!("Unknown cue {id}"),
                        ))?;
                        continue;
                    }
                }
                ServerEvent::Final { transcript, .. } if !json && link.is_none() => {
                    println!("{}", transcript.text)
                }
                ServerEvent::Error { message, .. } if !json => eprintln!("{message}"),
                ServerEvent::SpeakEnd { turn, reason } if playback_log => {
                    if let Some(device) = &playback {
                        let stats = device.stats();
                        eprintln!(
                            "jimmy: playback turn={turn} end={reason:?} underruns={} stretched_ms={} \
                             device_xruns={} period={} rate={} speed={:.2} stall_ms={:.0} \
                             reserve_ms={:.0} capture_lost_ms={} capture_xruns={}",
                            stats.underruns,
                            stats.stretched * 1_000 / device.rate as usize,
                            stats.xruns,
                            stats.period,
                            device.rate,
                            pace.speed,
                            pace.stall * 1_000.0,
                            pace.reserve() * 1_000.0,
                            capture.lost() * 1_000 / capture.rate as usize,
                            capture.xruns(),
                        );
                    }
                }
                _ => {}
            }
            if json {
                print_json(&event)?;
            }
            if let Some(link) = &link {
                link.event(&event);
            }
        }
        if eof
            && !session.is_speaking()
            && staged.is_empty()
            && playback.as_ref().is_none_or(Playback::idle)
        {
            break;
        }
        // The microphone comes last. Hearing can mean a transcription, which
        // keeps this thread for a while, and a stop that arrived meanwhile
        // should be the first thing dealt with after it.
        let mut mic_full = false;
        if !eof {
            let frames = capture.read(&mut mic)?;
            mic_full = frames == mic.len();
            session.push_audio(&mic[..frames]);
        }
        if !mic_full {
            thread::sleep(POLL);
        }
    }
    Ok(())
}

fn stops_speech(event: &ClientEvent) -> bool {
    matches!(
        event,
        ClientEvent::SpeakStop { .. } | ClientEvent::Cancel { .. }
    )
}

fn retry_playback(error: &anyhow::Error) -> Instant {
    eprintln!(
        "jimmy: {error}
jimmy: Still listening and retrying playback."
    );
    Instant::now() + OUTPUT_RETRY
}

fn read_cues(dir: Option<&Path>) -> Result<HashMap<String, Vec<i16>>> {
    let mut cues = HashMap::new();
    if let Some(dir) = dir {
        for file in fs::read_dir(dir)? {
            let path = file?.path();
            if path.extension().is_some_and(|ext| ext == "wav")
                && let Some(id) = path.file_stem().and_then(|name| name.to_str())
            {
                ensure!(
                    cues.len() < 128 && path.metadata()?.len() <= 2_000_000,
                    "Cue cache exceeds limits"
                );
                cues.insert(
                    id.into(),
                    jimmy_audio::read_wav(File::open(&path)?, OUTPUT_RATE)?,
                );
            }
        }
    }
    Ok(cues)
}

pub fn say(tts: &mut impl Tts, text: &str, speech: &Speech) -> Result<()> {
    let mut device = Playback::open()?;
    let text = jimmy_core::speech_text(text);
    ensure!(!text.is_empty(), "Speech input contains no speakable text");
    let mut stream = tts.start(&text, speech).map_err(anyhow::Error::msg)?;
    let mut resampler = Resampler::new(OUTPUT_RATE, device.rate);
    let mut pcm = Vec::new();
    let mut pace = Pace::new();
    device.expect_more(true);
    loop {
        pcm.clear();
        match stream.next_frame().map_err(anyhow::Error::msg)? {
            AudioStep::Frame(frame) => {
                pace.produced(frame.len());
                resampler.push(&frame, &mut pcm)
            }
            AudioStep::Pending => {
                pace.update(true, false);
                device.set_reserve(pace.reserve());
                thread::sleep(POLL);
                continue;
            }
            AudioStep::Done => break,
        }
        send_to_device(&mut device, &pcm)?;
    }
    resampler.finish(&mut pcm);
    send_to_device(&mut device, &pcm)?;
    device.expect_more(false);
    while !device.idle() {
        device.check()?;
        thread::sleep(POLL);
    }
    device.check()
}

fn send_to_device(device: &mut Playback, mut pcm: &[i16]) -> Result<()> {
    while !pcm.is_empty() {
        let count = device.write(0, pcm)?;
        pcm = &pcm[count..];
        if !pcm.is_empty() {
            thread::sleep(POLL);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_grows_with_the_synthesis_shortfall_and_with_stalls() {
        let mut pace = Pace::new();
        assert!((pace.reserve() - 0.25).abs() < 1e-6);
        // Faster than speaking needs no more than the floor.
        pace.measured(1.6);
        assert!((pace.reserve() - 0.25).abs() < 1e-6);
        // 80% of speaking speed loses a second over a five second phrase.
        pace.speed = 0.8;
        assert!((pace.reserve() - 1.25).abs() < 1e-3);
        // A queue that filled up proves synthesis is ahead.
        pace.update(true, true);
        assert!((pace.reserve() - 0.25).abs() < 1e-6);

        // A pass that comes 400 ms late raises the reserve by that much at once.
        pace.passed = Instant::now() - Duration::from_millis(400);
        pace.pass();
        assert!((0.65..0.7).contains(&pace.reserve()), "{}", pace.reserve());
        // Prompt passes let it fade, slowly.
        for _ in 0..3 {
            pace.passed = Instant::now() - Duration::from_millis(5);
            pace.pass();
        }
        assert!((0.64..0.7).contains(&pace.reserve()), "{}", pace.reserve());
    }

    #[test]
    fn a_device_that_underruns_every_cycle_is_told_apart_from_a_busy_moment() {
        let second = |storm: &mut Storm, xruns| {
            storm.since = Instant::now() - Duration::from_secs(1);
            storm.seen(xruns)
        };
        let mut storm = Storm::new(0);
        assert!(!storm.seen(100), "less than a second has passed");
        // A burst in one second, then quiet.
        assert!(!second(&mut storm, 9));
        assert!(!second(&mut storm, 10));
        // Six a second, as a sink that takes 8192 frames from a shorter buffer.
        assert!(!second(&mut storm, 16));
        assert!(second(&mut storm, 22));
    }

    #[test]
    fn synthesis_speed_is_measured_over_at_least_a_second() {
        let mut pace = Pace::new();
        pace.produced(1_920);
        assert_eq!(pace.samples, 0, "the first frame only starts the clock");
        pace.produced(12_000);
        pace.update(true, false);
        assert!((pace.speed - 1.0).abs() < 1e-6, "too early to judge");
        // Half a second of speech in a little over a second.
        pace.since = Some(Instant::now() - Duration::from_millis(1_000));
        pace.update(true, false);
        assert!((0.70..0.76).contains(&pace.speed), "{}", pace.speed);
        assert_eq!(pace.samples, 0);
        // Between replies the clock stops.
        pace.update(false, false);
        assert!(pace.since.is_none());
    }
}
