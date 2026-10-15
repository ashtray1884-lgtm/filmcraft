//! Audio plug-ins' own windows (VST3): opened from Effect Controls, kept in step with the clip's
//! effect. Knobs turned in a window become ordinary parameter changes (undoable, saved, and
//! keyframes where the parameter is animated); the plug-in's full state is saved in the effect
//! as it changes; parameters changed in FilmCraft are shown in the window.

use std::collections::HashMap;

use filmcraft_project::{ClipId, EffectInstance, ParamKind, ParamValue};
use serde_json::json;

use crate::FilmcraftApp;

pub struct OpenEditor {
    pub clip: ClipId,
    pub index: usize,
    pub effect: String,
    pub ed: Box<dyn filmcraft_render::external::PluginWindow>,
    /// The values the window and FilmCraft agree on (FilmCraft id → value).
    sent: HashMap<String, f64>,
}

/// The effect's parameters as plain numbers (switches 0/1, lists by index), at media time `mt`.
fn values(e: &EffectInstance, mt: filmcraft_time::Tick) -> Vec<(String, f64)> {
    let Some(def) = e.def() else { return Vec::new() };
    def.params
        .iter()
        .map(|pd| {
            let v = match e.params.get(pd.id).map(|p| p.value_at(mt)).unwrap_or_else(|| pd.default.clone()) {
                ParamValue::Float(f) => f,
                ParamValue::Bool(b) => b as i32 as f64,
                ParamValue::Choice(c) => c as f64,
                other => other.as_f64().unwrap_or(0.0),
            };
            (pd.id.to_string(), v)
        })
        .collect()
}

fn find_effect<'a>(app: &'a FilmcraftApp, clip: ClipId, index: usize, effect: &str) -> Option<(usize, &'a EffectInstance, filmcraft_time::Tick)> {
    let seq = app.session.active_sequence()?;
    let (_, it) = seq.find_item(clip)?;
    let ph = app.session.playhead();
    let mt = it.source_time_at(ph.clamp(it.start, (it.end() - filmcraft_time::Tick(1)).max(it.start)));
    // the same slot, or (after a reorder) the first effect of the same plug-in
    match it.effects.get(index).filter(|e| e.effect == effect) {
        Some(e) => Some((index, e, mt)),
        None => it.effects.iter().enumerate().find(|(_, e)| e.effect == effect).map(|(i, e)| (i, e, mt)),
    }
}

/// Open (or bring forward) the window of the plug-in at `index` on `clip`.
pub fn open(app: &mut FilmcraftApp, clip: ClipId, index: usize) {
    if let Some(o) = app.plugin_editors.iter().find(|o| o.clip == clip && o.index == index && o.ed.is_open()) {
        o.ed.focus();
        return;
    }
    let Some(seq) = app.session.active_sequence() else { return };
    let Some((_, it)) = seq.find_item(clip) else { return };
    let Some(e) = it.effects.get(index) else { return };
    let effect = e.effect.clone();
    let Some((_, e, mt)) = find_effect(app, clip, index, &effect) else { return };
    let vals = values(e, mt);
    let state = match e.params.get("__state").map(|p| &p.value) {
        Some(ParamValue::Text(s)) => Some(s.clone()),
        _ => None,
    };
    match filmcraft_render::external::open_plugin_window(&effect, state.as_deref(), vals.clone()) {
        Ok(ed) => app.plugin_editors.push(OpenEditor { clip, index, effect, ed, sent: vals.into_iter().collect() }),
        Err(err) => app.ui.status = format!("{}: {err}", tl!("The plug-in window could not open")),
    }
}

