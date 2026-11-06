//! Streaming observations of planar audio. This is measurement, not source identification.
//! Peaks/RMS/DC use finite samples only; non-finite input is counted explicitly and disables
//! repair evidence. Stereo correlation is centred Pearson correlation, not uncentred cosine.
use std::fmt;

pub const MAX_CHANNELS: usize = 256;
/// Bound each push's work, without buffering the recording. Call repeatedly for longer inputs.
pub const MAX_BLOCK_SAMPLES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// RMS silence threshold and per-sample near-silence threshold, in dBFS.
    pub silence_dbfs: f64,
    /// Minimum AC RMS for channel-repair evidence, in dBFS.
    pub signal_floor_dbfs: f64,
    /// Minimum RMS difference for a quieter channel already below the signal floor.
    pub one_sided_difference_db: f64,
    pub correlation_threshold: f64,
    /// Difference RMS / stereo RMS allowed for approximately identical channels.
    pub dual_mono_tolerance: f64,
    pub minimum_samples: u64,
    /// Absolute mean sample value; this is window DC evidence, not a persistent-noise diagnosis.
    pub dc_threshold: f64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            silence_dbfs: -80.0,
            signal_floor_dbfs: -60.0,
            one_sided_difference_db: 36.0,
            correlation_threshold: 0.995,
            dual_mono_tolerance: 0.01,
            minimum_samples: 1024,
            dc_threshold: 0.01,
        }
    }
}
impl Options {
    fn check(self) -> Result<Self, InspectError> {
        if !self.silence_dbfs.is_finite()
            || !(-180.0..=0.0).contains(&self.silence_dbfs)
            || !self.signal_floor_dbfs.is_finite()
            || !(self.silence_dbfs..=0.0).contains(&self.signal_floor_dbfs)
            || self.signal_floor_dbfs <= self.silence_dbfs
        {
            return Err(InspectError::InvalidOptions("finite silence < signal floor within -180..0 dBFS required"));
        }
        if !self.one_sided_difference_db.is_finite()
            || !(6.0..=120.0).contains(&self.one_sided_difference_db)
            || !self.correlation_threshold.is_finite()
            || !(0.9..=1.0).contains(&self.correlation_threshold)
            || !self.dual_mono_tolerance.is_finite()
            || !(1e-6..=0.1).contains(&self.dual_mono_tolerance)
            || !self.dc_threshold.is_finite()
            || !(1e-6..=1.0).contains(&self.dc_threshold)
            || !(1..=MAX_BLOCK_SAMPLES as u64).contains(&self.minimum_samples)
        {
            return Err(InspectError::InvalidOptions("invalid evidence threshold or minimum sample count"));
        }
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectError {
    ChannelCount { actual: usize, maximum: usize },
    Shape { expected: usize, actual: usize },
    Ragged { channel: usize, expected: usize, actual: usize },
    BlockTooLarge,
    SampleCountOverflow,
    InvalidOptions(&'static str),
}
impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelCount { actual, maximum } => write!(f, "audio inspection requires 1..{maximum} channels, got {actual}"),
            Self::Shape { expected, actual } => write!(f, "audio channel count changed: expected {expected}, got {actual}"),
            Self::Ragged { channel, expected, actual } => write!(f, "audio channel {channel} has {actual} samples; expected {expected}"),
            Self::BlockTooLarge => write!(f, "audio inspection block exceeds {MAX_BLOCK_SAMPLES} channel samples"),
            Self::SampleCountOverflow => f.write_str("audio inspection sample count overflows"),
            Self::InvalidOptions(reason) => write!(f, "audio inspection options: {reason}"),
        }
    }
}
impl std::error::Error for InspectError {}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelStats {
    pub samples: u64,
    pub finite_samples: u64,
    pub non_finite_samples: u64,
    /// Finite samples with absolute amplitude >= 1.0. These exceed nominal full scale;
    /// floating-point recordings can preserve recoverable headroom above this level.
    /// This count alone does not establish irreversible clipping.
    pub clipped_samples: u64,
    pub near_silent_samples: u64,
    /// Linear full-scale units. Zero with no finite samples; consult finite_samples.
    pub peak: f64,
    pub rms: Option<f64>,
    pub dc: Option<f64>,
    /// RMS after removing the measured mean; excludes constant DC from signal evidence.
    pub ac_rms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StereoStats {
    /// Frames observed in the first two channels, including non-finite pairs.
    pub samples: u64,
    pub finite_pairs: u64,
    /// None when either channel has zero centred variance or no finite pairs exist.
    pub correlation: Option<f64>,
    /// M = (L+R)/2, the mono average.
    pub mid_rms: Option<f64>,
    /// S = (L-R)/2.
    pub side_rms: Option<f64>,
    /// sqrt(mean((L²+R²)/2)), over finite pairs.
    pub stereo_rms: Option<f64>,
    /// Mid power / stereo power. Exact zero means total mono cancellation; None means no power.
    pub mono_power_ratio: Option<f64>,
    /// RMS of L-R, without the mid/side half scaling.
    pub difference_rms: Option<f64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    pub reliable: bool,
    pub silent: bool,
    pub left_only: bool,
    pub right_only: bool,
    pub dual_mono: bool,
    pub inverted: bool,
    /// Low measured correlation; it does not establish unrelated sources or distinct speakers.
    pub unrelated: bool,
    pub dc_offset: bool,
    pub insufficient_signal: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub frames: u64,
    pub channels: Vec<ChannelStats>,
    /// First two channels when at least two exist. Repair evidence requires exactly two.
    pub stereo: Option<StereoStats>,
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, Default)]
struct Moments {
    n: u64,
    mean: f64,
    m2: f64,
}
impl Moments {
    fn push(&mut self, x: f64) {
        // Counts are bounded by Analyzer's checked total before any sample is processed.
        // f32 inputs squared and summed for u64::MAX samples remain finite in f64.
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }
    fn variance(&self) -> Option<f64> {
        (self.n > 0).then(|| (self.m2 / self.n as f64).max(0.0))
    }
    fn rms(&self) -> Option<f64> {
        self.variance().map(|v| (v + self.mean * self.mean).sqrt())
    }
    fn ac_rms(&self) -> Option<f64> {
        self.variance().map(f64::sqrt)
    }
}
#[derive(Clone, Copy, Debug, Default)]
struct Channel {
    moments: Moments,
    non_finite: u64,
    clipped: u64,
    quiet: u64,
    peak: f64,
}
impl Channel {
    fn push(&mut self, sample: f32, silence: f64) {
        if !sample.is_finite() {
            self.non_finite += 1;
            return;
        }
        let x = f64::from(sample);
        let amplitude = x.abs();
        self.peak = self.peak.max(amplitude);
        self.clipped += u64::from(amplitude >= 1.0);
        self.quiet += u64::from(amplitude <= silence);
        self.moments.push(x);
    }
    fn report(&self, samples: u64) -> ChannelStats {
        ChannelStats {
            samples,
            finite_samples: self.moments.n,
            non_finite_samples: self.non_finite,
            clipped_samples: self.clipped,
            near_silent_samples: self.quiet,
            peak: self.peak,
            rms: self.moments.rms(),
            dc: (self.moments.n > 0).then_some(self.moments.mean),
            ac_rms: self.moments.ac_rms(),
        }
    }
}
#[derive(Clone, Debug, Default)]
struct Stereo {
    left: Moments,
    right: Moments,
    mid: Moments,
    side: Moments,
    covariance: f64,
}
impl Stereo {
    fn push(&mut self, l: f32, r: f32) {
        if !l.is_finite() || !r.is_finite() {
            return;
        }
        let (l, r) = (f64::from(l), f64::from(r));
        let dl = l - self.left.mean;
        self.left.push(l);
        self.right.push(r);
        self.covariance += dl * (r - self.right.mean);
        self.mid.push((l + r) * 0.5);
        self.side.push((l - r) * 0.5);
    }
    fn report(&self, samples: u64) -> StereoStats {
        let product = self.left.m2.max(0.0) * self.right.m2.max(0.0);
        let correlation = (product > 0.0).then(|| (self.covariance / product.sqrt()).clamp(-1.0, 1.0));
        let stereo_rms = self.left.rms().zip(self.right.rms()).map(|(l, r)| ((l * l + r * r) * 0.5).sqrt());
        let mid_rms = self.mid.rms();
        let mono_power_ratio = mid_rms.zip(stereo_rms).and_then(|(m, s)| (s > 0.0).then(|| ((m / s) * (m / s)).clamp(0.0, 1.0)));
        StereoStats {
            samples,
            finite_pairs: self.left.n,
            correlation,
            mid_rms,
            side_rms: self.side.rms(),
            stereo_rms,
            mono_power_ratio,
            difference_rms: self.side.rms().map(|s| s * 2.0),
        }
    }
}

