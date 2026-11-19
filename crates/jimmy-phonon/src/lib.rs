//! CPU Phonon 31m adapter. All sessions share the immutable synthesizer.
use anyhow::{Result, ensure};
use jimmy_core::{AudioStep, AudioStream, Resampler, Speech, Tts};
use ptts::{
    checkpoint::{Checkpoint, ResolveOptions},
    preprocess::Lang,
    synth::{DeviceKind, Quant, SpeechOptions, Synth},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
};

#[derive(Clone)]
pub struct Phonon {
    synth: Arc<Synth>,
    in_flight: Arc<AtomicUsize>,
    limit: usize,
    custom_voice: Option<String>,
}

impl Phonon {
    pub fn load(path: &Path, voice_file: Option<&Path>, limit: usize) -> Result<Self> {
        let checkpoint = Checkpoint::resolve(
            path,
            ResolveOptions {
                quant: Quant::Q80,
                weights: None,
            },
        )?;
        let mut synth = checkpoint
            .builder(Lang::En)
            .device(DeviceKind::Cpu)
            .build()?;
        checkpoint.register_voices(&mut synth);
        let custom_voice = if let Some(path) = voice_file {
            synth.add_voice_file("custom", path)?;
            Some("custom".into())
        } else {
            None
        };
        ensure!(
            synth.sample_rate() == 24_000,
            "Phonon checkpoint does not output 24 kHz audio"
        );
        Ok(Self {
            synth: Arc::new(synth),
            in_flight: Arc::new(AtomicUsize::new(0)),
            limit: limit.max(1),
            custom_voice,
        })
    }
}

type FrameResult = Result<Vec<i16>, String>;

struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Stream {
    model: Phonon,
    request: Option<(String, SpeechOptions)>,
    frames: Option<Receiver<FrameResult>>,
    ended: Arc<AtomicBool>,
    resampler: Resampler,
}

impl AudioStream for Stream {
    fn next_frame(&mut self) -> Result<AudioStep, String> {
        if self.request.is_some() {
            if self
                .model
                .in_flight
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    (n < self.model.limit).then_some(n + 1)
                })
                .is_err()
            {
                return Ok(AudioStep::Pending);
            }
            let slot = Slot(self.model.in_flight.clone());
            let (text, options) = self.request.take().expect("pending speech");
            let synth = self.model.synth.clone();
            let ended = self.ended.clone();
            // Room for about 2.5 s of frames, so synthesis keeps going while
            // the session thread is busy with something else, such as a
            // transcription.
            let (tx, rx) = mpsc::sync_channel(32);
            // The session can drop rx immediately on cancel. The vendor's own
            // cleanup joins on this thread before its CPU slot is released.
            std::thread::Builder::new()
                .name("jimmy-phonon".into())
                .spawn(move || {
                    let _slot = slot;
                    match synth.stream_with(&text, &options) {
                        Ok(stream) => {
                            for result in stream {
                                let frame = result
                                    .map(|frame| {
                                        frame
                                            .into_iter()
                                            .map(|sample| (sample * 32768.0).round() as i16)
                                            .collect()
                                    })
                                    .map_err(|e| e.to_string());
                                if tx.send(frame).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error.to_string()));
                        }
                    }
                    ended.store(true, Ordering::Release);
                })
                .map_err(|e| e.to_string())?;
            self.frames = Some(rx);
        }
        let mut out = Vec::new();
        match self.frames.as_ref().expect("started synthesis").try_recv() {
            Ok(frame) => {
                self.resampler.push(&frame?, &mut out);
                Ok(AudioStep::Frame(out))
            }
            Err(TryRecvError::Empty) => Ok(AudioStep::Pending),
            Err(TryRecvError::Disconnected) => {
                if !self.ended.load(Ordering::Acquire) {
                    return Err("Phonon worker stopped unexpectedly".into());
                }
                self.resampler.finish(&mut out);
                if out.is_empty() {
                    Ok(AudioStep::Done)
                } else {
                    Ok(AudioStep::Frame(out))
                }
            }
        }
    }
}

impl Tts for Phonon {
    fn start(&mut self, text: &str, options: &Speech) -> Result<Box<dyn AudioStream>, String> {
        let speech = SpeechOptions {
            voice: options.voice.clone().or_else(|| self.custom_voice.clone()),
            ..SpeechOptions::default()
        };
        let rate = (24_000.0 * options.rate.clamp(0.5, 2.0)).round() as u32;
        Ok(Box::new(Stream {
            model: self.clone(),
            request: Some((text.into(), speech)),
            frames: None,
            ended: Arc::new(AtomicBool::new(false)),
            resampler: Resampler::new(rate, 24_000),
        }))
    }

    fn voices(&self) -> Vec<String> {
        self.synth.voices()
    }
}
