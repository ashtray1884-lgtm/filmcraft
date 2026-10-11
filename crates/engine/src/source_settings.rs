//! Clip ▸ Source Settings for camera raw media (Blackmagic RAW): what can be set comes from the
//! media itself ([`filmcraft_media::MediaSource::source_settings`]); the settings changed from
//! the as-shot values are stored per clip (`Interpretation::source_settings`) and the media pool
//! reopens the clip with them ([`crate::media_pool::media_key`]).
//!
//! - `clip.sourceSettings {item?}` (query): the schema and the values in effect, or a message for
//!   media without source settings.
//! - `clip.setSourceSettings {item?, settings: {key: value|null}, reset?, merge?}`: one undo step
//!   (`merge`: fold into the previous Source Settings step of the same clip, for live dialogs).
//!   Values are checked against the schema: unknown keys and wrong types are refused, numbers
//!   clamped to their range, choices must be listed; `null` returns a setting to as shot.

use filmcraft_project::{ItemId, ItemKind};
use serde_json::{Map, Value, json};

use crate::{EngineError, Result, Session};

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::Other(msg.into())
}

/// The media clip (root item) the command acts on: `item`, else the selection.
fn target(s: &Session, p: &Value) -> Result<ItemId> {
    let ids: Vec<ItemId> = match p.get("item").and_then(Value::as_u64) {
        Some(i) => vec![ItemId(i)],
        None => s
            .state
            .project_selection
            .iter()
            .copied()
            .chain(s.state.selection.iter().filter_map(|c| s.active_sequence().and_then(|q| q.find_item(*c)).map(|(_, it)| it.item)))
            .collect(),
    };
    ids.into_iter().find_map(|i| s.project.resolve_media(i).map(|(root, _, _)| root)).ok_or_else(|| bad("select a clip"))
}

/// The schema of the clip's media, as decoded now (`None`: no source settings, or offline).
fn schema(s: &Session, item: ItemId) -> Option<Value> {
    s.media.full_res_source(&s.project, item, &*s.services).and_then(|src| src.source_settings())
}

pub(crate) fn query(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p)?;
    let (_, m, _) = s.project.resolve_media(id).ok_or_else(|| bad("select a clip"))?;
    let codec = m.info.video.as_ref().map(|v| v.codec.clone()).or_else(|| m.info.audio().map(|a| a.codec.clone())).unwrap_or_default();
    let container = m.info.container.clone();
    let changed = m.interpret.source_settings.clone().unwrap_or_else(|| json!({}));
    let sc = schema(s, id);
    if let Some(sc) = &sc
        && sc["settings"].as_array().is_some_and(|l| !l.is_empty())
    {
        return Ok(json!({"item": id.0, "codec": codec, "container": container, "raw": sc, "changed": changed}));
    }
    // camera raw whose decoder offers nothing to set: its reason
    if let Some(msg) = sc.as_ref().and_then(|sc| sc["message"].as_str()) {
        return Ok(json!({"item": id.0, "codec": codec, "container": container, "settings": [], "message": msg}));
    }
    Ok(json!({
        "item": id.0,
        "codec": codec,
        "container": container,
        "settings": [],
        "message": format!(
            "{} has no source settings in FilmCraft. Source settings apply to camera raw formats; FilmCraft decodes Blackmagic RAW (on Windows, through Blackmagic's installed runtime).",
            if codec.is_empty() { "This clip".to_string() } else { codec }
        ),
    }))
}

/// Check one value against its schema entry; returns the value to store.
pub(crate) fn check_value(entry: &Value, v: &Value) -> std::result::Result<Value, String> {
    let key = entry["key"].as_str().unwrap_or("?");
    if entry["readOnly"] == true {
        return Err(format!("`{key}` cannot be changed for this clip"));
    }
    match entry["type"].as_str() {
        Some("number") => {
            let x = v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| format!("`{key}` must be a number"))?;
            let lo = entry["min"].as_f64().unwrap_or(f64::MIN);
            let hi = entry["max"].as_f64().unwrap_or(f64::MAX);
            Ok(json!(x.clamp(lo.min(hi), hi.max(lo))))
        }
        Some("toggle") => v.as_bool().map(Value::Bool).ok_or_else(|| format!("`{key}` must be true or false")),
        Some("choice") => {
            let opts = entry["options"].as_array().cloned().unwrap_or_default();
            // a number option may arrive as a number or as its text ("800" for ISO 800)
            let hit = opts.iter().find(|o| *o == v || (o.is_number() && v.as_str().and_then(|t| t.trim().parse::<f64>().ok()) == o.as_f64()));
            hit.cloned().ok_or_else(|| format!("`{key}`: {v} is not one of the options"))
        }
        other => Err(format!("`{key}` has an unknown type {other:?}")),
    }
}

