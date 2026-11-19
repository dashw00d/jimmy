use jimmy_core::{
    AudioStep, AudioStream, Recognition, Resampler, Session, Settings, Speech, Stt, Tts, Vad,
    speech_text,
};
use jimmy_proto::{ClientEvent, Configure, EndReason, Mode, ServerEvent, Transcript, Word};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

struct Detector;
impl Vad for Detector {
    fn speech(&mut self, frame: &[i16]) -> bool {
        frame[0] != 0
    }
}

#[derive(Default)]
struct Recognizer {
    replies: VecDeque<Transcript>,
    lengths: Rc<RefCell<Vec<usize>>>,
}
impl Stt for Recognizer {
    fn transcribe(&mut self, audio: &[i16], _: &Recognition) -> Result<Transcript, String> {
        self.lengths.borrow_mut().push(audio.len());
        Ok(self.replies.pop_front().unwrap_or_default())
    }
}

#[derive(Default)]
struct Synth(Rc<RefCell<Vec<String>>>);
struct Frames(usize);
impl AudioStream for Frames {
    fn next_frame(&mut self) -> Result<AudioStep, String> {
        if self.0 == 0 {
            return Ok(AudioStep::Done);
        }
        self.0 -= 1;
        Ok(AudioStep::Frame(vec![12; 960]))
    }
}
impl Tts for Synth {
    fn start(&mut self, text: &str, _: &Speech) -> Result<Box<dyn AudioStream>, String> {
        self.0.borrow_mut().push(text.to_owned());
        Ok(Box::new(Frames(3)))
    }
}

fn transcript(text: &str) -> Transcript {
    let words = text
        .split_whitespace()
        .enumerate()
        .map(|(i, word)| Word {
            text: word.to_owned(),
            start: i as f32 / 4.0,
            end: (i + 1) as f32 / 4.0,
            probability: 0.95,
        })
        .collect();
    Transcript {
        text: text.into(),
        language: "en".into(),
        words,
    }
}

fn events(session: &mut Session<Recognizer, Synth, Detector>) -> Vec<ServerEvent> {
    std::iter::from_fn(|| session.poll()).collect()
}

fn speak(session: &mut Session<Recognizer, Synth, Detector>, text: &str, turn: Option<u32>) {
    session.push_event(ClientEvent::SpeakAppend {
        text: text.into(),
        turn,
    });
    session.push_event(ClientEvent::SpeakFlush { turn });
}

