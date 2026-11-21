//! Adaptive subband minimum-statistics voice activity detector for 16 kHz mono PCM.
//!
//! Pure Rust, zero per-frame heap allocations, and identical on native and WebAssembly.
//! Tracks a per-band noise floor using sliding minimum statistics so steady background
//! noise at any level never latches speech detection, while a low absolute floor preserves
//! quiet microphones (down to ~-55 dBFS) and soft word endings.
use std::{f32::consts::PI, sync::OnceLock};

use crate::Vad;
use jimmy_proto::Transcript;

const FFT_LEN: usize = 256;
const BANDS: usize = 6;
/// Bin ranges at 16 kHz / 256 = 62.5 Hz per bin:
/// 187-375, 375-750, 750-1500, 1500-2500, 2500-3750, 3750-6000 Hz.
const BAND_EDGES: [(usize, usize); BANDS] = [
    (3, 6),
    (6, 12),
    (12, 24),
    (24, 40),
    (40, 60),
    (60, 96),
];
/// Minimum-statistics window: 4 sub-windows of 20 frames (400 ms each, 1.6 s total).
const SUB_WINDOWS: usize = 4;
const SUB_LEN: usize = 20;
/// Compensates for the downward bias of taking a minimum over smoothed noise frames.
const MIN_STAT_BIAS: f32 = 1.65;
/// Floor per band (~RMS 10, roughly -70 dBFS).
const MIN_BAND_FLOOR: f32 = 100.0;
const ONSET_RMS: f32 = 24.0;
const SUSTAIN_RMS: f32 = 12.0;
const ONSET_SNR_DB: f32 = 7.5;
const ONSET_SECOND_BAND_DB: f32 = 4.0;
const SUSTAIN_SNR_DB: f32 = 5.0;
const ONSET_FRAMES: u8 = 3;
const HANGOVER_FRAMES: u8 = 5;

struct FftTables {
    window: [f32; FFT_LEN],
    bitrev: [usize; FFT_LEN],
    cos: [f32; FFT_LEN / 2],
    sin: [f32; FFT_LEN / 2],
}

fn tables() -> &'static FftTables {
    static TABLES: OnceLock<FftTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut window = [0.0f32; FFT_LEN];
        for (i, slot) in window.iter_mut().enumerate() {
            *slot = 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_LEN as f32).cos();
        }
        let mut bitrev = [0usize; FFT_LEN];
        let bits = FFT_LEN.trailing_zeros();
        for (i, slot) in bitrev.iter_mut().enumerate() {
            *slot = i.reverse_bits() >> (usize::BITS - bits);
        }
        let mut cos = [0.0f32; FFT_LEN / 2];
        let mut sin = [0.0f32; FFT_LEN / 2];
        for i in 0..(FFT_LEN / 2) {
            let angle = -2.0 * PI * i as f32 / FFT_LEN as f32;
            cos[i] = angle.cos();
            sin[i] = angle.sin();
        }
        FftTables {
            window,
            bitrev,
            cos,
            sin,
        }
    })
}

fn fft_in_place(re: &mut [f32; FFT_LEN], im: &mut [f32; FFT_LEN], t: &FftTables) {
    for i in 0..FFT_LEN {
        let j = t.bitrev[i];
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut size = 2;
    while size <= FFT_LEN {
        let half = size / 2;
        let step = FFT_LEN / size;
        let mut start = 0;
        while start < FFT_LEN {
            for k in 0..half {
                let tw = k * step;
                let wr = t.cos[tw];
                let wi = t.sin[tw];
                let u = start + k;
                let v = u + half;
                let tr = wr * re[v] - wi * im[v];
                let ti = wr * im[v] + wi * re[v];
                re[v] = re[u] - tr;
                im[v] = im[u] - ti;
                re[u] += tr;
                im[u] += ti;
            }
            start += size;
        }
        size *= 2;
    }
}

pub struct EnergyVad {
    hp_prev_x: f32,
    hp_prev_y: f32,
    noise: [f32; BANDS],
    fast: [f32; BANDS],
    smooth: [f32; BANDS],
    sub_min: [[f32; BANDS]; SUB_WINDOWS],
    cur_min: [f32; BANDS],
    cur_max: [f32; BANDS],
    sub_index: usize,
    sub_filled: usize,
    sub_frames: usize,
    initialized: bool,
    onset: u8,
    hangover: u8,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self {
            hp_prev_x: 0.0,
            hp_prev_y: 0.0,
            noise: [MIN_BAND_FLOOR; BANDS],
            fast: [MIN_BAND_FLOOR; BANDS],
            smooth: [MIN_BAND_FLOOR; BANDS],
            sub_min: [[f32::MAX; BANDS]; SUB_WINDOWS],
            cur_min: [f32::MAX; BANDS],
            cur_max: [0.0; BANDS],
            sub_index: 0,
            sub_filled: 0,
            sub_frames: 0,
            initialized: false,
            onset: 0,
            hangover: 0,
        }
    }
}

