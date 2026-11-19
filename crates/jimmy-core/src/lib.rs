//! Synchronous voice turn state machine, shared by native and browser hosts.
mod resample;
mod text;

pub use resample::Resampler;
pub use text::speech_text;

use jimmy_proto::{
    ClientEvent, Codec, Configure, EndReason, INPUT_RATE, Mode, OUTPUT_RATE, PROTOCOL, ServerEvent,
    Transcript, Turn, Word,
};
use std::collections::VecDeque;
use text::Chunker;

const FRAME: usize = 320;
const PRE_ROLL: usize = 4_800;
const MAX_UTTERANCE: usize = 25 * INPUT_RATE as usize;
const MAX_QUEUED_TEXT: usize = 64 * 1024;
const WAKE_REPLIES: &[&str] = &["What's up?", "How can I help?", "Go ahead.", "I'm here."];

#[derive(Debug, Default, Clone)]
pub struct Recognition {
    pub language: Option<String>,
    pub keywords: Vec<String>,
}

pub trait Stt {
    fn transcribe(&mut self, audio: &[i16], options: &Recognition) -> Result<Transcript, String>;

    /// Busy shared runtimes may skip a speculative pass. Finals still call transcribe.
    fn try_partial(
        &mut self,
        audio: &[i16],
        options: &Recognition,
    ) -> Result<Option<Transcript>, String> {
        self.transcribe(audio, options).map(Some)
    }
}

#[derive(Debug, Clone)]
pub struct Speech {
    pub voice: Option<String>,
    pub rate: f32,
}

pub enum AudioStep {
    Pending,
    Frame(Vec<i16>),
    Done,
}

pub trait AudioStream {
    fn next_frame(&mut self) -> Result<AudioStep, String>;
}

pub trait Tts {
    fn available(&self) -> bool {
        true
    }

    fn start(&mut self, text: &str, options: &Speech) -> Result<Box<dyn AudioStream>, String>;
    fn voices(&self) -> Vec<String> {
        Vec::new()
    }
}

pub trait Vad {
    fn speech(&mut self, frame: &[i16]) -> bool;
}

#[derive(Default)]
pub struct EnergyVad {
    noise: f32,
    onset: u8,
    hangover: u8,
}

impl Vad for EnergyVad {
    fn speech(&mut self, frame: &[i16]) -> bool {
        if frame.is_empty() {
            return false;
        }
        let (sum, energy) = frame.iter().fold((0.0, 0.0), |(sum, energy), &sample| {
            let sample = f32::from(sample);
            (sum + sample, energy + sample * sample)
        });
        let mean = sum / frame.len() as f32;
        let rms = (energy / frame.len() as f32 - mean * mean).max(0.0).sqrt();
        let voiced = rms > (self.noise * 3.5).max(100.0);
        if voiced {
            self.onset = (self.onset + 1).min(2);
            if self.onset >= 2 {
                self.hangover = 3;
            }
        } else {
            self.noise = (self.noise * 0.98 + rms * 0.02).min(350.0);
            self.onset = 0;
            self.hangover = self.hangover.saturating_sub(1);
        }
        self.hangover != 0
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: Mode,
    pub wake: String,
    pub wake_reply: bool,
    pub endpoint_ms: u32,
    pub followup_ms: u32,
    pub partial_ms: u32,
    pub input_rate: u32,
    pub output_codec: Codec,
    pub opus_supported: bool,
    pub echo_cancelled: bool,
    pub recognition: Recognition,
    pub voice: Option<String>,
    pub rate: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Wake,
            wake: "hey jimmy".into(),
            wake_reply: true,
            endpoint_ms: 600,
            followup_ms: 8_000,
            partial_ms: 300,
            input_rate: INPUT_RATE,
            output_codec: Codec::Pcm16,
            opus_supported: false,
            echo_cancelled: false,
            recognition: Recognition::default(),
            voice: None,
            rate: 1.0,
        }
    }
}

struct Utterance {
    audio: Vec<i16>,
    elapsed: usize,
    silence: usize,
    next_pass: usize,
    wake_required: bool,
    wake_match: Option<(String, f32)>,
    agreement: Agreement,
}

