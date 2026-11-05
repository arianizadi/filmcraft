//! Original local box/click-seeded color segmentation with a spatial graph regularizer and
//! component tracking. This produces pixel alpha, not a geometric mask. It is deliberately not
//! described as semantic person recognition: similar foreground/background colors, camouflage,
//! large lighting changes and multiple similar objects need correction or a host model.
use crate::Image;
use serde::{Deserialize, Serialize};
const MAX_PIXELS: usize = 65_536;
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prompt {
    /// Normalized source rectangle [x,y,width,height].
    pub rect: Option<[f32; 4]>,
    pub point: Option<[f32; 2]>,
    #[serde(default)]
    pub foreground: Vec<[f32; 2]>,
    #[serde(default)]
    pub background: Vec<[f32; 2]>,
}
impl Prompt {
    pub fn validate(&self) -> Result<[f32; 4], String> {
        if self.rect.is_some() == self.point.is_some() || self.foreground.len() > 128 || self.background.len() > 128 {
            return Err("choose exactly one rect or point; at most 128 correction points of each kind".into());
        }
        let point_valid = |p: &[f32; 2]| p.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v));
        if self.foreground.iter().chain(&self.background).any(|p| !point_valid(p)) {
            return Err("correction points must be finite normalized source coordinates".into());
        }
        if let Some(r) = self.rect {
            if r.iter().any(|v| !v.is_finite()) || r[0] < 0.0 || r[1] < 0.0 || r[2] <= 0.0 || r[3] <= 0.0 || r[0] + r[2] > 1.0 || r[1] + r[3] > 1.0 {
                return Err("rect must have positive area inside normalized source coordinates".into());
            }
            Ok(r)
        } else if let Some(p) = self.point {
            if !point_valid(&p) {
                return Err("point must be a finite normalized source coordinate".into());
            }
            let x = (p[0] - 0.04).clamp(0.0, 0.92);
            let y = (p[1] - 0.04).clamp(0.0, 0.92);
            Ok([x, y, 0.08, 0.08])
        } else {
            Err("subject prompt is required".into())
        }
    }
}
#[derive(Clone, Debug)]
pub struct Segmented {
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<u8>,
    pub confidence: f32,
    pub failed: bool,
    pub reacquired: bool,
    pub centroid: Option<[f32; 2]>,
    pub visible_fraction: f32,
}
struct Component {
    indices: Vec<usize>,
    center: [f32; 2],
    rect: [f32; 4],
    separation: f32,
}
pub struct Tracker {
    width: usize,
    height: usize,
    foreground: Vec<[f32; 3]>,
    background: Vec<[f32; 3]>,
    rect: [f32; 4],
    center: [f32; 2],
    velocity: [f32; 2],
    expected_area: usize,
    lost: usize,
    frames: usize,
    minimum_confidence: f32,
    boxed: bool,
}
fn pixels(image: &Image) -> Result<Vec<[f32; 3]>, String> {
    let count = image.w.checked_mul(image.h).filter(|n| *n > 0 && *n <= MAX_PIXELS).ok_or("subject analysis requires 1..65536 pixels")?;
    if image.w > 256 || image.h > 256 || image.px.len() != count.saturating_mul(4) || image.px.iter().any(|v| !v.is_finite()) {
        return Err("invalid subject image storage or dimensions".into());
    }
    Ok(image.over_black_rgba8().as_chunks::<4>().0.iter().map(|p| [f32::from(p[0]) / 255.0, f32::from(p[1]) / 255.0, f32::from(p[2]) / 255.0]).collect())
}
fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(b).map(|(a, b)| (*a - b).powi(2)).sum()
}
fn nearest(p: [f32; 3], centers: &[[f32; 3]]) -> f32 {
    centers.iter().map(|&c| distance(p, c)).fold(3.0, f32::min)
}
fn prototypes(samples: &[[f32; 3]], maximum: usize) -> Vec<[f32; 3]> {
    let Some(first) = samples.first().copied() else {
        return Vec::new();
    };
    let mut centers = vec![first];
    for _ in 1..maximum {
        let next = samples.iter().max_by(|&&a, &&b| nearest(a, &centers).total_cmp(&nearest(b, &centers))).copied();
        if let Some(next) = next
            && nearest(next, &centers) > 0.005
        {
            centers.push(next);
        } else {
            break;
        }
    }
    for _ in 0..8 {
        let mut sums = vec![[0f64; 3]; centers.len()];
        let mut counts = vec![0usize; centers.len()];
        for &sample in samples {
            let best = centers.iter().enumerate().min_by(|(_, a), (_, b)| distance(sample, **a).total_cmp(&distance(sample, **b))).map_or(0, |p| p.0);
            if let (Some(sum), Some(count)) = (sums.get_mut(best), counts.get_mut(best)) {
                for (i, v) in sample.iter().enumerate() {
                    if let Some(s) = sum.get_mut(i) {
                        *s += f64::from(*v);
                    }
                }
                *count = count.saturating_add(1);
            }
        }
        for ((center, sum), count) in centers.iter_mut().zip(sums).zip(counts) {
            if count > 0 {
                *center = sum.map(|v| (v / count as f64) as f32);
            }
        }
    }
    centers
}
fn inside(p: [f32; 2], r: [f32; 4]) -> bool {
    p[0] >= r[0] && p[1] >= r[1] && p[0] <= r[0] + r[2] && p[1] <= r[1] + r[3]
}
impl Tracker {
    pub fn new(image: &Image, prompt: &Prompt, minimum_confidence: f32) -> Result<Self, String> {
        let rect = prompt.validate()?;
        if !minimum_confidence.is_finite() || !(0.0..=1.0).contains(&minimum_confidence) {
            return Err("minimum confidence must be finite 0..1".into());
        }
        let rgb = pixels(image)?;
        let mut fg = Vec::new();
        let mut bg = Vec::new();
        let stride = rgb.len().div_ceil(4096).max(1);
        for (index, &color) in rgb.iter().enumerate().step_by(stride) {
            let p = [(index % image.w) as f32 / image.w as f32, (index / image.w) as f32 / image.h as f32];
            if inside(p, rect) {
                fg.push(color);
            } else {
                bg.push(color);
            }
        }
        let sample = |p: [f32; 2]| {
            let x = (p[0] * image.w as f32).floor() as usize;
            let y = (p[1] * image.h as f32).floor() as usize;
            rgb.get(y.min(image.h - 1) * image.w + x.min(image.w - 1)).copied()
        };
        for &p in &prompt.foreground {
            if let Some(c) = sample(p) {
                fg.extend(std::iter::repeat_n(c, 64));
            }
        }
        for &p in &prompt.background {
            if let Some(c) = sample(p) {
                bg.extend(std::iter::repeat_n(c, 64));
            }
        }
        if let Some(point) = prompt.point
            && let Some(c) = sample(point)
        {
            fg = vec![c];
            bg.retain(|color| distance(*color, c) > 0.04);
        }
        if fg.is_empty() || bg.is_empty() {
            return Err("prompt needs foreground and visible background samples; use a tighter box".into());
        }
        let background = prototypes(&bg, 12);
        let distinctive: Vec<_> = fg.iter().copied().filter(|color| nearest(*color, &background) > 0.004).collect();
        let foreground = if distinctive.is_empty() { prototypes(&fg, 6) } else { prototypes(&distinctive, 6) };
        Ok(Self {
            width: image.w,
            height: image.h,
            foreground,
            background,
            rect,
            center: [rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0],
            velocity: [0.0, 0.0],
            expected_area: 0,
            lost: 0,
            frames: 0,
            minimum_confidence,
            boxed: prompt.rect.is_some(),
        })
    }
    pub fn step(&mut self, image: &Image) -> Result<Segmented, String> {
        if image.w != self.width || image.h != self.height {
            return Err("subject analysis dimensions changed during tracking".into());
        }
        let rgb = pixels(image)?;
        let len = rgb.len();
        let mut unary = Vec::with_capacity(len);
        let mut separation = Vec::with_capacity(len);
        let mut labels = Vec::with_capacity(len);
        let predicted = [(self.center[0] + self.velocity[0]).clamp(0.0, 1.0), (self.center[1] + self.velocity[1]).clamp(0.0, 1.0)];
        for (index, &color) in rgb.iter().enumerate() {
            let f = nearest(color, &self.foreground);
            let b = nearest(color, &self.background);
            let margin = (b - f) / (b + f + 0.01);
            let p = [(index % self.width) as f32 / self.width as f32, (index / self.width) as f32 / self.height as f32];
            let spatial = if self.frames == 0 && self.boxed && !inside(p, self.rect) { 4.0 } else { 0.0 };
            unary.push(f - b + spatial);
            separation.push(margin.max(0.0));
            labels.push(margin > 0.08 && spatial == 0.0);
        }
        // Four deterministic ICM passes on an edge-weighted four-neighbor Potts graph.
        // Storage and index bounds were validated by pixels(); neighbors stay in the rectangle.
        for _ in 0..4 {
            let mut next = labels.clone();
            for y in 0..self.height {
                for x in 0..self.width {
                    let index = y * self.width + x;
                    let mut energy = unary[index];
                    for (dx, dy) in [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)] {
                        let (nx, ny) = (x as isize + dx, y as isize + dy);
                        if nx >= 0 && ny >= 0 && nx < self.width as isize && ny < self.height as isize {
                            let other = ny as usize * self.width + nx as usize;
                            let cost = 0.02 * (-distance(rgb[index], rgb[other]) / 0.02).exp();
                            energy += if labels[other] { -cost } else { cost };
                        }
                    }
                    next[index] = energy < -0.001 && separation[index] > 0.02;
                }
            }
            labels = next;
        }
        let components = components(&labels, &separation, self.width, self.height);
        let best = components
            .into_iter()
            .filter(|c| c.indices.len() >= 4)
            .max_by(|a, b| self.component_score(a, predicted).total_cmp(&self.component_score(b, predicted)));
        let mut alpha = vec![0u8; len];
        let mut confidence = 0.0;
        let mut centroid = None;
        let mut visible_fraction = 0.0;
        let mut rect = self.rect;
        if let Some(component) = best {
            let area = component.indices.len();
            visible_fraction = area as f32 / len as f32;
            let size = if self.expected_area == 0 { 1.0 } else { (area.min(self.expected_area) as f32 / area.max(self.expected_area) as f32).sqrt() };
            let shift = ((component.center[0] - predicted[0]).powi(2) + (component.center[1] - predicted[1]).powi(2)).sqrt();
            let motion = (1.0 - shift * 0.5).clamp(0.4, 1.0);
            confidence = (0.25 + 0.75 * component.separation) * size.powi(2) * motion;
            if self.expected_area > 0 && area as f32 > self.expected_area as f32 * 1.6 {
                confidence *= 0.5;
            }
            if self.expected_area > 0 && area < self.expected_area / 8 {
                confidence *= 0.2;
            }
            if confidence >= self.minimum_confidence {
                for &i in &component.indices {
                    if let Some(p) = alpha.get_mut(i) {
                        *p = 255;
                    }
                }
                centroid = Some(component.center);
                rect = component.rect;
                if self.expected_area == 0 {
                    self.expected_area = area;
                }
            }
        }
        let failed = centroid.is_none();
        let reacquired = !failed && self.lost > 0;
        if let Some(center) = centroid {
            self.velocity = [(center[0] - self.center[0]) * 0.5, (center[1] - self.center[1]) * 0.5];
            self.center = center;
            self.rect = rect;
            self.lost = 0;
        } else {
            self.lost = self.lost.saturating_add(1);
            self.velocity = self.velocity.map(|v| v * 0.7);
        }
        self.frames = self.frames.saturating_add(1);
        Ok(Segmented { width: self.width, height: self.height, alpha, confidence: confidence.clamp(0.0, 1.0), failed, reacquired, centroid, visible_fraction })
    }
    fn component_score(&self, c: &Component, predicted: [f32; 2]) -> f32 {
        let d = (c.center[0] - predicted[0]).powi(2) + (c.center[1] - predicted[1]).powi(2);
        let area = c.indices.len() as f32;
        let size = if self.expected_area == 0 { area.sqrt() } else { (area.min(self.expected_area as f32) / area.max(self.expected_area as f32)).sqrt() };
        c.separation * size / (1.0 + d * if self.lost > 0 { 1.0 } else { 4.0 })
    }
}
fn components(labels: &[bool], separation: &[f32], w: usize, h: usize) -> Vec<Component> {
    let mut seen = vec![false; labels.len()];
    let mut out = Vec::new();
    for start in 0..labels.len() {
        if seen[start] || !labels[start] {
            continue;
        }
        let mut indices = vec![start];
        seen[start] = true;
        let mut cursor = 0usize;
        let (mut x, mut y, mut quality) = (0f64, 0f64, 0f64);
        let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (w, h, 0usize, 0usize);
        while let Some(&i) = indices.get(cursor) {
            cursor += 1;
            let (px, py) = (i % w, i / w);
            x += px as f64 + 0.5;
            y += py as f64 + 0.5;
            quality += f64::from(separation.get(i).copied().unwrap_or(0.0));
            lo_x = lo_x.min(px);
            lo_y = lo_y.min(py);
            hi_x = hi_x.max(px + 1);
            hi_y = hi_y.max(py + 1);
            for (dx, dy) in [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (px as isize + dx, py as isize + dy);
                if nx >= 0 && ny >= 0 && nx < w as isize && ny < h as isize {
                    let j = ny as usize * w + nx as usize;
                    if !seen[j] && labels[j] {
                        seen[j] = true;
                        indices.push(j);
                    }
                }
            }
        }
        let count = indices.len().max(1) as f64;
        out.push(Component {
            indices,
            center: [(x / count / w as f64) as f32, (y / count / h as f64) as f32],
            rect: [lo_x as f32 / w as f32, lo_y as f32 / h as f32, (hi_x - lo_x) as f32 / w as f32, (hi_y - lo_y) as f32 / h as f32],
            separation: (quality / count) as f32,
        });
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame(at: usize, hidden: bool) -> Image {
        let mut im = Image::filled(64, 48, [0.0, 0.0, 1.0, 1.0]);
        if !hidden {
            for y in 10..36 {
                for x in at..at + 16 {
                    if !((x == at + 7 || x == at + 8) && (y == 21 || y == 22)) {
                        let i = (y * im.w + x) * 4;
                        im.px[i..i + 4].copy_from_slice(&[1.0, 0.0, 0.0, 1.0]);
                    }
                }
            }
        }
        im
    }
    #[test]
    fn genuine_alpha_tracks_motion_hole_occlusion_and_reacquisition() {
        let first = frame(8, false);
        let prompt = Prompt { rect: Some([7.0 / 64.0, 9.0 / 48.0, 18.0 / 64.0, 28.0 / 48.0]), ..Default::default() };
        let mut tracker = Tracker::new(&first, &prompt, 0.35).unwrap();
        let a = tracker.step(&first).unwrap();
        assert!(!a.failed);
        assert_eq!(a.alpha[21 * 64 + 15], 0);
        assert_eq!(a.alpha[15 * 64 + 10], 255);
        let b = tracker.step(&frame(14, false)).unwrap();
        assert!(!b.failed);
        assert!(b.centroid.unwrap()[0] > a.centroid.unwrap()[0]);
        let hidden = tracker.step(&frame(14, true)).unwrap();
        assert!(hidden.failed);
        assert!(hidden.alpha.iter().all(|a| *a == 0));
        let found = tracker.step(&frame(24, false)).unwrap();
        assert!(!found.failed);
        assert!(found.reacquired);
    }
    #[test]
    fn indistinguishable_subject_is_failed_and_hostile_prompts_are_errors() {
        let image = Image::filled(32, 32, [0.2, 0.2, 0.2, 1.0]);
        let prompt = Prompt { rect: Some([0.2, 0.2, 0.5, 0.5]), ..Default::default() };
        let mut tracker = Tracker::new(&image, &prompt, 0.35).unwrap();
        assert!(tracker.step(&image).unwrap().failed);
        for prompt in [
            Prompt::default(),
            Prompt { point: Some([f32::NAN, 0.0]), ..Default::default() },
            Prompt { rect: Some([0.0, 0.0, 2.0, 1.0]), ..Default::default() },
        ] {
            assert!(std::panic::catch_unwind(|| Tracker::new(&image, &prompt, 0.35)).unwrap().is_err());
        }
    }
}
