//! Typed arguments for the inspect/propose/preview/apply/verify MCP surface.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanParams {
    pub label: Option<String>,
    /// Revision returned by inspection. Omit only when accepting the current revision.
    pub base_revision: Option<u64>,
    /// Only eligible in-memory commands; I/O, jobs and arbitrary UI actions are rejected.
    pub steps: Vec<super::BatchStep>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanIdParams {
    pub plan: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OperationParams {
    /// Copy from agent_session. Reuse this exact identity on retries, including after a restart.
    pub session_id: String,
    pub operation_id: String,
    pub idempotency_key: String,
    pub id: String,
    #[serde(default)]
    pub params: Option<Value>,
    pub base_revision: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyParams {
    pub plan: u64,
    pub session_id: String,
    pub operation_id: String,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetOperationParams {
    pub operation_id: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct InspectParams {
    pub sequence: Option<u64>,
    pub clip: Option<u64>,
    /// Inclusive timeline tick; 254016000000 ticks = 1 second.
    pub start: Option<i64>,
    /// Exclusive timeline tick.
    pub end: Option<i64>,
    /// Top-level sequence fields; revision and id are always included.
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderTarget {
    Sequence,
    Clip,
    Source,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effects {
    Pre,
    Post,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FramesParams {
    pub target: Option<RenderTarget>,
    pub sequence: Option<u64>,
    pub clip: Option<u64>,
    pub item: Option<u64>,
    /// Exact zero-based frame indexes in the declared target's timebase.
    pub frames: Option<Vec<i64>>,
    /// Exact ticks; use either frames or times.
    pub times: Option<Vec<i64>>,
    /// Longest output edge in pixels; 0 requests bounded native resolution.
    pub max_side: Option<u32>,
    pub effects: Option<Effects>,
    pub captions: Option<bool>,
    pub plan: Option<u64>,
    /// Render the same target before and after a staged plan.
    pub comparison: Option<bool>,
    pub contact_sheet: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GradeGetParams {
    pub clip: u64,
    pub sequence: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GradePatchParams {
    pub clip: u64,
    pub sequence: Option<u64>,
    /// Address returned by grade_get, or "new". Existing addresses require expected_revision.
    pub effect_instance: String,
    pub expected_revision: u64,
    /// Parameter id -> {value?: typed value, keyframes?: typed keyframe array}.
    pub params: Option<Value>,
    pub masks: Option<Vec<Value>>,
    pub enabled: Option<bool>,
    pub replace_keyframes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeParams {
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MatchReference {
    pub clip: Option<u64>,
    pub sequence: Option<u64>,
    pub item: Option<u64>,
    pub frames: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MatchTarget {
    pub clip: u64,
    pub sequence: Option<u64>,
    pub effect_instance: String,
    pub frames: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Full,
    Exposure,
    WhiteBalance,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MatchParams {
    pub reference: MatchReference,
    pub targets: Vec<MatchTarget>,
    pub expected_revision: u64,
    pub mode: Option<MatchMode>,
    pub samples_per_clip: Option<u32>,
    pub min_confidence: Option<f64>,
    pub skin_protection: Option<bool>,
    pub replace_keyframes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Roi {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ScopesParams {
    pub target: Option<RenderTarget>,
    pub sequence: Option<u64>,
    pub clip: Option<u64>,
    pub item: Option<u64>,
    pub frames: Option<Vec<i64>>,
    pub times: Option<Vec<i64>>,
    pub effects: Option<Effects>,
    pub roi: Option<Roi>,
    /// Subject/frame fields for the reference, using engine camelCase parameter names.
    pub reference: Option<Value>,
    pub plan: Option<u64>,
    pub color_space: Option<String>,
    pub images: Option<bool>,
    pub bins: Option<bool>,
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GradeAddress {
    pub clip: u64,
    pub sequence: Option<u64>,
    pub effect_instance: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GradeCopyParams {
    pub source: GradeAddress,
    pub targets: Vec<GradeAddress>,
    pub expected_revision: u64,
    pub replace_keyframes: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GradeResetParams {
    pub clip: u64,
    pub sequence: Option<u64>,
    pub effect_instance: String,
    pub expected_revision: u64,
    pub params: Option<Vec<String>>,
    pub replace_keyframes: Option<bool>,
}

// The following focused tools use snake_case MCP arguments. Nested profile override fields
// retain the engine's documented names (camelCase profiles/delivery, snake_case caption style).
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
pub enum AutoEditMode {
    #[serde(rename = "talkingHead")]
    TalkingHead,
    #[serde(rename = "dj")]
    Dj,
    #[serde(rename = "selects")]
    Selects,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareFootageParams {
    /// Imported original source IDs; supply items or paths. At most 32 unique sources.
    #[schemars(length(min = 1, max = 32))]
    pub items: Option<Vec<u64>>,
    /// Absolute source paths. Imports and recognition stay staged until approval; no downloads.
    #[schemars(length(min = 1, max = 32))]
    pub paths: Option<Vec<String>>,
    #[schemars(length(min = 1, max = 128))]
    pub profile: Option<String>,
    /// Default talkingHead preserves dialogue, proposing saved transcript pause/filler removals.
    pub mode: Option<AutoEditMode>,
    /// An audio source ID included in items; used as the music bed.
    pub music_item: Option<u64>,
    pub name: Option<String>,
    #[schemars(range(min = 1, max = 96))]
    pub max_shots: Option<u32>,
    /// Positive exact ticks (254016000000 per second), at most one hour. DJ/selects target.
    #[schemars(range(min = 1, max = 914457600000000_i64))]
    pub target_duration_ticks: Option<i64>,
    /// One-off profile fields; nested names follow the existing engine profile schema.
    pub overrides: Option<ProfileOverrides>,
    /// Default true. Measures bounded source samples; selection remains explicitly heuristic.
    pub analyze: Option<bool>,
    /// Default true. Measure and normalize available audio to the resolved profile target.
    pub normalize_audio: Option<bool>,
    /// Default true for talkingHead; derives captions from available saved/local transcripts.
    pub captions: Option<bool>,
    /// Optional false only. Preparation never applies edits; review and approve the returned plan.
    #[schemars(extend("enum" = [false, null]))]
    pub apply: Option<bool>,
    /// Default true. Uses an already installed local model only; never downloads automatically.
    pub transcribe: Option<bool>,
    /// Default follows analyze. Includes bounded timeline QC evidence in the proposal.
    pub qc: Option<bool>,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileOverrides {
    /// Named shared look. Explicit null clears the existing preference.
    #[serde(default, deserialize_with = "nullable_text", skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub reference_look: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub skin_tone_preference: Option<String>,
    /// fast/slow have special pause thresholds; other text uses balanced pacing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub pacing: Option<String>,
    /// Source pause padding in exact ticks; 0–60 seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 15240960000000_i64))]
    pub pause_padding_ticks: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption_style: Option<CaptionStyleOverrides>,
    /// Integrated loudness target in LUFS.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = -60.0, max = 0.0))]
    pub loudness_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryOverrides>,
}
fn nullable_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    let value = Value::deserialize(d)?;
    if value.is_null() || value.as_str().is_some_and(|s| s.len() <= 1024) {
        Ok(Some(value))
    } else {
        Err(serde::de::Error::custom("expected null or text up to 1024 bytes"))
    }
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptionStyleOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1024))]
    pub font: Option<String>,
    /// Font pixels for a 1080-line frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1.0, max = 1000.0))]
    pub size: Option<f64>,
    /// sRGB RGBA byte channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<[u8; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<CaptionAlign>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<CaptionAnchor>,
    /// Fraction of frame height.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub margin: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.1, max = 10.0))]
    pub line_spacing: Option<f64>,
    /// Outline pixels for a 1080-line frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 100.0))]
    pub outline: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline_color: Option<[u8; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub font_style: Option<String>,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
pub enum CaptionAlign {
    Left,
    Center,
    Right,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
pub enum CaptionAnchor {
    Top,
    Middle,
    Bottom,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryOverrides {
    /// Pixels; engine additionally caps total frame pixels and frame-rate product.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 32768))]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 32768))]
    pub height: Option<u32>,
    /// Positive rational frames per second, subject to exact tick and encoder/container limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub fps_num: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub fps_den: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<DeliveryFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_space: Option<DeliveryColorSpace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionDelivery>,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryFormat {
    H264,
    Hevc,
    Prores,
    Dnxhr,
    Png,
    Wav,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
pub enum DeliveryColorSpace {
    #[serde(rename = "rec709")]
    Rec709,
    #[serde(rename = "rec2100-pq")]
    Rec2100Pq,
    #[serde(rename = "rec2100-hlg")]
    Rec2100Hlg,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CaptionDelivery {
    None,
    Srt,
    Vtt,
    Burn,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliverEditParams {
    #[schemars(range(min = 1))]
    pub plan: u64,
    /// Current inspected revision. The approved plan must still describe the live project.
    pub base_revision: u64,
    #[schemars(length(min = 1, max = 8192))]
    pub path: String,
    pub profile: Option<String>,
    /// Default false starts a durable export job; poll it and run the returned QC request.
    pub wait: Option<bool>,
    /// Caller-owned operation identity; reuse all three fields exactly after a lost response.
    #[schemars(length(min = 1, max = 256))]
    pub session_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub operation_id: String,
    #[schemars(length(min = 1, max = 256))]
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparationStatusParams {
    /// Durable job ID from prepare_footage; omit to inspect available preparation statuses.
    pub job: Option<u64>,
}

#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaSearchParams {
    #[schemars(length(min = 1, max = 8192))]
    pub query: String,
    /// Default 10; thumbnail requests permit at most 10 hits.
    #[schemars(range(min = 1, max = 50))]
    pub limit: Option<u32>,
    pub thumbnails: Option<bool>,
    /// Advanced external vectors must match the indexed model and provenance exactly.
    pub embedding: Option<SearchEmbedding>,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchEmbedding {
    pub model: EmbeddingModel,
    #[schemars(length(min = 1, max = 4096))]
    pub vector: Vec<f64>,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingModel {
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    #[schemars(range(min = 1, max = 4096))]
    pub dimension: u32,
    #[schemars(length(min = 1, max = 2048))]
    pub provenance: String,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalysisSubject {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<RenderTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<Effects>,
}
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootageAnalysisParams {
    pub subject: AnalysisSubject,
    /// Inclusive target time in exact ticks. Each analysis window is limited to 300 seconds.
    #[schemars(range(min = 0))]
    pub start: Option<i64>,
    /// Exclusive target time in exact ticks (254016000000 per second).
    #[schemars(range(min = 1))]
    pub end: Option<i64>,
    /// Positive sequence/source frame stride. Omit to derive a stride within max_frames.
    #[schemars(range(min = 1))]
    pub stride_frames: Option<u32>,
    #[schemars(range(min = 1, max = 1200))]
    pub max_frames: Option<u32>,
    pub video: Option<bool>,
    pub audio: Option<bool>,
    /// Opt-in inference with an installed local model; default false.
    pub speech: Option<bool>,
    pub model: Option<String>,
    pub music: Option<bool>,
    pub waveform: Option<bool>,
    pub details: Option<bool>,
    /// Fraction of near-black pixels classifying a sampled frame as black (default .98).
    #[schemars(range(min = 0.0, max = 1.0))]
    pub black_threshold: Option<f64>,
    /// Mean absolute RGB difference threshold (default .001).
    #[schemars(range(min = 0.0, max = 1.0))]
    pub freeze_threshold: Option<f64>,
    #[schemars(range(min = 0.01, max = 1.0))]
    pub shot_threshold: Option<f64>,
    /// RMS silence threshold in dBFS (default -42).
    #[schemars(range(min = -120.0, max = 0.0))]
    pub silence_db: Option<f64>,
    /// Adjacent audio sample delta (default .8).
    #[schemars(range(min = 0.05, max = 2.0))]
    pub click_threshold: Option<f64>,
}

/// Map only the focused tool's top-level naming convention; keep nested profile and model data
/// intact, including explicit referenceLook:null and false/zero values.
pub fn engine_params<T: serde::Serialize>(p: &T) -> Result<Value, crate::AutomationError> {
    let value = serde_json::to_value(p).map_err(|e| crate::AutomationError::BadRequest(e.to_string()))?;
    let Value::Object(map) = value else { return Err(crate::AutomationError::BadRequest("expected parameter object".into())) };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        let engine_key = match key.as_str() {
            "music_item" => "musicItem",
            "max_shots" => "maxShots",
            "target_duration_ticks" => "targetDurationTicks",
            "normalize_audio" => "normalizeAudio",
            "stride_frames" => "strideFrames",
            "max_frames" => "maxFrames",
            "black_threshold" => "blackThreshold",
            "freeze_threshold" => "freezeThreshold",
            "shot_threshold" => "shotThreshold",
            "silence_db" => "silenceDb",
            "click_threshold" => "clickThreshold",
            _ => &key,
        };
        out.insert(engine_key.to_string(), value);
    }
    Ok(Value::Object(out))
}
