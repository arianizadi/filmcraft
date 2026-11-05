//! Bounded source-space subject alpha keyframes. Raster runs store `(length << 8) | alpha`;
//! no external file or model is needed to reopen an approved mask.
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};

pub const MAX_ALPHA_FRAMES: usize = 1200;
pub const MAX_ALPHA_PIXELS: usize = 65_536;
pub const MAX_ALPHA_TOTAL_PIXELS: usize = 8_388_608;
pub const MAX_ALPHA_RUNS: usize = 1_048_576;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlphaFrame {
    pub time: Tick,
    pub width: u16,
    pub height: u16,
    pub runs: Vec<u32>,
    pub confidence: f32,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub corrected: bool,
}
impl AlphaFrame {
    pub fn validate(&self) -> Result<usize, String> {
        let pixels = usize::from(self.width)
            .checked_mul(usize::from(self.height))
            .filter(|n| *n > 0 && *n <= MAX_ALPHA_PIXELS)
            .ok_or("alpha raster must contain 1..65536 pixels")?;
        if self.width > 256
            || self.height > 256
            || self.runs.len() > pixels
            || self.time < Tick::ZERO
            || self.time > Tick::MAX
            || !self.confidence.is_finite()
            || !(0.0..=1.0).contains(&self.confidence)
        {
            return Err("invalid alpha dimensions, time, confidence or run count".into());
        }
        let mut count = 0usize;
        for run in &self.runs {
            let len = (run >> 8) as usize;
            if len == 0 {
                return Err("alpha run must be nonempty".into());
            }
            count = count.checked_add(len).filter(|n| *n <= pixels).ok_or("alpha run length exceeds raster")?;
        }
        if count != pixels {
            return Err("alpha runs do not fill the raster".into());
        }
        Ok(pixels)
    }
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        let pixels = self.validate()?;
        let mut alpha = Vec::new();
        alpha.try_reserve_exact(pixels).map_err(|_| "not enough memory for alpha raster")?;
        for run in &self.runs {
            let length = (run >> 8) as usize;
            let value = (run & 255) as u8;
            alpha.resize(alpha.len().saturating_add(length), value);
        }
        Ok(alpha)
    }
    pub fn encode(time: Tick, width: u16, height: u16, alpha: &[u8], confidence: f32, failed: bool, corrected: bool) -> Result<Self, String> {
        let pixels = usize::from(width).checked_mul(usize::from(height)).filter(|n| *n > 0 && *n <= MAX_ALPHA_PIXELS).ok_or("invalid alpha dimensions")?;
        if alpha.len() != pixels {
            return Err("alpha storage does not match dimensions".into());
        }
        let mut runs = Vec::new();
        let mut prior = None;
        let mut length = 0u32;
        for &value in alpha {
            if prior == Some(value) {
                length = length.saturating_add(1);
            } else {
                if let Some(old) = prior {
                    runs.push((length << 8) | u32::from(old));
                }
                prior = Some(value);
                length = 1;
            }
        }
        if let Some(value) = prior {
            runs.push((length << 8) | u32::from(value));
        }
        let frame = Self { time, width, height, runs, confidence, failed, corrected };
        frame.validate()?;
        Ok(frame)
    }
    /// Normalized centroid and visible area from the actual alpha, not its prompt rectangle.
    pub fn centroid(&self) -> Result<Option<([f64; 2], f64)>, String> {
        if self.failed {
            return Ok(None);
        }
        let alpha = self.decode()?;
        let mut mass = 0f64;
        let (mut x, mut y) = (0f64, 0f64);
        let width = usize::from(self.width);
        for (index, &value) in alpha.iter().enumerate() {
            let weight = f64::from(value) / 255.0;
            mass += weight;
            x += (index % width) as f64 * weight;
            y += (index / width) as f64 * weight;
        }
        if mass <= 0.0 {
            return Ok(None);
        }
        Ok(Some(([(x / mass + 0.5) / f64::from(self.width), (y / mass + 0.5) / f64::from(self.height)], mass / alpha.len() as f64)))
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlphaTrack {
    pub source_width: u32,
    pub source_height: u32,
    pub end: Tick,
    pub method: String,
    pub provenance: String,
    pub frames: Vec<AlphaFrame>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrackData {
    source_width: u32,
    source_height: u32,
    end: Tick,
    method: String,
    provenance: String,
    frames: Vec<AlphaFrame>,
}
impl<'de> Deserialize<'de> for AlphaTrack {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = TrackData::deserialize(d)?;
        let track =
            Self { source_width: v.source_width, source_height: v.source_height, end: v.end, method: v.method, provenance: v.provenance, frames: v.frames };
        track.validate().map_err(serde::de::Error::custom)?;
        Ok(track)
    }
}
impl AlphaTrack {
    pub fn validate(&self) -> Result<(), String> {
        if self.source_width == 0
            || self.source_height == 0
            || self.source_width > 8192
            || self.source_height > 8192
            || self.frames.is_empty()
            || self.frames.len() > MAX_ALPHA_FRAMES
            || self.method.is_empty()
            || self.method.len() > 128
            || self.provenance.is_empty()
            || self.provenance.len() > 2048
            || self.end <= Tick::ZERO
            || self.end > Tick::MAX
        {
            return Err("invalid alpha track size, provenance or frame count".into());
        }
        let (mut pixels, mut runs) = (0usize, 0usize);
        let mut prior = None;
        for frame in &self.frames {
            pixels = pixels.saturating_add(frame.validate()?);
            runs = runs.saturating_add(frame.runs.len());
            if prior.is_some_and(|t| frame.time <= t) || frame.time >= self.end {
                return Err("alpha times must increase inside the track interval".into());
            }
            prior = Some(frame.time);
        }
        if pixels > MAX_ALPHA_TOTAL_PIXELS || runs > MAX_ALPHA_RUNS {
            return Err("alpha track exceeds 8 Mi raster pixels / 1 Mi runs".into());
        }
        Ok(())
    }
    /// Hold the preceding sampled raster; outside the reviewed interval stays transparent.
    /// Key order/shape are checked again because callers may construct project data directly.
    pub fn frame_at(&self, time: Tick) -> Option<&AlphaFrame> {
        if self.frames.len() > MAX_ALPHA_FRAMES
            || time >= self.end
            || self.frames.windows(2).any(|p| p.first().zip(p.get(1)).is_some_and(|(a, b)| a.time >= b.time))
        {
            return None;
        }
        let at = self.frames.partition_point(|f| f.time <= time).checked_sub(1)?;
        self.frames.get(at)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raster_roundtrip_and_centroid_preserve_holes() {
        let raster = [0, 255, 255, 0, 0, 255, 0, 0];
        let f = AlphaFrame::encode(Tick::ZERO, 4, 2, &raster, 0.9, false, false).unwrap();
        assert_eq!(f.decode().unwrap(), raster);
        let (center, area) = f.centroid().unwrap().unwrap();
        assert!((area - 3.0 / 8.0).abs() < 1e-8);
        assert!(center[0] > 0.3 && center[0] < 0.6);
    }
    #[test]
    fn hostile_rasters_and_tracks_return_errors_without_panics() {
        let f = AlphaFrame::encode(Tick::ZERO, 2, 2, &[0, 255, 255, 0], 1.0, false, false).unwrap();
        for runs in [vec![0], vec![u32::MAX], vec![256], vec![]] {
            let mut bad = f.clone();
            bad.runs = runs;
            assert!(std::panic::catch_unwind(|| bad.decode()).unwrap().is_err());
        }
        let mut t = AlphaTrack {
            source_width: 4,
            source_height: 4,
            end: Tick(10),
            method: "test".into(),
            provenance: "original test".into(),
            frames: vec![f.clone(), f],
        };
        assert!(t.validate().is_err());
        t.frames.truncate(1);
        assert!(t.validate().is_ok());
        let mut value = serde_json::to_value(t).unwrap();
        value["frames"][0]["runs"] = serde_json::json!([u32::MAX]);
        assert!(std::panic::catch_unwind(|| serde_json::from_value::<AlphaTrack>(value)).unwrap().is_err());
    }
}