/// Streaming, bounded state: construction/report allocate at most MAX_CHANNELS entries; push
/// never allocates. Invalid block shape/size/count is refused before modifying accumulated data.
#[derive(Clone, Debug)]
pub struct Analyzer {
    channels: Vec<Channel>,
    stereo: Stereo,
    frames: u64,
    options: Options,
    silence: f64,
    signal_floor: f64,
}
impl Analyzer {
    pub fn new(channels: usize) -> Result<Self, InspectError> {
        Self::with_options(channels, Options::default())
    }
    pub fn with_options(channels: usize, options: Options) -> Result<Self, InspectError> {
        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(InspectError::ChannelCount { actual: channels, maximum: MAX_CHANNELS });
        }
        let options = options.check()?;
        Ok(Self {
            channels: vec![Channel::default(); channels],
            stereo: Stereo::default(),
            frames: 0,
            silence: 10f64.powf(options.silence_dbfs / 20.0),
            signal_floor: 10f64.powf(options.signal_floor_dbfs / 20.0),
            options,
        })
    }
    pub fn reset(&mut self) {
        self.channels.fill(Channel::default());
        self.stereo = Stereo::default();
        self.frames = 0;
    }
    pub fn push(&mut self, channels: &[&[f32]]) -> Result<(), InspectError> {
        if channels.len() != self.channels.len() {
            return Err(InspectError::Shape { expected: self.channels.len(), actual: channels.len() });
        }
        let frames = channels.first().map_or(0, |c| c.len());
        if channels.len().checked_mul(frames).is_none_or(|n| n > MAX_BLOCK_SAMPLES) {
            return Err(InspectError::BlockTooLarge);
        }
        for (i, channel) in channels.iter().enumerate() {
            if channel.len() != frames {
                return Err(InspectError::Ragged { channel: i, expected: frames, actual: channel.len() });
            }
        }
        let added = u64::try_from(frames).map_err(|_| InspectError::SampleCountOverflow)?;
        let total = self.frames.checked_add(added).ok_or(InspectError::SampleCountOverflow)?;
        // All slice lengths and both channel counts are validated above, once for this hot loop.
        for i in 0..frames {
            for (acc, channel) in self.channels.iter_mut().zip(channels) {
                acc.push(channel[i], self.silence);
            }
            if channels.len() >= 2 {
                self.stereo.push(channels[0][i], channels[1][i]);
            }
        }
        self.frames = total;
        Ok(())
    }
    pub fn report(&self) -> Report {
        let channels = self.channels.iter().map(|c| c.report(self.frames)).collect::<Vec<_>>();
        let stereo = (channels.len() >= 2).then(|| self.stereo.report(self.frames));
        let reliable = self.frames >= self.options.minimum_samples && channels.iter().all(|c| c.non_finite_samples == 0);
        let active = |c: &ChannelStats| c.ac_rms.is_some_and(|r| r >= self.signal_floor);
        let mut evidence = Evidence {
            reliable,
            silent: reliable && channels.iter().all(|c| c.rms.is_some_and(|r| r <= self.silence)),
            dc_offset: reliable && channels.iter().any(|c| c.dc.is_some_and(|d| d.abs() >= self.options.dc_threshold)),
            insufficient_signal: !reliable || !channels.iter().any(active),
            ..Default::default()
        };
        if reliable
            && channels.len() == 2
            && let Some(pair) = &stereo
        {
            let (l, r) = (&channels[0], &channels[1]);
            let quiet = |channel: &ChannelStats, other: &ChannelStats| {
                channel
                    .rms
                    .zip(other.rms)
                    .is_some_and(|(q, a)| q <= self.silence || q <= self.signal_floor && q <= a * 10f64.powf(-self.options.one_sided_difference_db / 20.0))
            };
            evidence.left_only = active(l) && quiet(r, l);
            evidence.right_only = active(r) && quiet(l, r);
            if active(l) && active(r) {
                evidence.dual_mono = pair.correlation.is_some_and(|c| c >= self.options.correlation_threshold)
                    && pair.difference_rms.zip(pair.stereo_rms).is_some_and(|(d, s)| s > 0.0 && d / s <= self.options.dual_mono_tolerance);
                evidence.inverted =
                    pair.correlation.is_some_and(|c| c <= -self.options.correlation_threshold) && pair.mono_power_ratio.is_some_and(|r| r <= 0.05);
                evidence.unrelated = pair.correlation.is_some_and(|c| c.abs() <= 0.2);
            }
        }
        Report { frames: self.frames, channels, stereo, evidence }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, cycles: f64, amplitude: f64) -> Vec<f32> {
        (0..n).map(|i| (amplitude * (i as f64 * cycles * std::f64::consts::TAU / n as f64).sin()) as f32).collect()
    }
    fn measure(channels: &[&[f32]]) -> Report {
        let mut analyzer = Analyzer::new(channels.len()).unwrap();
        analyzer.push(channels).unwrap();
        analyzer.report()
    }
    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-7, "{actual} != {expected}");
    }
    fn no_repair(e: &Evidence) {
        assert!(!e.left_only && !e.right_only && !e.dual_mono && !e.inverted && !e.unrelated, "{e:?}");
    }

    #[test]
    fn identical_sine_has_correct_levels_and_dual_mono_evidence() {
        let x = sine(4096, 32.0, 0.5);
        let report = measure(&[&x, &x]);
        assert_eq!(report.frames, 4096);
        assert_eq!(report.channels[0].finite_samples, 4096);
        close(report.channels[0].rms.unwrap(), 0.5 / 2f64.sqrt());
        close(report.channels[0].dc.unwrap(), 0.0);
        close(report.channels[0].peak, 0.5);
        let stereo = report.stereo.unwrap();
        close(stereo.correlation.unwrap(), 1.0);
        close(stereo.mid_rms.unwrap(), 0.5 / 2f64.sqrt());
        close(stereo.side_rms.unwrap(), 0.0);
        close(stereo.mono_power_ratio.unwrap(), 1.0);
        assert!(report.evidence.reliable && report.evidence.dual_mono);
        assert!(!report.evidence.inverted && !report.evidence.left_only);
    }

    #[test]
    fn left_and_right_only_require_real_signal_and_quiet_opposite_channel() {
        let x = sine(4096, 16.0, 0.5);
        let quiet = sine(4096, 64.0, 1e-5);
        let left = measure(&[&x, &quiet]);
        assert!(left.evidence.left_only && !left.evidence.right_only);
        let right = measure(&[&quiet, &x]);
        assert!(right.evidence.right_only && !right.evidence.left_only);
        let weak = sine(4096, 16.0, 1e-5);
        let zero = vec![0.0; 4096];
        let report = measure(&[&weak, &zero]);
        no_repair(&report.evidence);
        assert!(report.evidence.silent && report.evidence.insufficient_signal);
        let low = sine(4096, 16.0, 0.0005);
        let report = measure(&[&low, &zero]);
        no_repair(&report.evidence);
        assert!(!report.evidence.silent && report.evidence.insufficient_signal);
        let dc = vec![0.25; 4096];
        let report = measure(&[&dc, &zero]);
        no_repair(&report.evidence);
        assert!(report.evidence.dc_offset && report.evidence.insufficient_signal);
        assert_eq!(report.stereo.unwrap().correlation, None);
    }

    #[test]
    fn inverted_and_orthogonal_channels_have_distinct_mono_evidence() {
        let x = sine(4096, 32.0, 0.5);
        let inverted: Vec<_> = x.iter().map(|x| -x).collect();
        let report = measure(&[&x, &inverted]);
        assert!(report.evidence.inverted && !report.evidence.dual_mono);
        let pair = report.stereo.unwrap();
        close(pair.correlation.unwrap(), -1.0);
        close(pair.mono_power_ratio.unwrap(), 0.0);
        close(pair.side_rms.unwrap(), 0.5 / 2f64.sqrt());
        let other = sine(4096, 57.0, 0.5);
        let report = measure(&[&x, &other]);
        assert!(report.evidence.unrelated && !report.evidence.inverted && !report.evidence.dual_mono);
        close(report.stereo.unwrap().correlation.unwrap(), 0.0);
    }

    #[test]
    fn pearson_correlation_is_centred_and_gain_mismatch_is_not_dual_mono() {
        let x = sine(4096, 32.0, 0.25);
        let l: Vec<_> = x.iter().map(|x| x + 0.3).collect();
        let r: Vec<_> = x.iter().map(|x| x * 2.0 - 0.15).collect();
        let report = measure(&[&l, &r]);
        close(report.stereo.unwrap().correlation.unwrap(), 1.0);
        assert!(!report.evidence.dual_mono && report.evidence.dc_offset);
    }

    #[test]
    fn non_finite_and_over_full_scale_counts_preserve_float_headroom() {
        let x = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0, -1.0, 2.5, -3.25, 0.0, 0.5];
        let report = measure(&[&x, &x]);
        let c = &report.channels[0];
        assert_eq!((c.samples, c.finite_samples, c.non_finite_samples, c.clipped_samples), (9, 6, 3, 4));
        close(c.peak, 3.25);
        close(c.rms.unwrap(), ((1.0f64 + 1.0 + 6.25 + 10.5625 + 0.25) / 6.0).sqrt());
        assert!(!report.evidence.reliable);
        no_repair(&report.evidence);
        assert_eq!(report.stereo.unwrap().finite_pairs, 6);
        let extremes = [f32::MAX, -f32::MAX];
        let report = measure(&[&extremes, &extremes]);
        assert!(report.channels[0].rms.unwrap().is_finite());
        assert!(report.stereo.unwrap().correlation.unwrap().is_finite());
    }

    #[test]
    fn block_splits_and_reset_produce_identical_reports() {
        let mut x = sine(20_000, 37.0, 0.37);
        x[1789] = f32::NAN;
        let y = sine(20_000, 43.0, 0.13);
        let expected = measure(&[&x, &y]);
        let mut analyzer = Analyzer::new(2).unwrap();
        let mut offset = 0;
        for size in [0, 1, 1024, 13, 0, 47, 10000, 34, 8881] {
            let end = (offset + size).min(x.len());
            analyzer.push(&[&x[offset..end], &y[offset..end]]).unwrap();
            offset = end;
        }
        analyzer.push(&[&x[offset..], &y[offset..]]).unwrap();
        assert_eq!(analyzer.report(), expected);
        analyzer.reset();
        assert_eq!(analyzer.report(), Analyzer::new(2).unwrap().report());
        analyzer.push(&[&x, &y]).unwrap();
        assert_eq!(analyzer.report(), expected);
    }

    #[test]
    fn invalid_blocks_fail_atomically_and_hostile_options_never_panic() {
        for channels in [0, MAX_CHANNELS + 1, usize::MAX] {
            assert!(Analyzer::new(channels).is_err());
        }
        let mut a = Analyzer::new(2).unwrap();
        a.push(&[&[0.1, 0.2], &[0.3, 0.4]]).unwrap();
        let initial = a.report();
        assert!(a.push(&[]).is_err());
        assert!(a.push(&[&[0.0], &[]]).is_err());
        let huge = vec![0.0; MAX_BLOCK_SAMPLES / 2 + 1];
        assert_eq!(a.push(&[&huge, &huge]), Err(InspectError::BlockTooLarge));
        assert_eq!(a.report(), initial);
        a.frames = u64::MAX;
        let before = a.report();
        assert_eq!(a.push(&[&[0.0], &[0.0]]), Err(InspectError::SampleCountOverflow));
        assert_eq!(a.report(), before);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1e300, 1e300] {
            let invalid = [
                Options { silence_dbfs: value, ..Default::default() },
                Options { signal_floor_dbfs: value, ..Default::default() },
                Options { one_sided_difference_db: value, ..Default::default() },
                Options { correlation_threshold: value, ..Default::default() },
                Options { dual_mono_tolerance: value, ..Default::default() },
                Options { dc_threshold: value, ..Default::default() },
            ];
            for options in invalid {
                assert!(std::panic::catch_unwind(|| Analyzer::with_options(2, options)).unwrap().is_err());
            }
        }
        assert!(Analyzer::with_options(2, Options { minimum_samples: 0, ..Default::default() }).is_err());
        assert!(Analyzer::with_options(2, Options { silence_dbfs: -20.0, signal_floor_dbfs: -30.0, ..Default::default() }).is_err());
    }

    #[test]
    fn mono_multichannel_and_empty_inputs_have_explicit_evidence() {
        let x = sine(2048, 32.0, 0.25);
        let mono = measure(&[&x]);
        assert!(mono.stereo.is_none());
        no_repair(&mono.evidence);
        let mut empty = Analyzer::new(2).unwrap();
        empty.push(&[&[], &[]]).unwrap();
        let report = empty.report();
        assert_eq!(report.frames, 0);
        assert_eq!(report.channels[0].rms, None);
        assert!(report.evidence.insufficient_signal);
        no_repair(&report.evidence);
        let slices = vec![x.as_slice(); MAX_CHANNELS];
        let report = measure(&slices);
        assert_eq!(report.channels.len(), MAX_CHANNELS);
        assert!(report.stereo.is_some() && report.evidence.reliable);
        no_repair(&report.evidence);
    }
}
