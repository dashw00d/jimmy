//! Streaming, frame-boundary-independent box resampling for mono PCM.
#[derive(Debug)]
pub struct Resampler {
    from: u32,
    to: u32,
    used: u32,
    sum: i64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        assert!(from > 0 && to > 0);
        Self {
            from,
            to,
            used: 0,
            sum: 0,
        }
    }

    pub fn push(&mut self, samples: &[i16], out: &mut Vec<i16>) {
        if self.from == self.to {
            out.extend_from_slice(samples);
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
        if self.used > 0 {
            out.push((self.sum / i64::from(self.used)) as i16);
            self.used = 0;
            self.sum = 0;
        }
    }
}