impl EnergyVad {
    fn band_powers(&mut self, frame: &[i16]) -> (f32, [f32; BANDS]) {
        let t = tables();
        let mut re = [0.0f32; FFT_LEN];
        let mut im = [0.0f32; FFT_LEN];
        let start = frame.len().saturating_sub(FFT_LEN) / 2;
        let mut energy = 0.0f32;
        for (idx, &raw) in frame.iter().enumerate() {
            let x = f32::from(raw);
            // First-order high-pass (~130 Hz cutoff at 16 kHz) removes DC and mains hum.
            let y = x - self.hp_prev_x + 0.95 * self.hp_prev_y;
            self.hp_prev_x = x;
            self.hp_prev_y = y;
            energy += y * y;
            if idx >= start && idx - start < FFT_LEN {
                let k = idx - start;
                re[k] = y * t.window[k];
            }
        }
        let rms = (energy / frame.len() as f32).sqrt();
        fft_in_place(&mut re, &mut im, t);

        // Parseval normalization for Hann-windowed 256-pt FFT so flat white noise
        // of RMS `r` yields band power ≈ r^2.
        let scale = 1.0 / (0.375 * FFT_LEN as f32);
        let mut powers = [0.0f32; BANDS];
        for (b, &(lo, hi)) in BAND_EDGES.iter().enumerate() {
            let mut sum = 0.0f32;
            for k in lo..hi {
                sum += re[k] * re[k] + im[k] * im[k];
            }
            powers[b] = (sum / (hi - lo) as f32) * scale;
        }
        (rms, powers)
    }
}

