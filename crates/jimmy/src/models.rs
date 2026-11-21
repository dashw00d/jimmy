use anyhow::{Context, Result, ensure};
use jimmy_core::{AudioStep, Recognition, Resampler, Speech, Stt, Tts};
use jimmy_phonon::Phonon;
use jimmy_proto::{INPUT_RATE, OUTPUT_RATE, Transcript};
use jimmy_whistle::Whistle;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const WHISTLE_BASE: &str = "https://huggingface.co/Cactus-Compute/whistle/resolve/88cb00eeb8928ddbcd410b491584f938ba030b07";
const PHONON_BASE: &str =
    "https://huggingface.co/gradium/phonon-31m-en/resolve/92741c5b07d085ff840e26fea8ce43110b87852f";
const PHONON: &[(&str, &str)] = &[
    (
        "config.json",
        "b5d0f88353638370d94035b7d73d7d6577c0554d78f8d7ecfd40490aae39ae85",
    ),
    (
        "default-voice.safetensors",
        "dc537478eaadc1a5257fcda2e3501ab81e14c111a5d95a5838f350db2afd0f1a",
    ),
    (
        "tokenizer.json",
        "691a00068fa7b989f2f3e5ae5daa2d1a5bb319f2d5360deafcfb19a8bb6e7936",
    ),
    (
        "model.q8.gguf",
        "a7d9aad2cd6dc66473f24a08bc6d2939030ef4d751ef82349361fbef3272dbae",
    ),
];

#[derive(Clone)]
pub struct Models {
    root: PathBuf,
    local: bool,
}

impl Models {
    pub fn new(local: Option<PathBuf>) -> Result<Self> {
        if let Some(root) = local {
            return Ok(Self { root, local: true });
        }
        let dirs = directories::ProjectDirs::from("", "", "jimmy")
            .context("Cannot find a user model cache")?;
        Ok(Self {
            root: dirs.cache_dir().join("models"),
            local: false,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn whistle(&self) -> Result<Whistle> {
        Whistle::load(&self.ensure_whistle()?)
    }

    pub fn phonon(&self, voice: Option<&Path>, concurrency: usize) -> Result<Phonon> {
        Phonon::load(&self.ensure_phonon()?, voice, concurrency)
    }

    pub fn pull(&self) -> Result<()> {
        self.ensure_whistle()?;
        self.ensure_phonon()?;
        Ok(())
    }

    fn ensure_whistle(&self) -> Result<PathBuf> {
        let path = self.root.join(jimmy_whistle::MODEL);
        self.ensure(&path, WHISTLE_BASE, jimmy_whistle::SHA256)?;
        Ok(path)
    }

    fn ensure_phonon(&self) -> Result<PathBuf> {
        let dir = self.root.join("phonon");
        if self.local {
            ensure!(
                dir.is_dir(),
                "Missing local Phonon checkpoint {}",
                dir.display()
            );
        } else {
            for (name, hash) in PHONON {
                self.ensure(&dir.join(name), PHONON_BASE, hash)?;
            }
        }
        Ok(dir)
    }

    fn ensure(&self, path: &Path, base: &str, hash: &str) -> Result<()> {
        if self.local {
            ensure!(
                path.is_file(),
                "Missing local model file {}",
                path.display()
            );
            return Ok(());
        }
        if let Ok(file) = fs::File::open(path)
            && hash_reader(file)? == hash
        {
            return Ok(());
        }
        let dir = path.parent().context("Model file has no parent")?;
        fs::create_dir_all(dir)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("Invalid model filename")?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(feature = "bundled")]
        {
            let _ = base;
            tmp.write_all(bundled(name).context("Missing bundled model file")?)?;
        }
        #[cfg(not(feature = "bundled"))]
        {
            eprintln!("Fetching {name}");
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(180)))
                .build()
                .into();
            let response = agent
                .get(format!("{base}/{name}?download=true"))
                .call()
                .with_context(|| format!("fetch {name}"))?;
            let mut reader = response.into_body().into_reader().take(150_000_001);
            std::io::copy(&mut reader, &mut tmp)?;
        }
        tmp.flush()?;
        ensure!(
            hash_reader(fs::File::open(tmp.path())?)? == hash,
            "SHA-256 mismatch for {name}"
        );
        tmp.persist(path)?;
        Ok(())
    }
}

