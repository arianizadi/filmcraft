//! Typed audio analysis, audition and reviewable repair/synchronisation tools.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub clip: Option<u64>,
    #[schemars(length(max = 256))]
    pub clips: Option<Vec<u64>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Source,
    Clip,
    Sequence,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effects {
    Pre,
    Post,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Snapshot {
    Before,
    After,
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub target: Option<Target>,
    pub item: Option<u64>,
    pub clip: Option<u64>,
    pub sequence: Option<u64>,
    pub plan: Option<u64>,
    pub effects: Option<Effects>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inspect {
    pub subject: Subject,
    /// Exact inclusive ticks in source or timeline time; 254016000000 ticks =1s.
    pub start: Option<i64>,
    /// Exact exclusive ticks; inspection max300s, audition max10s.
    pub end: Option<i64>,
    /// Frozen plan snapshot; before requires subject.plan.
    pub snapshot: Option<Snapshot>,
    /// Compare both snapshots of the same staged plan.
    pub compare: Option<bool>,
    /// Source-only native container audio stream, zero based. Timeline clips retain their routing.
    pub stream: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job: u64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub enum Routing {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "stereo")]
    Stereo,
    #[serde(rename = "left")]
    Left,
    #[serde(rename = "right")]
    Right,
    #[serde(rename = "mono")]
    Mono,
    #[serde(rename = "swap")]
    Swap,
    #[serde(rename = "midSideEncode")]
    MidSideEncode,
    #[serde(rename = "midSideDecode")]
    MidSideDecode,
    #[serde(rename = "side")]
    Side,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub enum Preset {
    #[serde(rename = "dialogueClean")]
    DialogueClean,
    #[serde(rename = "dialogueWarm")]
    DialogueWarm,
    #[serde(rename = "podcast")]
    Podcast,
    #[serde(rename = "musicGentle")]
    MusicPreserved,
    #[serde(rename = "reset")]
    Reset,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Repair {
    pub clip: Option<u64>,
    #[schemars(length(max = 16))]
    pub clips: Option<Vec<u64>>,
    pub routing: Option<Routing>,
    pub preset: Option<Preset>,
    /// Full-clip LUFS measurement and peak-limited output gain. Defaultfalse.
    pub normalize: Option<bool>,
    #[schemars(range(min=-60,max=0))]
    pub target_lufs: Option<f64>,
    #[schemars(range(min = 0, max = 24))]
    pub max_gain_db: Option<f64>,
    #[schemars(range(min=-12,max=0))]
    pub true_peak_dbtp: Option<f64>,
    /// Explicit consent before replacing affected routing or managed preset keyframes.
    pub replace_animation: Option<bool>,
    pub base_revision: Option<u64>,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VoiceTone {
    Neutral,
    Warm,
    Bright,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    /// Explicit main microphone timeline audio clips. Camera/video references are not expanded.
    #[schemars(length(min = 1, max = 16))]
    pub main: Vec<u64>,
    /// Explicit camera/safety clips retained muted in the proposal, never deleted.
    #[schemars(length(max = 32))]
    pub backup: Option<Vec<u64>>,
    /// Explicit music clips to duck under these main microphones only.
    #[schemars(length(max = 16))]
    pub music: Option<Vec<u64>>,
    pub tone: Option<VoiceTone>,
    /// Conservative recipe strength; no automatic hardware/mic identity inference.
    #[schemars(range(min = 0, max = 1))]
    pub intensity: Option<f64>,
    /// Defaulttrue: conservatively repair only measured one-sided inputs.
    pub repair_channels: Option<bool>,
    /// Defaulttrue: normalize complete measurable main/music clips with bounded gain and peak headroom.
    pub normalize: Option<bool>,
    #[schemars(range(min=-60,max=0))]
    pub target_lufs: Option<f64>,
    #[schemars(range(min = 0, max = 24))]
    pub max_gain_db: Option<f64>,
    #[schemars(range(min=-12,max=0))]
    pub true_peak_dbtp: Option<f64>,
    /// Defaulttrue when music IDs are supplied: generate new ducking. Set false to preserve music Volume; replacing existing animation also requires replace_animation.
    pub duck_music: Option<bool>,
    #[schemars(range(min=-40,max=0))]
    pub duck_db: Option<f64>,
    #[schemars(range(min = 0, max = 5))]
    pub duck_fade_seconds: Option<f64>,
    /// Defaulttrue: append an owned post-fader master sample limiter; final true peaks remain separately measured.
    pub final_limiter: Option<bool>,
    #[schemars(range(min=-12,max=0))]
    pub limiter_ceiling_dbfs: Option<f64>,
    pub replace_animation: Option<bool>,
    pub base_revision: Option<u64>,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncMethod {
    Waveform,
    Manual,
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    /// Timeline camera clip or its chosen scratch-audio clip; remains fixed.
    pub reference: u64,
    /// External microphone timeline audio clip to move with its linked partners.
    pub clip: u64,
    pub method: Option<SyncMethod>,
    /// Manual anchor in reference SOURCE ticks, such as the clap position.
    pub reference_time: Option<i64>,
    /// Matching manual clap position in microphone SOURCE ticks.
    pub clip_time: Option<i64>,
    #[schemars(range(min = 0.1, max = 120))]
    pub window_seconds: Option<f64>,
    #[schemars(range(min = 0.0, max = 120))]
    pub max_lag_seconds: Option<f64>,
    /// Optional known hardware delay correction at48kHz; positive moves the mic later.
    pub offset_samples: Option<i64>,
    /// Mute only the selected/reference scratch-audio clip; video stays enabled.
    pub mute_reference: Option<bool>,
}

pub fn params<T: Serialize>(input: &T) -> Result<Value, crate::AutomationError> {
    let Value::Object(map) = serde_json::to_value(input).map_err(|e| crate::AutomationError::BadRequest(e.to_string()))? else {
        return Err(crate::AutomationError::BadRequest("expected audio parameter object".into()));
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        let key = match key.as_str() {
            "repair_channels" => "repairChannels",
            "duck_music" => "duckMusic",
            "duck_db" => "duckDb",
            "duck_fade_seconds" => "duckFadeSeconds",
            "final_limiter" => "finalLimiter",
            "limiter_ceiling_dbfs" => "limiterCeilingDbfs",
            "target_lufs" => "targetLufs",
            "max_gain_db" => "maxGainDb",
            "true_peak_dbtp" => "truePeakDbtp",
            "replace_animation" => "replaceAnimation",
            "base_revision" => "baseRevision",
            "reference_time" => "referenceTime",
            "clip_time" => "clipTime",
            "window_seconds" => "windowSeconds",
            "max_lag_seconds" => "maxLagSeconds",
            "offset_samples" => "offsetSamples",
            "mute_reference" => "muteReference",
            _ => &key,
        };
        out.insert(key.to_string(), value);
    }
    Ok(Value::Object(out))
}

/// Move large WAV payloads out of JSON and into native MCP audio blocks.
pub fn audition(mut report: Value) -> rmcp::model::CallToolResult {
    use rmcp::model::{CallToolResult, ContentBlock};
    fn take_audio(value: &mut Value, blocks: &mut Vec<ContentBlock>) {
        if let Some(audio) = value.get_mut("audio").and_then(Value::as_object_mut)
            && let Some(Value::String(data)) = audio.remove("base64")
        {
            blocks.push(ContentBlock::audio(data, "audio/wav"));
            audio.insert("payload".into(), json!("provided as an MCP audio block"));
        }
    }
    let mut blocks = Vec::new();
    if report.get("before").is_some() {
        if let Some(before) = report.get_mut("before") {
            take_audio(before, &mut blocks);
        }
        if let Some(after) = report.get_mut("after") {
            take_audio(after, &mut blocks);
        }
    } else {
        take_audio(&mut report, &mut blocks);
    }
    blocks.insert(0, ContentBlock::text(report.to_string()));
    CallToolResult::success(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audio_arguments_keep_engine_names_and_strip_only_top_level_nulls() {
        let p: Sync =
            serde_json::from_value(json!({"reference":1,"clip":2,"method":"manual","reference_time":123,"clip_time":456,"offset_samples":-12})).unwrap();
        let v = params(&p).unwrap();
        assert_eq!(v["referenceTime"], 123);
        assert_eq!(v["clipTime"], 456);
        assert_eq!(v["offsetSamples"], -12);
        assert!(v.get("muteReference").is_none());
        assert!(serde_json::from_value::<Sync>(json!({"reference":1,"clip":2,"unknown":true})).is_err());
    }
    #[test]
    fn audition_uses_audio_blocks_and_removes_base64_from_metadata() {
        let result = audition(json!({"before":{"audio":{"base64":"AAAA","frames":1}},"after":{"audio":{"base64":"BBBB","frames":1}}}));
        let data = serde_json::to_value(result).unwrap();
        assert_eq!(data["content"].as_array().unwrap().len(), 3);
        assert_eq!(data["content"][1]["type"], "audio");
        let metadata = data["content"][0]["text"].as_str().unwrap();
        assert!(!metadata.contains("AAAA"));
        assert!(!metadata.contains("BBBB"));
    }
}
