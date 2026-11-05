//! `scopes.read` on generated sequences with known colours (a colour matte, colour bars).

use serde_json::json;

use crate::Session;

fn matte_sequence(hex: &str) -> Session {
    let mut s = Session::default();
    let id = s.execute("file.newColorMatte", json!({"color": hex, "seconds": 2})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequenceFromClip", json!({"items": [id]})).unwrap();
    assert!(s.active_sequence().is_some());
    s
}

#[test]
fn scopes_read_reports_exact_levels_of_a_colour_matte() {
    let mut s = matte_sequence("#c86432");
    let r = s.execute("scopes.read", json!({"columns": 4})).unwrap();
    assert_eq!(r["colorSpace"], "Rec. 709");
    let h = &r["histogram"];
    assert_eq!(h["peakBin"], json!({"r": 200, "g": 100, "b": 50, "y": h["peakBin"]["y"].clone()}));
    let n = h["samples"].as_u64().unwrap();
    assert_eq!(h["r"][200].as_u64(), Some(n), "every sample in bin 200");
    // levels in percent: 200 / 255 = 78.43 %
    assert_eq!(r["stats"]["r"]["mean"], json!(78.43));
    assert_eq!(r["stats"]["b"]["max"], json!(19.61));
    for col in r["waveform"]["traces"]["R"].as_array().unwrap() {
        assert!((col["mean"].as_f64().unwrap() - 78.43).abs() < 0.01, "{col}");
    }
    assert_eq!(r["parade"]["traces"]["G"].as_array().unwrap().len(), 4);
    // the vectorscope: one spot at the matte's chroma
    let [_, cb, cr] = filmcraft_color::rgb_to_ycbcr(200.0 / 255.0, 100.0 / 255.0, 50.0 / 255.0, filmcraft_color::Matrix::Bt709);
    let pk = &r["vectorscopeYuv"]["peaks"][0];
    assert_eq!(pk["share"], json!(1.0));
    assert!((pk["cb"].as_f64().unwrap() - cb as f64 * 100.0).abs() < 0.3 && (pk["cr"].as_f64().unwrap() - cr as f64 * 100.0).abs() < 0.3, "{pk}");
    // Rec. 601 moves luma; the matrix is reported
    let r601 = s.execute("scopes.read", json!({"colorSpace": "601", "scopes": ["histogram"]})).unwrap();
    assert_eq!(r601["matrix"], "Bt601");
    assert_ne!(r601["histogram"]["peakBin"]["y"], h["peakBin"]["y"]);
    assert!(r601.get("waveform").is_none());
    // options and errors
    let r = s.execute("scopes.read", json!({"scopes": ["waveform"], "waveformType": "yc", "bins": false})).unwrap();
    assert_eq!(r["waveform"]["type"], "yc");
    assert!(r["waveform"]["traces"]["C"].is_array());
    assert!(s.execute("scopes.read", json!({"scopes": ["oscilloscope"]})).is_err());
    assert!(s.execute("scopes.read", json!({"colorSpace": "P3"})).is_err());
    assert!(Session::default().execute("scopes.read", json!({})).is_err());
}

#[test]
fn scopes_read_finds_the_colour_bar_targets() {
    let mut s = Session::default();
    let id = s.execute("file.newBarsAndTone", json!({"seconds": 2})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequenceFromClip", json!({"items": [id]})).unwrap();
    let r = s.execute("scopes.read", json!({"scopes": ["vectorscopeYuv"], "scale": 0.25})).unwrap();
    let peaks = r["vectorscopeYuv"]["peaks"].as_array().unwrap();
    let cells: Vec<(u64, u64)> = peaks.iter().take(7).map(|p| (p["x"].as_u64().unwrap(), p["y"].as_u64().unwrap())).collect();
    let p = filmcraft_scopes::Params::default();
    for t in filmcraft_scopes::targets(filmcraft_color::Matrix::Bt709, 191.0 / 255.0) {
        let (x, y) = filmcraft_scopes::cell_of(t.cb, t.cr, p.vector_size, filmcraft_scopes::VECTOR_EXTENT).unwrap();
        assert!(cells.contains(&(x as u64, y as u64)), "{} target ({x}, {y}) not among {cells:?}", t.name);
    }
}

#[test]
fn scope_subjects_ignore_overlay_and_return_roi_reference_deltas() {
    let mut s = matte_sequence("#406080");
    let sequence = s.state.active_sequence.unwrap();
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].clone();
    let overlay_item = s.execute("file.newColorMatte", json!({"color":"#ff0000","seconds":2})).unwrap()["item"].as_u64().unwrap();
    // A second video track fully covers the base clip. Isolation must remove it from the snapshot.
    let p = std::sync::Arc::make_mut(&mut s.project);
    let mut overlay = clip.clone();
    overlay.id = filmcraft_project::ClipId(p.alloc_id());
    overlay.item = filmcraft_project::ItemId(overlay_item);
    let track_id = filmcraft_project::TrackId(p.alloc_id());
    let q = p.sequence_mut(sequence).unwrap();
    let mut track = filmcraft_project::Track::new(track_id, filmcraft_project::TrackKind::Video, "Overlay".into());
    track.items.push(overlay);
    q.video_tracks.push(track);
    let revision = s.revision;
    let state = serde_json::to_value(&s.state).unwrap();
    let composite = s.execute("scopes.read", json!({"frame":0,"bins":false,"scopes":["histogram"]})).unwrap();
    let isolated=s.execute("scopes.read",json!({"target":"clip","clip":clip.id.0,"frames":[0,1],"roi":{"x":0.25,"y":0.25,"width":0.5,"height":0.5},"scopes":["histogram","waveform"],"images":true,
        "reference":{"target":"source","item":clip.item.0,"frame":0,"roi":{"x":0.25,"y":0.25,"width":0.5,"height":0.5}}})).unwrap();
    assert_eq!(composite["stats"]["r"]["mean"], 100.0);
    assert_eq!(isolated["frames"][0]["stats"]["r"]["mean"], 25.1);
    assert_eq!(isolated["frames"][0]["referenceDelta"]["channels"]["r"]["mean"], 0.0);
    assert_eq!(isolated["frames"][1]["frame"], 1);
    assert_eq!(isolated["frames"][0]["histogram"]["image"]["mimeType"], "image/png");
    assert_eq!(isolated["frames"][0]["clipping"]["whiteCode"], json!([0, 0, 0]));
    assert_eq!(s.revision, revision);
    assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
}

#[test]
fn scope_source_does_not_need_an_active_sequence_and_pre_post_effects_differ() {
    let mut s = matte_sequence("#406080");
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].clone();
    s.execute("effects.apply", json!({"clips":[clip.id.0],"effect":"lumetri"})).unwrap();
    s.execute("effects.setParam", json!({"clip":clip.id.0,"effect":"lumetri","param":"exposure","value":2.0})).unwrap();
    let pre = s.execute("scopes.read", json!({"target":"clip","clip":clip.id.0,"frame":0,"effects":"pre","bins":false})).unwrap();
    let post = s.execute("scopes.read", json!({"target":"clip","clip":clip.id.0,"frame":0,"effects":"post","bins":false})).unwrap();
    assert!(post["stats"]["y"]["mean"].as_f64().unwrap() > pre["stats"]["y"]["mean"].as_f64().unwrap());
    s.state.active_sequence = None;
    let source = s.execute("scopes.read", json!({"target":"source","item":clip.item.0,"frame":0,"bins":false})).unwrap();
    assert_eq!(source["stats"], pre["stats"]);
}