impl Vad for EnergyVad {
    fn speech(&mut self, frame: &[i16]) -> bool {
        if frame.is_empty() {
            return false;
        }
        let (rms, powers) = self.band_powers(frame);

        if !self.initialized {
            self.initialized = true;
            self.fast = powers;
            self.smooth = powers;
            self.cur_min = powers;
            self.cur_max = powers;
        } else {
            for (b, &power) in powers.iter().enumerate() {
                // Smooth rising edges to damp random noise spikes; release immediately
                // when energy drops so hangover alone controls speech endpointing.
                self.fast[b] = if power > self.fast[b] {
                    0.45 * self.fast[b] + 0.55 * power
                } else {
                    power
                };
                self.smooth[b] = 0.65 * self.smooth[b] + 0.35 * power;
                if self.smooth[b] < self.cur_min[b] {
                    self.cur_min[b] = self.smooth[b];
                }
                if self.smooth[b] > self.cur_max[b] {
                    self.cur_max[b] = self.smooth[b];
                }
            }
        }

        // Compute per-band SNR in dB against the current noise floor using the
        // lightly smoothed spectrum to suppress single-frame noise variance.
        let mut snrs = [0.0f32; BANDS];
        for (b, snr) in snrs.iter_mut().enumerate() {
            let denom = self.noise[b].max(MIN_BAND_FLOOR);
            let ratio = (self.fast[b] / denom).max(1.0);
            *snr = (10.0 * ratio.log10()).min(30.0);
        }
        // Average of the top 3 speech subbands so vowels (low/mid formants) and
        // fricatives (upper bands) both trigger cleanly while narrow hum or flat
        // broadband noise stays low.
        snrs.sort_unstable_by(|a, b| b.total_cmp(a));
        let top3_snr = (snrs[0] + snrs[1] + snrs[2]) / 3.0;

        let active = self.hangover != 0;
        // On onset require at least two active speech subbands, meaningful formant
        // energy above 375 Hz (rejects low-frequency bumps/hums even with FFT
        // window leakage), and spectral contrast on faint audio (rejects hiss).
        let total_power: f32 = powers.iter().sum();
        let upper_power: f32 = powers[1..].iter().sum();
        let has_formants = upper_power >= total_power * 0.05;
        let faint_flat = rms < 80.0 && (snrs[0] - snrs[BANDS - 1]) < 3.5;
        let voiced = if active {
            rms >= SUSTAIN_RMS && top3_snr >= SUSTAIN_SNR_DB && has_formants
        } else {
            rms >= ONSET_RMS
                && top3_snr >= ONSET_SNR_DB
                && snrs[1] >= ONSET_SECOND_BAND_DB
                && has_formants
                && !faint_flat
        };

        if voiced {
            self.onset = (self.onset + 1).min(ONSET_FRAMES);
            if self.onset >= ONSET_FRAMES || active {
                self.hangover = HANGOVER_FRAMES;
            }
        } else {
            self.onset = 0;
            self.hangover = self.hangover.saturating_sub(1);
            // Smoothly track the noise floor during non-speech frames.
            for (noise, &power) in self.noise.iter_mut().zip(&powers) {
                let target = power.max(MIN_BAND_FLOOR);
                let alpha = if target < *noise { 0.15 } else { 0.04 };
                *noise = (1.0 - alpha) * *noise + alpha * target;
            }
        }

        // Sliding minimum-statistics update runs on EVERY frame (even while voiced)
        // so stationary background noise can never latch the detector.
        self.sub_frames += 1;
        if self.sub_frames >= SUB_LEN {
            self.sub_min[self.sub_index] = self.cur_min;
            let flat_bands = (0..BANDS)
                .filter(|&b| self.cur_max[b] <= self.cur_min[b].max(1.0) * 4.0)
                .count();
            let stationary = flat_bands >= 4;
            self.sub_index = (self.sub_index + 1) % SUB_WINDOWS;
            self.sub_filled = (self.sub_filled + 1).min(SUB_WINDOWS);
            for b in 0..BANDS {
                let mut floor = f32::MAX;
                for w in 0..self.sub_filled {
                    floor = floor.min(self.sub_min[w][b]);
                }
                let est = (floor * MIN_STAT_BIAS).max(MIN_BAND_FLOOR);
                if est < self.noise[b] {
                    self.noise[b] = 0.5 * self.noise[b] + 0.5 * est;
                } else if stationary || self.sub_filled == SUB_WINDOWS {
                    // Rapidly absorb stationary noise; gradually track slow drift otherwise.
                    let alpha = if stationary { 0.65 } else { 0.25 };
                    self.noise[b] = (1.0 - alpha) * self.noise[b] + alpha * est;
                }
            }
            self.cur_min = [f32::MAX; BANDS];
            self.cur_max = [0.0; BANDS];
            self.sub_frames = 0;
        }

        self.hangover != 0
    }
}

/// Known Whisper/Whistle phantom phrases emitted on non-speech audio (clicks,
/// breaths, room rumble, or silence). Only transcripts matching this list are
/// subjected to the acoustic voiced-speech check below.
const PHANTOM_PHRASES: &[&str] = &[
    "thank you",
    "thank you very much",
    "thank you so much",
    "thanks",
    "thanks for watching",
    "thank you for watching",
    "thanks for listening",
    "thank you for listening",
    "bye",
    "goodbye",
    "you",
    "subscribe",
    "subtitles by",
];