impl Utterance {
    fn new(audio: Vec<i16>, wake_required: bool, partial_ms: u32) -> Self {
        Self {
            audio,
            elapsed: 0,
            silence: 0,
            next_pass: samples(if wake_required { 800 } else { partial_ms }),
            wake_required,
            wake_match: None,
            agreement: Agreement::default(),
        }
    }

    fn command_words(&self, transcript: &Transcript) -> Vec<Word> {
        let mut words = word_list(transcript);
        if let Some((phrase, end)) = &self.wake_match {
            let count = wake_prefix(phrase, &words).map_or_else(
                || {
                    words
                        .iter()
                        .take_while(|word| (word.start + word.end) * 0.5 < *end)
                        .count()
                },
                |matched| matched.count,
            );
            words.drain(..count);
        }
        words
    }
}

#[derive(Default)]
struct Agreement {
    committed: Vec<Word>,
    previous: Vec<Word>,
}

impl Agreement {
    fn tail(&self, mut words: Vec<Word>) -> Vec<Word> {
        let same_prefix = words.len() >= self.committed.len()
            && words
                .iter()
                .zip(&self.committed)
                .all(|(a, b)| normalized(&a.text) == normalized(&b.text));
        let skipped = if same_prefix {
            self.committed.len()
        } else {
            let end = self.committed.last().map_or(0.0, |word| word.end);
            words
                .iter()
                .take_while(|word| (word.start + word.end) * 0.5 < end)
                .count()
        };
        words.drain(..skipped);
        words
    }

    fn update(&mut self, words: Vec<Word>) -> String {
        let mut tail = self.tail(words);
        let stable = tail
            .iter()
            .zip(&self.previous)
            .take_while(|(a, b)| normalized(&a.text) == normalized(&b.text))
            .count();
        self.committed.extend(tail.drain(..stable));
        self.previous = tail;
        words_text(&self.previous)
    }

    fn finish(self, words: Vec<Word>) -> Vec<Word> {
        let tail = self.tail(words);
        self.committed.into_iter().chain(tail).collect()
    }
}

enum SpeakItem {
    Text(String, Speech),
    Cue(String),
}

pub struct Session<S, T, V = EnergyVad> {
    stt: S,
    tts: T,
    vad: V,
    settings: Settings,
    events: VecDeque<ServerEvent>,
    input: Resampler,
    frame: Vec<i16>,
    pre_roll: VecDeque<i16>,
    utterance: Option<Utterance>,
    wait_silence: Option<usize>,
    ptt: bool,
    turn: Turn,
    open: bool,
    reserved_turn: bool,
    playback_revision: u32,
    followup: usize,
    next_wake_reply: usize,
    chunker: Chunker,
    queued_text: usize,
    speak: VecDeque<SpeakItem>,
    stream: Option<Box<dyn AudioStream>>,
    output: VecDeque<i16>,
    speaking: bool,
    flush: bool,
}

impl<S: Stt, T: Tts> Session<S, T> {
    pub fn new(stt: S, tts: T, settings: Settings) -> Self {
        Self::with_vad(stt, tts, EnergyVad::default(), settings)
    }
}

impl<S: Stt, T: Tts, V: Vad> Session<S, T, V> {
    pub fn with_vad(stt: S, tts: T, vad: V, settings: Settings) -> Self {
        let mut models = vec!["whistle".into()];
        if tts.available() {
            models.push("phonon-31m-en".into());
        }
        let ready = ServerEvent::Ready {
            turn: 0,
            protocol: PROTOCOL,
            models,
            voices: tts.voices(),
            sample_rates: [INPUT_RATE, OUTPUT_RATE],
        };
        let input = Resampler::new(settings.input_rate, INPUT_RATE);
        Self {
            stt,
            tts,
            vad,
            settings,
            events: VecDeque::from([ready]),
            input,
            frame: Vec::new(),
            pre_roll: VecDeque::with_capacity(PRE_ROLL),
            utterance: None,
            wait_silence: None,
            ptt: false,
            turn: 0,
            open: false,
            reserved_turn: false,
            playback_revision: 0,
            followup: 0,
            next_wake_reply: 0,
            chunker: Chunker::default(),
            queued_text: 0,
            speak: VecDeque::new(),
            stream: None,
            output: VecDeque::new(),
            speaking: false,
            flush: false,
        }
    }

    pub fn turn(&self) -> Turn {
        self.turn
    }
    pub fn playback_revision(&self) -> u32 {
        self.playback_revision
    }

