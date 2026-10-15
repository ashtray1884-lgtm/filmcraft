//! Finding installed plug-ins, scanning them safely, and listing them as FilmCraft effects.
//!
//! - **Where:** the folders OpenFX hosts share (Windows `%CommonProgramFiles%\OFX\Plugins`, Linux
//!   `/usr/OFX/Plugins`, macOS `/Library/OFX/Plugins`, the same ones DaVinci Resolve, Nuke and
//!   Vegas read) plus `OFX_PLUGIN_PATH`; every `*.ofx.bundle` in them, at any depth up to six.
//! - **Scan:** each binary is described in a child process (`filmcraft --ofx-scan <binary>`), so a
//!   plug-in that crashes or hangs while loading costs only that plug-in, never FilmCraft. A scan
//!   that takes longer than two minutes is given up. Results are cached by path, size and
//!   modification time in `ofx-plugins.json` in FilmCraft's data folder; only new or changed
//!   binaries are scanned again.
//! - **List:** each plug-in becomes a video effect (`ofx:<id>`, Video Effects ▸ its vendor group)
//!   and, if it has a transition context, a video transition (`ofxt:<id>`), with its parameters
//!   as Effect Controls rows ([`layout`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use filmcraft_project::ParamValue;
use filmcraft_project::{EffectDef, EffectKind, ParamDef, ParamKind};
use serde::{Deserialize, Serialize};

use super::ffi;
use super::plugin::{ParamInfo, PluginInfo};

/// How a plug-in parameter appears among FilmCraft's effect parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Float,
    Int,
    Bool,
    Choice,
    StrChoice,
    /// RGB (3) or RGBA (4).
    Color(usize),
    /// A 2D position (OpenFX XY parameters), in the plug-in's own units.
    Point,
    /// Several numbers, one row each (`name#0`, `name#1`, …).
    Split {
        dims: usize,
        int: bool,
    },
    Text,
    /// Not shown (groups, pages, buttons, custom data, secret parameters): the default is used.
    Hidden,
}

pub fn layout(p: &ParamInfo) -> Layout {
    if p.secret {
        return Layout::Hidden;
    }
    match p.ty.as_str() {
        ffi::ParamTypeDouble => Layout::Float,
        ffi::ParamTypeInteger => Layout::Int,
        ffi::ParamTypeBoolean => Layout::Bool,
        ffi::ParamTypeChoice => Layout::Choice,
        ffi::ParamTypeStrChoice => Layout::StrChoice,
        ffi::ParamTypeRGB => Layout::Color(3),
        ffi::ParamTypeRGBA => Layout::Color(4),
        ffi::ParamTypeDouble2D if p.double_type == ffi::DoubleTypeXY || p.double_type == ffi::DoubleTypeXYAbsolute => Layout::Point,
        ffi::ParamTypeDouble2D => Layout::Split { dims: 2, int: false },
        ffi::ParamTypeInteger2D => Layout::Split { dims: 2, int: true },
        ffi::ParamTypeDouble3D => Layout::Split { dims: 3, int: false },
        ffi::ParamTypeInteger3D => Layout::Split { dims: 3, int: true },
        ffi::ParamTypeString if p.string_mode != ffi::ParamStringIsLabel => Layout::Text,
        _ => Layout::Hidden,
    }
}

fn leak(s: impl Into<String>) -> &'static str {
    Box::leak(s.into().into_boxed_str())
}

/// A finite number within ±1e9 (plug-ins say "unbounded" with ±DBL_MAX or INT_MIN/MAX).
fn bound(v: Option<f64>, fallback: f64) -> f64 {
    match v {
        Some(x) if x.is_finite() && x.abs() < 1e9 && x.abs() < i32::MAX as f64 - 1.0 => x,
        _ => fallback,
    }
}

fn float_kind(p: &ParamInfo, i: usize, int: bool) -> (ParamKind, f64) {
    let def = p.default.get(i).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
    let min = bound(p.min.get(i).copied(), -1e6);
    let max = bound(p.max.get(i).copied(), 1e6).max(min);
    let smin = bound(p.display_min.get(i).copied(), if min > -1e6 { min } else { def.min(0.0) }).clamp(min, max);
    let smax = bound(p.display_max.get(i).copied(), if max < 1e6 { max } else { def.max(1.0) * 2.0 }).clamp(min, max).max(smin);
    let decimals = if int { 0 } else { p.digits.clamp(0, 4) as u8 };
    (ParamKind::Float { min, max, soft_min: smin, soft_max: smax, unit: "", decimals }, def.clamp(min, max))
}