fn normalized_phrase(transcript: &Transcript) -> String {
    let source = if transcript.words.is_empty() {
        transcript.text.as_str()
    } else {
        ""
    };
    let mut parts = Vec::new();
    let push_norm = |word: &str, out: &mut Vec<String>| {
        let clean: String = word
            .chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if !clean.is_empty() {
            out.push(clean);
        }
    };
    if transcript.words.is_empty() {
        for word in source.split_whitespace() {
            push_norm(word, &mut parts);
        }
    } else {
        for word in &transcript.words {
            push_norm(&word.text, &mut parts);
        }
    }
    parts.join(" ")
}

/// Counts 20 ms frames inside `audio` (16 kHz mono) that contain genuine
/// voiced harmonic speech: speech-band energy above the floor, mid-band
/// formant content (rejecting low-frequency tones/rumble), and a distinct
/// periodic pitch peak in 80–444 Hz (rejecting clicks, breaths, and static).
fn voiced_harmonic_frames(audio: &[i16]) -> usize {
    if audio.len() < 320 {
        return 0;
    }
    let mut hp_prev_x = 0.0f32;
    let mut hp_prev_y = 0.0f32;
    let mut frame = [0.0f32; 320];
    let mut count = 0usize;
    for chunk in audio.as_chunks::<320>().0 {
        let mut tot_e = 0.0f32;
        let mut diff_e = 0.0f32;
        for (i, &raw) in chunk.iter().enumerate() {
            let x = f32::from(raw);
            let y = x - hp_prev_x + 0.95 * hp_prev_y;
            hp_prev_x = x;
            hp_prev_y = y;
            frame[i] = y;
            tot_e += y * y;
            if i > 0 {
                let d = y - frame[i - 1];
                diff_e += d * d;
            }
        }
        let rms = (tot_e / 320.0).sqrt();
        if rms < ONSET_RMS || diff_e / (tot_e + 1e-6) < 0.04 {
            continue;
        }
        // Normalized autocorrelation over 120-sample window for lags 18..200 (step 2).
        const W: usize = 120;
        let e0: f32 = frame[..W].iter().map(|&v| v * v).sum::<f32>() + 1e-6;
        let mut corrs = [0.0f32; 100];
        let mut best = -1.0f32;
        let mut best_lag = 36usize;
        for lag in (18..200).step_by(2) {
            let mut e1 = 1e-6f32;
            let mut dot = 0.0f32;
            for i in 0..W {
                let a = frame[i];
                let b = frame[i + lag];
                e1 += b * b;
                dot += a * b;
            }
            let c = dot / (e0 * e1).sqrt();
            corrs[lag / 2] = c;
            if lag >= 36 && c > best {
                best = c;
                best_lag = lag;
            }
        }
        let lo = (best_lag / 4).max(18) & !1;
        let hi = ((best_lag * 3) / 4).max(lo) & !1;
        let mut valley = 1.0f32;
        for lag in (lo..=hi).step_by(2) {
            valley = valley.min(corrs[lag / 2]);
        }
        if best >= 0.62 && (best - valley) >= 0.45 {
            count += 1;
        }
    }
    count
}

