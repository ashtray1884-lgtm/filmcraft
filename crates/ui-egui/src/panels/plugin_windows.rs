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
