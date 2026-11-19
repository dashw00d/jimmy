//! Feed 16 kHz mono signed PCM from stdin and emit JSONL wake events with playback offsets.
use anyhow::Result;
use jimmy_core::{AudioStream, Session, Settings, Speech, Tts};
use jimmy_proto::{INPUT_RATE, ServerEvent};
use jimmy_whistle::Whistle;
use std::io::{self, Read};

struct Silent;
impl Tts for Silent {
    fn start(&mut self, _: &str, _: &Speech) -> Result<Box<dyn AudioStream>, String> {
        Err("Wake soak does not synthesize".into())
    }
}

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Usage: wake_soak PATH/whistle.cact < audio.s16le"))?;
    let stt = Whistle::load(path.as_ref())?;
    let mut session = Session::new(stt, Silent, Settings::default());
    let mut stdin = io::stdin().lock();
    let mut frame = [0; 640];
    let mut samples = 0u64;
    let mut filled = 0;
    while let n @ 1..=640 = stdin.read(&mut frame[filled..])? {
        filled += n;
        if filled < frame.len() {
            continue;
        }
        let audio = jimmy_proto::decode_pcm16(&frame).expect("aligned PCM16 frame");
        session.push_audio(&audio);
        samples += audio.len() as u64;
        filled = 0;
        while let Some(event) = session.poll() {
            match event {
                ServerEvent::Wake { .. } => println!(
                    "{}",
                    serde_json::json!({
                        "seconds": samples as f64 / f64::from(INPUT_RATE), "event": event,
                    })
                ),
                ServerEvent::Error { message, .. } => anyhow::bail!("{message}"),
                _ => {}
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"duration_seconds": samples as f64 / f64::from(INPUT_RATE)})
    );
    Ok(())
}
