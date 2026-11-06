//! Dedicated, non-destructive stereo routing. The modes describe a fixed two-channel matrix;
//! gains and polarity follow routing. Floating-point headroom is preserved above nominal full
//! scale. A genuinely mono buffer remains mono and uses the left output gain/polarity only.
use crate::{AudioEffect, ParamSpec, ParamValues, Smoothed, Unit, block_len, db_to_gain, param_plumbing};

pub struct ChannelRouter {
    pv: ParamValues,
    channels: usize,
    matrix: [Smoothed; 4],
    gains: [Smoothed; 2],
}

impl ChannelRouter {
    /// Stable indices are shared with the project catalogue and agent commands.
    pub const MODES: &'static [&'static str] =
        &["Stereo", "Left to Both", "Right to Both", "Mono Average", "Swap", "Mid/Side Encode", "Mid/Side Decode", "Side to Both"];
    pub const PARAMS: &'static [ParamSpec] = &[
        ParamSpec::choice("mode", "Mode", Self::MODES, 0),
        ParamSpec::new("gain_l_db", "Left Output Gain", -60.0, 24.0, 0.0, Unit::Decibels),
        ParamSpec::new("gain_r_db", "Right Output Gain", -60.0, 24.0, 0.0, Unit::Decibels),
        ParamSpec::toggle("invert_l", "Invert Left", false),
        ParamSpec::toggle("invert_r", "Invert Right", false),
    ];

    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let rate = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate.clamp(1.0, 768_000.0) } else { 48_000.0 };
        let mut s = Self {
            pv: ParamValues::new(Self::PARAMS),
            channels: channels.min(2),
            matrix: [Smoothed::with_ms(0.0, rate, 5.0); 4],
            gains: [Smoothed::with_ms(1.0, rate, 5.0); 2],
        };
        s.apply_params(true);
        s
    }

    fn apply_params(&mut self, snap: bool) {
        let matrix = match self.pv.idx("mode") {
            1 => [1.0, 0.0, 1.0, 0.0],
            2 => [0.0, 1.0, 0.0, 1.0],
            3 => [0.5, 0.5, 0.5, 0.5],
            4 => [0.0, 1.0, 1.0, 0.0],
            5 => [0.5, 0.5, 0.5, -0.5],
            6 => [1.0, 1.0, 1.0, -1.0],
            7 => [0.5, -0.5, 0.5, -0.5],
            _ => [1.0, 0.0, 0.0, 1.0],
        };
        for (smoothed, value) in self.matrix.iter_mut().zip(matrix) {
            smoothed.set(value);
        }
        for (i, (gain_id, invert_id)) in [("gain_l_db", "invert_l"), ("gain_r_db", "invert_r")].into_iter().enumerate() {
            self.gains[i].set(db_to_gain(self.pv.v(gain_id)) * if self.pv.on(invert_id) { -1.0 } else { 1.0 });
        }
        if snap {
            self.reset();
        }
    }
}

// Accumulating/multiplying in f64 avoids intermediate f32 overflow. Saturating only at f32's
// representable limit retains recoverable float headroom; this is deliberately not ±1 clipping.
fn representable(x: f64) -> f32 {
    x.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
}
fn finite(x: f32) -> f64 {
    if x.is_finite() { f64::from(x) } else { 0.0 }
}

impl AudioEffect for ChannelRouter {
    param_plumbing!("channel_router");
    fn reset(&mut self) {
        self.matrix.iter_mut().for_each(Smoothed::snap);
        self.gains.iter_mut().for_each(Smoothed::snap);
    }

