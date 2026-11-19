use jimmy_core::{
    AudioStep, AudioStream, Recognition, Resampler, Session, Settings, Speech, Stt, Tts,
};
use jimmy_proto::{
    ClientEvent, Mode, OUTPUT_RATE, ServerEvent, Transcript, encode_audio, pcm16_bytes,
};
use js_sys::{Array, Function, Int16Array, Uint8Array};
use wasm_bindgen::prelude::*;

struct JsStt(Function);
struct JsTts {
    start: Function,
    pull: Function,
    cancel: Function,
    voices: Vec<String>,
}

struct JsSpeech {
    id: JsValue,
    pull: Function,
    cancel: Function,
    resampler: Resampler,
}

fn js_error(e: JsValue) -> String {
    e.as_string()
        .or_else(|| {
            js_sys::Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|value| value.as_string())
        })
        .unwrap_or_else(|| "Browser model error".into())
}

impl Stt for JsStt {
    fn transcribe(&mut self, audio: &[i16], options: &Recognition) -> Result<Transcript, String> {
        let audio = Int16Array::from(audio);
        let language = options
            .language
            .as_deref()
            .map_or(JsValue::NULL, JsValue::from_str);
        let keywords = JsValue::from_str(&options.keywords.join("\n"));
        let json = self
            .0
            .call3(&JsValue::NULL, &audio, &language, &keywords)
            .map_err(js_error)?;
        serde_json::from_str(
            &json
                .as_string()
                .ok_or("Cactus did not return a transcript")?,
        )
        .map_err(|e| e.to_string())
    }
}

impl Tts for JsTts {
    fn voices(&self) -> Vec<String> {
        self.voices.clone()
    }

    fn start(&mut self, text: &str, options: &Speech) -> Result<Box<dyn AudioStream>, String> {
        let voice = options
            .voice
            .as_deref()
            .map_or(JsValue::NULL, JsValue::from_str);
        let id = self
            .start
            .call2(&JsValue::NULL, &text.into(), &voice)
            .map_err(js_error)?;
        let rate = (OUTPUT_RATE as f32 * options.rate.clamp(0.5, 2.0)).round() as u32;
        Ok(Box::new(JsSpeech {
            id,
            pull: self.pull.clone(),
            cancel: self.cancel.clone(),
            resampler: Resampler::new(rate, OUTPUT_RATE),
        }))
    }
}

impl AudioStream for JsSpeech {
    fn next_frame(&mut self) -> Result<AudioStep, String> {
        let value = self
            .pull
            .call1(&JsValue::NULL, &self.id)
            .map_err(js_error)?;
        if value.is_undefined() {
            return Ok(AudioStep::Pending);
        }
        let mut out = Vec::new();
        if value.is_null() {
            self.resampler.finish(&mut out);
            if out.is_empty() {
                return Ok(AudioStep::Done);
            }
        } else {
            self.resampler
                .push(&Int16Array::from(value).to_vec(), &mut out);
        }
        Ok(AudioStep::Frame(out))
    }
}

impl Drop for JsSpeech {
    fn drop(&mut self) {
        let _ = self.cancel.call1(&JsValue::NULL, &self.id);
    }
}

#[wasm_bindgen]
pub struct BrowserSession {
    session: Session<JsStt, JsTts>,
    output_turn: u32,
    seq: u32,
}

#[wasm_bindgen]
impl BrowserSession {
    #[wasm_bindgen(constructor)]
    pub fn new(
        stt: Function,
        start: Function,
        pull: Function,
        cancel: Function,
        voices: Vec<String>,
    ) -> Self {
        let settings = Settings {
            mode: Mode::Ptt,
            ..Settings::default()
        };
        Self {
            session: Session::new(
                JsStt(stt),
                JsTts {
                    start,
                    pull,
                    cancel,
                    voices,
                },
                settings,
            ),
            output_turn: 0,
            seq: 0,
        }
    }

    pub fn event(&mut self, json: &str) -> Result<(), JsValue> {
        if json.len() > 96_000 {
            return Err("Control message is too large".into());
        }
        let event: ClientEvent =
            serde_json::from_str(json).map_err(|e| JsValue::from_str(&e.to_string()))?;
        self.session.push_event(event);
        Ok(())
    }

    pub fn audio(&mut self, samples: &[i16]) -> Result<(), JsValue> {
        if samples.len() > 48_000 {
            return Err("Audio frame is too large".into());
        }
        self.session.push_audio(samples);
        Ok(())
    }

    pub fn turn(&self) -> u32 {
        self.session.turn()
    }

    pub fn events(&mut self) -> Array {
        let events = Array::new();
        while let Some(event) = self.session.poll() {
            let json = serde_json::to_string(&event).unwrap_or_else(|_| {
                serde_json::to_string(&ServerEvent::error(
                    Some(self.session.turn()),
                    "encoding",
                    "Event encoding failed",
                ))
                .unwrap()
            });
            events.push(&json.into());
        }
        events
    }

    pub fn pull_audio(&mut self) -> Option<Uint8Array> {
        let turn = self.session.turn();
        if turn != self.output_turn {
            self.seq = 0;
            self.output_turn = turn;
        }
        let mut buffer = [0; 480];
        let count = self.session.pull_audio(&mut buffer);
        if count == 0 {
            return None;
        }
        let frame = encode_audio(turn, self.seq, &pcm16_bytes(&buffer[..count]));
        self.seq = self.seq.wrapping_add(1);
        Some(Uint8Array::from(frame.as_slice()))
    }
}
