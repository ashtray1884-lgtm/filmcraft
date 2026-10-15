//! Finding installed VST3 plug-ins, scanning them in a child process, and listing them as
//! FilmCraft audio effects.
//!
//! - **Where:** the standard VST3 folders (Windows `%CommonProgramFiles%\VST3` and
//!   `%LOCALAPPDATA%\Programs\Common\VST3`; Linux `~/.vst3`, `/usr/lib/vst3`,
//!   `/usr/local/lib/vst3`) plus `VST3_PATH`; every `*.vst3` bundle (or Windows single-file
//!   `.vst3`) up to six folders deep.
//! - **Scan:** `filmcraft --vst3-scan <bundle>` describes a bundle's audio effects (name, vendor,
//!   parameters with their units and ranges, editor, tail) in a child process: a plug-in that
//!   crashes, hangs or shows a dialog while loading costs only itself. Two minutes at most each;
//!   results cached by path, size and time in `vst3-plugins.json`.
//! - **List:** each effect becomes an audio effect `vst3:<class id>` under Audio Effects ▸ its
//!   vendor, usable on clips and as mixer track inserts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use filmcraft_project::{EffectDef, EffectKind, ParamDef, ParamKind, ParamValue};
use serde::{Deserialize, Serialize};

use super::fx::{kind_of, param_id};
use super::module::{Instance, ParamInfo, cid_hex, open};

/// One audio effect class in a bundle.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub bundle: String,
    /// Class id, 32 hex digits.
    pub cid: String,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub sub_categories: String,
    #[serde(default)]
    pub params: Vec<ParamInfo>,
    #[serde(default)]
    pub has_editor: bool,
    #[serde(default)]
    pub latency: usize,
    #[serde(default)]
    pub tail_seconds: f64,
    /// Channels of the main input and output bus as the plug-in arranged them for stereo.
    #[serde(default)]
    pub inputs: usize,
    #[serde(default)]
    pub outputs: usize,
}

/// Describe every audio effect in `bundle` (what the scan child does).
pub fn describe_bundle(bundle: &Path) -> Result<Vec<PluginInfo>, String> {
    let m = open(bundle)?;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for c in m.classes() {
        if c.category != "Audio Module Class" {
            continue;
        }
        let subs = c.sub_categories.to_ascii_lowercase();
        if subs.contains("instrument") && !subs.contains("fx") {
            continue;
        }
        match Instance::new(&m, &c.cid, 48_000.0, 2, 1024) {
            Ok(inst) => {
                out.push(PluginInfo {
                    bundle: bundle.to_string_lossy().to_string(),
                    cid: cid_hex(&c.cid),
                    name: c.name.clone(),
                    vendor: c.vendor.clone(),
                    version: c.version.clone(),
                    sub_categories: c.sub_categories.clone(),
                    params: inst.params(),
                    has_editor: inst.has_editor(),
                    latency: inst.latency(),
                    tail_seconds: inst.tail() as f64 / 48_000.0,
                    inputs: inst.ins.first().copied().unwrap_or(0),
                    outputs: inst.outs.first().copied().unwrap_or(0),
                });
            }
            Err(e) => errors.push(format!("{}: {e}", c.name)),
        }
    }
    if out.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(out)
}

fn leak(s: impl Into<String>) -> &'static str {
    Box::leak(s.into().into_boxed_str())
}

