//! Latency measurement with clicks aligned to the system's monotonic clock.
//!
//! A sender in test mode replaces its microphone with silence plus one short
//! click per second, placed so that each click is "captured" exactly on a
//! whole second of `CLOCK_MONOTONIC`. Any receiver on the same machine can
//! then tell how old a click is from the time it would be played: the delay
//! is simply that time modulo one second. No coordination is needed between
//! the two sides, and the delay must stay below one second.

pub const SAMPLE_RATE: u64 = 48_000;
const NS_PER_SEC: u64 = 1_000_000_000;
/// Distance between clicks.
pub const CLICK_PERIOD_NS: u64 = NS_PER_SEC;
const CLICK_LENGTH_NS: u64 = 5_000_000;
const CLICK_FREQ_HZ: f64 = 2_000.0;
const CLICK_AMPLITUDE: f64 = 0.5 * i16::MAX as f64;

/// A sample this loud starts a click.
const ONSET_THRESHOLD: i16 = (0.1 * i16::MAX as f64) as i16;
/// Samples quieter than this count as silence between clicks.
const QUIET_THRESHOLD: i16 = (0.02 * i16::MAX as f64) as i16;
/// Silence required before the next click is accepted, so that the tail of
/// one click is not taken for the start of another.
const QUIET_SAMPLES: u64 = SAMPLE_RATE / 10;

fn sample_time(first_sample_ns: u64, index: usize) -> u64 {
    first_sample_ns + index as u64 * NS_PER_SEC / SAMPLE_RATE
}

/// Fills `samples` with silence and clicks. `first_sample_ns` is the
/// monotonic time at which the first sample is captured.
pub fn write_clicks(samples: &mut [i16], first_sample_ns: u64) {
    for (i, sample) in samples.iter_mut().enumerate() {
        let phase_ns = sample_time(first_sample_ns, i) % CLICK_PERIOD_NS;
        *sample = if phase_ns < CLICK_LENGTH_NS {
            let t = phase_ns as f64 / NS_PER_SEC as f64;
            (CLICK_AMPLITUDE * (std::f64::consts::TAU * CLICK_FREQ_HZ * t).sin()) as i16
        } else {
            0
        };
    }
}

/// A click is over well within this long after its onset. Anything still
/// sounding by then is speech or a tone, not a measurement click.
const VERDICT_SAMPLES: u64 = SAMPLE_RATE * 30 / 1000;
/// Silence required just before the verdict for the onset to count.
const VERDICT_QUIET_SAMPLES: u64 = SAMPLE_RATE * 5 / 1000;

/// Finds measurement clicks in a stream of mono 48 kHz samples.
pub struct ClickDetector {
    quiet_run: u64,
    /// Set after enough silence; the next loud sample is then an onset.
    armed: bool,
    /// An onset waiting to be confirmed as a click: its delay, and the
    /// number of samples seen since.
    candidate: Option<(u64, u64)>,
}

impl Default for ClickDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClickDetector {
    pub fn new() -> Self {
        // Start disarmed so that joining in the middle of a click does not
        // count as an onset.
        Self {
            quiet_run: 0,
            armed: false,
            candidate: None,
        }
    }

    /// Feeds one buffer whose first sample is played at `first_sample_ns`
    /// (monotonic). Returns the delay of a click once it has been confirmed,
    /// which is shortly after it ended.
    pub fn process(&mut self, samples: &[i16], first_sample_ns: u64) -> Option<u64> {
        let mut confirmed = None;
        for (i, &sample) in samples.iter().enumerate() {
            let level = sample.saturating_abs();
            if level < QUIET_THRESHOLD {
                self.quiet_run += 1;
                self.armed |= self.quiet_run >= QUIET_SAMPLES;
            } else {
                // A codec smooths the start of a click, so the level passes
                // through this in-between range on its way up. That must not
                // disarm the detector.
                self.quiet_run = 0;
                if self.armed && level >= ONSET_THRESHOLD {
                    self.armed = false;
                    let delay = sample_time(first_sample_ns, i) % CLICK_PERIOD_NS;
                    self.candidate = Some((delay, 0));
                }
            }
            if let Some((delay, age)) = &mut self.candidate {
                *age += 1;
                if *age >= VERDICT_SAMPLES {
                    if self.quiet_run >= VERDICT_QUIET_SAMPLES {
                        confirmed = Some(*delay);
                    }
                    self.candidate = None;
                }
            }
        }
        confirmed
    }
}