/// Drops known Whisper/Whistle silence hallucinations (such as `"Thank you."`)
/// when the underlying 16 kHz audio span contains no genuine voiced speech.
/// Real spoken `"Thank you."` (even at -50 dBFS) easily passes the check.
pub fn filter_hallucination(audio: &[i16], transcript: &mut Transcript) {
    let phrase = normalized_phrase(transcript);
    if !PHANTOM_PHRASES.contains(&phrase.as_str()) {
        return;
    }
    // Check inside the claimed word window (with ±60 ms margin) when timestamps exist.
    let slice = if let (Some(first), Some(last)) = (transcript.words.first(), transcript.words.last())
        && last.end > 0.0
    {
        let start = ((first.start - 0.06).max(0.0) * 16_000.0) as usize;
        let end = ((last.end + 0.06).max(0.0) * 16_000.0) as usize;
        let start = start.min(audio.len());
        let end = end.min(audio.len());
        if end > start { &audio[start..end] } else { audio }
    } else {
        audio
    };
    if voiced_harmonic_frames(slice) < 4 {
        transcript.text.clear();
        transcript.words.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_noise(len: usize, rms: f32, seed: u64) -> Vec<i16> {
        let mut state = seed;
        let mut raw = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Sum two uniforms for approximate zero-mean triangular distribution.
            let u1 = ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0;
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u2 = ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0;
            raw.push(u1 + u2);
        }
        let cur_rms = (raw.iter().map(|&x| x * x).sum::<f32>() / len as f32).sqrt();
        let gain = rms / cur_rms.max(1e-6);
        raw.into_iter()
            .map(|x| (x * gain).clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    fn speech_like(len: usize, rms: f32) -> Vec<i16> {
        // Multi-formant harmonic signal with syllabic 4 Hz amplitude modulation.
        let mut raw = Vec::with_capacity(len);
        for i in 0..len {
            let t = i as f32 / 16_000.0;
            let env = 0.5 + 0.5 * (2.0 * PI * 4.0 * t).sin();
            let wave = 0.5 * (2.0 * PI * 300.0 * t).sin()
                + 0.35 * (2.0 * PI * 850.0 * t).sin()
                + 0.25 * (2.0 * PI * 1900.0 * t).sin()
                + 0.15 * (2.0 * PI * 2800.0 * t).sin();
            raw.push(env * wave);
        }
        let cur_rms = (raw.iter().map(|&x| x * x).sum::<f32>() / len as f32).sqrt();
        let gain = rms / cur_rms.max(1e-6);
        raw.into_iter()
            .map(|x| (x * gain).clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    #[test]
    fn silence_dc_and_mains_hum_are_not_speech() {
        let mut vad = EnergyVad::default();
        for _ in 0..50 {
            assert!(!vad.speech(&[0i16; 320]));
        }
        // Pure DC offset.
        for _ in 0..50 {
            let _ = vad.speech(&[4000i16; 320]);
        }
        assert!(!vad.speech(&[4000i16; 320]));
        // 60 Hz hum at -20 dBFS.
        let hum: Vec<i16> = (0..16_000)
            .map(|i| (3000.0 * (2.0 * PI * 60.0 * i as f32 / 16_000.0).sin()) as i16)
            .collect();
        let mut vad = EnergyVad::default();
        let voiced = hum
            .as_chunks::<320>()
            .0
            .iter()
            .enumerate()
            .filter(|&(i, f)| {
                let v = vad.speech(f);
                i >= 5 && v
            })
            .count();
        assert_eq!(voiced, 0, "60 Hz hum must not trigger speech");
    }

    #[test]
    fn quiet_speech_at_minus_50_dbfs_is_detected() {
        let mut vad = EnergyVad::default();
        // -50 dBFS ≈ RMS 103; also test even quieter -55 dBFS ≈ RMS 58.
        for _ in 0..25 {
            assert!(!vad.speech(&[0i16; 320]));
        }
        let quiet = speech_like(16_000, 58.0);
        let voiced = quiet
            .as_chunks::<320>()
            .0
            .iter()
            .filter(|f| vad.speech(*f))
            .count();
        assert!(
            voiced >= 40,
            "expected quiet speech to be detected, got {voiced}/50 frames"
        );
    }

    #[test]
    fn steady_noise_at_minus_40_dbfs_adapts_and_speech_over_it_triggers() {
        let mut vad = EnergyVad::default();
        // 2 seconds of steady -40 dBFS noise (RMS ≈ 328).
        let noise = lcg_noise(32_000, 328.0, 1);
        let mut late_voiced = 0;
        for (i, frame) in noise.as_chunks::<320>().0.iter().enumerate() {
            let v = vad.speech(frame);
            if i >= 45 && v {
                late_voiced += 1;
            }
        }
        assert_eq!(
            late_voiced, 0,
            "steady -40 dBFS noise must settle to non-speech"
        );

        // Now speak at -19.5 dBFS (RMS ≈ 3470) over the same -40 dBFS noise.
        let bg = lcg_noise(16_000, 328.0, 2);
        let talk = speech_like(16_000, 3470.0);
        let mixed: Vec<i16> = bg
            .iter()
            .zip(&talk)
            .map(|(&a, &b)| a.saturating_add(b))
            .collect();
        let voiced = mixed
            .as_chunks::<320>()
            .0
            .iter()
            .filter(|f| vad.speech(*f))
            .count();
        assert!(
            voiced >= 40,
            "speech over -40 dBFS noise must trigger, got {voiced}/50"
        );

        // Returning to steady noise must go quiet again for endpointing.
        let tail = lcg_noise(16_000, 328.0, 3);
        let tail_voiced_after_hangover = tail
            .as_chunks::<320>()
            .0
            .iter()
            .enumerate()
            .filter(|&(i, f)| {
                let v = vad.speech(f);
                i >= 6 && v
            })
            .count();
        assert_eq!(
            tail_voiced_after_hangover, 0,
            "steady noise after speech must endpoint cleanly"
        );
    }

    #[test]
    fn clicks_and_single_tone_bumps_do_not_trigger_vad() {
        let mut vad = EnergyVad::default();
        // 40 ms click inside quiet room must be rejected by 3-frame onset.
        let mut click = vec![0i16; 16_000];
        for (i, sample) in click[4_000..4_500].iter_mut().enumerate() {
            let decay = (-(i as f32) / 120.0).exp();
            *sample = (2000.0 * (2.0 * PI * 600.0 * i as f32 / 16_000.0).sin() * decay) as i16;
        }
        let voiced = click
            .as_chunks::<320>()
            .0
            .iter()
            .filter(|f| vad.speech(*f))
            .count();
        assert_eq!(voiced, 0, "short click must not trigger VAD");

        // Pure 220 Hz single-tone bump excites only 1 band and must be rejected.
        let mut vad = EnergyVad::default();
        let bump: Vec<i16> = (0..16_000)
            .map(|i| (500.0 * (2.0 * PI * 220.0 * i as f32 / 16_000.0).sin()) as i16)
            .collect();
        let voiced = bump
            .as_chunks::<320>()
            .0
            .iter()
            .filter(|f| vad.speech(*f))
            .count();
        assert_eq!(voiced, 0, "single-frequency bump must not trigger VAD");
    }

    #[test]
    fn hallucination_filter_drops_phantom_thank_you_on_nonspeech_but_keeps_real_speech() {
        use jimmy_proto::Word;
        let fake_transcript = || Transcript {
            text: "Thank you.".into(),
            language: "en".into(),
            words: vec![
                Word {
                    text: "Thank".into(),
                    start: 0.0,
                    end: 0.24,
                    probability: 0.9,
                },
                Word {
                    text: "you.".into(),
                    start: 0.24,
                    end: 0.64,
                    probability: 1.0,
                },
            ],
        };

        // Silence, white noise, and single-tone bump all drop phantom "Thank you."
        for clip in [
            vec![0i16; 16_000],
            lcg_noise(16_000, 120.0, 99),
            (0..16_000)
                .map(|i| (400.0 * (2.0 * PI * 220.0 * i as f32 / 16_000.0).sin()) as i16)
                .collect(),
        ] {
            let mut t = fake_transcript();
            filter_hallucination(&clip, &mut t);
            assert!(t.text.is_empty() && t.words.is_empty(), "got {t:?}");
        }

        // Real harmonic multi-formant speech (even quiet -50 dBFS) keeps "Thank you."
        let quiet_speech = speech_like(16_000, 100.0);
        let mut t = fake_transcript();
        filter_hallucination(&quiet_speech, &mut t);
        assert_eq!(t.text, "Thank you.");
        assert_eq!(t.words.len(), 2);

        // Non-phantom phrases are never filtered regardless of audio.
        let mut other = Transcript {
            text: "Open the list.".into(),
            language: "en".into(),
            words: Vec::new(),
        };
        filter_hallucination(&vec![0i16; 16_000], &mut other);
        assert_eq!(other.text, "Open the list.");
    }
}
