//! Source alpha coverage sampled through a bounded signed-distance field. The source raster is
//! actual segmented alpha; expansion/feather use the same mask controls as geometric masks.
use filmcraft_project::AlphaTrack;
use filmcraft_time::Tick;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct RasterCoverage {
    w: usize,
    h: usize,
    distance: Arc<Vec<f32>>,
    canvas: [f32; 2],
    cell: f32,
}
impl RasterCoverage {
    pub fn prepare(track: &AlphaTrack, time: Tick, scale: f32) -> Self {
        let empty = || Self { w: 0, h: 0, distance: Arc::new(Vec::new()), canvas: [1.0, 1.0], cell: 1.0 };
        if !scale.is_finite() || scale <= 0.0 || track.source_width == 0 || track.source_height == 0 || track.source_width > 8192 || track.source_height > 8192
        {
            return empty();
        }
        let Some(frame) = track.frame_at(time) else {
            return empty();
        };
        if frame.failed {
            return empty();
        }
        let Ok(alpha) = frame.decode() else {
            return empty();
        };
        let w = usize::from(frame.width);
        let h = usize::from(frame.height);
        let distance = signed_distance(&alpha, w, h);
        Self {
            w,
            h,
            distance: Arc::new(distance),
            canvas: [track.source_width as f32 * scale, track.source_height as f32 * scale],
            cell: track.source_width as f32 / w.max(1) as f32 * scale,
        }
    }
    pub fn signed_at(&self, x: f32, y: f32) -> f32 {
        if self.w == 0 || self.h == 0 || !x.is_finite() || !y.is_finite() || self.canvas.iter().any(|n| !n.is_finite() || *n <= 0.0) {
            return -1e6;
        }
        let fx = x / self.canvas[0] * self.w as f32 - 0.5;
        let fy = y / self.canvas[1] * self.h as f32 - 0.5;
        if fx < -1.0 || fy < -1.0 || fx > self.w as f32 || fy > self.h as f32 {
            return -1e6;
        }
        let (x0, y0) = (fx.floor() as isize, fy.floor() as isize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let get = |x: isize, y: isize| {
            if x < 0 || y < 0 || x >= self.w as isize || y >= self.h as isize {
                -1.0
            } else {
                self.distance.get(y as usize * self.w + x as usize).copied().unwrap_or(-1e6)
            }
        };
        let a = get(x0, y0) * (1.0 - tx) + get(x0 + 1, y0) * tx;
        let b = get(x0, y0 + 1) * (1.0 - tx) + get(x0 + 1, y0 + 1) * tx;
        (a * (1.0 - ty) + b * ty) * self.cell
    }
}
fn signed_distance(alpha: &[u8], w: usize, h: usize) -> Vec<f32> {
    let len = w.saturating_mul(h);
    if len == 0 || len > filmcraft_project::alpha_mask::MAX_ALPHA_PIXELS || alpha.len() != len {
        return Vec::new();
    }
    let mut distance = vec![512f32; len];
    // Dimensions and storage validated above; the hot loops use indices inside this rectangle.
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let inside = alpha[i] >= 128;
            let edge = (x > 0 && (alpha[i - 1] >= 128) != inside)
                || (x + 1 < w && (alpha[i + 1] >= 128) != inside)
                || (y > 0 && (alpha[i - w] >= 128) != inside)
                || (y + 1 < h && (alpha[i + w] >= 128) != inside)
                || (inside && (x == 0 || y == 0 || x + 1 == w || y + 1 == h));
            if edge {
                distance[i] = 0.5;
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let mut d = distance[i];
            if x > 0 {
                d = d.min(distance[i - 1] + 1.0);
            }
            if y > 0 {
                d = d.min(distance[i - w] + 1.0);
                if x > 0 {
                    d = d.min(distance[i - w - 1] + std::f32::consts::SQRT_2);
                }
                if x + 1 < w {
                    d = d.min(distance[i - w + 1] + std::f32::consts::SQRT_2);
                }
            }
            distance[i] = d;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            let mut d = distance[i];
            if x + 1 < w {
                d = d.min(distance[i + 1] + 1.0);
            }
            if y + 1 < h {
                d = d.min(distance[i + w] + 1.0);
                if x > 0 {
                    d = d.min(distance[i + w - 1] + std::f32::consts::SQRT_2);
                }
                if x + 1 < w {
                    d = d.min(distance[i + w + 1] + std::f32::consts::SQRT_2);
                }
            }
            distance[i] = d;
        }
    }
    for (value, &alpha) in distance.iter_mut().zip(alpha) {
        if alpha < 128 {
            *value = -*value;
        }
    }
    distance
}
#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::AlphaFrame;
    #[test]
    fn pixel_alpha_keeps_a_hole_and_rejects_failed_frames() {
        let alpha = [255, 255, 255, 255, 0, 255, 255, 255, 255];
        let frame = AlphaFrame::encode(Tick::ZERO, 3, 3, &alpha, 1.0, false, false).unwrap();
        let mut track =
            AlphaTrack { source_width: 30, source_height: 30, end: Tick(10), method: "original test".into(), provenance: "test".into(), frames: vec![frame] };
        let p = RasterCoverage::prepare(&track, Tick::ZERO, 1.0);
        assert!(p.signed_at(15.0, 15.0) < 0.0);
        assert!(p.signed_at(5.0, 5.0) > 0.0);
        track.frames[0].failed = true;
        assert!(RasterCoverage::prepare(&track, Tick::ZERO, 1.0).signed_at(5.0, 5.0) < -1000.0);
    }
}
