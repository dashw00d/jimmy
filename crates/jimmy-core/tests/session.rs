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

fn events<S: Stt>(session: &mut Session<S, Synth, Detector>) -> Vec<ServerEvent> {
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
    // With no reply to prompt them, the person gets time to say the command.
    assert!(
        !events(&mut session)
            .iter()
            .any(|event| matches!(event, ServerEvent::Final { .. }))
    );
    session.push_audio(&[0; 38_400]);
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

/// Hears words from the audio itself: each run of sample value `n` is word `n`.
struct Spoken;
const VOCABULARY: [&str; 6] = ["", "Hey", "Jimmy,", "what's", "going", "on?"];
impl Stt for Spoken {
    fn transcribe(&mut self, audio: &[i16], _: &Recognition) -> Result<Transcript, String> {
        let mut words: Vec<Word> = Vec::new();
        let mut previous = 0;
        for (index, &sample) in audio.iter().enumerate() {
            let time = index as f32 / 16_000.0;
            if sample != previous && sample > 0 {
                words.push(Word {
                    text: VOCABULARY[sample as usize].into(),
                    start: time,
                    end: time,
                    probability: 0.95,
                });
            }
            if sample > 0 {
                words.last_mut().unwrap().end = time;
            }
            previous = sample;
        }
        let text = words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Transcript {
            text,
            language: "en".into(),
            words,
        })
    }
}

/// 300 ms of audio per word, so "Hey Jimmy," lasts 600 ms.
fn say(words: &[i16]) -> Vec<i16> {
    words.iter().flat_map(|&word| [word; 4_800]).collect()
}

fn quiet(ms: usize) -> Vec<i16> {
    vec![0; ms * 16]
}

const HEY_JIMMY: &[i16] = &[1, 2];
const WHATS_GOING_ON: &[i16] = &[3, 4, 5];
const COMMAND: &str = "what's going on?";

fn silent_wake_session() -> Session<Spoken, Synth, Detector> {
    let mut session = Session::with_vad(Spoken, Synth::default(), Detector, Settings::default());
    // What Plexa sends to `jimmy pipe`.
    session.push_event(ClientEvent::Configure(Configure {
        mode: Some(Mode::Wake),
        wake: Some("hey jimmy".into()),
        wake_reply: Some(false),
        echo_cancelled: Some(false),
        ..Configure::default()
    }));
    session.push_audio(&quiet(300));
    session
}

fn wakes_and_finals(all: &[ServerEvent]) -> (Vec<u32>, Vec<(u32, String)>) {
    let wakes = all
        .iter()
        .filter_map(|event| match event {
            ServerEvent::Wake { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    let finals = all
        .iter()
        .filter_map(|event| match event {
            ServerEvent::Final { turn, transcript } => Some((*turn, transcript.text.clone())),
            _ => None,
        })
        .collect();
    (wakes, finals)
}

#[test]
fn silent_wake_keeps_the_turn_open_through_a_pause_before_the_command() {
    let mut session = silent_wake_session();
    session.push_audio(&say(HEY_JIMMY));
    session.push_audio(&quiet(1_000));
    session.push_audio(&say(WHATS_GOING_ON));
    session.push_audio(&quiet(1_000));
    let all = events(&mut session);
    let (wakes, finals) = wakes_and_finals(&all);
    assert_eq!(wakes, [1]);
    assert_eq!(finals, [(1, COMMAND.to_owned())], "events: {all:?}");
    let ends = all
        .iter()
        .filter(|event| matches!(event, ServerEvent::SpeechEnd { turn: 1 }))
        .count();
    assert_eq!(
        ends, 1,
        "one speech segment carries the wake and the command"
    );
}

#[test]
fn silent_wake_without_a_command_finalizes_empty_then_hears_the_next_wake() {
    let mut session = silent_wake_session();
    session.push_audio(&say(HEY_JIMMY));
    session.push_audio(&quiet(3_200));
    let (wakes, finals) = wakes_and_finals(&events(&mut session));
    assert_eq!(wakes, [1]);
    assert_eq!(
        finals,
        [(1, String::new())],
        "a bare wake still ends its turn"
    );

    // Nothing from the client: the next wake phrase opens a new turn.
    let mut next = say(HEY_JIMMY);
    next.extend(say(WHATS_GOING_ON));
    session.push_audio(&next);
    session.push_audio(&quiet(1_000));
    let (wakes, finals) = wakes_and_finals(&events(&mut session));
    assert_eq!(wakes, [2]);
    assert_eq!(finals, [(2, COMMAND.to_owned())]);
}

#[test]
fn wake_phrase_is_heard_after_other_speech_in_the_same_breath() {
    let mut session = silent_wake_session();
    let mut audio = say(WHATS_GOING_ON);
    audio.extend(quiet(200));
    audio.extend(say(HEY_JIMMY));
    audio.extend(say(WHATS_GOING_ON));
    session.push_audio(&audio);
    session.push_audio(&quiet(1_000));
    let all = events(&mut session);
    let (wakes, finals) = wakes_and_finals(&all);
    assert_eq!(wakes, [1], "events: {all:?}");
    assert_eq!(finals, [(1, COMMAND.to_owned())]);
}

#[test]
fn pausing_after_each_wake_still_delivers_each_command() {
    // The Plexa trace: "hey jimmy", a pause, "what's going on", twice.
    let mut session = silent_wake_session();
    for _ in 0..2 {
        session.push_audio(&say(HEY_JIMMY));
        session.push_audio(&quiet(800));
        session.push_audio(&say(WHATS_GOING_ON));
        session.push_audio(&quiet(1_000));
    }
    let all = events(&mut session);
    let (wakes, finals) = wakes_and_finals(&all);
    assert_eq!(wakes, [1, 2], "events: {all:?}");
    assert_eq!(finals, [(1, COMMAND.to_owned()), (2, COMMAND.to_owned())]);
}
