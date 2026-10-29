use audio_stream_protocol::CALIBRATION_FREQUENCIES;

pub struct LevelWindow {
    count: u32,
    sum: f64,
    squares: f64,
    clipped: bool,
    bins: [(f64, f64, f64); 6],
}

impl Default for LevelWindow {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            squares: 0.0,
            clipped: false,
            bins: CALIBRATION_FREQUENCIES.map(|frequency| {
                (
                    2.0 * (std::f64::consts::TAU * frequency / 48_000.0).cos(),
                    0.0,
                    0.0,
                )
            }),
        }
    }
}

impl LevelWindow {
    pub fn push(&mut self, sample: f64) {
        self.clipped |= !sample.is_finite() || sample.abs() >= 0.98;
        self.count += 1;
        self.sum += sample;
        self.squares = f64::mul_add(sample, sample, self.squares);
        for (coefficient, previous, older) in &mut self.bins {
            let next = f64::mul_add(*coefficient, *previous, sample) - *older;
            *older = *previous;
            *previous = next;
        }
    }

    pub fn tone_present(&self) -> bool {
        if self.count == 0 || self.clipped {
            return false;
        }
        let count = f64::from(self.count);
        let total = (self.sum / count)
            .mul_add(-(self.sum / count), self.squares / count)
            .max(1e-12);
        let powers = self.bins.map(|(coefficient, previous, older)| {
            2.0 * f64::mul_add(
                coefficient * previous,
                -older,
                f64::mul_add(older, older, previous * previous),
            ) / (count * count)
        });
        powers.iter().sum::<f64>() >= total * 0.65
            && powers.iter().filter(|&&p| p >= total * 0.01).count() >= 3
    }

    pub fn finish(&self) -> Result<f64, &'static str> {
        if self.count == 0 {
            return Err("No microphone samples received");
        }
        if self.clipped {
            return Err("Microphone clipped; lower its gain or move it farther away");
        }
        let mean = self.sum / f64::from(self.count);
        let power = mean
            .mul_add(-mean, self.squares / f64::from(self.count))
            .max(1e-12);
        Ok(10.0 * power.log10())
    }
}

#[cfg(windows)]
pub fn median_level(levels: &mut [f64]) -> Result<(f64, f64), &'static str> {
    if levels.len() < 8 {
        return Err("Insufficient microphone data");
    }
    levels.sort_by(f64::total_cmp);
    Ok((
        levels[levels.len() / 2],
        levels[levels.len() - 2] - levels[1],
    ))
}

pub fn signal_level(measured: f64, background: f64) -> Result<f64, &'static str> {
    if measured - background < 10.0 {
        return Err("Signal is too close to ambient noise");
    }
    Ok(10.0 * (10_f64.powf(measured / 10.0) - 10_f64.powf(background / 10.0)).log10())
}

pub fn closest_step(previous: (u16, f64), current: (u16, f64), target: f64) -> (u16, f64) {
    if (previous.1 - target).abs() <= (current.1 - target).abs() {
        previous
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_rms_removes_dc_and_measures_relative_db() {
        let mut low = LevelWindow::default();
        let mut high = LevelWindow::default();
        for i in 0..4800 {
            let sample = (f64::from(i) * std::f64::consts::TAU / 48.0).sin() * 0.1;
            low.push(sample + 0.2);
            high.push(sample.mul_add(2.0, 0.2));
        }
        assert!((high.finish().unwrap() - low.finish().unwrap() - 6.0206).abs() < 0.01);
    }

    #[test]
    fn clipped_or_noise_dominated_measurements_are_rejected() {
        let mut window = LevelWindow::default();
        window.push(1.0);
        assert!(window.finish().is_err());
        assert!(signal_level(-40.0, -45.0).is_err());
    }

    #[test]
    fn calibration_detection_rejects_unrelated_steady_sound() {
        let mut stimulus = LevelWindow::default();
        let mut unrelated = LevelWindow::default();
        for frame in 0..4800 {
            let t = f64::from(frame) / 48_000.0;
            stimulus.push(
                CALIBRATION_FREQUENCIES
                    .iter()
                    .map(|f| (std::f64::consts::TAU * f * t).sin())
                    .sum::<f64>()
                    * 0.01,
            );
            unrelated.push(0.1 * (std::f64::consts::TAU * 1000.0 * t).sin());
        }
        assert!(stimulus.tone_present());
        assert!(!unrelated.tone_present());
    }

    #[test]
    fn discrete_step_matching_uses_nearest_not_always_louder() {
        assert_eq!(closest_step((4, -32.0), (5, -26.0), -30.0), (4, -32.0));
    }
}
