//! Jimmy protocol v1.
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;
pub const INPUT_RATE: u32 = 16_000;
pub const OUTPUT_RATE: u32 = 24_000;
pub type Turn = u32;

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Wake,
    Ptt,
    Open,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    Pcm16,
    Opus,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Configure {
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub mode: Option<Mode>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub wake: Option<String>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub wake_reply: Option<bool>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub language: Option<String>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub keywords: Option<Vec<String>>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub voice: Option<String>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub rate: Option<f32>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub input_rate: Option<u32>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub output_codec: Option<Codec>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub endpoint_ms: Option<u32>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub followup_ms: Option<u32>,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub echo_cancelled: Option<bool>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientEvent {
    #[serde(rename = "session.configure")]
    Configure(Configure),
    #[serde(rename = "listen.start")]
    ListenStart,
    #[serde(rename = "listen.stop")]
    ListenStop,
    #[serde(rename = "speak.append")]
    SpeakAppend {
        text: String,
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
    },
    #[serde(rename = "speak.flush")]
    SpeakFlush {
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
    },
    #[serde(rename = "speak.stop")]
    SpeakStop {
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
    },
    #[serde(rename = "cue.play")]
    CuePlay {
        id: String,
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
    },
    #[serde(rename = "cancel")]
    Cancel {
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
    },
    #[serde(other)]
    Unknown,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    #[serde(alias = "word")]
    pub text: String,
    pub start: f32,
    pub end: f32,
    pub probability: f32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub language: String,
    #[serde(default)]
    pub words: Vec<Word>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Done,
    Cancelled,
    Error,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    #[serde(rename = "session.ready")]
    Ready {
        turn: Turn,
        protocol: u32,
        models: Vec<String>,
        voices: Vec<String>,
        sample_rates: [u32; 2],
    },
    #[serde(rename = "wake")]
    Wake {
        turn: Turn,
        phrase: String,
        confidence: f32,
    },
    #[serde(rename = "speech.start")]
    SpeechStart { turn: Turn },
    #[serde(rename = "speech.end")]
    SpeechEnd { turn: Turn },
    #[serde(rename = "transcript.partial")]
    Partial {
        turn: Turn,
        committed: String,
        pending: String,
    },
    #[serde(rename = "transcript.final")]
    Final {
        turn: Turn,
        #[serde(flatten)]
        transcript: Transcript,
    },
    #[serde(rename = "speak.start")]
    SpeakStart {
        turn: Turn,
        sample_rate: u32,
        codec: Codec,
    },
    #[serde(rename = "speak.end")]
    SpeakEnd { turn: Turn, reason: EndReason },
    #[serde(rename = "cue")]
    Cue { turn: Turn, id: String },
    #[serde(rename = "error")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional = nullable))]
        turn: Option<Turn>,
        code: String,
        message: String,
        fatal: bool,
    },
}

impl ServerEvent {
    pub fn error(turn: Option<Turn>, code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            turn,
            code: code.into(),
            message: message.into(),
            fatal: false,
        }
    }
}

pub fn encode_audio(turn: Turn, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend(turn.to_le_bytes());
    frame.extend(seq.to_le_bytes());
    frame.extend(payload);
    frame
}

pub fn decode_audio(frame: &[u8]) -> Option<(Turn, u32, &[u8])> {
    let (header, payload) = frame.split_at_checked(8)?;
    Some((
        u32::from_le_bytes(header[..4].try_into().ok()?),
        u32::from_le_bytes(header[4..].try_into().ok()?),
        payload,
    ))
}

pub fn decode_pcm16(frame: &[u8]) -> Option<Vec<i16>> {
    let (samples, rest) = frame.as_chunks::<2>();
    rest.is_empty()
        .then(|| samples.iter().copied().map(i16::from_le_bytes).collect())
}

pub fn pcm16_bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}
