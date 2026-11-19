use crate::print_json;
use anyhow::{Result, ensure};
use jimmy_audio::{Capture, Playback};
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
    let mut playback = match tts.is_some().then(Playback::open).transpose() {
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
    let mut resampled = Vec::new();
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
                        Ok(event) => Input::Event(event),
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
    let mut pending: &[i16] = &[];
    let mut playback_tag = 0u32;
    let mut playback_key = (session.turn(), session.playback_revision());
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..32 {
            match rx.try_recv() {
                Ok(Input::Event(mut event)) => {
                    if let ClientEvent::Configure(config) = &mut event {
                        config.input_rate = Some(capture.rate);
                        config.echo_cancelled = Some(false);
                    }
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
        if !eof {
            let frames = capture.read(&mut mic)?;
            session.push_audio(&mic[..frames]);
        }
        if playback.is_none() && !eof && retry_output.is_some_and(|at| Instant::now() >= at) {
            if let Ok(device) = Playback::open() {
                device.set_turn(playback_tag);
                resampler = Some(Resampler::new(OUTPUT_RATE, device.rate));
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
                pending = &[];
                resampler = Some(Resampler::new(OUTPUT_RATE, device.rate));
            }
            if pending.is_empty() && device.available() >= device.rate as usize / 110 {
                let count = session.pull_audio(&mut out);
                resampled.clear();
                resampler
                    .as_mut()
                    .unwrap()
                    .push(&out[..count], &mut resampled);
                pending = &resampled;
            }
            match device.write(playback_tag, pending) {
                Ok(count) => {
                    pending = &pending[count..];
                    if retry_output.is_some_and(|at| Instant::now() >= at) {
                        retry_output = None;
                        eprintln!("jimmy: Output device restored.");
                    }
                }
                Err(error) => {
                    playback = None;
                    pending = &[];
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
                        resampled.clear();
                        let mut cue_resampler = Resampler::new(OUTPUT_RATE, device.rate);
                        cue_resampler.push(pcm, &mut resampled);
                        cue_resampler.finish(&mut resampled);
                        pending = &resampled;
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
            && pending.is_empty()
            && playback.as_ref().is_none_or(Playback::idle)
        {
            break;
        }
        thread::sleep(POLL);
    }
    Ok(())
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
    loop {
        pcm.clear();
        match stream.next_frame().map_err(anyhow::Error::msg)? {
            AudioStep::Frame(frame) => resampler.push(&frame, &mut pcm),
            AudioStep::Pending => {
                thread::sleep(POLL);
                continue;
            }
            AudioStep::Done => break,
        }
        send_to_device(&mut device, &pcm)?;
    }
    resampler.finish(&mut pcm);
    send_to_device(&mut device, &pcm)?;
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
