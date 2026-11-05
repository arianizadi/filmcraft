//! Reviewed command access classification, independent of history/journal metadata.
//!
//! A command can avoid history while still writing files, starting a job or changing live
//! controls. New commands default to requiring Edit. Add a read here only after auditing its
//! implementation and frontend routing; the registry test forces review of new query commands.

use serde_json::Value;

const READ_ONLY: &[&str] = &[
    "analysis.footage",
    "workflow.status",
    "motion.templates",
    "subject.inspect",
    "library.search",
    "library.model.status",
    "library.indexStatus",
    "qc.timeline",
    "qc.export",
    "library.index",
    "review.checkpoints",
    "review.diff",
    "review.annotations",
    "review.activity",
    "profile.list",
    "profile.get",
    "profile.resolve",
    "agent.session",
    "agent.connections",
    "agent.operation",
    "agent.activity",
    "agent.inspect",
    "agent.preview",
    "agent.plans",
    "render.frames",
    "grade.get",
    "gallery.list",
    "gallery.get",
    "grade.version.list",
    "grade.look.list",
    "grade.cdl.inspect",
    "scopes.read",
    "command.list",
    "project.inspect",
    "sequence.inspect",
    "state.inspect",
    "effects.list",
    "history.list",
    "file.recoveryList",
    "file.autoSaveStatus",
    "file.listAutoSaves",
    "prefs.get",
    "jobs.list",
    "jobs.get",
    "perf.stats",
    "clip.audioPeak",
    "sequence.renderBar",
    "source.inspect",
    "trim.monitor",
    "color.spaces",
    "lumetri.presets",
    "media.colorInfo",
    "lut.list",
    "graphics.template.list",
    "graphics.template.controls",
    "graphics.fonts.used",
    "graphics.list",
    "fonts.list",
    "sequence.duplicateFrames",
    "sequence.throughEdits",
    "project.searchBinItems",
    "file.templates",
    "file.mediaPropertiesFile",
    "file.mediaProperties",
    "clip.sourceSettings",
    "clip.generateAudioWaveform",
    "help.systemReport",
    "masks.list",
    "captions.list",
    "media.proxyPresets",
    "mixer.inspect",
    "media.findMissing",
    "media.status",
    "media.search",
    "multicam.grid",
    "multicam.inspect",
    "project.view.get",
    "project.columns.list",
    "project.items",
    "project.viewPreset.list",
    "project.freeform.layout",
    "project.freeform.arrangements",
    "mediaBrowser.roots",
    "mediaBrowser.list",
    "mediaBrowser.probe",
    "events.list",
    "metadata.get",
    "project.notes",
    "tts.voices",
    "tts.inspect",
    "transcript.sourceWords",
    "transcript.inspect",
    "transcript.search",
    "transcript.models",
    "export.presets.list",
    "export.presets.get",
    "export.formats",
    "export.resolve",
    "presets.list",
    "essentialSound.inspect",
    "prefs.schema",
    "mediaCache.info",
    "labels.presets",
    "shortcuts.list",
    "shortcuts.get",
    "shortcuts.conflicts",
    "shortcuts.forKey",
    "shortcuts.resolve",
    "shortcuts.presets",
    "shortcuts.audit",
];

// Optional file output is a write even though the no-path command only returns image bytes.
const OPTIONAL_OUTPUT: &[&str] = &["lumetri.presetThumbnails", "graphics.template.thumbnail", "grade.cdl.export", "grade.bakeLut"];

