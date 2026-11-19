//! Replays Phonon speech through the session the way `jimmy pipe` drives it:
//! Whistle and Phonon loaded, 48 kHz capture chunks, and Plexa's configure.
//! No audio device is opened.
//!
//! Run with `cargo test -p jimmy --test pipe_wake -- --ignored`. Set
//! JIMMY_MODEL_DIR to use models outside the default cache.
use jimmy_core::{AudioStep, Resampler, Session, Settings, Speech, Tts};
use jimmy_phonon::Phonon;
use jimmy_proto::{ClientEvent, Configure, Mode, OUTPUT_RATE, ServerEvent};
use jimmy_whistle::Whistle;
use std::path::PathBuf;

const CAPTURE_RATE: u32 = 48_000;
const CAPTURE_CHUNK: usize = 1_920;

fn model_dir() -> PathBuf {
    std::env::var_os("JIMMY_MODEL_DIR").map_or_else(
        || {
            directories::ProjectDirs::from("", "", "jimmy")
                .expect("user cache")
                .cache_dir()
                .join("models")
        },
        PathBuf::from,
    )
}

fn speak(tts: &mut Phonon, text: &str) -> Vec<i16> {
    let options = Speech {
        voice: None,
        rate: 1.0,
    };
    let mut stream = tts.start(text, &options).expect("start synthesis");
    let mut pcm = Vec::new();
    loop {
        match stream.next_frame().expect("synthesis") {
            AudioStep::Frame(frame) => pcm.extend(frame),
            AudioStep::Pending => std::thread::sleep(std::time::Duration::from_millis(2)),
            AudioStep::Done => break,
        }
    }
    let mut resampler = Resampler::new(OUTPUT_RATE, CAPTURE_RATE);
    let mut out = Vec::new();
    resampler.push(&pcm, &mut out);
    resampler.finish(&mut out);
    out
}

fn quiet(ms: usize) -> Vec<i16> {
    vec![0; ms * CAPTURE_RATE as usize / 1_000]
}

fn words(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_alphanumeric() || ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[ignore = "needs the Whistle and Phonon models from `jimmy pull`"]
fn spoken_commands_survive_a_pause_after_the_wake_phrase() {
    let dir = model_dir();
    let stt = Whistle::load(&dir.join(jimmy_whistle::MODEL)).expect("load Whistle");
    let mut tts = Phonon::load(&dir.join("phonon"), None, 1).expect("load Phonon");
    let wake = speak(&mut tts, "Hey Jimmy.");
    let command = speak(&mut tts, "What's going on?");
    let same_breath = speak(&mut tts, "What's going on? Hey Jimmy, what's going on?");

    // Phonon ends each clip with about 0.8 s of silence, so every "Hey Jimmy."
    // below is followed by a pause longer than the 600 ms endpoint.
    let mut audio = quiet(1_000);
    for _ in 0..2 {
        audio.extend(&wake);
        audio.extend(quiet(300));
        audio.extend(&command);
        audio.extend(quiet(1_500));
    }
    audio.extend(&same_breath);
    audio.extend(quiet(2_000));

    let settings = Settings {
        input_rate: CAPTURE_RATE,
        ..Settings::default()
    };
    let mut session = Session::new(stt, tts, settings);
    // What Plexa sends, with the fields `local::run` overrides in pipe mode.
    session.push_event(ClientEvent::Configure(Configure {
        mode: Some(Mode::Wake),
        wake: Some("hey jimmy".into()),
        wake_reply: Some(false),
        input_rate: Some(CAPTURE_RATE),
        echo_cancelled: Some(false),
        ..Configure::default()
    }));
    let mut out = [0; 480];
    let mut all = Vec::new();
    for chunk in audio.chunks(CAPTURE_CHUNK) {
        session.push_audio(chunk);
        while session.pull_audio(&mut out) > 0 {}
        all.extend(std::iter::from_fn(|| session.poll()));
    }

    let wakes: Vec<_> = all
        .iter()
        .filter_map(|event| match event {
            ServerEvent::Wake { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    let finals: Vec<_> = all
        .iter()
        .filter_map(|event| match event {
            ServerEvent::Final { turn, transcript } => Some((*turn, words(&transcript.text))),
            _ => None,
        })
        .collect();
    let log = all
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(wakes, [1, 2, 3], "{log}");
    assert_eq!(
        finals,
        [1, 2, 3].map(|turn| (turn, "whats going on".to_owned())),
        "{log}"
    );
}