#[test]
fn hostile_scope_parameters_fail_without_panicking_or_mutation() {
    let mut s = matte_sequence("#808080");
    let revision = s.revision;
    for p in [
        json!({"roi":{"x":0,"y":0,"width":0,"height":1}}),
        json!({"roi":{"x":0.5,"y":0,"width":0.6,"height":1}}),
        json!({"roi":{"x":"bad","y":0,"width":1,"height":1}}),
        json!({"scale":-1}),
        json!({"scale":1e300}),
        json!({"scale":0}),
        json!({"scale":0.5,"maxSide":200}),
        json!({"reference":[]}),
        json!({"scopes":vec!["histogram";10000]}),
        json!({"columns":u64::MAX}),
        json!({"peaks":-1}),
        json!({"images":"yes"}),
        json!({"frames":[-1]}),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("scopes.read", p.clone())));
        assert!(result.is_ok(), "panic for {p}");
        assert!(result.unwrap().is_err(), "accepted {p}");
        assert_eq!(s.revision, revision);
    }
}

#[test]
fn roi_is_cropped_before_scope_decimation() {
    let mut s = matte_sequence("#808080");
    let subject = crate::inspection::subject(&s, &json!({}), false).unwrap();
    let r = s.execute("scopes.read", json!({"roi":{"x":0.2,"y":0.2,"width":0.01,"height":0.01},"maxSide":1000,"bins":false})).unwrap();
    assert!(r["samples"][0].as_u64().unwrap() > 1, "small region keeps its local samples");
    assert!(subject.validate_scale(0.5, 24).is_ok());
}

#[test]
fn isolated_scope_respects_intrinsic_opacity_and_reports_clipping() {
    let mut s = matte_sequence("#ffffff");
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
    s.execute("effects.setParam", json!({"clip":clip.0,"effect":"opacity","param":"opacity","value":50.0})).unwrap();
    let r = s.execute("scopes.read", json!({"target":"clip","clip":clip.0,"frame":0,"scopes":["histogram"]})).unwrap();
    let level = r["stats"]["r"]["mean"].as_f64().unwrap();
    assert!((level - 73.73).abs() < 0.5, "50 percent linear opacity must not look like an opaque clip: {level}");
    assert_eq!(r["clipping"]["whiteCode"], json!([0, 0, 0]));
    s.execute("effects.setParam", json!({"clip":clip.0,"effect":"opacity","param":"opacity","value":100.0})).unwrap();
    let r = s.execute("scopes.read", json!({"target":"clip","clip":clip.0,"frame":0,"scopes":["histogram"]})).unwrap();
    assert_eq!(r["clipping"]["whiteCodePercent"], json!([100.0, 100.0, 100.0]));
}