    pub fn codec(&self) -> Codec {
        self.settings.output_codec
    }

    pub fn push_audio(&mut self, input: &[i16]) {
        let mut frames = std::mem::take(&mut self.frame);
        self.input.push(input, &mut frames);
        let (chunks, remainder) = frames.as_chunks::<FRAME>();
        for frame in chunks {
            self.on_frame(frame);
        }
        let rest = remainder.len();
        let start = frames.len() - rest;
        frames.copy_within(start.., 0);
        frames.truncate(rest);
        self.frame = frames;
    }

    pub fn push_event(&mut self, event: ClientEvent) {
        match event {
            ClientEvent::Configure(config) => self.configure(config),
            ClientEvent::ListenStart if !self.ptt => {
                if self.open {
                    self.cancel();
                }
                self.pre_roll.clear();
                self.wait_silence = None;
                self.ptt = true;
                self.open_turn();
                self.start_speech(false);
            }
            ClientEvent::ListenStop => {
                self.ptt = false;
                // Preserve the last sub-frame when a push-to-talk edge lands between PCM frames.
                if let Some(utterance) = &mut self.utterance {
                    self.input.finish(&mut self.frame);
                    utterance.audio.append(&mut self.frame);
                }
                self.finish_speech(false);
            }
            ClientEvent::SpeakAppend { text, turn } if self.accepts(turn) && !text.is_empty() => {
                if self.queued_text + self.chunker.buffered() + text.len() > MAX_QUEUED_TEXT {
                    self.error("queue_full", "Speech queue is full");
                    return;
                }
                self.open_turn();
                self.flush = false;
                self.chunker.append(&text);
                self.enqueue_text(false);
            }
            ClientEvent::SpeakFlush { turn } if self.accepts(turn) => {
                self.enqueue_text(true);
                self.flush = true;
            }
            ClientEvent::SpeakStop { turn } if self.accepts(turn) => {
                if self.is_speaking() || self.chunker.buffered() > 0 {
                    self.end_speak(EndReason::Cancelled);
                }
                self.clear_speech();
            }
            ClientEvent::CuePlay { id, turn } if self.accepts(turn) => {
                if id.is_empty() || id.len() > 128 || self.speak.len() >= 256 {
                    self.error("invalid_cue", "Cue ID or queue is out of bounds");
                    return;
                }
                self.open_turn();
                self.enqueue_text(true);
                self.speak.push_back(SpeakItem::Cue(id));
                self.flush = true;
            }
            ClientEvent::Cancel { turn } if self.accepts(turn) => self.cancel(),
            _ => {}
        }
    }

    pub fn poll(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking || !self.speak.is_empty()
    }

    /// Retire a failed reply while keeping any active microphone turn open.
    pub fn playback_failed(&mut self) {
        if self.is_speaking() || self.chunker.buffered() > 0 {
            self.error("audio_output", "Playback is unavailable");
            self.end_speak(EndReason::Error);
            self.clear_speech();
            self.open = self.utterance.is_some() || self.ptt;
        }
    }

    pub fn pull_audio(&mut self, out: &mut [i16]) -> usize {
        if out.is_empty() {
            return 0;
        }
        if self.output.is_empty() {
            if self.stream.is_none() {
                match self.speak.pop_front() {
                    Some(SpeakItem::Text(text, options)) => {
                        self.queued_text -= text.len();
                        match self.tts.start(&text, &options) {
                            Ok(stream) => {
                                self.stream = Some(stream);
                                if !self.speaking {
                                    self.speaking = true;
                                    self.events.push_back(ServerEvent::SpeakStart {
                                        turn: self.turn,
                                        sample_rate: OUTPUT_RATE,
                                        codec: self.codec(),
                                    });
                                }
                            }
                            Err(error) => {
                                self.speech_error(error);
                                return 0;
                            }
                        }
                    }
                    Some(SpeakItem::Cue(id)) => {
                        if self.speaking {
                            self.end_speak(EndReason::Done);
                        }
                        self.events.push_back(ServerEvent::Cue {
                            turn: self.turn,
                            id,
                        });
                        return 0;
                    }
                    None => {
                        if self.speaking && self.flush {
                            self.end_speak(EndReason::Done);
                            self.open = self.utterance.is_some() || self.ptt;
                            self.chunker = Chunker::default();
                        }
                        return 0;
                    }
                }
            }
            match self.stream.as_mut().expect("active synthesis").next_frame() {
                Ok(AudioStep::Frame(frame)) => self.output.extend(frame),
                Ok(AudioStep::Pending) => return 0,
                Ok(AudioStep::Done) => {
                    self.stream = None;
                    return 0;
                }
                Err(error) => {
                    self.speech_error(error);
                    return 0;
                }
            }
        }
        let count = out.len().min(self.output.len());
        for (target, sample) in out.iter_mut().zip(self.output.drain(..count)) {
            *target = sample;
        }
        count
    }