/// Each frame: apply what the windows report, send FilmCraft's changes to them, forget closed ones.
pub fn poll(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if app.plugin_editors.is_empty() {
        return;
    }
    let mut editors = std::mem::take(&mut app.plugin_editors);
    let mut keep = Vec::new();
    for mut o in editors.drain(..) {
        let mut closed = false;
        for u in o.ed.poll() {
            match u {
                filmcraft_render::external::WindowUpdate::Param(id, v) => {
                    let kind = find_effect(app, o.clip, o.index, &o.effect).and_then(|(_, e, _)| e.def()).and_then(|d| d.param(&id)).map(|p| p.kind.clone());
                    let value = match kind {
                        Some(ParamKind::Bool) => json!(v >= 0.5),
                        Some(ParamKind::Choice(_)) => json!(v.round().max(0.0) as u64),
                        Some(_) => json!(v),
                        None => continue,
                    };
                    let r = app.session.execute("effects.setParam", json!({"clip": o.clip.0, "effect": o.index, "param": id, "value": value, "merge": true}));
                    if r.is_ok() {
                        o.sent.insert(id, v);
                    }
                }
                filmcraft_render::external::WindowUpdate::State(s) => {
                    let _ = app.session.execute("effects.setPluginState", json!({"clip": o.clip.0, "index": o.index, "state": s}));
                }
                filmcraft_render::external::WindowUpdate::Closed(err) => {
                    closed = true;
                    if let Some(e) = err {
                        app.ui.status = format!("{}: {e}", tl!("The plug-in window closed"));
                    }
                }
            }
        }
        // the effect went away (deleted clip or effect): close its window
        let Some((idx, e, mt)) = find_effect(app, o.clip, o.index, &o.effect) else {
            o.ed.close();
            continue;
        };
        o.index = idx;
        if closed || !o.ed.is_open() {
            continue;
        }
        for (id, v) in values(e, mt) {
            if o.sent.get(&id).is_none_or(|s| (s - v).abs() > 1e-9) {
                o.ed.set_param(&id, v);
                o.sent.insert(id, v);
            }
        }
        keep.push(o);
    }
    if !keep.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    // windows opened while this ran stay too
    keep.append(&mut app.plugin_editors);
    app.plugin_editors = keep;
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::{EffectDef, EffectKind, ParamDef};

    fn plugin_defs() {
        let f = |id: &'static str, label: &'static str, min, max, d| ParamDef {
            id,
            label,
            kind: ParamKind::Float { min, max, soft_min: min, soft_max: max, unit: "dB", decimals: 1 },
            default: ParamValue::Float(d),
            animatable: true,
            group: None,
        };
        filmcraft_project::register_plugin_effects(vec![
            EffectDef {
                id: "ofx:com.test.Glow",
                name: "Test Glow",
                kind: EffectKind::Video,
                category: &["Video Effects", "Test FX"],
                params: vec![f("amount", "Amount", 0.0, 1.0, 0.5)],
                intrinsic: false,
                accelerated: false,
                float32: true,
                yuv: false,
            },
            EffectDef {
                id: "vst3:00112233445566778899AABBCCDDEEFF",
                name: "Test Comp",
                kind: EffectKind::Audio,
                category: &["Audio Effects", "Test Audio"],
                params: vec![
                    f("p1", "Threshold", -60.0, 0.0, -18.0),
                    ParamDef {
                        id: "p2",
                        label: "Mode",
                        kind: ParamKind::Choice(&["Clean", "Warm"]),
                        default: ParamValue::Choice(0),
                        animatable: false,
                        group: None,
                    },
                ],
                intrinsic: false,
                accelerated: false,
                float32: true,
                yuv: false,
            },
        ]);
    }

    /// Plug-in effects behave like FilmCraft's own: applied, listed in Effect Controls with their
    /// rows, set through the usual command, saved with the project (with an audio plug-in's
    /// state), and a missing plug-in renders the clip unchanged instead of failing.
    #[test]
    fn plugin_effects_apply_show_save_and_reload() {
        plugin_defs();
        let mut s = filmcraft_engine::Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let seq = s.active_sequence().unwrap();
        let vclip = seq.video_tracks[0].items[0].id;
        let aclip = seq.audio_tracks.iter().flat_map(|t| t.items.iter()).next().map(|i| i.id).expect("demo has audio");
        s.execute("effects.apply", json!({"clips": [vclip.0], "effect": "ofx:com.test.Glow"})).unwrap();
        s.execute("effects.apply", json!({"clips": [aclip.0], "effect": "Test Comp"})).unwrap();
        let vidx =
            s.active_sequence().unwrap().find_item(vclip).unwrap().1.effects.iter().position(|e| e.effect == "ofx:com.test.Glow").expect("applied by id");
        let aidx =
            s.active_sequence().unwrap().find_item(aclip).unwrap().1.effects.iter().position(|e| e.effect.starts_with("vst3:")).expect("applied by name");
        s.execute("effects.setParam", json!({"clip": vclip.0, "effect": vidx, "param": "amount", "value": 0.8})).unwrap();
        s.execute("effects.setParam", json!({"clip": aclip.0, "effect": aidx, "param": "p2", "value": "Warm"})).unwrap();
        s.execute("effects.setPluginState", json!({"clip": aclip.0, "index": aidx, "state": "vst3:1:AAEC:"})).unwrap();
        assert!(s.execute("effects.setPluginState", json!({"clip": vclip.0, "index": vidx, "state": "x"})).is_err(), "only audio plug-ins have a state");

        // Effect Controls draws the plug-in effect and its rows
        s.execute("timeline.select", json!({"clips": [vclip.0]})).unwrap();
        let mut app = crate::FilmcraftApp::new(s);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx, &crate::theme::Tokens::for_kind(Default::default()));
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0));
        for _ in 0..3 {
            app.auto.begin_frame();
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| crate::panels::effect_controls::show(&mut app, ui, rect));
            out.textures_delta.clear();
        }
        let ids: Vec<String> = app.auto.query("effectControls.").into_iter().map(|e| e.id.clone()).collect();
        assert!(ids.iter().any(|i| i == "effectControls.effect.ofx:com.test.Glow"), "{ids:?}");
        // no plug-in window here (the plug-in is not installed), so no button
        assert!(!ids.iter().any(|i| i.ends_with(".pluginWindow")));

        // a frame with the missing plug-in still renders (unchanged picture)
        assert!(app.session.try_render_program_at(0.25, filmcraft_time::Tick::ZERO).is_ok());

        // saved and opened again: effects, values and state come back
        let path = std::env::temp_dir().join(format!("fc-plugin-effects-{}.fcproj", std::process::id()));
        app.session.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
        let mut s2 = filmcraft_engine::Session::default();
        s2.execute("file.open", json!({"path": path.to_string_lossy()})).unwrap();
        let seq = s2.active_sequence().unwrap();
        let v = &seq.find_item(vclip).unwrap().1.effects[vidx];
        assert_eq!(v.params["amount"].value, ParamValue::Float(0.8));
        let a = &seq.find_item(aclip).unwrap().1.effects[aidx];
        assert_eq!(a.params["p2"].value, ParamValue::Choice(1));
        assert_eq!(a.params["__state"].value, ParamValue::Text("vst3:1:AAEC:".into()));
        let _ = std::fs::remove_file(&path);
    }
}
