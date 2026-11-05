//! Persistent color work: immutable reference stills, named grades and shared camera/scene looks.
//! A shared look renders before the clip's local color effects, which remain per-shot trims.

use std::collections::BTreeMap;
use std::sync::Arc;

use filmcraft_color::ColorPipeline;
use filmcraft_time::{FrameRate, Tick};
use serde::{Deserialize, Serialize};

use crate::{ClipId, EffectInstance, ItemId};

pub const MAX_COLOR_RECORDS: usize = 256;
pub const MAX_COLOR_EFFECTS: usize = 32;
pub const MAX_GALLERY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REFERENCE_PIXELS: usize = 65_536;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColorWorkflow {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gallery: BTreeMap<u64, GalleryStill>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<ClipId, Vec<GradeVersion>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub looks: BTreeMap<u64, SharedLook>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrozenImage {
    pub width: usize,
    pub height: usize,
    /// Immutable premultiplied working-linear RGBA, at the explicitly recorded sample size.
    pub pixels: Arc<[f32]>,
}

impl FrozenImage {
    pub fn validate(&self) -> Result<(), String> {
        let count = self.width.checked_mul(self.height).filter(|n| *n > 0 && *n <= MAX_REFERENCE_PIXELS).and_then(|n| n.checked_mul(4));
        if count != Some(self.pixels.len())
            || !self.pixels.iter().all(|p| p.is_finite() && p.abs() <= 1.0e6)
            || self.pixels.as_chunks::<4>().0.iter().any(|p| p.get(3).is_none_or(|a| !(0.0..=1.0).contains(a)))
        {
            return Err("invalid frozen reference image dimensions or pixels".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GalleryStill {
    pub id: u64,
    pub name: String,
    pub sequence: Option<ItemId>,
    pub clip: Option<ClipId>,
    pub item: Option<ItemId>,
    pub time: Tick,
    pub frame: i64,
    pub rate: FrameRate,
    pub revision: u64,
    pub pipeline: ColorPipeline,
    pub width: u32,
    pub height: u32,
    /// Exact display-render PNG bytes encoded as base64, kept inside the saved project.
    pub png_base64: Arc<str>,
    /// The same frame's working-linear reference sample, independent of later source/grade edits.
    pub reference: FrozenImage,
    pub grade: Arc<[EffectInstance]>,
    pub transform: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradeVersion {
    pub id: u64,
    pub name: String,
    pub effects: Arc<[EffectInstance]>,
    pub revision: u64,
    pub pipeline: ColorPipeline,
    #[serde(default)]
    pub shared_look: Option<u64>,
    /// Positions among the complete effect stack, including intrinsic/non-color effects.
    pub color_positions: Vec<usize>,
    pub non_color_effects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedLook {
    pub id: u64,
    pub name: String,
    /// User-defined scene/camera grouping, e.g. "Camera B" or "Scene 12".
    pub group: String,
    pub effects: Arc<[EffectInstance]>,
    pub pipeline: ColorPipeline,
    pub revision: u64,
}

pub fn is_color_effect(id: &str) -> bool {
    matches!(
        id,
        "lumetri"
            | "asc_cdl"
            | "brightness_contrast"
            | "proc_amp"
            | "tint"
            | "black_white"
            | "color_balance"
            | "three_way_color"
            | "leave_color"
            | "change_to_color"
            | "color_pass"
            | "gamma_correction"
            | "levels"
            | "channel_mix"
            | "video_limiter"
    )
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("color record names must contain 1..256 bytes without control characters".into());
    }
    Ok(())
}

fn finite_value(value: &crate::ParamValue) -> bool {
    use crate::ParamValue;
    match value {
        ParamValue::Float(v) => v.is_finite(),
        ParamValue::Vec2(v) => v.x.is_finite() && v.y.is_finite() && v.x.abs() <= 1.0e7 && v.y.abs() <= 1.0e7,
        ParamValue::Color(v) => v.iter().all(|v| v.is_finite()),
        ParamValue::Curve(v) => v.len() <= 4096 && v.iter().flatten().all(|v| v.is_finite()),
        ParamValue::Path(v) => {
            v.vertices.len() <= 4096
                && v.vertices
                    .iter()
                    .all(|v| [v.p, v.t_in, v.t_out].iter().all(|p| p.x.is_finite() && p.y.is_finite() && p.x.abs() <= 1.0e7 && p.y.abs() <= 1.0e7))
        }
        ParamValue::Text(v) => v.len() <= 65_536,
        ParamValue::Bool(_) | ParamValue::Choice(_) => true,
    }
}

pub fn validate_effects(effects: &[EffectInstance]) -> Result<(), String> {
    if effects.len() > MAX_COLOR_EFFECTS {
        return Err("color stacks are limited to 32 effects".into());
    }
    for effect in effects {
        if !is_color_effect(&effect.effect) || effect.params.len() > 256 || effect.masks.len() > 64 {
            return Err("invalid effect in saved color stack".into());
        }
        for p in effect.params.values().chain(effect.masks.iter().flat_map(|m| [&m.path, &m.feather, &m.opacity, &m.expansion])) {
            if !finite_value(&p.value) || p.keyframes.len() > 16_384 {
                return Err("invalid value or excessive keyframes in color stack".into());
            }
            let mut previous = None;
            for k in &p.keyframes {
                if !(Tick::MIN..=Tick::MAX).contains(&k.time)
                    || previous.is_some_and(|t| k.time <= t)
                    || !finite_value(&k.value)
                    || !k.in_influence.is_finite()
                    || !k.out_influence.is_finite()
                    || !(0.0..=1.0).contains(&k.in_influence)
                    || !(0.0..=1.0).contains(&k.out_influence)
                {
                    return Err("invalid keyframes in saved color stack".into());
                }
                previous = Some(k.time);
            }
        }
    }
    Ok(())
}

impl ColorWorkflow {
    pub fn is_empty(&self) -> bool {
        self.gallery.is_empty() && self.versions.is_empty() && self.looks.is_empty()
    }

    pub fn shared_effects(&self, item: &crate::TrackItem) -> &[EffectInstance] {
        item.shared_look.and_then(|id| self.looks.get(&id)).map(|look| look.effects.as_ref()).unwrap_or(&[])
    }
    pub fn has_shared_effects(&self, item: &crate::TrackItem) -> bool {
        self.shared_effects(item).iter().any(|e| e.enabled)
    }
    pub fn linked_clips(project: &crate::Project, look: u64) -> Vec<(ItemId, ClipId)> {
        project
            .sequences()
            .flat_map(|p| match &p.kind {
                crate::ItemKind::Sequence(q) => {
                    q.video_tracks.iter().flat_map(|t| t.items.iter()).filter(|c| c.shared_look == Some(look)).map(|c| (p.id, c.id)).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect()
    }
    pub fn validate_references(&self, project: &crate::Project) -> Result<(), String> {
        for item in project.sequences() {
            if let crate::ItemKind::Sequence(q) = &item.kind {
                if q.audio_tracks.iter().flat_map(|track| &track.items).any(|clip| clip.shared_look.is_some()) {
                    return Err("audio clips cannot refer to shared color looks".into());
                }
                for clip in q.video_tracks.iter().flat_map(|t| t.items.iter()) {
                    if clip.shared_look.is_some_and(|id| !self.looks.contains_key(&id)) {
                        return Err("clip refers to a missing shared look".into());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn max_id(&self) -> u64 {
        let mut max = 0u64;
        for (id, still) in &self.gallery {
            max = max.max(*id).max(still.id).max(still.sequence.map_or(0, |i| i.0)).max(still.clip.map_or(0, |i| i.0)).max(still.item.map_or(0, |i| i.0));
        }
        for (clip, versions) in &self.versions {
            max = max.max(clip.0);
            for version in versions {
                max = max.max(version.id).max(version.shared_look.unwrap_or(0));
            }
        }
        for (id, look) in &self.looks {
            max = max.max(*id).max(look.id);
        }
        max
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.gallery.len() > MAX_COLOR_RECORDS || self.looks.len() > MAX_COLOR_RECORDS || self.versions.len() > 4096 {
            return Err("saved color workflow exceeds collection limits".into());
        }
        if self.max_id() >= crate::Project::MAX_NEXT_ID {
            return Err("saved color record ID exceeds project identity bounds".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        for (id, still) in &self.gallery {
            if *id == 0 || !ids.insert(*id) || !names.insert(still.name.to_ascii_lowercase()) || *id != still.id {
                return Err("gallery ID does not match its key".into());
            }
            validate_name(&still.name)?;
            still.reference.validate()?;
            validate_effects(&still.grade)?;
            if still.frame != still.rate.frame_at(still.time) || !(Tick::MIN..=Tick::MAX).contains(&still.time) || still.transform.len() > 65_536 {
                return Err("invalid gallery frame metadata".into());
            }
            crate::validate_frame_size(still.width, still.height)?;
            crate::SequenceSettings { width: still.width, height: still.height, frame_rate: still.rate, ..Default::default() }.validate()?;
            bytes = bytes
                .checked_add(still.png_base64.len())
                .and_then(|n| n.checked_add(still.reference.pixels.len().saturating_mul(4)))
                .ok_or("gallery size overflow")?;
            if bytes > MAX_GALLERY_BYTES {
                return Err("saved still gallery exceeds 64 MiB".into());
            }
        }
        for versions in self.versions.values() {
            let mut names = std::collections::BTreeSet::new();
            if versions.len() > 64 {
                return Err("at most 64 grade versions per clip".into());
            }
            for version in versions {
                if version.id == 0 || !ids.insert(version.id) || !names.insert(version.name.to_ascii_lowercase()) {
                    return Err("duplicate saved grade identity/name".into());
                }
                validate_name(&version.name)?;
                validate_effects(&version.effects)?;
                let count = version.effects.len().checked_add(version.non_color_effects.len()).ok_or("grade version effect count overflow")?;
                if version.non_color_effects.len() > 256
                    || version.color_positions.len() != version.effects.len()
                    || version.color_positions.iter().any(|p| *p >= count)
                    || !version.color_positions.windows(2).all(|p| p.first().zip(p.get(1)).is_some_and(|(a, b)| a < b))
                {
                    return Err("invalid saved grade effect positions".into());
                }
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for (id, look) in &self.looks {
            if *id == 0 || !ids.insert(*id) || !names.insert(look.name.to_ascii_lowercase()) || *id != look.id {
                return Err("shared look ID does not match its key".into());
            }
            validate_name(&look.name)?;
            if look.group.len() > 256 {
                return Err("look group exceeds 256 bytes".into());
            }
            validate_effects(&look.effects)?;
            if look.effects.iter().any(|e| e.is_animated() || !e.masks.is_empty()) {
                return Err("shared looks must be static and unmasked".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_projects_default_empty_and_references_reserve_identity_space() {
        let project = crate::Project::new("legacy");
        let mut data = serde_json::to_value(&project).unwrap();
        data.as_object_mut().unwrap().remove("color_workflow");
        let mut project: crate::Project = serde_json::from_value(data).unwrap();
        assert!(project.color_workflow.is_empty());
        project.color_workflow.looks.insert(
            90,
            SharedLook { id: 90, name: "Camera".into(), group: String::new(), effects: Vec::new().into(), pipeline: ColorPipeline::REC709, revision: 1 },
        );
        assert_eq!(project.max_id(), 90);
        assert!(project.color_workflow.validate().is_ok());
    }
    #[test]
    fn shared_color_looks_are_rejected_on_audio_even_when_the_master_exists() {
        let mut project = crate::Project::new("hostile audio shared look");
        let sequence = project.new_sequence("Timeline", crate::SequenceSettings::default(), 1, 1, None);
        let mut clip = project
            .make_track_item(
                sequence,
                crate::TrackKind::Audio,
                Tick::ZERO,
                filmcraft_time::TimeRange::new(Tick::ZERO, Tick(filmcraft_time::TICKS_PER_SECOND)),
                FrameRate::FPS_24,
            )
            .unwrap();
        clip.shared_look = Some(90);
        project.color_workflow.looks.insert(
            90,
            SharedLook { id: 90, name: "Camera".into(), group: String::new(), effects: Vec::new().into(), pipeline: ColorPipeline::REC709, revision: 1 },
        );
        project.sequence_mut(sequence).unwrap().audio_tracks[0].items.push(clip);
        assert!(project.color_workflow.validate().is_ok());
        assert!(project.color_workflow.validate_references(&project).unwrap_err().contains("audio"));
        project.sequence_mut(sequence).unwrap().audio_tracks[0].items[0].shared_look = None;
        assert!(project.color_workflow.validate_references(&project).is_ok());
    }
    #[test]
    fn hostile_frozen_images_and_effects_fail_without_panicking() {
        for image in [
            FrozenImage { width: usize::MAX, height: 2, pixels: Vec::new().into() },
            FrozenImage { width: 1, height: 1, pixels: vec![0.0, 0.0, 0.0, 2.0].into() },
            FrozenImage { width: 1, height: 1, pixels: vec![f32::NAN, 0.0, 0.0, 1.0].into() },
        ] {
            assert!(std::panic::catch_unwind(|| image.validate()).unwrap().is_err());
        }
        let mut e = crate::find_effect("lumetri").unwrap().instance();
        e.params.get_mut("exposure").unwrap().value = crate::ParamValue::Float(f64::INFINITY);
        assert!(validate_effects(&[e]).is_err());
    }
}