    fn configure(&mut self, c: Configure) {
        let invalid = c
            .rate
            .is_some_and(|rate| !rate.is_finite() || !(0.5..=2.0).contains(&rate))
            || c.input_rate
                .is_some_and(|rate| !(8_000..=192_000).contains(&rate))
            || c.endpoint_ms.is_some_and(|ms| !(100..=5_000).contains(&ms))
            || c.followup_ms.is_some_and(|ms| ms > 60_000)
            || c.wake
                .as_ref()
                .is_some_and(|wake| wake.len() > 128 || normalized(wake).is_empty())
            || c.keywords
                .as_ref()
                .is_some_and(|keys| keys.len() > 128 || keys.iter().any(|k| k.len() > 256))
            || c.output_codec == Some(Codec::Opus) && !self.settings.opus_supported
            || c.output_codec
                .is_some_and(|codec| codec != self.codec() && self.speaking);
        if invalid {
            self.error("invalid_config", "Unsupported session configuration");
            return;
        }
        if let Some(mode) = c.mode
            && mode != self.settings.mode
        {
            if self.utterance.is_some() {
                self.finish_speech(false);
            }
            self.ptt = false;
            self.pre_roll.clear();
            self.wait_silence = None;
            self.settings.mode = mode;
        }
        if let Some(rate) = c.input_rate
            && rate != self.settings.input_rate
        {
            self.input = Resampler::new(rate, INPUT_RATE);
            self.frame.clear();
            self.settings.input_rate = rate;
        }
        if let Some(wake) = c.wake {
            self.settings.wake = wake;
        }
        if let Some(enabled) = c.wake_reply {
            self.settings.wake_reply = enabled;
        }
        if let Some(ms) = c.endpoint_ms {
            self.settings.endpoint_ms = ms;
        }
        if let Some(ms) = c.followup_ms {
            self.settings.followup_ms = ms;
            self.followup = self.followup.min(samples(ms));
        }
        if let Some(language) = c.language {
            self.settings.recognition.language =
                (!language.is_empty() && language != "auto").then_some(language);
        }
        if let Some(keywords) = c.keywords {
            self.settings.recognition.keywords = keywords;
        }
        if let Some(voice) = c.voice {
            self.settings.voice = (!voice.is_empty()).then_some(voice);
        }
        if let Some(rate) = c.rate {
            self.settings.rate = rate;
        }
        if let Some(codec) = c.output_codec {
            self.settings.output_codec = codec;
        }
        if let Some(echo_cancelled) = c.echo_cancelled {
            self.settings.echo_cancelled = echo_cancelled;
        }
    }

    fn on_frame(&mut self, frame: &[i16]) {
        let voiced = self.vad.speech(frame);
        let followup = self.followup > 0;
        self.followup = self.followup.saturating_sub(FRAME);
        if let Some(wait) = &mut self.wait_silence {
            *wait = if voiced { 0 } else { *wait + FRAME };
            if *wait >= samples(500) {
                self.wait_silence = None;
            }
            return;
        }
        if self.utterance.is_none() {
            let listening = self.ptt || self.settings.mode != Mode::Ptt;
            let speaker = self.speaking && !self.settings.echo_cancelled;
            let wake_required =
                !self.ptt && ((self.settings.mode == Mode::Wake && !followup) || speaker);
            if listening && voiced {
                if !wake_required && self.speaking {
                    self.cancel();
                }
                self.start_speech(wake_required);
            } else {
                if listening {
                    if self.pre_roll.len() == PRE_ROLL {
                        self.pre_roll.drain(..FRAME);
                    }
                    self.pre_roll.extend(frame);
                }
                return;
            }
        }
        let utterance = self.utterance.as_mut().expect("speech onset");
        utterance.audio.extend_from_slice(frame);
        utterance.elapsed += FRAME;
        utterance.silence = if voiced { 0 } else { utterance.silence + FRAME };
        let endpoint = !self.ptt && utterance.silence >= samples(self.settings.endpoint_ms);
        let rollover = utterance.audio.len() >= MAX_UTTERANCE;
        if utterance.wake_required {
            if utterance.elapsed >= utterance.next_pass || endpoint {
                self.check_wake(endpoint);
                if endpoint {
                    self.finish_speech(true);
                }
            }
        } else if endpoint || rollover {
            self.finish_speech(endpoint);
            if rollover && self.ptt {
                self.start_speech(false);
            }
        } else if utterance.elapsed >= utterance.next_pass {
            self.partial();
        }
    }

