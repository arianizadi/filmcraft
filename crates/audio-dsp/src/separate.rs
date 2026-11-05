//! Local harmonic/percussive separation using median-filtered STFT masks.
//! This is deterministic signal processing, not a learned instrument/vocal classifier.
use crate::fft::Fft;
const FFT: usize = 1024;
const HOP: usize = 256;
const RADIUS: usize = 8;
/// Context on each side of a streamed block, including median and transform support.
pub const CONTEXT: usize = 4096;
pub const MAX_BLOCK: usize = 65536;
pub type StereoStems = (Vec<Vec<f32>>, Vec<Vec<f32>>);

/// Separate a bounded mono/stereo block. Harmonic + percussive reconstructs the input.
pub fn hpss(channels: &[Vec<f32>]) -> Result<StereoStems, String> {
    let n = channels.first().map_or(0, Vec::len);
    if !(1..=2).contains(&channels.len()) || n == 0 || n > MAX_BLOCK || channels.iter().any(|c| c.len() != n || c.iter().any(|x| !x.is_finite())) {
        return Err("HPSS needs a finite, equally sized mono/stereo block of 1..65536 samples".into());
    }
    let maximum = channels.iter().flatten().map(|v| v.abs()).fold(1.0f32, f32::max);
    let normalization = 1.0 / maximum;
    let fft = Fft::new(FFT);
    let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT as f32).cos()).collect();
    let frames = n.div_ceil(HOP) + FFT / HOP;
    let bins = FFT / 2 + 1;
    let mut power = vec![vec![0f32; bins]; frames];
    let mut spectra = Vec::with_capacity(channels.len());
    for channel in channels {
        let mut data = Vec::with_capacity(frames);
        for (frame, power) in power.iter_mut().enumerate() {
            let start = frame as isize * HOP as isize - FFT as isize / 2;
            let mut real = vec![0f32; FFT];
            let mut imag = vec![0f32; FFT];
            for (i, value) in real.iter_mut().enumerate() {
                let at = start + i as isize;
                if at >= 0
                    && let Some(sample) = channel.get(at as usize)
                {
                    *value = (*sample * normalization) * window.get(i).copied().unwrap_or(0.0);
                }
            }
            fft.forward(&mut real, &mut imag);
            for ((magnitude, r), i) in power.iter_mut().zip(&real).zip(&imag) {
                *magnitude += (r * r + i * i).sqrt() / channels.len() as f32;
            }
            data.push((real, imag));
        }
        spectra.push(data);
    }
    let median = |mut values: Vec<f32>| {
        values.sort_unstable_by(f32::total_cmp);
        values.get(values.len() / 2).copied().unwrap_or(0.0)
    };
    let mut masks = vec![vec![0f32; bins]; frames];
    for (frame, row) in masks.iter_mut().enumerate() {
        for (bin, mask) in row.iter_mut().enumerate() {
            let horizontal = median(
                power.iter().take((frame + RADIUS + 1).min(frames)).skip(frame.saturating_sub(RADIUS)).filter_map(|row| row.get(bin)).copied().collect(),
            );
            let vertical = median(
                power.get(frame).into_iter().flat_map(|row| row.iter().take((bin + RADIUS + 1).min(bins)).skip(bin.saturating_sub(RADIUS))).copied().collect(),
            );
            let (h, p) = (f64::from(horizontal).powi(2), f64::from(vertical).powi(2));
            *mask = if h + p > 1e-30 { (h / (h + p)) as f32 } else { 0.5 };
        }
    }
    let mut harmonic = vec![vec![0f32; n]; channels.len()];
    let mut weights = vec![0f32; n];
    for (channel, spectra) in spectra.into_iter().enumerate() {
        let output = harmonic.get_mut(channel).ok_or("missing output channel")?;
        for (frame, (mut real, mut imag)) in spectra.into_iter().enumerate() {
            let mask = masks.get(frame).ok_or("missing spectrum mask")?;
            for (bin, (r, i)) in real.iter_mut().zip(&mut imag).enumerate() {
                let m = mask.get(bin.min(FFT - bin)).copied().unwrap_or(0.0);
                *r *= m;
                *i *= m;
            }
            fft.inverse(&mut real, &mut imag);
            let start = frame as isize * HOP as isize - FFT as isize / 2;
            for (index, (&value, &window)) in real.iter().zip(&window).enumerate() {
                let at = start + index as isize;
                if at < 0 {
                    continue;
                }
                if let Some(out) = output.get_mut(at as usize) {
                    *out += value * window / FFT as f32;
                    if channel == 0
                        && let Some(weight) = weights.get_mut(at as usize)
                    {
                        *weight += window * window;
                    }
                }
            }
        }
    }
    let mut percussion = channels.to_vec();
    for ((harmonic, percussion), input) in harmonic.iter_mut().zip(&mut percussion).zip(channels) {
        for (((h, p), x), w) in harmonic.iter_mut().zip(percussion).zip(input).zip(&weights) {
            if *w > 1e-12 {
                *h = (*h / *w) / normalization;
            } else {
                *h = 0.0;
            }
            *p = *x - *h;
        }
    }
    if harmonic.iter().chain(&percussion).flatten().any(|v| !v.is_finite()) {
        return Err("audio magnitude exceeds the numerical separation range".into());
    }
    Ok((harmonic, percussion))
}