/// The Effect Controls group of a parameter: its group's label (nested groups joined with ▸).
fn group_of(p: &ParamInfo, all: &[ParamInfo]) -> Option<&'static str> {
    let mut names = Vec::new();
    let mut cur = p.parent.clone();
    for _ in 0..8 {
        if cur.is_empty() {
            break;
        }
        let Some(g) = all.iter().find(|q| q.name == cur && q.ty == ffi::ParamTypeGroup) else { break };
        names.push(g.label.clone());
        cur = g.parent.clone();
    }
    names.reverse();
    (!names.is_empty()).then(|| leak(names.join(" ▸ ")))
}

/// FilmCraft parameter rows for the plug-in's parameters.
pub fn param_defs(params: &[ParamInfo], skip_transition_param: bool) -> Vec<ParamDef> {
    let mut out = Vec::new();
    for p in params {
        if skip_transition_param && p.name == ffi::TransitionParam {
            continue;
        }
        let group = group_of(p, params);
        let label = leak(p.label.clone());
        let mut push = |id: &'static str, label: &'static str, kind: ParamKind, default: ParamValue, animatable: bool| {
            out.push(ParamDef { id, label, kind, default, animatable, group });
        };
        let def = |i: usize| p.default.get(i).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
        match layout(p) {
            Layout::Float => {
                let (kind, d) = float_kind(p, 0, false);
                let kind = if p.double_type == ffi::DoubleTypeAngle { ParamKind::Angle } else { kind };
                push(leak(p.name.clone()), label, kind, ParamValue::Float(d), p.animates);
            }
            Layout::Int => {
                let (kind, d) = float_kind(p, 0, true);
                push(leak(p.name.clone()), label, kind, ParamValue::Float(d.round()), p.animates);
            }
            Layout::Bool => push(leak(p.name.clone()), label, ParamKind::Bool, ParamValue::Bool(def(0) != 0.0), false),
            Layout::Choice | Layout::StrChoice => {
                let opts: Vec<&'static str> = p.choices.iter().map(|c| leak(c.clone())).collect();
                if opts.is_empty() {
                    continue;
                }
                let d = if layout(p) == Layout::StrChoice {
                    p.choice_enums.iter().position(|x| *x == p.default_str).unwrap_or(0)
                } else {
                    def(0).max(0.0) as usize
                };
                let d = d.min(opts.len() - 1) as u32;
                push(leak(p.name.clone()), label, ParamKind::Choice(Box::leak(opts.into_boxed_slice())), ParamValue::Choice(d), false);
            }
            Layout::Color(n) => {
                let a = if n == 4 { def(3) } else { 1.0 };
                push(leak(p.name.clone()), label, ParamKind::Color, ParamValue::Color([def(0) as f32, def(1) as f32, def(2) as f32, a as f32]), p.animates);
            }
            Layout::Point => push(leak(p.name.clone()), label, ParamKind::Point, ParamValue::Vec2(filmcraft_geom::Vec2::new(def(0), def(1))), p.animates),
            Layout::Split { dims, int } => {
                for i in 0..dims {
                    let suffix = p.dimension_labels.get(i).cloned().unwrap_or_else(|| ["X", "Y", "Z"].get(i).unwrap_or(&"").to_string());
                    let (kind, d) = float_kind(p, i, int);
                    push(
                        leak(format!("{}#{i}", p.name)),
                        leak(format!("{} {}", p.label, suffix)),
                        kind,
                        ParamValue::Float(if int { d.round() } else { d }),
                        p.animates,
                    );
                }
            }
            Layout::Text => push(leak(p.name.clone()), label, ParamKind::Text, ParamValue::Text(p.default_str.clone()), false),
            Layout::Hidden => {}
        }
    }
    out
}

