//! Explicit microphone roles and frozen sequence audition, using the parent bounded worker.
use super::{Arc, FilmcraftApp, Seed, State, Value, button, checkbox, choices, json};
#[derive(Clone, Default)]
struct MixState {
    identity: Arc<()>,
    main: String,
    backup: String,
    music: String,
    tone: usize,
    intensity: f64,
    repair: bool,
    normalize: bool,
    duck: bool,
    duck_db: f64,
    limiter: bool,
}
impl MixState {
    fn new(identity: Arc<()>) -> Self {
        Self {
            identity,
            main: String::new(),
            backup: String::new(),
            music: String::new(),
            tone: 0,
            intensity: 0.35,
            repair: true,
            normalize: true,
            duck: true,
            duck_db: -12.0,
            limiter: true,
        }
    }
}
fn ids(text: &str, maximum: usize, required: bool) -> Result<Vec<u64>, String> {
    if text.len() > 1024 {
        return Err("Clip identity list is too long".into());
    }
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for word in text.split([',', ' ', '\n', '\t']).filter(|v| !v.is_empty()) {
        let id = word.parse::<u64>().map_err(|_| "Use unsigned audio clip identities separated by commas".to_string())?;
        if !seen.insert(id) {
            return Err("A clip occurs more than once in this role".into());
        }
        out.push(id);
        if out.len() > maximum {
            return Err(format!("At most{maximum} clips in this role"));
        }
    }
    if required && out.is_empty() {
        return Err("Choose at least one main microphone clip".into());
    }
    Ok(out)
}
fn params(m: &MixState, state: &State) -> Result<Value, String> {
    let main = ids(&m.main, 16, true)?;
    let backup = ids(&m.backup, 32, false)?;
    let music = ids(&m.music, 16, false)?;
    let mut seen = std::collections::BTreeSet::new();
    if main.iter().chain(&backup).chain(&music).any(|id| !seen.insert(*id)) {
        return Err("Main, backup and music roles must use different audio clips".into());
    }
    Ok(json!({"main":main,"backup":backup,"music":music,
        "tone":(["neutral","warm","bright"].get(m.tone).copied().unwrap_or("neutral")),"intensity":m.intensity,
        "repairChannels":m.repair,"normalize":m.normalize,"targetLufs":state.target_lufs,
        "maxGainDb":12,"truePeakDbtp":-1,"duckMusic":m.duck,"duckDb":m.duck_db,
        "duckFadeSeconds":0.4,"finalLimiter":m.limiter,"limiterCeilingDbfs":-1.5,
        "replaceAnimation":state.replace_animation}))
}
fn role(app: &mut FilmcraftApp, state: &State, ui: &mut egui::Ui, name: &str, label: &str, value: &mut String) {
    ui.label(label);
    let r = ui.add(egui::TextEdit::singleline(value).char_limit(1024).desired_width(ui.available_width()));
    app.auto.add_response(&format!("audioTools.mix.{name}"), &r, label);
    if button(app, ui, &format!("mix.{name}.useSelection"), "Use selected audio", !state.selection.is_empty()) {
        *value = state.selection.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    }
    if let Ok(ids) = ids(value, 32, false) {
        let names = ids
            .iter()
            .take(4)
            .filter_map(|id| app.session.active_sequence()?.find_item(filmcraft_project::ClipId(*id)).map(|(_, c)| format!("#{} {}", id, c.name)))
            .collect::<Vec<_>>();
        if !names.is_empty() {
            ui.small(names.join(" · "));
        }
    }
}
fn preview(app: &mut FilmcraftApp, state: &mut State) -> Result<(), String> {
    let plan = state.plan.ok_or("Prepare an audio proposal first")?;
    let (before_project, before_state, revision) = app.session.agent_plan_snapshot(plan, true).map_err(|e| e.to_string())?;
    let (after_project, after_state, after_revision) = app.session.agent_plan_snapshot(plan, false).map_err(|e| e.to_string())?;
    if !(revision == app.session.revision && *before_project == *app.session.project || *after_project == *app.session.project) {
        return Err("This proposal is stale. Prepare the mix again before listening.".into());
    }
    let sequence = after_state.active_sequence.ok_or("Proposal has no active sequence")?;
    let end = before_project
        .sequence(sequence)
        .zip(after_project.sequence(sequence))
        .map(|(a, b)| a.duration().min(b.duration()))
        .ok_or("Sequence does not exist in both snapshots")?;
    if end <= filmcraft_time::Tick::ZERO {
        return Err("Sequence has no audio preview range".into());
    }
    let playhead = app.session.playhead();
    let start = if playhead >= filmcraft_time::Tick::ZERO && playhead < end { playhead } else { filmcraft_time::Tick::ZERO };
    let stop = start + filmcraft_time::Tick::from_units(5, 1).min(end - start);
    let mut before = Seed::capture(&app.session);
    before.project = before_project;
    before.state = before_state;
    before.revision = revision;
    let mut after = before.clone();
    after.project = after_project;
    after.state = after_state;
    after.revision = after_revision;
    start_preview(app, state, json!({"subject":{"target":"sequence","sequence":sequence.0,"effects":"post"},"start":start.0,"end":stop.0}), before, after);
    Ok(())
}
fn start_preview(app: &mut FilmcraftApp, state: &mut State, p: Value, before: Seed, after: Seed) {
    super::start(app, state, "audio.preview", p, false, Some((before, after, false)));
}
pub(super) fn show(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui) {
    let key = egui::Id::unique("audioTools.mix.state");
    let mut mix = ui
        .data_mut(|d| d.remove_temp::<MixState>(key))
        .filter(|s| Arc::ptr_eq(&s.identity, &app.session.project_instance))
        .unwrap_or_else(|| MixState::new(app.session.project_instance.clone()));
    let open=egui::CollapsingHeader::new("Microphone mix automation").id_salt("audioTools.mix").show(ui,|ui|{
        ui.small("Choose explicit audio clip roles. Main voices stay positioned; backup recordings are retained muted. No microphone identity is guessed.");
        role(app,state,ui,"main","Main microphone clip IDs",&mut mix.main);
        role(app,state,ui,"backup","Camera / safety clip IDs (optional)",&mut mix.backup);
        role(app,state,ui,"music","Music clip IDs (optional)",&mut mix.music);
        choices(app,ui,"mix.tone","Voice tone",&mut mix.tone,&[("neutral","Neutral"),("warm","Warm"),("bright","Bright")]);
        let r=ui.add(egui::Slider::new(&mut mix.intensity,0.0..=1.0).text("Tone strength"));
        app.auto.add_response("audioTools.mix.intensity",&r,"Voice tone strength");
        checkbox(app,ui,"mix.repair","Repair confidently one-sided inputs",&mut mix.repair);
        checkbox(app,ui,"mix.normalize","Balance loudness to target above",&mut mix.normalize);
        checkbox(app,ui,"mix.duck","Duck selected music under these microphones",&mut mix.duck);
        if mix.duck{
            let r=ui.add(egui::Slider::new(&mut mix.duck_db,-40.0..=0.0).text("Duck dB"));
            app.auto.add_response("audioTools.mix.duckDb",&r,"Music reduction in dB");
        }
        checkbox(app,ui,"mix.limiter","Protect output with final limiter",&mut mix.limiter);
        ui.small("New music automation is proposed when ducking is checked. Existing affected animation needs the replacement checkbox above. A final limiter still needs exported-file true-peak verification.");
        let ready=state.pending.is_none();
        let p=params(&mix,state);
        if let Err(e)=&p{ui.small(e);}
        if button(app,ui,"mix.inspect","Analyze mix roles",ready&&p.is_ok()) && let Ok(p)=p.clone(){
            super::start(app,state,"audio.mix.inspect",p,false,None);
        }
        if button(app,ui,"mix.prepare","Prepare microphone mix…",ready&&p.is_ok()) && let Ok(p)=p{
            super::start(app,state,"audio.mix.prepare",p,true,None);
        }
        if button(app,ui,"mix.preview","Build staged sequence A/B",ready&&state.plan.is_some()) && let Err(e)=preview(app,state){
            state.message=e;
        }
        ui.small("Sequence A/B uses five seconds at the playhead in both frozen snapshots. Use Listen original / processed below to audition it.");
    });
    app.auto.add_response("audioTools.mix", &open.header_response, "Microphone mix automation");
    ui.data_mut(|d| d.insert_temp(key, mix));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_roles_are_bounded_disjoint_and_preserve_the_requested_voice_controls() {
        let mut mix = MixState::new(Arc::new(()));
        let state = State { target_lufs: -16.0, replace_animation: true, ..Default::default() };
        assert!(params(&mix, &state).is_err());
        mix.main = "7,8".into();
        mix.backup = "9".into();
        mix.music = "10".into();
        mix.tone = 2;
        let p = params(&mix, &state).unwrap();
        assert_eq!(p["main"], json!([7, 8]));
        assert_eq!(p["tone"], "bright");
        assert_eq!(p["replaceAnimation"], true);
        mix.backup = "8".into();
        assert!(params(&mix, &state).is_err());
        mix.backup = "9,9".into();
        assert!(params(&mix, &state).is_err());
        assert!(ids("-1", 16, false).is_err());
        assert!(ids(&"1,".repeat(1000), 16, false).is_err());
    }
}