/// A center/side decomposition. Center is shared stereo content; side preserves the remainder.
/// A center-panned instrument will be in the center stem alongside any centered dialogue.
pub fn mid_side(channels: &[Vec<f32>]) -> Result<StereoStems, String> {
    let Some(left) = channels.first() else { return Err("mid/side needs stereo audio".into()) };
    let Some(right) = channels.get(1) else { return Err("mid/side needs stereo audio".into()) };
    if channels.len() != 2 || left.len() != right.len() || left.len() > MAX_BLOCK || left.iter().chain(right).any(|x| !x.is_finite()) {
        return Err("invalid bounded stereo block".into());
    }
    let center: Vec<f32> = left.iter().zip(right).map(|(l, r)| l * 0.5 + r * 0.5).collect();
    let sides = channels.iter().map(|channel| channel.iter().zip(&center).map(|(x, c)| x - c).collect()).collect();
    Ok((vec![center.clone(), center], sides))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stems_reconstruct_and_tones_remain_harmonic() {
        let input: Vec<f32> = (0..12000).map(|i| (std::f32::consts::TAU * 512.0 * i as f32 / 16384.0).sin() * 0.3).collect();
        let (h, p) = hpss(std::slice::from_ref(&input)).unwrap();
        let energy = |x: &[f32]| x.iter().map(|x| x * x).sum::<f32>();
        assert!(energy(&h[0][CONTEXT..8000]) > 20.0 * energy(&p[0][CONTEXT..8000]));
        for ((h, p), x) in h[0].iter().zip(&p[0]).zip(&input) {
            assert!((h + p - x).abs() < 1e-6);
        }
        let impulse = (0..12000).map(|i| if i == 6000 { 1.0 } else { 0.0 }).collect::<Vec<_>>();
        let (h, p) = hpss(&[impulse]).unwrap();
        assert!(p[0][6000] > 0.9 && h[0][6000].abs() < 0.1);
    }
    #[test]
    fn extreme_finite_samples_are_finite_or_return_an_actionable_error() {
        let source = vec![vec![f32::MAX; 1024], vec![f32::MAX; 1024]];
        let (c, s) = mid_side(&source).unwrap();
        assert!(c.iter().chain(&s).flatten().all(|v| v.is_finite()));
        let result = std::panic::catch_unwind(|| hpss(&source)).unwrap();
        match result {
            Ok((a, b)) => assert!(a.iter().chain(&b).flatten().all(|v| v.is_finite())),
            Err(message) => assert!(message.contains("numerical")),
        }
    }

    #[test]
    fn center_and_side_reconstruct_and_hostile_blocks_refuse() {
        let source = vec![vec![0.8, -0.4, 0.2], vec![0.2, 0.4, 0.6]];
        let (c, s) = mid_side(&source).unwrap();
        for ch in 0..2 {
            for i in 0..3 {
                assert!((c[ch][i] + s[ch][i] - source[ch][i]).abs() < 1e-6);
            }
        }
        assert!(hpss(&[]).is_err());
        assert!(hpss(&[vec![f32::NAN]]).is_err());
        assert!(hpss(&[vec![0.0; MAX_BLOCK + 1]]).is_err());
        assert!(mid_side(&[vec![0.0]]).is_err());
    }
}