/// The Effects-panel folder for a plug-in: its grouping's first part ("BorisFX Sapphire/Blur" →
/// "BorisFX Sapphire ▸ Blur"), or "OpenFX".
fn folder(info: &PluginInfo) -> &'static str {
    let g = info.grouping.trim().trim_matches('/');
    if g.is_empty() { "OpenFX" } else { leak(g.split('/').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ▸ ")) }
}

/// FilmCraft effect definitions for scanned plug-ins (latest version of each id).
pub fn effect_defs(infos: &[PluginInfo]) -> Vec<EffectDef> {
    let mut latest: BTreeMap<&str, &PluginInfo> = BTreeMap::new();
    for i in infos {
        let newer = latest.get(i.id.as_str()).is_none_or(|o| (i.major, i.minor) >= (o.major, o.minor));
        if newer {
            latest.insert(&i.id, i);
        }
    }
    let mut out = Vec::new();
    for info in latest.values() {
        let f = folder(info);
        if info.effect_context.is_some() {
            out.push(EffectDef {
                id: leak(format!("ofx:{}", info.id)),
                name: leak(info.label.clone()),
                kind: EffectKind::Video,
                category: Box::leak(vec!["Video Effects", f].into_boxed_slice()),
                params: param_defs(&info.effect_params, false),
                intrinsic: false,
                accelerated: false,
                float32: info.depths.is_empty() || info.depths.iter().any(|d| d == ffi::BitDepthFloat),
                yuv: false,
            });
        }
        if info.is_transition() {
            out.push(EffectDef {
                id: leak(format!("ofxt:{}", info.id)),
                name: leak(info.label.clone()),
                kind: EffectKind::VideoTransition,
                category: Box::leak(vec!["Video Transitions", f].into_boxed_slice()),
                params: param_defs(&info.transition_params, true),
                intrinsic: false,
                accelerated: false,
                float32: info.depths.is_empty() || info.depths.iter().any(|d| d == ffi::BitDepthFloat),
                yuv: false,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Finding plug-ins

/// The folders searched, most specific first.
pub fn search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::var_os("OFX_PLUGIN_PATH") {
        v.extend(std::env::split_paths(&p).filter(|p| !p.as_os_str().is_empty()));
    }
    #[cfg(windows)]
    for var in ["CommonProgramFiles", "CommonProgramW6432"] {
        if let Some(root) = std::env::var_os(var) {
            v.push(PathBuf::from(root).join("OFX").join("Plugins"));
        }
    }
    #[cfg(target_os = "linux")]
    v.push(PathBuf::from("/usr/OFX/Plugins"));
    #[cfg(target_os = "macos")]
    v.push(PathBuf::from("/Library/OFX/Plugins"));
    let mut seen = Vec::new();
    v.retain(|p| {
        let k = p.to_string_lossy().to_ascii_lowercase();
        let new = !seen.contains(&k);
        seen.push(k);
        new
    });
    v
}

/// The binary folder inside a bundle for this system.
fn arch_dir() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "Win64"
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        "WinArm64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "Linux-x86-64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "Linux-aarch64"
    } else if cfg!(target_os = "macos") {
        "MacOS"
    } else {
        "Unknown"
    }
}

/// Plug-in binaries in `dir` (bundles at any depth up to six).
pub fn find_binaries(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        if depth > 6 || out.len() > 2000 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name.to_ascii_lowercase().ends_with(".ofx.bundle") {
                let bin_dir = p.join("Contents").join(arch_dir());
                let stem = &name[..name.len() - ".bundle".len()];
                let main = bin_dir.join(stem);
                if main.is_file() {
                    out.push(main);
                } else if let Ok(rd) = std::fs::read_dir(&bin_dir) {
                    out.extend(rd.flatten().map(|e| e.path()).filter(|f| f.is_file() && f.extension().is_some_and(|x| x.eq_ignore_ascii_case("ofx"))));
                }
            } else {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out
}

// ---------------------------------------------------------------------------------------------
// Scanning and the cache

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
    pub binaries: BTreeMap<String, Entry>,
}

const CACHE_VERSION: u32 = 1;

fn stamp(p: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Some((m.len(), t))
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

/// How a binary gets described: in a child process (the app), or in this process (tests).
pub enum Scanner {
    /// Run `exe --ofx-scan <binary>`.
    Process(PathBuf),
    InProcess,
}

/// Describe `binary` with `scanner` (child processes: two minutes at most).
pub fn scan_one(binary: &Path, scanner: &Scanner) -> Entry {
    let (size, modified) = stamp(binary).unwrap_or((0, 0));
    let result = match scanner {
        Scanner::InProcess => super::plugin::describe_binary(binary),
        Scanner::Process(exe) => scan_in_child(exe, binary),
    };
    match result {
        Ok(plugins) => Entry { size, modified, error: None, plugins },
        Err(e) => {
            log::warn!("OpenFX: {} skipped: {e}", binary.display());
            Entry { size, modified, error: Some(e), plugins: Vec::new() }
        }
    }
}

fn scan_in_child(exe: &Path, binary: &Path) -> Result<Vec<PluginInfo>, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(exe)
        .arg("--ofx-scan")
        .arg(binary)
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
    if !status.success() {
        return Err(format!("the plug-in crashed while loading ({status})"));
    }
    let text = String::from_utf8_lossy(&out);
    let json = text.lines().rev().find(|l| l.starts_with("OFXSCAN ")).ok_or("the scan printed nothing")?;
    let v: Result<Vec<PluginInfo>, String> = serde_json::from_str(&json["OFXSCAN ".len()..]).map_err(|e| e.to_string())?;
    v
}

/// `--ofx-scan <binary>`: describe it and print the result (the child side of [`scan_in_child`]).
/// Returns whether the arguments asked for a scan (the caller then exits).
pub fn scan_main() -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_none_or(|a| a != "--ofx-scan") {
        return false;
    }
    let Some(bin) = args.get(2) else {
        println!("OFXSCAN {}", serde_json::json!({"Err": "no binary given"}));
        return true;
    };
    let r = super::plugin::describe_binary(Path::new(bin));
    let line = serde_json::to_string(&r).unwrap_or_else(|e| serde_json::json!({"Err": e.to_string()}).to_string());
    println!("OFXSCAN {line}");
    true
}

/// Bring the cache up to date with what is installed: entries of binaries that are gone are
/// dropped, new or changed binaries are scanned (and `on_new` called with each result).
pub fn refresh(cache: &mut Cache, dirs: &[PathBuf], scanner: &Scanner, mut on_new: impl FnMut(&[PluginInfo])) {
    let mut present = Vec::new();
    for d in dirs {
        for b in find_binaries(d) {
            let key = b.to_string_lossy().to_string();
            present.push(key.clone());
            let st = stamp(&b);
            let fresh = cache.binaries.get(&key).is_some_and(|e| st == Some((e.size, e.modified)));
            if fresh {
                continue;
            }
            log::info!("OpenFX: scanning {}", b.display());
            let e = scan_one(&b, scanner);
            on_new(&e.plugins);
            cache.binaries.insert(key, e);
        }
    }
    cache.binaries.retain(|k, _| present.contains(k));
}

/// Every plug-in in the cache.
pub fn all(cache: &Cache) -> Vec<PluginInfo> {
    cache.binaries.values().flat_map(|e| e.plugins.iter().cloned()).collect()
}

/// Start plug-in support: install the renderer hook, list what the cache knows now, then scan
/// new or changed plug-ins on a background thread (their effects appear when it finishes).
pub fn start(data_dir: Option<PathBuf>) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = start_with(data_dir, Scanner::Process(exe), search_paths());
    }
}