// These historically carry journal:false. They must never inherit query access from that flag.
#[cfg(test)]
const SIDE_EFFECT_QUERIES: &[&str] = &[
    "library.model.download",
    "library.model.load",
    "library.model.unload",
    "library.indexAsync",
    "library.embeddings",
    "library.load",
    "library.clear",
    "motion.plan",
    "subject.trackPlan",
    "subject.correctPlan",
    "subject.reframePlan",
    "library.selectsPlan",
    "library.brollPlan",
    "library.save",
    "presets.scan",
    "lut.export",
    "file.reveal",
    "file.projectManager",
    "export.presets.export",
    "presets.export",
    "media.toggleProxies",
    "trim.tick",
    "mixer.touch",
    "clipMixer.touch",
    "mixer.recordStart",
    "clipMixer.setMode",
    "multicam.recordStart",
    "masks.select",
    "effects.select",
    "media.linkMedia",
    "tts.preview",
    "tts.render",
    "tts.downloadVoices",
    "export.queue.list",
    "agent.subset",
    "transcript.proposeCuts",
    "profile.plan",
    "profile.layout",
    "workflow.autoEdit",
    "workflow.prepare",
    "workflow.prepareAsync",
    "agent.pairRequest",
    "agent.plan",
    "agent.discard",
];

/// True only for audited read-only command forms. Unknown commands fail closed.
pub fn is_read_only(id: &str, params: &Value) -> bool {
    if READ_ONLY.contains(&id) {
        return true;
    }
    if OPTIONAL_OUTPUT.contains(&id) {
        return params.get("path").is_none_or(Value::is_null);
    }
    if id == "sequence.settings" {
        return params.is_null() || params.as_object().is_some_and(|p| p.is_empty());
    }
    if id == "file.projectManager" {
        return params.get("dryRun").and_then(Value::as_bool) == Some(true);
    }
    false
}

/// Commands that prepare isolated proposals without applying a live project edit.
pub fn is_proposal(id: &str) -> bool {
    matches!(
        id,
        "agent.plan"
            | "library.model.load"
            | "library.indexAsync"
            | "agent.discard"
            | "agent.subset"
            | "transcript.proposeCuts"
            | "profile.plan"
            | "profile.layout"
            | "library.selectsPlan"
            | "library.brollPlan"
            | "workflow.autoEdit"
            | "workflow.prepare"
            | "workflow.prepareAsync"
            | "motion.plan"
            | "subject.trackPlan"
            | "subject.correctPlan"
            | "subject.reframePlan"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_nonjournal_command_has_an_explicit_reviewed_classification() {
        let unknown: Vec<&str> = crate::command_specs()
            .iter()
            .filter(|spec| !spec.journal)
            .map(|spec| spec.id)
            .filter(|id| !READ_ONLY.contains(id) && !OPTIONAL_OUTPUT.contains(id) && !SIDE_EFFECT_QUERIES.contains(id))
            .collect();
        assert!(unknown.is_empty(), "Audit new query implementations and classify them explicitly: {unknown:?}");
    }

    #[test]
    fn file_output_jobs_and_live_controls_require_edit_despite_query_metadata() {
        for id in SIDE_EFFECT_QUERIES {
            assert!(!is_read_only(id, &json!({})), "{id} must require write permission");
        }
        for id in OPTIONAL_OUTPUT {
            assert!(is_read_only(id, &json!({})));
            assert!(is_read_only(id, &json!({"path":null})));
            assert!(!is_read_only(id, &json!({"path":"/tmp/output.png"})));
            assert!(!is_read_only(id, &json!({"path":false})), "malformed output must fail closed");
        }
        assert!(is_read_only("file.projectManager", &json!({"dryRun":true})));
        assert!(!is_read_only("file.projectManager", &json!({"dryRun":false})));
        assert!(!is_read_only("file.projectManager", &json!({"dryRun":"true"})));
    }

    #[test]
    fn unknown_commands_and_parameterized_settings_fail_closed() {
        assert!(!is_read_only("unreviewed.query", &json!({})));
        assert!(is_read_only("sequence.settings", &json!({})));
        assert!(is_read_only("sequence.settings", &Value::Null));
        assert!(!is_read_only("sequence.settings", &json!({"width":1920})));
        for id in READ_ONLY {
            assert!(is_read_only(id, &json!({})), "{id}");
        }
    }
}