    fn process(&mut self, channels: &mut [&mut [f32]]) {
        let n = block_len(channels);
        let active = self.channels.min(channels.len());
        if active == 0 || n == 0 {
            return;
        }
        // block_len validated every participating slice length once. Only first two outputs
        // are routed; any surround/auxiliary channels and ragged tails remain untouched.
        if active == 1 {
            let Some(channel) = channels.first_mut() else { return };
            for sample in channel.iter_mut().take(n) {
                self.matrix.iter_mut().for_each(|m| {
                    m.tick();
                });
                let gain = self.gains[0].tick();
                self.gains[1].tick();
                *sample = representable(finite(*sample) * f64::from(gain));
            }
            return;
        }
        let (left, right) = channels.split_at_mut(1);
        let (l, r) = (&mut left[0], &mut right[0]);
        for i in 0..n {
            let [a, b, c, d] = self.matrix.each_mut().map(Smoothed::tick).map(f64::from);
            let [gl, gr] = self.gains.each_mut().map(Smoothed::tick).map(f64::from);
            let (input_l, input_r) = (finite(l[i]), finite(r[i]));
            l[i] = representable((a * input_l + b * input_r) * gl);
            r[i] = representable((c * input_l + d * input_r) * gr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(mode: usize) -> ChannelRouter {
        let mut s = ChannelRouter::new(48000.0, 2);
        assert!(s.set_param("mode", mode as f32));
        s.reset();
        s
    }
    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
    }
    fn sine(n: usize, cycles: f64, amplitude: f64) -> Vec<f32> {
        (0..n).map(|i| (amplitude * (i as f64 * cycles * std::f64::consts::TAU / n as f64).sin()) as f32).collect()
    }

    #[test]
    fn every_mode_matches_its_matrix_on_rendered_sines() {
        let input_l = sine(4096, 32.0, 0.5);
        let input_r = sine(4096, 53.0, 0.3);
        let matrices = [
            [1.0, 0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0, 0.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.5, 0.5, 0.5, 0.5],
            [0.0, 1.0, 1.0, 0.0],
            [0.5, 0.5, 0.5, -0.5],
            [1.0, 1.0, 1.0, -1.0],
            [0.5, -0.5, 0.5, -0.5],
        ];
        for (mode, [a, b, c, d]) in matrices.into_iter().enumerate() {
            let (mut l, mut r) = (input_l.clone(), input_r.clone());
            router(mode).process(&mut [&mut l, &mut r]);
            for i in 0..l.len() {
                close(l[i], a * input_l[i] + b * input_r[i]);
                close(r[i], c * input_l[i] + d * input_r[i]);
            }
        }
    }

    #[test]
    fn single_mic_routes_to_both_without_attenuation_or_float_clipping() {
        let input = [0.25, 2.5, -3.25, 0.0, -0.75];
        for mode in [1, 2] {
            let (mut l, mut r) = if mode == 1 { (input, [0.0; 5]) } else { ([0.0; 5], input) };
            let mut fx = router(mode);
            fx.process(&mut [&mut l, &mut r]);
            assert_eq!(l, input);
            assert_eq!(r, input);
            fx.set_param("gain_l_db", -12.0);
            fx.set_param("gain_r_db", -12.0);
            fx.reset();
            // Read the preserved floating recording again; attenuating before PCM conversion
            // recovers its headroom instead of reproducing previously clipped plateaux.
            (l, r) = if mode == 1 { (input, [0.0; 5]) } else { ([0.0; 5], input) };
            fx.process(&mut [&mut l, &mut r]);
            for i in 0..input.len() {
                close(l[i], input[i] * db_to_gain(-12.0));
                assert_eq!(l[i], r[i]);
                assert!(l[i].abs() < 1.0);
            }
        }
    }

    #[test]
    fn mid_side_roundtrip_and_side_routing_are_normalized() {
        let (input_l, input_r) = (sine(4096, 32.0, 0.5), sine(4096, 53.0, 0.3));
        let (mut l, mut r) = (input_l.clone(), input_r.clone());
        router(5).process(&mut [&mut l, &mut r]);
        router(6).process(&mut [&mut l, &mut r]);
        for i in 0..l.len() {
            close(l[i], input_l[i]);
            close(r[i], input_r[i]);
        }
        let mut common = input_l.clone();
        let mut common2 = input_l.clone();
        router(7).process(&mut [&mut common, &mut common2]);
        assert!(common.iter().chain(&common2).all(|&x| x == 0.0));
    }

    #[test]
    fn output_gains_and_polarity_follow_routing_with_finite_extremes() {
        let mut fx = router(1);
        assert!(fx.set_param("gain_l_db", 6.0));
        assert!(fx.set_param("gain_r_db", -6.0));
        assert!(fx.set_param("invert_r", 1.0));
        fx.reset();
        let (mut l, mut r) = ([0.5, -2.5, f32::MAX], [0.0; 3]);
        fx.process(&mut [&mut l, &mut r]);
        close(l[0], 0.5 * db_to_gain(6.0));
        close(r[0], -0.5 * db_to_gain(-6.0));
        close(l[1], -2.5 * db_to_gain(6.0));
        assert!(l[2].is_finite() && r[2].is_finite());
        assert_eq!(l[2], f32::MAX);
        let (mut l, mut r) = ([f32::NAN, f32::INFINITY, f32::NEG_INFINITY], [1.0; 3]);
        fx.process(&mut [&mut l, &mut r]);
        assert_eq!(l, [0.0; 3]);
        assert_eq!(r, [0.0; 3]);
    }

    #[test]
    fn mono_extra_channels_ragged_and_empty_blocks_never_invent_outputs() {
        for mode in 0..ChannelRouter::MODES.len() {
            let mut fx = router(mode);
            fx.set_param("gain_l_db", -6.0);
            fx.reset();
            let mut mono = [0.25, -2.5];
            fx.process(&mut [&mut mono]);
            close(mono[0], 0.25 * db_to_gain(-6.0));
            close(mono[1], -2.5 * db_to_gain(-6.0));
            fx.process(&mut []);
            fx.process(&mut [&mut []]);
        }
        let mut fx = router(1);
        let (mut l, mut r, mut aux) = ([0.25, 0.5, 0.75], [0.0, 0.0], [0.33, 0.66, 0.99]);
        fx.process(&mut [&mut l, &mut r, &mut aux]);
        assert_eq!(l, [0.25, 0.5, 0.75]);
        assert_eq!(r, [0.25, 0.5]);
        assert_eq!(aux, [0.33, 0.66, 0.99]);
        let mut fx = ChannelRouter::new(48000.0, 1);
        fx.set_param("mode", 2.0);
        fx.reset();
        let (mut l, mut r) = ([0.5; 4], [0.25; 4]);
        fx.process(&mut [&mut l, &mut r]);
        assert_eq!(l, [0.5; 4]);
        assert_eq!(r, [0.25; 4]);
        let mut zero = ChannelRouter::new(48000.0, 0);
        zero.process(&mut [&mut l, &mut r]);
        assert_eq!(l, [0.5; 4]);
    }

    fn process_chunks(fx: &mut ChannelRouter, l: &mut [f32], r: &mut [f32], chunks: &[usize]) {
        let mut offset = 0;
        for &size in chunks {
            let end = (offset + size).min(l.len());
            fx.process(&mut [&mut l[offset..end], &mut r[offset..end]]);
            offset = end;
        }
        fx.process(&mut [&mut l[offset..], &mut r[offset..]]);
    }

    #[test]
    fn parameter_changes_are_sample_and_block_invariant() {
        let input = (sine(2048, 32.0, 0.5), sine(2048, 53.0, 0.3));
        let (mut l, mut r) = input.clone();
        let (mut split_l, mut split_r) = input;
        let (mut whole, mut split) = (ChannelRouter::new(48000.0, 2), ChannelRouter::new(48000.0, 2));
        for fx in [&mut whole, &mut split] {
            fx.set_param("mode", 1.0);
            fx.set_param("gain_r_db", -3.0);
        }
        whole.process(&mut [&mut l[..127], &mut r[..127]]);
        process_chunks(&mut split, &mut split_l[..127], &mut split_r[..127], &[0, 1, 4, 16, 0, 19]);
        for fx in [&mut whole, &mut split] {
            fx.set_param("mode", 4.0);
            fx.set_param("invert_l", 1.0);
            fx.set_param("gain_r_db", 3.0);
        }
        whole.process(&mut [&mut l[127..], &mut r[127..]]);
        process_chunks(&mut split, &mut split_l[127..], &mut split_r[127..], &[0, 17, 0, 43, 3, 256, 1, 19]);
        assert_eq!(l, split_l);
        assert_eq!(r, split_r);
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    }

    #[test]
    fn hostile_parameters_and_rates_are_bounded_and_unknown_ids_fail() {
        for rate in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
            let mut fx = ChannelRouter::new(rate, usize::MAX);
            for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, -f32::MAX] {
                for param in ChannelRouter::PARAMS {
                    assert!(fx.set_param(param.id, value));
                    assert!(fx.param(param.id).unwrap().is_finite());
                }
                fx.reset();
                let (mut l, mut r) = ([f32::MAX, -f32::MAX, 2.5], [-f32::MAX, f32::MAX, -3.25]);
                fx.process(&mut [&mut l, &mut r]);
                assert!(l.iter().chain(&r).all(|x| x.is_finite()));
            }
            assert!(!fx.set_param("unknown", 1.0));
            assert_eq!(fx.param("unknown"), None);
        }
    }

    #[test]
    fn registry_default_is_exact_unity_and_reset_snaps_parameter_targets() {
        let mut fx = crate::create_effect("channel_router", 48000.0, 2).unwrap();
        assert_eq!(fx.id(), "channel_router");
        assert_eq!(fx.params(), ChannelRouter::PARAMS);
        let (mut l, mut r) = ([0.0, 2.5, -3.25], [0.125, -0.25, 0.375]);
        let original = (l, r);
        fx.process(&mut [&mut l, &mut r]);
        assert_eq!((l, r), original);
        fx.set_param("mode", 1.0);
        fx.reset();
        fx.process(&mut [&mut l, &mut r]);
        assert_eq!(l, r);
    }
}