/// FilmCraft rows for a plug-in's parameters (automatable or not; hidden, read-only, bypass and
/// program-change parameters are left out: FilmCraft has its own on/off switch).
pub fn param_defs(params: &[ParamInfo]) -> Vec<ParamDef> {
    params
        .iter()
        .filter(|p| p.visible())
        .map(|p| {
            let id = leak(param_id(p.id));
            let label = leak(p.title.clone());
            let animatable = p.flags & vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::kCanAutomate != 0;
            let (toggle, steps) = kind_of(p);
            if toggle {
                return ParamDef { id, label, kind: ParamKind::Bool, default: ParamValue::Bool(p.default >= 0.5), animatable: false, group: None };
            }
            if steps > 0 {
                let opts: &'static [&'static str] = Box::leak(p.choices.iter().map(|c| leak(c.clone())).collect::<Vec<_>>().into_boxed_slice());
                let d = ((p.default * steps as f64).round() as u32).min(opts.len().saturating_sub(1) as u32);
                return ParamDef { id, label, kind: ParamKind::Choice(opts), default: ParamValue::Choice(d), animatable: false, group: None };
            }
            if !p.plain_ok {
                return ParamDef {
                    id,
                    label,
                    kind: ParamKind::Float { min: 0.0, max: 1.0, soft_min: 0.0, soft_max: 1.0, unit: "", decimals: 3 },
                    default: ParamValue::Float(p.default),
                    animatable,
                    group: None,
                };
            }
            let (lo, hi) = if p.plain_min <= p.plain_max { (p.plain_min, p.plain_max) } else { (p.plain_max, p.plain_min) };
            let (lo, hi) = (lo.clamp(-1e9, 1e9), hi.clamp(-1e9, 1e9).max(lo.clamp(-1e9, 1e9)));
            let decimals = if p.steps > 64 {
                0
            } else if (hi - lo).abs() >= 100.0 {
                1
            } else {
                2
            };
            ParamDef {
                id,
                label,
                kind: ParamKind::Float { min: lo, max: hi, soft_min: lo, soft_max: hi, unit: leak(p.units.trim().to_string()), decimals },
                default: ParamValue::Float(p.plain_default.clamp(lo, hi)),
                animatable,
                group: None,
            }
        })
        .collect()
}

