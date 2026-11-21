//! Streaming, frame-boundary-independent resampling for mono PCM.
//!
//! Downsampling (`from > to`) uses exact boxcar area-averaging to attenuate
//! out-of-band energy. Upsampling (`from < to`) uses streaming linear
//! interpolation to avoid zero-order-hold staircase imaging.
#[derive(Debug)]
pub struct Resampler {
    from: u32,
    to: u32,
    /// Downsampling accumulator: weight consumed in the current output bin (0..from).
    used: u32,
    sum: i64,
    /// Upsampling state: previous input sample and fractional position (0..to).
    prev: Option<i16>,
    frac: u32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        assert!(from > 0 && to > 0);
        Self {
            from,
            to,
            used: 0,
            sum: 0,
            prev: None,
            frac: 0,
        }
    }

    pub fn push(&mut self, samples: &[i16], out: &mut Vec<i16>) {
        if self.from == self.to {
            out.extend_from_slice(samples);
            return;
        }
        if self.from < self.to {
            for &sample in samples {
                let Some(prev) = self.prev else {
                    self.prev = Some(sample);
                    continue;
                };
                let a = i64::from(prev);
                let b = i64::from(sample);
                let diff = b - a;
                let to = i64::from(self.to);
                while self.frac < self.to {
                    let interp = a + (diff * i64::from(self.frac)) / to;
                    out.push(interp as i16);
                    self.frac += self.from;
                }
                self.frac -= self.to;
                self.prev = Some(sample);
            }
            return;
        }
        for &sample in samples {
            let mut remaining = self.to;
            while remaining > 0 {
                let weight = remaining.min(self.from - self.used);
                self.sum += i64::from(sample) * i64::from(weight);
                self.used += weight;
                remaining -= weight;
                if self.used == self.from {
                    out.push((self.sum / i64::from(self.from)) as i16);
                    self.used = 0;
                    self.sum = 0;
                }
            }
        }
    }

    pub fn finish(&mut self, out: &mut Vec<i16>) {
        if self.from < self.to {
            if let Some(prev) = self.prev.take() {
                while self.frac < self.to {
                    out.push(prev);
                    self.frac += self.from;
                }
            }
            self.frac = 0;
            return;
        }
        if self.used > 0 {
            out.push((self.sum / i64::from(self.used)) as i16);
            self.used = 0;
            self.sum = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsampling_interpolates_smoothly_and_is_chunk_invariant() {
        let ramp: Vec<i16> = (0..100).map(|i| (i * 100) as i16).collect();
        let mut contiguous = Vec::new();
        let mut resampler = Resampler::new(24_000, 48_000);
        resampler.push(&ramp, &mut contiguous);
        resampler.finish(&mut contiguous);
        assert_eq!(contiguous.len(), 200);
        assert_eq!(&contiguous[..6], &[0, 50, 100, 150, 200, 250]);

        let mut split = Vec::new();
        for chunk in ramp.chunks(7) {
            resampler.push(chunk, &mut split);
        }
        resampler.finish(&mut split);
        assert_eq!(split, contiguous);

        // Non-integer ratio (24 kHz -> 44.1 kHz) must also be chunk-invariant.
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut r = Resampler::new(24_000, 44_100);
        r.push(&ramp, &mut a);
        r.finish(&mut a);
        for chunk in ramp.chunks(13) {
            r.push(chunk, &mut b);
        }
        r.finish(&mut b);
        assert_eq!(a, b);
    }
}