/// Collected click delays, in nanoseconds.
#[derive(Default)]
pub struct Summary {
    delays_ns: Vec<u64>,
}

impl Summary {
    pub fn add(&mut self, delay_ns: u64) {
        self.delays_ns.push(delay_ns);
    }

    /// Minimum, median and maximum in milliseconds, with the sample count.
    pub fn stats(&self) -> Option<(f64, f64, f64, usize)> {
        let mut sorted = self.delays_ns.clone();
        sorted.sort_unstable();
        let ms = |ns: u64| ns as f64 / 1e6;
        Some((
            ms(*sorted.first()?),
            ms(sorted[sorted.len() / 2]),
            ms(*sorted.last()?),
            sorted.len(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUFFER: usize = 480;
    const BUFFER_NS: u64 = 10_000_000;

    /// Sends generated clicks through a pure delay and returns what the
    /// detector reports.
    fn measure(start_ns: u64, delay_ns: u64, buffers: u64) -> Vec<u64> {
        let mut detector = ClickDetector::new();
        let mut delays = Vec::new();
        for n in 0..buffers {
            let captured_at = start_ns + n * BUFFER_NS;
            let mut samples = [0i16; BUFFER];
            write_clicks(&mut samples, captured_at);
            delays.extend(detector.process(&samples, captured_at + delay_ns));
        }
        delays
    }

    #[test]
    fn clicks_start_on_whole_seconds() {
        let mut samples = [0i16; BUFFER];
        write_clicks(&mut samples, 7 * NS_PER_SEC - BUFFER_NS / 2);
        let first_loud = samples.iter().position(|s| s.abs() >= ONSET_THRESHOLD);
        // The click begins halfway through the buffer; the sine needs a few
        // samples to rise above the threshold.
        assert!(matches!(first_loud, Some(240..=244)), "{first_loud:?}");
        assert!(samples[..240].iter().all(|&s| s == 0));
    }

    #[test]
    fn detector_recovers_the_delay() {
        let delay_ns = 73_000_000;
        let delays = measure(41 * NS_PER_SEC + 123_456_789, delay_ns, 350);
        assert_eq!(delays.len(), 3, "one click per second over 3.5 s");
        for measured in delays {
            let error = measured.abs_diff(delay_ns);
            assert!(error < 200_000, "measured {measured} ns");
        }
    }

    #[test]
    fn a_click_already_in_progress_is_not_an_onset() {
        // Starting 1 ms into a click: the detector has seen no silence yet.
        let delays = measure(5 * NS_PER_SEC + 1_000_000, 0, 50);
        assert!(delays.is_empty());
    }

    #[test]
    fn a_click_with_a_soft_start_is_still_found() {
        let mut detector = ClickDetector::new();
        let mut samples = vec![0i16; 2 * SAMPLE_RATE as usize];
        write_clicks(&mut samples, 0);
        // Fade the click at 1 s in over 1 ms, as a codec would.
        let click_start = SAMPLE_RATE as usize;
        for i in 0..48 {
            samples[click_start + i] = (samples[click_start + i] as i32 * i as i32 / 48) as i16;
        }
        let delay = detector.process(&samples[4800..], 100_000_000);
        assert!(matches!(delay, Some(0..1_000_000)), "{delay:?}");
    }

    #[test]
    fn sound_that_keeps_going_is_not_a_click() {
        let mut detector = ClickDetector::new();
        let mut samples = vec![0i16; SAMPLE_RATE as usize];
        // Half a second of silence, then a steady tone, like speech starting.
        for (i, sample) in samples.iter_mut().enumerate().skip(24_000) {
            *sample = (10_000.0 * (i as f64 * 0.2).sin()) as i16;
        }
        assert_eq!(detector.process(&samples, 0), None);
    }

    #[test]
    fn summary_reports_min_median_max() {
        let mut summary = Summary::default();
        assert!(summary.stats().is_none());
        for ms in [80, 70, 90] {
            summary.add(ms * 1_000_000);
        }
        assert_eq!(summary.stats(), Some((70.0, 80.0, 90.0, 3)));
    }
}