    fn start_speech(&mut self, wake_required: bool) {
        self.utterance = Some(Utterance::new(
            self.pre_roll.drain(..).collect(),
            wake_required,
            self.settings.partial_ms,
        ));
        if !wake_required {
            self.open_turn();
            self.events
                .push_back(ServerEvent::SpeechStart { turn: self.turn });
        }
    }

    fn check_wake(&mut self, endpoint: bool) {
        let mut utterance = self.utterance.take().expect("wake audio");
        let result = self
            .stt
            .transcribe(&utterance.audio, &self.settings.recognition);
        if let Ok(transcript) = &result
            && let Some(matched) = wake_prefix(&self.settings.wake, &transcript.words)
        {
            if self.open {
                self.cancel();
            }
            self.open_turn();
            self.events.push_back(ServerEvent::Wake {
                turn: self.turn,
                phrase: self.settings.wake.clone(),
                confidence: matched.confidence,
            });
            self.events
                .push_back(ServerEvent::SpeechStart { turn: self.turn });
            utterance.wake_required = false;
            // Keep the phrase as recognition context. Decoding its clipped tail
            // in isolation can turn the last syllable into spurious command words.
            utterance.wake_match = Some((self.settings.wake.clone(), matched.end));
            utterance.next_pass = utterance.elapsed + samples(self.settings.partial_ms);
            self.utterance = Some(utterance);
            return;
        }
        if let Err(error) = result {
            self.error("stt", error);
        }
        if endpoint || utterance.elapsed >= samples(2_400) {
            self.wait_silence = (utterance.silence < samples(500)).then_some(utterance.silence);
        } else {
            utterance.next_pass = utterance.elapsed + samples(400);
            self.utterance = Some(utterance);
        }
    }

    fn partial(&mut self) {
        let utterance = self.utterance.as_mut().expect("active speech");
        utterance.next_pass += samples(self.settings.partial_ms);
        match self
            .stt
            .try_partial(&utterance.audio, &self.settings.recognition)
        {
            Ok(Some(transcript)) => {
                let words = utterance.command_words(&transcript);
                if words.is_empty()
                    && utterance.wake_match.is_some()
                    && utterance.agreement.previous.is_empty()
                    && utterance.agreement.committed.is_empty()
                {
                    return;
                }
                let pending = utterance.agreement.update(words);
                self.events.push_back(ServerEvent::Partial {
                    turn: self.turn,
                    committed: words_text(&utterance.agreement.committed),
                    pending,
                });
            }
            Ok(None) => {}
            Err(error) => self.error("stt", error),
        }
    }

    fn finish_speech(&mut self, endpoint: bool) {
        let Some(utterance) = self.utterance.take() else {
            return;
        };
        if utterance.wake_required {
            return;
        }
        self.events
            .push_back(ServerEvent::SpeechEnd { turn: self.turn });
        let wake = utterance.wake_match.is_some();
        let trim = if wake {
            utterance.silence.saturating_sub(samples(100))
        } else {
            0
        };
        let audio = &utterance.audio[..utterance.audio.len().saturating_sub(trim)];
        match self.stt.transcribe(audio, &self.settings.recognition) {
            Ok(mut transcript) => {
                let words = utterance.command_words(&transcript);
                transcript.words = utterance.agreement.finish(words);
                if wake
                    && endpoint
                    && transcript.words.is_empty()
                    && self.settings.wake_reply
                    && self.tts.available()
                    && !self.is_speaking()
                    && self.chunker.buffered() == 0
                {
                    let reply = WAKE_REPLIES[self.next_wake_reply];
                    self.next_wake_reply = (self.next_wake_reply + 1) % WAKE_REPLIES.len();
                    self.enqueue_speech(reply.into());
                    self.flush = true;
                    return;
                }
                transcript.text = words_text(&transcript.words);
                self.events.push_back(ServerEvent::Final {
                    turn: self.turn,
                    transcript,
                });
            }
            Err(error) => self.error("stt", error),
        }
    }