/// [`start`] with a chosen scanner and folders; returns the background scan (`None` if already
/// started).
pub fn start_with(data_dir: Option<PathBuf>, scanner: Scanner, dirs: Vec<PathBuf>) -> Option<std::thread::JoinHandle<()>> {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let mut handle = None;
    STARTED.call_once(|| {
        let host = Arc::new(super::render::Host::new());
        filmcraft_render::external::set_external(host.clone());
        let cache_path = cache_file(data_dir);
        let mut cache = load_cache(&cache_path);
        let known = all(&cache);
        host.add(&known);
        filmcraft_project::register_plugin_effects(effect_defs(&known));
        let spawned = std::thread::Builder::new().name("ofx-scan".into()).spawn(move || {
            refresh(&mut cache, &dirs, &scanner, |found| {
                host.add(found);
                filmcraft_project::register_plugin_effects(effect_defs(found));
            });
            save_cache(&cache_path, &cache);
            let n: usize = cache.binaries.values().map(|e| e.plugins.len()).sum();
            log::info!("OpenFX: {n} plug-ins from {} folders", dirs.len());
        });
        match spawned {
            Ok(h) => handle = Some(h),
            Err(e) => log::warn!("OpenFX: could not start the plug-in scan: {e}"),
        }
    });
    handle
}

fn cache_file(data_dir: Option<PathBuf>) -> PathBuf {
    data_dir.unwrap_or_else(std::env::temp_dir).join("ofx-plugins.json")
}

/// Scan now (in child processes) and report what is installed: `filmcraft-cli plugins`.
pub fn scan_and_report(data_dir: Option<PathBuf>) -> String {
    let path = cache_file(data_dir);
    let mut cache = load_cache(&path);
    if let Ok(exe) = std::env::current_exe() {
        refresh(&mut cache, &search_paths(), &Scanner::Process(exe), |_| {});
        save_cache(&path, &cache);
    }
    report_cache(&cache)
}

