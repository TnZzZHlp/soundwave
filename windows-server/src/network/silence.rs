use audio_stream_protocol::SAMPLE_RATE;

pub(super) struct SilenceDetector {
    silent_since: Option<u64>,
    idle: bool,
}

impl SilenceDetector {
    pub(super) fn new() -> Self {
        Self {
            silent_since: None,
            idle: false,
        }
    }

    pub(super) fn update(&mut self, timestamp: u64, silent: bool, enabled: bool) -> (bool, bool) {
        let was_idle = self.idle;
        if !enabled {
            self.silent_since = None;
            self.idle = true;
        } else if silent {
            let since = *self.silent_since.get_or_insert(timestamp);
            self.idle =
                self.idle || timestamp.wrapping_sub(since) >= u64::from(SAMPLE_RATE) * 300 / 1_000;
        } else {
            self.silent_since = None;
            self.idle = false;
        }
        (!self.idle, self.idle && !was_idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_sleeps_after_grace_and_preserves_first_nonzero_block() {
        let mut detector = SilenceDetector::new();
        assert_eq!(detector.update(0, false, true), (true, false));
        assert_eq!(detector.update(480, true, true), (true, false));
        assert_eq!(detector.update(14_400, true, true), (true, false));
        assert_eq!(detector.update(14_880, true, true), (false, true));
        assert_eq!(detector.update(15_360, true, true), (false, false));
        assert_eq!(detector.update(15_840, false, true), (true, false));
    }

    #[test]
    fn disable_sleeps_immediately_and_reenable_waits_for_sound() {
        let mut detector = SilenceDetector::new();
        assert_eq!(detector.update(0, false, false), (false, true));
        assert_eq!(detector.update(480, true, true), (false, false));
        assert_eq!(detector.update(960, false, true), (true, false));
    }
}