    fn enqueue_text(&mut self, flush: bool) {
        while let Some(text) = self.chunker.take(flush) {
            if text.is_empty() {
                continue;
            }
            self.enqueue_speech(text);
        }
    }

    fn enqueue_speech(&mut self, text: String) {
        self.queued_text += text.len();
        let options = Speech {
            voice: self.settings.voice.clone(),
            rate: self.settings.rate,
        };
        self.speak.push_back(SpeakItem::Text(text, options));
    }

    fn accepts(&self, turn: Option<Turn>) -> bool {
        turn.is_none() || (self.open && turn == Some(self.turn))
    }

    fn open_turn(&mut self) {
        if !self.open {
            if !self.reserved_turn {
                self.turn = self.turn.wrapping_add(1).max(1);
            }
            self.reserved_turn = false;
            self.open = true;
        }
    }

    fn cancel(&mut self) {
        if self
            .utterance
            .take()
            .is_some_and(|utterance| !utterance.wake_required)
        {
            self.events
                .push_back(ServerEvent::SpeechEnd { turn: self.turn });
        }
        if self.speaking || !self.speak.is_empty() || self.chunker.buffered() > 0 {
            self.end_speak(EndReason::Cancelled);
        }
        self.clear_speech();
        self.open = false;
        self.turn = self.turn.wrapping_add(1).max(1);
        self.reserved_turn = true;
        self.ptt = false;
        self.followup = 0;
        self.pre_roll.clear();
        self.frame.clear();
    }

    fn end_speak(&mut self, reason: EndReason) {
        self.events.push_back(ServerEvent::SpeakEnd {
            turn: self.turn,
            reason,
        });
        self.speaking = false;
        if reason == EndReason::Done {
            self.followup = samples(self.settings.followup_ms);
        }
    }

    fn speech_error(&mut self, error: String) {
        self.error("tts", error);
        self.end_speak(EndReason::Error);
        self.clear_speech();
        self.open = self.utterance.is_some() || self.ptt;
    }

    fn clear_speech(&mut self) {
        self.playback_revision = self.playback_revision.wrapping_add(1);
        self.stream = None;
        self.output.clear();
        self.speak.clear();
        self.queued_text = 0;
        self.chunker = Chunker::default();
        self.flush = false;
    }

    fn error(&mut self, code: &str, message: impl Into<String>) {
        self.events
            .push_back(ServerEvent::error(Some(self.turn), code, message));
    }
}

fn samples(ms: u32) -> usize {
    ms as usize * (INPUT_RATE as usize / 1000)
}

fn words_text(words: &[Word]) -> String {
    words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn word_list(t: &Transcript) -> Vec<Word> {
    if !t.words.is_empty() {
        return t.words.clone();
    }
    t.text
        .split_whitespace()
        .map(|text| Word {
            text: text.to_owned(),
            start: 0.0,
            end: 0.0,
            probability: 0.0,
        })
        .collect()
}

fn normalized(word: &str) -> String {
    word.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

struct WakePrefix {
    count: usize,
    end: f32,
    confidence: f32,
}

fn wake_prefix(phrase: &str, words: &[Word]) -> Option<WakePrefix> {
    let wanted: Vec<_> = phrase
        .split_whitespace()
        .map(normalized)
        .filter(|word| !word.is_empty())
        .collect();
    let prefix = words.get(..wanted.len())?;
    let mut probability = 1.0f32;
    for (target, word) in wanted.iter().zip(prefix) {
        let heard = normalized(&word.text);
        let alias = matches!(
            (target.as_str(), heard.as_str()),
            ("hey", "hay") | ("jimmy", "jimmie")
        );
        if target != &heard && !alias {
            return None;
        }
        probability = probability.min(word.probability);
    }
    Some(WakePrefix {
        count: prefix.len(),
        end: prefix.last()?.end.max(0.0),
        confidence: probability,
    })
}
