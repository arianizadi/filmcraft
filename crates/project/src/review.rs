//! Project-owned review records and versioned editing preferences. Checkpoint JSON is a bounded
//! document snapshot with its own review records removed, so snapshots never nest recursively.
use crate::{CaptionStyle, ItemId, Tick};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_CHECKPOINTS: usize = 8;
pub const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ANNOTATIONS: usize = 2000;
pub const MAX_ACTIVITY: usize = 1000;
pub const MAX_PROFILES: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewState {
    pub next_id: u64,
    pub checkpoints: Vec<Checkpoint>,
    pub annotations: Vec<Annotation>,
    pub activity: Vec<Activity>,
    pub profiles: BTreeMap<String, Vec<EditingProfile>>,
    pub active_profile: Option<String>,
    /// The resolved, approved version including one-off overrides used for delivery.
    pub active_profile_settings: Option<EditingProfile>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub id: u64,
    pub name: String,
    pub created_utc: String,
    pub revision: u64,
    pub document: std::sync::Arc<str>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Annotation {
    pub id: u64,
    pub sequence: ItemId,
    pub start: Tick,
    pub end: Tick,
    pub text: String,
    pub kind: String,
    pub author: String,
    pub status: String,
    pub created_utc: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activity {
    pub id: u64,
    pub command: String,
    pub label: String,
    pub before_revision: u64,
    pub after_revision: u64,
    pub created_utc: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct EditingProfile {
    pub version: u32,
    pub name: String,
    pub reference_look: Option<String>,
    pub skin_tone_preference: String,
    pub pacing: String,
    pub pause_padding_ticks: i64,
    pub caption_style: CaptionStyle,
    pub loudness_lufs: f64,
    pub delivery: DeliveryTarget,
}
impl Default for EditingProfile {
    fn default() -> Self {
        Self {
            version: 1,
            name: String::new(),
            reference_look: None,
            skin_tone_preference: "natural".into(),
            pacing: "balanced".into(),
            pause_padding_ticks: 38_102_400_000,
            caption_style: CaptionStyle::default(),
            loudness_lufs: -16.0,
            delivery: DeliveryTarget::default(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryTarget {
    pub width: u32,
    pub height: u32,
    pub fps_num: i64,
    pub fps_den: i64,
    pub format: String,
    pub color_space: String,
    pub captions: String,
}
impl Default for DeliveryTarget {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps_num: 30000, fps_den: 1001, format: "h264".into(), color_space: "rec709".into(), captions: "srt".into() }
    }
}
impl EditingProfile {
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 128 || self.version == 0 {
            return Err("profile requires a name up to 128 bytes and a positive version".into());
        }
        if self.skin_tone_preference.len() > 1024 || self.pacing.len() > 1024 || self.reference_look.as_ref().is_some_and(|s| s.len() > 1024) {
            return Err("profile text is too long".into());
        }
        if !(0..=filmcraft_time::TICKS_PER_SECOND * 60).contains(&self.pause_padding_ticks)
            || !self.loudness_lufs.is_finite()
            || !(-60.0..=0.0).contains(&self.loudness_lufs)
        {
            return Err("padding must be 0–60 seconds and loudness -60–0 LUFS".into());
        }
        crate::validate_frame_size(self.delivery.width, self.delivery.height)?;
        crate::SequenceSettings {
            width: self.delivery.width,
            height: self.delivery.height,
            frame_rate: filmcraft_time::FrameRate::new(self.delivery.fps_num, self.delivery.fps_den),
            ..Default::default()
        }
        .validate()?;
        if !["h264", "hevc", "prores", "dnxhr", "png", "wav"].contains(&self.delivery.format.as_str())
            || !["rec709", "rec2100-pq", "rec2100-hlg"].contains(&self.delivery.color_space.as_str())
            || !["none", "srt", "vtt", "burn"].contains(&self.delivery.captions.as_str())
        {
            return Err("unsupported delivery format, color space or caption mode".into());
        }
        // Caption styles are ordinary typed project data; reject non-finite scalar controls.
        if !self.caption_style.size.is_finite()
            || !(1.0..=1000.0).contains(&self.caption_style.size)
            || !self.caption_style.margin.is_finite()
            || !(0.0..=1.0).contains(&self.caption_style.margin)
            || !self.caption_style.line_spacing.is_finite()
            || !(0.1..=10.0).contains(&self.caption_style.line_spacing)
            || !self.caption_style.outline.is_finite()
            || !(0.0..=100.0).contains(&self.caption_style.outline)
            || self.caption_style.font.len() > 1024
            || self.caption_style.font_style.len() > 256
        {
            return Err("invalid caption style".into());
        }
        let value = serde_json::to_value(&self.caption_style).map_err(|e| e.to_string())?;
        if value.to_string().len() > 16_384 {
            return Err("caption style is too large".into());
        }
        Ok(())
    }
}
impl ReviewState {
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
            && self.annotations.is_empty()
            && self.activity.is_empty()
            && self.profiles.is_empty()
            && self.active_profile.is_none()
            && self.active_profile_settings.is_none()
    }
    pub fn alloc(&mut self) -> Result<u64, String> {
        self.next_id = self.next_id.checked_add(1).ok_or("review IDs exhausted")?;
        Ok(self.next_id)
    }
    pub fn check(&self) -> Result<(), String> {
        if self.checkpoints.len() > MAX_CHECKPOINTS
            || self.annotations.len() > MAX_ANNOTATIONS
            || self.activity.len() > MAX_ACTIVITY
            || self.profiles.len() > MAX_PROFILES
        {
            return Err("project review record limit exceeded".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for (id, text) in self
            .checkpoints
            .iter()
            .map(|c| (c.id, c.name.as_str()))
            .chain(self.annotations.iter().map(|a| (a.id, a.text.as_str())))
            .chain(self.activity.iter().map(|a| (a.id, a.label.as_str())))
        {
            if id == 0 || id > self.next_id || !ids.insert(id) || text.len() > 16_384 {
                return Err("invalid or duplicate review record identity/text".into());
            }
        }
        for c in &self.checkpoints {
            if c.document.len() > MAX_CHECKPOINT_BYTES || c.name.trim().is_empty() || c.name.len() > 128 || c.created_utc.len() > 128 {
                return Err("invalid checkpoint name or snapshot size".into());
            }
        }
        for a in &self.annotations {
            if a.start.0 < 0
                || a.end < a.start
                || !["suggestion", "finding", "note"].contains(&a.kind.as_str())
                || !["open", "accepted", "dismissed"].contains(&a.status.as_str())
                || a.author.len() > 256
                || a.created_utc.len() > 128
                || a.end > Tick::MAX
            {
                return Err("invalid annotation bounds, kind, status or author".into());
            }
        }
        for event in &self.activity {
            if event.command.len() > 1024 || event.created_utc.len() > 128 {
                return Err("invalid activity command or timestamp".into());
            }
        }
        for (name, versions) in &self.profiles {
            if versions.is_empty() || versions.len() > 32 {
                return Err("profile must have 1–32 versions".into());
            }
            for (i, p) in versions.iter().enumerate() {
                p.check()?;
                if &p.name != name || p.version != i as u32 + 1 {
                    return Err("profile version history is inconsistent".into());
                }
            }
        }
        if let Some(profile) = &self.active_profile_settings {
            profile.check()?;
            if self.active_profile.as_deref() != Some(profile.name.as_str())
                || !self.profiles.get(&profile.name).is_some_and(|versions| versions.iter().any(|p| p.version == profile.version))
            {
                return Err("active resolved profile does not match a saved version".into());
            }
        }
        if self.active_profile.as_ref().is_some_and(|name| !self.profiles.contains_key(name)) {
            return Err("active editing profile is missing".into());
        }
        Ok(())
    }
}