pub(crate) fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let id = target(s, p)?;
    let sc = schema(s, id).filter(|sc| sc["settings"].as_array().is_some_and(|l| !l.is_empty())).ok_or_else(|| bad("this clip has no source settings"))?;
    let entries = sc["settings"].as_array().cloned().unwrap_or_default();
    let reset = p.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let mut cur: Map<String, Value> = if reset {
        Map::new()
    } else {
        s.project.resolve_media(id).and_then(|(_, m, _)| m.interpret.source_settings.clone()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
    };
    match p.get("settings") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => {
            for (k, v) in o {
                let entry = entries.iter().find(|e| e["key"] == *k).ok_or_else(|| bad(format!("`{k}` is not a source setting of this clip")))?;
                if v.is_null() {
                    cur.remove(k);
                } else {
                    cur.insert(k.clone(), check_value(entry, v).map_err(bad)?);
                }
            }
        }
        Some(_) => return Err(bad("`settings` must be an object of setting → value")),
    }
    let stored = if cur.is_empty() { None } else { Some(Value::Object(cur)) };
    let out = stored.clone().unwrap_or_else(|| json!({}));
    let apply = move |pr: &mut filmcraft_project::Project, _: &mut crate::EditorState| -> Result<()> {
        match pr.item_mut(id).map(|it| &mut it.kind) {
            Some(ItemKind::Media(m)) => {
                m.interpret.source_settings = stored;
                Ok(())
            }
            _ => Err(bad("the clip is gone")),
        }
    };
    let key = format!("source-settings:{}", id.0);
    if p.get("merge").and_then(Value::as_bool).unwrap_or(false) {
        s.edit_merged("Source Settings", &key, apply)?;
    } else {
        // a new undo step that later `merge` calls (the same dialog) fold into
        s.edit("Source Settings", apply)?;
        s.history.merge_key = Some(key);
    }
    Ok(json!({"item": id.0, "settings": out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_checked_against_the_schema() {
        let num = json!({"key": "saturation", "type": "number", "min": 0.0, "max": 2.0});
        assert_eq!(check_value(&num, &json!(1.5)), Ok(json!(1.5)));
        assert_eq!(check_value(&num, &json!(9)), Ok(json!(2.0)));
        assert!(check_value(&num, &json!("x")).is_err());
        assert!(check_value(&num, &json!(null)).is_err());
        let ch = json!({"key": "gamma", "type": "choice", "options": ["Blackmagic Design Film", "Blackmagic Design Video"]});
        assert_eq!(check_value(&ch, &json!("Blackmagic Design Video")), Ok(json!("Blackmagic Design Video")));
        assert!(check_value(&ch, &json!("Rec.2020 HLG")).is_err());
        let iso = json!({"key": "iso", "type": "choice", "options": [400, 800, 1600]});
        assert_eq!(check_value(&iso, &json!(800)), Ok(json!(800)));
        assert_eq!(check_value(&iso, &json!("1600")), Ok(json!(1600)));
        assert!(check_value(&iso, &json!(500)).is_err());
        let t = json!({"key": "highlightRecovery", "type": "toggle"});
        assert_eq!(check_value(&t, &json!(true)), Ok(json!(true)));
        assert!(check_value(&t, &json!(1)).is_err());
        let ro = json!({"key": "colorScienceGen", "type": "number", "readOnly": true});
        assert!(check_value(&ro, &json!(5)).is_err());
        assert!(check_value(&json!({"key": "k", "type": "matrix"}), &json!(1)).is_err());
    }
}