#[cfg(feature = "bundled")]
fn bundled(name: &str) -> Option<&'static [u8]> {
    let data: &[u8] = match name {
        "whistle.cact" => include_bytes!(concat!(env!("JIMMY_BUNDLE_DIR"), "/whistle.cact")),
        "config.json" => include_bytes!(concat!(env!("JIMMY_BUNDLE_DIR"), "/phonon/config.json")),
        "tokenizer.json" => {
            include_bytes!(concat!(env!("JIMMY_BUNDLE_DIR"), "/phonon/tokenizer.json"))
        }
        "model.q8.gguf" => {
            include_bytes!(concat!(env!("JIMMY_BUNDLE_DIR"), "/phonon/model.q8.gguf"))
        }
        "default-voice.safetensors" => include_bytes!(concat!(
            env!("JIMMY_BUNDLE_DIR"),
            "/phonon/default-voice.safetensors"
        )),
        _ => return None,
    };
    Some(data)
}

fn hash_reader(mut reader: impl Read) -> Result<String> {
    let mut sha = Sha256::new();
    std::io::copy(&mut reader, &mut sha)?;
    Ok(format!("{:x}", sha.finalize()))
}

pub fn transcribe(stt: &mut impl Stt, pcm: &[i16], options: &Recognition) -> Result<Transcript> {
    let mut result = Transcript::default();
    let mut offset = 0;
    while offset < pcm.len() {
        let left = &pcm[offset..];
        let mut end = left.len().min(25 * INPUT_RATE as usize);
        if end < left.len() {
            let frame = INPUT_RATE as usize / 50;
            let earliest = end - 4 * INPUT_RATE as usize;
            // Prefer a quiet frame near the maximum pass size.
            if let Some((index, _)) = left[earliest..end]
                .chunks_exact(frame)
                .enumerate()
                .filter_map(|(i, pcm)| {
                    let peak = pcm.iter().map(|x| x.unsigned_abs()).max()?;
                    (peak < 350).then_some((i, peak))
                })
                .next_back()
            {
                end = earliest + (index + 1) * frame;
            }
        }
        let mut part = stt
            .transcribe(&left[..end], options)
            .map_err(anyhow::Error::msg)?;
        jimmy_core::filter_hallucination(&left[..end], &mut part);
        if !part.text.is_empty() {
            if !result.text.is_empty() {
                result.text.push(' ');
            }
            result.text.push_str(&part.text);
        }
        if result.language.is_empty() {
            result.language = part.language;
        }
        let seconds = offset as f32 / INPUT_RATE as f32;
        result.words.extend(part.words.into_iter().map(|mut word| {
            word.start += seconds;
            word.end += seconds;
            word
        }));
        offset += end;
    }
    Ok(result)
}

pub fn synthesize(tts: &mut impl Tts, text: &str, options: &Speech) -> Result<Vec<i16>> {
    let text = jimmy_core::speech_text(text);
    ensure!(!text.is_empty(), "Speech input contains no speakable text");
    let mut stream = tts.start(&text, options).map_err(anyhow::Error::msg)?;
    let mut pcm = Vec::new();
    loop {
        match stream.next_frame().map_err(anyhow::Error::msg)? {
            AudioStep::Pending => std::thread::sleep(std::time::Duration::from_millis(2)),
            AudioStep::Frame(frame) => {
                pcm.extend(frame);
                ensure!(
                    pcm.len() <= 600 * OUTPUT_RATE as usize,
                    "Speech output exceeds ten minutes"
                );
            }
            AudioStep::Done => return Ok(pcm),
        }
    }
}

pub fn at_input_rate(pcm: &[i16]) -> Vec<i16> {
    let mut resampler = Resampler::new(OUTPUT_RATE, INPUT_RATE);
    let mut out = Vec::new();
    resampler.push(pcm, &mut out);
    resampler.finish(&mut out);
    out
}