#[test]
fn confirmed_prefix_survives_revisions_and_final() {
    let stt = Recognizer {
        replies: [
            "turn on light",
            "turn on lights",
            "turn off lights",
            "turn off lights",
        ]
        .into_iter()
        .map(transcript)
        .collect(),
        ..Recognizer::default()
    };
    let settings = Settings {
        mode: Mode::Open,
        endpoint_ms: 100,
        ..Settings::default()
    };
    let mut session = Session::with_vad(stt, Synth::default(), Detector, settings);
    session.push_audio(&[1; 16_000]);
    session.push_audio(&[0; 1_600]);
    let all = events(&mut session);
    let partials: Vec<_> = all
        .iter()
        .filter_map(|event| match event {
            ServerEvent::Partial { committed, .. } => Some(committed.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(partials.len(), 3);
    assert_eq!(partials[1], "turn on");
    assert!(partials.windows(2).all(|pair| pair[1].starts_with(pair[0])));
    assert!(matches!(&all[0], ServerEvent::Ready { protocol: 1, .. }));
    assert!(all.iter().any(|event| matches!(event, ServerEvent::Final { transcript, .. } if transcript.text == "turn on lights")));
}

#[test]
fn wake_trims_phrase_and_preserves_timing_offset() {
    let stt = Recognizer {
        replies: [
            "Hay Jimmie",
            "Hey Jimmy weather tomorrow",
            "Hey Jimmy weather tomorrow",
            "Hey Jimmy weather tomorrow",
        ]
        .into_iter()
        .map(transcript)
        .collect(),
        ..Recognizer::default()
    };
    let lengths = stt.lengths.clone();
    let mut session = Session::with_vad(stt, Synth::default(), Detector, Settings::default());
    session.push_audio(&[0; 4_800]);
    session.push_audio(&[1; 22_400]);
    session.push_audio(&[0; 9_600]);
    let all = events(&mut session);
    assert!(matches!(&all[1], ServerEvent::Wake { turn: 1, .. }));
    let final_text = all
        .iter()
        .find_map(|event| match event {
            ServerEvent::Final { transcript, .. } => Some(transcript),
            _ => None,
        })
        .unwrap();
    assert_eq!(final_text.text, "weather tomorrow");
    assert_eq!(final_text.words[0].start, 0.5);
    assert_eq!(lengths.borrow()[0], 17_600);
}

#[test]
fn standalone_wake_waits_for_the_pause_then_rotates_replies() {
    let stt = Recognizer {
        replies: std::iter::repeat_n(transcript("Hey Jimmy."), 8).collect(),
        ..Recognizer::default()
    };
    let synth = Synth::default();
    let calls = synth.0.clone();
    let settings = Settings {
        followup_ms: 0,
        ..Settings::default()
    };
    let mut session = Session::with_vad(stt, synth, Detector, settings);
    for expected in ["What's up?", "How can I help?"] {
        session.push_audio(&[1; 12_800]);
        session.push_audio(&[0; 9_280]);
        assert_eq!(session.pull_audio(&mut [0; 480]), 0);
        session.push_audio(&[0; 320]);
        for _ in 0..12 {
            session.pull_audio(&mut [0; 480]);
        }
        assert_eq!(calls.borrow().last().unwrap(), expected);
        let all = events(&mut session);
        assert!(
            all.iter()
                .any(|event| matches!(event, ServerEvent::Wake { .. }))
        );
        assert!(all.iter().any(|event| matches!(
            event,
            ServerEvent::SpeakEnd {
                reason: EndReason::Done,
                ..
            }
        )));
        assert!(!all.iter().any(|event| matches!(
            event,
            ServerEvent::Final { .. } | ServerEvent::Partial { .. }
        )));
    }
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn wake_reply_can_be_disabled_by_the_client() {
    let stt = Recognizer {
        replies: std::iter::repeat_n(transcript("Hey Jimmy."), 4).collect(),
        ..Recognizer::default()
    };
    let mut session = Session::with_vad(stt, Synth::default(), Detector, Settings::default());
    session.push_event(ClientEvent::Configure(Configure {
        wake_reply: Some(false),
        ..Configure::default()
    }));
    session.push_audio(&[1; 12_800]);
    session.push_audio(&[0; 9_600]);
    assert_eq!(session.pull_audio(&mut [0; 480]), 0);
    assert!(events(&mut session).iter().any(|event| matches!(
        event, ServerEvent::Final { transcript, .. } if transcript.text.is_empty()
    )));
}

#[test]
fn failed_wake_spends_five_passes_until_silence() {
    let stt = Recognizer::default();
    let lengths = stt.lengths.clone();
    let mut session = Session::with_vad(stt, Synth::default(), Detector, Settings::default());
    session.push_audio(&[1; 16_000 * 5]);
    assert_eq!(lengths.borrow().len(), 5);
    assert_eq!(events(&mut session).len(), 1);
    session.push_audio(&[0; 8_000]);
    session.push_audio(&[1; 12_800]);
    assert_eq!(lengths.borrow().len(), 6);
}

#[test]
fn cancel_drops_remainder_and_rejects_stale_append() {
    let synth = Synth::default();
    let calls = synth.0.clone();
    let mut session =
        Session::with_vad(Recognizer::default(), synth, Detector, Settings::default());
    speak(&mut session, "first", None);
    let mut output = [0; 480];
    assert_eq!(session.pull_audio(&mut output), 480);
    let old = session.turn();
    session.push_event(ClientEvent::Cancel { turn: Some(old) });
    assert_eq!(session.turn(), old + 1);
    speak(&mut session, "stale", Some(old));
    assert_eq!(session.pull_audio(&mut output), 0);
    speak(&mut session, "next", None);
    assert_eq!(session.pull_audio(&mut output), 480);
    assert_eq!(session.turn(), old + 1);
    assert_eq!(&*calls.borrow(), &["first", "next"]);
    assert!(events(&mut session).iter().any(|event| matches!(event, ServerEvent::SpeakEnd { turn, reason: EndReason::Cancelled } if *turn == old)));
}

#[test]
fn stop_cuts_the_filler_and_accepts_streamed_speech_on_the_same_turn() {
    let synth = Synth::default();
    let calls = synth.0.clone();
    let mut session =
        Session::with_vad(Recognizer::default(), synth, Detector, Settings::default());
    session.push_event(ClientEvent::ListenStart);
    session.push_event(ClientEvent::ListenStop);
    let turn = session.turn();
    session.push_event(ClientEvent::SpeakAppend {
        text: "Hmm. ".into(),
        turn: Some(turn),
    });
    let mut output = [0; 480];
    assert!(session.pull_audio(&mut output) > 0);
    let revision = session.playback_revision();

    session.push_event(ClientEvent::SpeakStop { turn: Some(turn) });
    assert_eq!(session.turn(), turn);
    assert_ne!(session.playback_revision(), revision);
    assert_eq!(session.pull_audio(&mut output), 0);
    session.push_event(ClientEvent::SpeakAppend {
        text: "You picked ".into(),
        turn: Some(turn),
    });
    session.push_event(ClientEvent::SpeakAppend {
        text: "a grilled cheese sandwich.".into(),
        turn: Some(turn),
    });
    assert!(
        session.pull_audio(&mut output) > 0,
        "a sentence can play before flush"
    );
    assert_eq!(
        &*calls.borrow(),
        &["Hmm.", "You picked a grilled cheese sandwich."]
    );
    assert!(events(&mut session).iter().any(|event| matches!(
        event, ServerEvent::SpeakEnd { turn: id, reason: EndReason::Cancelled } if *id == turn
    )));
}

#[test]
fn playback_failure_retires_audio_without_closing_the_microphone_turn() {
    let stt = Recognizer {
        replies: [transcript("keep listening")].into_iter().collect(),
        ..Recognizer::default()
    };
    let mut session = Session::with_vad(stt, Synth::default(), Detector, Settings::default());
    session.push_event(ClientEvent::ListenStart);
    let turn = session.turn();
    speak(&mut session, "interrupted reply", Some(turn));
    assert!(session.pull_audio(&mut [0; 480]) > 0);

    session.playback_failed();
    session.playback_failed();
    assert_eq!(session.turn(), turn);
    assert_eq!(session.pull_audio(&mut [0; 480]), 0);
    session.push_audio(&[1; 640]);
    session.push_event(ClientEvent::ListenStop);

    let all = events(&mut session);
    assert_eq!(
        all.iter()
            .filter(|event| matches!(
                event,
                ServerEvent::SpeakEnd {
                    reason: EndReason::Error,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(all.iter().any(
        |event| matches!(event, ServerEvent::Final { turn: heard, transcript }
        if *heard == turn && transcript.text == "keep listening")
    ));
    speak(&mut session, "new reply", Some(turn));
    assert!(session.pull_audio(&mut [0; 480]) > 0);
}

#[test]
fn playback_failure_rejects_the_old_reply_and_opens_a_fresh_input_turn() {
    let stt = Recognizer {
        replies: [transcript("first question")].into_iter().collect(),
        ..Recognizer::default()
    };
    let settings = Settings {
        mode: Mode::Open,
        endpoint_ms: 100,
        ..Settings::default()
    };
    let mut session = Session::with_vad(stt, Synth::default(), Detector, settings);
    session.push_audio(&[1; 640]);
    let old_turn = session.turn();
    session.push_audio(&[0; 1_600]);
    speak(&mut session, "first reply", Some(old_turn));
    assert!(session.pull_audio(&mut [0; 480]) > 0);
    session.playback_failed();

    speak(&mut session, "stale reply", Some(old_turn));
    assert_eq!(session.pull_audio(&mut [0; 480]), 0);
    session.push_audio(&[1; 320]);
    assert_ne!(session.turn(), old_turn);
}

#[test]
fn barge_in_requires_echo_cancellation_or_explicit_ptt() {
    let settings = Settings {
        mode: Mode::Open,
        ..Settings::default()
    };
    let mut session =
        Session::with_vad(Recognizer::default(), Synth::default(), Detector, settings);
    speak(&mut session, "testing", None);
    assert!(session.pull_audio(&mut [0; 480]) > 0);
    session.push_audio(&[1; 640]);
    assert_eq!(session.turn(), 1);
    session.push_event(ClientEvent::ListenStart);
    assert_eq!(session.turn(), 2);
    session.push_event(ClientEvent::ListenStop);
    session.push_event(ClientEvent::Configure(Configure {
        echo_cancelled: Some(true),
        ..Configure::default()
    }));
    speak(&mut session, "again", Some(2));
    assert!(session.pull_audio(&mut [0; 480]) > 0);
    session.push_audio(&[1; 320]);
    assert_eq!(session.turn(), 3);
}

#[test]
fn followup_after_speech_opens_without_wake_phrase() {
    let mut session = Session::with_vad(
        Recognizer::default(),
        Synth::default(),
        Detector,
        Settings::default(),
    );
    speak(&mut session, "reply", None);
    for _ in 0..12 {
        session.pull_audio(&mut [0; 480]);
    }
    assert!(!session.is_speaking());
    session.push_audio(&[1; 640]);
    let all = events(&mut session);
    assert!(
        all.iter()
            .any(|event| matches!(event, ServerEvent::SpeechStart { turn: 2 }))
    );
    assert!(
        !all.iter()
            .any(|event| matches!(event, ServerEvent::Wake { .. }))
    );
}

#[test]
fn markdown_is_filtered_across_chunks_and_cues_follow_speech() {
    let synth = Synth::default();
    let calls = synth.0.clone();
    let mut session =
        Session::with_vad(Recognizer::default(), synth, Detector, Settings::default());
    for text in [
        "## Heading\nHe",
        "llo **there**. `",
        "``rust\n",
        "fn hidden() {}\n``",
        "` [Useful](https://example.com)",
    ] {
        session.push_event(ClientEvent::SpeakAppend {
            text: text.into(),
            turn: None,
        });
    }
    session.push_event(ClientEvent::CuePlay {
        id: "chime".into(),
        turn: None,
    });
    for _ in 0..32 {
        session.pull_audio(&mut [0; 480]);
    }
    let spoken = calls.borrow().join(" ");
    assert_eq!(spoken, "Heading Hello there. Useful");
    assert_eq!(
        speech_text("Hi! ```hidden``` [A link](https://example.com)"),
        "Hi!   A link"
    );
    let all = events(&mut session);
    let end = all
        .iter()
        .position(|event| {
            matches!(
                event,
                ServerEvent::SpeakEnd {
                    reason: EndReason::Done,
                    ..
                }
            )
        })
        .unwrap();
    let cue = all
        .iter()
        .position(|event| matches!(event, ServerEvent::Cue { id, .. } if id == "chime"))
        .unwrap();
    assert!(end < cue);
}

#[test]
fn resampling_is_invariant_to_input_chunks() {
    let audio: Vec<i16> = (0..1_003).map(|n| (n * 13) as i16).collect();
    let mut contiguous = Vec::new();
    let mut resampler = Resampler::new(44_100, 16_000);
    resampler.push(&audio, &mut contiguous);
    resampler.finish(&mut contiguous);
    let mut split = Vec::new();
    for chunk in audio.chunks(19) {
        resampler.push(chunk, &mut split);
    }
    resampler.finish(&mut split);
    assert_eq!(split.len(), 364);
    assert_eq!(split, contiguous);
}