fn report_cache(cache: &Cache) -> String {
    let mut lines = vec![format!("OpenFX plug-in folders: {}", search_paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; "))];
    if cache.binaries.is_empty() {
        lines.push("  no OpenFX plug-ins found".into());
    }
    let (mut fx, mut tr) = (0, 0);
    for (bin, e) in &cache.binaries {
        match &e.error {
            Some(err) => lines.push(format!("  {bin}: skipped ({err})")),
            None => {
                lines.push(format!("  {bin}: {}", e.plugins.iter().map(|p| p.label.as_str()).collect::<Vec<_>>().join(", ")));
                fx += e.plugins.iter().filter(|p| p.effect_context.is_some()).count();
                tr += e.plugins.iter().filter(|p| p.is_transition()).count();
            }
        }
    }
    lines.push(format!("  {fx} video effects, {tr} transitions"));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, ty: &str) -> ParamInfo {
        ParamInfo { name: name.into(), ty: ty.into(), label: name.to_uppercase(), animates: true, ..Default::default() }
    }

    #[test]
    fn parameters_become_effect_controls_rows() {
        let mut amount = p("amount", ffi::ParamTypeDouble);
        amount.default = vec![0.5];
        amount.min = vec![-f64::MAX];
        amount.max = vec![f64::MAX];
        amount.display_min = vec![0.0];
        amount.display_max = vec![1.0];
        amount.parent = "adv".into();
        let mut grp = p("adv", ffi::ParamTypeGroup);
        grp.label = "Advanced".into();
        let mut mode = p("mode", ffi::ParamTypeChoice);
        mode.choices = vec!["Soft".into(), "Hard".into()];
        mode.default = vec![1.0];
        let mut center = p("center", ffi::ParamTypeDouble2D);
        center.double_type = ffi::DoubleTypeXY.into();
        center.default = vec![0.5, 0.5];
        let size = p("size", ffi::ParamTypeInteger2D);
        let mut secret = p("state", ffi::ParamTypeString);
        secret.secret = true;
        let defs = param_defs(&[grp, amount, mode, center, size, secret, p("go", ffi::ParamTypePushButton)], false);
        let ids: Vec<_> = defs.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["amount", "mode", "center", "size#0", "size#1"]);
        assert_eq!(defs[0].group, Some("Advanced"));
        match defs[0].kind {
            ParamKind::Float { min, max, soft_min, soft_max, .. } => {
                assert!(min.is_finite() && max.is_finite(), "unbounded becomes finite");
                assert_eq!((soft_min, soft_max), (0.0, 1.0));
            }
            ref k => panic!("{k:?}"),
        }
        assert_eq!(defs[1].default, ParamValue::Choice(1));
        assert_eq!(defs[2].kind, ParamKind::Point);
    }

    #[test]
    fn plugins_list_as_effects_and_transitions_in_their_vendor_folder() {
        let info = PluginInfo {
            id: "com.example.Glow".into(),
            label: "Glow".into(),
            grouping: "Example FX/Light".into(),
            contexts: vec![ffi::ContextFilter.into(), ffi::ContextTransition.into()],
            effect_context: Some(ffi::ContextFilter.into()),
            transition_params: vec![p(ffi::TransitionParam, ffi::ParamTypeDouble), p("soft", ffi::ParamTypeDouble)],
            major: 1,
            ..Default::default()
        };
        let mut old = info.clone();
        old.major = 0;
        old.label = "Glow (old)".into();
        let defs = effect_defs(&[old, info]);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].id, "ofx:com.example.Glow");
        assert_eq!(defs[0].name, "Glow", "the newest version wins");
        assert_eq!(defs[0].category, &["Video Effects", "Example FX ▸ Light"]);
        assert_eq!(defs[1].kind, EffectKind::VideoTransition);
        assert_eq!(defs[1].params.iter().map(|d| d.id).collect::<Vec<_>>(), vec!["soft"], "the progress parameter is FilmCraft's");
    }

    #[test]
    fn bundles_are_found_at_depth() {
        let dir = std::env::temp_dir().join(format!("fc-ofx-find-{}", std::process::id()));
        let bin = dir.join("Vendor").join("Glow.ofx.bundle").join("Contents").join(arch_dir());
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("Glow.ofx"), b"not really").unwrap();
        std::fs::create_dir_all(dir.join("Empty.ofx.bundle")).unwrap();
        let found = find_binaries(&dir);
        assert_eq!(found, vec![bin.join("Glow.ofx")]);
        // a binary that is not a plug-in is recorded as skipped, not fatal
        let e = scan_one(&found[0], &Scanner::InProcess);
        assert!(e.error.is_some() && e.plugins.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