pub fn effect_defs(infos: &[PluginInfo]) -> Vec<EffectDef> {
    infos
        .iter()
        .map(|i| {
            let vendor = if i.vendor.trim().is_empty() { "VST3" } else { leak(i.vendor.trim().to_string()) };
            EffectDef {
                id: leak(format!("vst3:{}", i.cid)),
                name: leak(i.name.clone()),
                kind: EffectKind::Audio,
                category: Box::leak(vec!["Audio Effects", vendor].into_boxed_slice()),
                params: param_defs(&i.params),
                intrinsic: false,
                accelerated: false,
                float32: true,
                yuv: false,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Finding, scanning, caching

pub fn search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::var_os("VST3_PATH") {
        v.extend(std::env::split_paths(&p).filter(|p| !p.as_os_str().is_empty()));
    }
    #[cfg(windows)]
    {
        for var in ["CommonProgramFiles", "CommonProgramW6432"] {
            if let Some(root) = std::env::var_os(var) {
                v.push(PathBuf::from(root).join("VST3"));
            }
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(l).join("Programs").join("Common").join("VST3"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join(".vst3"));
        }
        v.push(PathBuf::from("/usr/lib/vst3"));
        v.push(PathBuf::from("/usr/local/lib/vst3"));
    }
    let mut seen = Vec::new();
    v.retain(|p| {
        let k = p.to_string_lossy().to_ascii_lowercase();
        let new = !seen.contains(&k);
        seen.push(k);
        new
    });
    v
}

pub fn find_bundles(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        if depth > 6 || out.len() > 4000 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let is_vst3 = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3"));
            if is_vst3 && (p.is_dir() || cfg!(windows)) {
                out.push(p);
            } else if p.is_dir() {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub modified: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub plugins: Vec<PluginInfo>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub bundles: BTreeMap<String, Entry>,
}

/// 3: parameter defaults are the values plug-ins start with; `plain_ok`.
const CACHE_VERSION: u32 = 3;

/// Size and time of a bundle: its binary's, so an update in place is noticed.
fn stamp(bundle: &Path) -> Option<(u64, u64)> {
    let bin = super::module::binary_of(bundle).unwrap_or_else(|| bundle.to_path_buf());
    let m = std::fs::metadata(bin).ok()?;
    let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Some((m.len(), t))
}

fn cache_file(data_dir: Option<PathBuf>) -> PathBuf {
    data_dir.unwrap_or_else(std::env::temp_dir).join("vst3-plugins.json")
}

pub fn load_cache(path: &Path) -> Cache {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .filter(|c| c.version == CACHE_VERSION)
        .unwrap_or(Cache { version: CACHE_VERSION, ..Default::default() })
}

fn save_cache(path: &Path, c: &Cache) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(b) = serde_json::to_vec_pretty(c) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub enum Scanner {
    Process(PathBuf),
    InProcess,
}

fn scan_one(bundle: &Path, scanner: &Scanner) -> Entry {
    let (size, modified) = stamp(bundle).unwrap_or((0, 0));
    let r = match scanner {
        Scanner::InProcess => describe_bundle(bundle),
        Scanner::Process(exe) => scan_in_child(exe, bundle),
    };
    match r {
        Ok(plugins) => Entry { size, modified, error: None, plugins },
        Err(e) => {
            log::warn!("VST3: {} skipped: {e}", bundle.display());
            Entry { size, modified, error: Some(e), plugins: Vec::new() }
        }
    }
}

fn scan_in_child(exe: &Path, bundle: &Path) -> Result<Vec<PluginInfo>, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(exe)
        .arg("--vst3-scan")
        .arg(bundle)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start the scan ({e})"))?;
    let mut stdout = child.stdout.take().ok_or("no scan output")?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > Duration::from_secs(120) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the plug-in did not finish loading within two minutes".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = reader.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&out);
    let line = text
        .lines()
        .rev()
        .find(|l| l.starts_with("VST3SCAN "))
        .ok_or_else(|| if status.success() { "the scan printed nothing".to_string() } else { format!("the plug-in crashed while loading ({status})") })?;
    serde_json::from_str::<Result<Vec<PluginInfo>, String>>(&line["VST3SCAN ".len()..]).map_err(|e| e.to_string())?
}

/// `--vst3-scan <bundle>`: the child side of [`scan_in_child`]; returns whether it ran.
pub fn scan_main() -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_none_or(|a| a != "--vst3-scan") {
        return false;
    }
    let r = match args.get(2) {
        Some(b) => describe_bundle(Path::new(b)),
        None => Err("no bundle given".to_string()),
    };
    let line = serde_json::to_string(&r).unwrap_or_else(|e| serde_json::json!({"Err": e.to_string()}).to_string());
    println!("VST3SCAN {line}");
    // plug-ins may leave threads running: leave without unloading them
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

pub fn refresh(cache: &mut Cache, dirs: &[PathBuf], scanner: &Scanner, mut on_new: impl FnMut(&[PluginInfo])) {
    let mut present = Vec::new();
    for d in dirs {
        for b in find_bundles(d) {
            let key = b.to_string_lossy().to_string();
            present.push(key.clone());
            let st = stamp(&b);
            if cache.bundles.get(&key).is_some_and(|e| st == Some((e.size, e.modified))) {
                continue;
            }
            log::info!("VST3: scanning {}", b.display());
            let e = scan_one(&b, scanner);
            on_new(&e.plugins);
            cache.bundles.insert(key, e);
        }
    }
    cache.bundles.retain(|k, _| present.contains(k));
}

pub fn all(cache: &Cache) -> Vec<PluginInfo> {
    cache.bundles.values().flat_map(|e| e.plugins.iter().cloned()).collect()
}

pub fn start(data_dir: Option<PathBuf>) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = start_with(data_dir, Scanner::Process(exe), search_paths());
    }
}

pub fn start_with(data_dir: Option<PathBuf>, scanner: Scanner, dirs: Vec<PathBuf>) -> Option<std::thread::JoinHandle<()>> {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let mut handle = None;
    STARTED.call_once(|| {
        let host = Arc::new(super::fx::Host::new());
        filmcraft_render::external::set_audio_plugins(host.clone());
        super::HOST.get_or_init(|| host.clone());
        #[cfg(windows)]
        filmcraft_render::external::set_plugin_windows(Arc::new(super::editor::Windows));
        let path = cache_file(data_dir);
        let mut cache = load_cache(&path);
        let known = all(&cache);
        host.add(&known);
        filmcraft_project::register_plugin_effects(effect_defs(&known));
        let spawned = std::thread::Builder::new().name("vst3-scan".into()).spawn(move || {
            refresh(&mut cache, &dirs, &scanner, |found| {
                host.add(found);
                filmcraft_project::register_plugin_effects(effect_defs(found));
            });
            save_cache(&path, &cache);
            log::info!("VST3: {} audio effects", cache.bundles.values().map(|e| e.plugins.len()).sum::<usize>());
        });
        match spawned {
            Ok(h) => handle = Some(h),
            Err(e) => log::warn!("VST3: could not start the plug-in scan: {e}"),
        }
    });
    handle
}

/// Scan now and list what is installed (`filmcraft-cli plugins`).
pub fn scan_and_report(data_dir: Option<PathBuf>) -> String {
    let path = cache_file(data_dir);
    let mut cache = load_cache(&path);
    if let Ok(exe) = std::env::current_exe() {
        refresh(&mut cache, &search_paths(), &Scanner::Process(exe), |_| {});
        save_cache(&path, &cache);
    }
    let mut lines = vec![format!("VST3 plug-in folders: {}", search_paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; "))];
    if cache.bundles.is_empty() {
        lines.push("  no VST3 plug-ins found".into());
    }
    let mut n = 0;
    for (b, e) in &cache.bundles {
        match &e.error {
            Some(err) => lines.push(format!("  {b}: skipped ({err})")),
            None => {
                n += e.plugins.len();
                lines.push(format!(
                    "  {b}: {}",
                    e.plugins
                        .iter()
                        .map(|p| format!(
                            "{} ({} parameters{})",
                            p.name,
                            p.params.iter().filter(|q| q.visible()).count(),
                            if p.has_editor { ", own window" } else { "" }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    lines.push(format!("  {n} audio effects"));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_::*;

    fn p(id: u32, title: &str, steps: i32, flags: i32) -> ParamInfo {
        ParamInfo {
            id,
            title: title.into(),
            units: "dB".into(),
            steps,
            default: 0.5,
            flags,
            plain_min: -24.0,
            plain_max: 24.0,
            plain_default: 0.0,
            choices: Vec::new(),
            plain_ok: true,
        }
    }

    #[test]
    fn parameters_become_rows_in_the_plugins_units() {
        let mut mode = p(3, "Mode", 2, kCanAutomate);
        mode.choices = vec!["Clean".into(), "Warm".into(), "Hot".into()];
        let defs =
            param_defs(&[p(1, "Gain", 0, kCanAutomate), p(2, "Bypass", 1, kCanAutomate | kIsBypass), mode, p(4, "On", 1, 0), p(5, "Meter", 0, kIsReadOnly)]);
        assert_eq!(defs.iter().map(|d| d.id).collect::<Vec<_>>(), vec!["p1", "p3", "p4"]);
        match defs[0].kind {
            ParamKind::Float { min, max, unit, .. } => assert_eq!((min, max, unit), (-24.0, 24.0, "dB")),
            ref k => panic!("{k:?}"),
        }
        assert!(defs[0].animatable);
        assert_eq!(defs[1].default, ParamValue::Choice(1));
        assert_eq!(defs[2].kind, ParamKind::Bool);
    }

    #[test]
    fn bundles_are_found_in_vendor_folders() {
        let dir = std::env::temp_dir().join(format!("fc-vst3-find-{}", std::process::id()));
        let b = dir.join("Vendor").join("Comp.vst3");
        std::fs::create_dir_all(b.join("Contents")).unwrap();
        std::fs::create_dir_all(dir.join("Other")).unwrap();
        assert_eq!(find_bundles(&dir), vec![b.clone()]);
        // a bundle with no binary is an error, not a crash
        let e = scan_one(&b, &Scanner::InProcess);
        assert!(e.error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
