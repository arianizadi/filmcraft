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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderTarget {
    Sequence,
    Clip,
    Source,
}

#[derive(Debug, Deserialize, JsonSchema)]
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
