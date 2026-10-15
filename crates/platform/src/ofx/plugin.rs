//! Plug-in binaries: loading, describing (what [`PluginInfo`] records for the Effects panel), and
//! running instances.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::ffi::{self, OfxStatus, Raw};
use super::host::{self, Clip, Effect, Param, cstr};
use super::props::{PropSet, Val};

/// One parameter as described by the plug-in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub ty: String,
    pub label: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub animates: bool,
    #[serde(default)]
    pub default: Vec<f64>,
    #[serde(default)]
    pub default_str: String,
    #[serde(default)]
    pub min: Vec<f64>,
    #[serde(default)]
    pub max: Vec<f64>,
    #[serde(default)]
    pub display_min: Vec<f64>,
    #[serde(default)]
    pub display_max: Vec<f64>,
    #[serde(default)]
    pub digits: i32,
    #[serde(default)]
    pub double_type: String,
    #[serde(default)]
    pub choices: Vec<String>,
    /// StrChoice: the value each option stands for.
    #[serde(default)]
    pub choice_enums: Vec<String>,
    #[serde(default)]
    pub string_mode: String,
    #[serde(default)]
    pub dimension_labels: Vec<String>,
}

/// One plug-in in a binary, as FilmCraft lists it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub binary: String,
    pub index: usize,
    pub id: String,
    pub major: u32,
    pub minor: u32,
    pub label: String,
    #[serde(default)]
    pub grouping: String,
    #[serde(default)]
    pub description: String,
    pub contexts: Vec<String>,
    /// The context video effects use (Filter, else General), if any.
    #[serde(default)]
    pub effect_context: Option<String>,
    #[serde(default)]
    pub effect_params: Vec<ParamInfo>,
    #[serde(default)]
    pub transition_params: Vec<ParamInfo>,
    #[serde(default)]
    pub depths: Vec<String>,
    #[serde(default)]
    pub multires: bool,
    #[serde(default)]
    pub temporal: bool,
    #[serde(default)]
    pub safety: String,
}

impl PluginInfo {
    pub fn is_transition(&self) -> bool {
        self.contexts.iter().any(|c| c == ffi::ContextTransition)
    }
}

/// A loaded plug-in binary (never unloaded: plug-ins may keep threads alive).
pub struct Binary {
    _lib: libloading::Library,
    count: Option<unsafe extern "C" fn() -> c_int>,
    get: unsafe extern "C" fn(c_int) -> *const ffi::OfxPlugin,
}

// SAFETY: the library handle and function pointers are process-wide and thread-agnostic.
unsafe impl Send for Binary {}
// SAFETY: as above.
unsafe impl Sync for Binary {}

static BINARIES: Mutex<Option<HashMap<String, Arc<Binary>>>> = Mutex::new(None);

/// Load `path` (once per process).
pub fn open_binary(path: &Path) -> Result<Arc<Binary>, String> {
    let key = path.to_string_lossy().to_string();
    let mut g = BINARIES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(b) = g.as_ref().and_then(|m| m.get(&key)) {
        return Ok(b.clone());
    }
    // SAFETY: loading a plug-in runs its initialisers; that is what hosting a plug-in means. Its
    // own folder is searched for its dependencies (Windows: LOAD_WITH_ALTERED_SEARCH_PATH).
    let lib = unsafe {
        #[cfg(windows)]
        {
            libloading::os::windows::Library::load_with_flags(path, 0x0000_0008).map(libloading::Library::from)
        }
        #[cfg(not(windows))]
        {
            libloading::Library::new(path)
        }
    }
    .map_err(|e| format!("could not load {}: {e}", path.display()))?;
    // SAFETY: OpenFX's entry points have these C signatures (ofxCore.h).
    let get = unsafe { lib.get::<unsafe extern "C" fn(c_int) -> *const ffi::OfxPlugin>(b"OfxGetPlugin\0") }
        .map(|s| *s)
        .map_err(|_| "not an OpenFX plug-in (no OfxGetPlugin)".to_string())?;
    // SAFETY: as above.
    let count = unsafe { lib.get::<unsafe extern "C" fn() -> c_int>(b"OfxGetNumberOfPlugins\0") }.ok().map(|s| *s);
    // OpenFX 1.4: an optional OfxSetHost called before anything else
    // SAFETY: as above.
    if let Ok(set) = unsafe { lib.get::<unsafe extern "C" fn(*const ffi::OfxHost) -> OfxStatus>(b"OfxSetHost\0") } {
        // SAFETY: our host struct lives for the process.
        let _ = unsafe { set(host::host()) };
    }
    let b = Arc::new(Binary { _lib: lib, count, get });
    g.get_or_insert_with(HashMap::new).insert(key, b.clone());
    Ok(b)
}

impl Binary {
    pub fn count(&self) -> usize {
        // SAFETY: the binary's own entry point.
        self.count.map_or(1, |f| unsafe { f() }.clamp(0, 10_000) as usize)
    }
}

fn action(name: &str) -> CString {
    CString::new(name).unwrap_or_default()
}

/// A plug-in ready to describe and instantiate.
pub struct Plugin {
    _bin: Arc<Binary>,
    entry: ffi::MainEntry,
    pub id: String,
    pub major: u32,
    pub minor: u32,
    base: Box<Effect>,
    /// Descriptors per context (described on first use).
    contexts: Mutex<HashMap<String, Box<Effect>>>,
}

// SAFETY: descriptors are FilmCraft objects behind mutexes; the entry point is thread-agnostic
// (the plug-in declares its thread safety, which the renderer honours).
unsafe impl Send for Plugin {}
// SAFETY: as above.
unsafe impl Sync for Plugin {}

impl Plugin {
    /// Load plug-in `index` of `bin`: `setHost`, the load action and describe.
    pub fn load(bin: Arc<Binary>, index: usize) -> Result<Plugin, String> {
        let i = c_int::try_from(index).map_err(|_| "plug-in index out of range")?;
        // SAFETY: the binary's own entry point, with an index below its count.
        let p = unsafe { (bin.get)(i) };
        if p.is_null() {
            return Err(format!("plug-in {index} is missing"));
        }
        // SAFETY: OfxGetPlugin returns a struct that lives as long as the binary (contract).
        let pl = unsafe { &*p };
        // SAFETY: NUL-terminated strings or null.
        let api = unsafe { cstr(pl.plugin_api) }.unwrap_or("");
        if api != ffi::IMAGE_EFFECT_API || pl.api_version != 1 {
            return Err(format!("not an image effect ({api} v{})", pl.api_version));
        }
        // SAFETY: as above.
        let id = unsafe { cstr(pl.plugin_identifier) }.unwrap_or("").to_string();
        let entry = pl.main_entry.ok_or("no main entry")?;
        if let Some(set) = pl.set_host {
            // SAFETY: our host struct lives for the process.
            unsafe { set(host::host()) };
        }
        let base = Effect::new();
        base.props.s(ffi::PropType, ffi::TypeImageEffect);
        base.props.s(ffi::PropLabel, &id);
        base.props.s(ffi::IEPluginPropGrouping, "");
        base.props.ss(ffi::IEPropSupportedContexts, &[]);
        base.props.ss(ffi::IEPropSupportedPixelDepths, &[]);
        base.props.i(ffi::IEPluginPropSingleInstance, 0);
        base.props.s(ffi::IEPluginRenderThreadSafety, ffi::RenderInstanceSafe);
        base.props.i(ffi::IEPluginPropHostFrameThreading, 0);
        base.props.i(ffi::IEPropSupportsMultiResolution, 1);
        base.props.i(ffi::IEPropSupportsTiles, 1);
        base.props.i(ffi::IEPropTemporalClipAccess, 0);
        base.props.i(ffi::IEPropSupportsMultipleClipDepths, 0);
        base.props.s(ffi::PluginPropFilePath, "");
        let me = Plugin { _bin: bin, entry, id, major: pl.version_major, minor: pl.version_minor, base, contexts: Mutex::new(HashMap::new()) };
        let st = me.call(ffi::ActionLoad, std::ptr::null(), None, None);
        if !ffi::not_failed(st) {
            return Err(format!("the plug-in refused to load (status {st})"));
        }
        let st = me.call(ffi::ActionDescribe, me.base.raw(), None, None);
        if !ffi::not_failed(st) {
            return Err(format!("describe failed (status {st})"));
        }
        Ok(me)
    }

    /// Call the plug-in's main entry point.
    pub(crate) fn call(&self, act: &str, handle: *const std::ffi::c_void, inp: Option<&PropSet>, out: Option<&PropSet>) -> OfxStatus {
        let a = action(act);
        let raw = |p: Option<&PropSet>| p.map_or(std::ptr::null_mut(), |p| p.raw());
        // SAFETY: the plug-in's entry point with a valid action string and handles we own.
        unsafe { (self.entry)(a.as_ptr() as *const c_char, handle, raw(inp), raw(out)) }
    }

    pub fn props(&self) -> &PropSet {
        &self.base.props
    }

    /// The descriptor for `ctx`, describing it the first time.
    pub fn context(&self, ctx: &str) -> Result<Raw, String> {
        let mut g = self.contexts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(d) = g.get(ctx) {
            return Ok(d.raw());
        }
        let d = Effect::new();
        d.props.copy_from(&self.base.props);
        d.props.s(ffi::IEPropContext, ctx);
        let inp = PropSet::new();
        inp.s(ffi::IEPropContext, ctx);
        let st = self.call(ffi::ActionDescribeInContext, d.raw(), Some(&inp), None);
        if !ffi::not_failed(st) {
            return Err(format!("describing {ctx} failed (status {st})"));
        }
        let raw = d.raw();
        g.insert(ctx.to_string(), d);
        Ok(raw)
    }

    fn with_context<R>(&self, ctx: &str, f: impl FnOnce(&Effect) -> R) -> Result<R, String> {
        self.context(ctx)?;
        let g = self.contexts.lock().unwrap_or_else(PoisonError::into_inner);
        g.get(ctx).map(|d| f(d)).ok_or_else(|| "no descriptor".to_string())
    }

    /// What FilmCraft records about the plug-in (for the Effects panel and the scan cache).
    pub fn info(&self, binary: &str, index: usize) -> PluginInfo {
        let p = &self.base.props;
        let contexts = p.strs(ffi::IEPropSupportedContexts);
        // a transition that also offers the general context is listed as a transition only (its
        // general context wants two inputs, which a clip effect does not have)
        let is_transition = contexts.iter().any(|c| c == ffi::ContextTransition);
        let effect_context = [ffi::ContextFilter, ffi::ContextGeneral]
            .into_iter()
            .filter(|c| !(is_transition && *c == ffi::ContextGeneral))
            .find(|c| contexts.iter().any(|x| x == c))
            .map(str::to_string);
        let params_of = |ctx: &str| self.with_context(ctx, describe_params).unwrap_or_default();
        let effect_params = effect_context.as_deref().map(params_of).unwrap_or_default();
        let transition_params = if is_transition { params_of(ffi::ContextTransition) } else { Vec::new() };
        let label = p.str_(ffi::PropLabel).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| self.id.clone());
        PluginInfo {
            binary: binary.to_string(),
            index,
            id: self.id.clone(),
            major: self.major,
            minor: self.minor,
            label,
            grouping: p.str_(ffi::IEPluginPropGrouping).unwrap_or_default(),
            description: p.str_(ffi::PropPluginDescription).unwrap_or_default(),
            contexts,
            effect_context,
            effect_params,
            transition_params,
            depths: p.strs(ffi::IEPropSupportedPixelDepths),
            multires: p.int(ffi::IEPropSupportsMultiResolution).unwrap_or(1) != 0,
            temporal: p.int(ffi::IEPropTemporalClipAccess).unwrap_or(0) != 0,
            safety: p.str_(ffi::IEPluginRenderThreadSafety).unwrap_or_default(),
        }
    }

    /// A new instance in `ctx`, set up by `setup` (project size, rate, clip formats) before the
    /// create-instance action.
    pub fn instantiate(&self, ctx: &str, setup: impl FnOnce(&Effect)) -> Result<Box<Effect>, String> {
        self.context(ctx)?;
        let inst = Effect::new();
        {
            let g = self.contexts.lock().unwrap_or_else(PoisonError::into_inner);
            let d = g.get(ctx).ok_or("no descriptor")?;
            inst.props.copy_from(&d.props);
            inst.params.props.copy_from(&d.params.props);
            let params: Vec<Box<Param>> = d.params.list.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|p| p.instantiate()).collect();
            *inst.params.list.lock().unwrap_or_else(PoisonError::into_inner) = params;
            let clips: Vec<Box<Clip>> = d
                .clips
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|c| {
                    let n = Clip::new(&c.name, inst.raw() as usize);
                    n.props.copy_from(&c.props);
                    n
                })
                .collect();
            *inst.clips.lock().unwrap_or_else(PoisonError::into_inner) = clips;
        }
        inst.props.s(ffi::PropType, ffi::TypeImageEffectInstance);
        inst.props.s(ffi::IEPropContext, ctx);
        inst.props.p(ffi::PropInstanceData, std::ptr::null_mut());
        inst.props.i(ffi::PropIsInteractive, 0);
        setup(&inst);
        let st = self.call(ffi::ActionCreateInstance, inst.raw(), None, None);
        if !ffi::not_failed(st) {
            return Err(format!("the plug-in could not create an instance (status {st}){}", last_message()));
        }
        Ok(inst)
    }

    pub fn destroy(&self, inst: Box<Effect>) {
        let _ = self.call(ffi::ActionDestroyInstance, inst.raw(), None, None);
        drop(inst);
    }
}

/// The plug-in's last error message, as ": message" (or empty).
pub fn last_message() -> String {
    host::LAST_MESSAGE.lock().unwrap_or_else(PoisonError::into_inner).take().map(|m| format!(": {m}")).unwrap_or_default()
}

fn describe_params(d: &Effect) -> Vec<ParamInfo> {
    let list = d.params.list.lock().unwrap_or_else(PoisonError::into_inner);
    list.iter()
        .map(|p| {
            let ps = &p.props;
            let label = ps.str_(ffi::PropLabel).filter(|s| !s.trim().is_empty()).unwrap_or_else(|| p.name.clone());
            let default_str = match ps.get(ffi::ParamPropDefault) {
                Some(Val::Str(v)) => v.first().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                _ => String::new(),
            };
            ParamInfo {
                name: p.name.clone(),
                ty: p.ty.clone(),
                label,
                hint: ps.str_(ffi::ParamPropHint).unwrap_or_default(),
                parent: ps.str_(ffi::ParamPropParent).unwrap_or_default(),
                secret: ps.int(ffi::ParamPropSecret).unwrap_or(0) != 0,
                animates: ps.int(ffi::ParamPropAnimates).unwrap_or(0) != 0,
                default: ps.dbls(ffi::ParamPropDefault),
                default_str,
                min: ps.dbls(ffi::ParamPropMin),
                max: ps.dbls(ffi::ParamPropMax),
                display_min: ps.dbls(ffi::ParamPropDisplayMin),
                display_max: ps.dbls(ffi::ParamPropDisplayMax),
                digits: ps.int(ffi::ParamPropDigits).unwrap_or(2),
                double_type: ps.str_(ffi::ParamPropDoubleType).unwrap_or_default(),
                choices: ps.strs(ffi::ParamPropChoiceOption),
                choice_enums: ps.strs(ffi::ParamPropChoiceEnum),
                string_mode: ps.str_(ffi::ParamPropStringMode).unwrap_or_default(),
                dimension_labels: ps.strs(ffi::ParamPropDimensionLabel),
            }
        })
        .collect()
}

/// Describe every plug-in in `path` (what the scan process does).
pub fn describe_binary(path: &Path) -> Result<Vec<PluginInfo>, String> {
    let bin = open_binary(path)?;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for i in 0..bin.count().min(2000) {
        match Plugin::load(bin.clone(), i) {
            Ok(p) => {
                let info = p.info(&path.to_string_lossy(), i);
                if info.effect_context.is_some() || info.is_transition() {
                    out.push(info);
                }
                // keep the plug-in loaded: some unload badly
                std::mem::forget(p);
            }
            Err(e) => errors.push(format!("plug-in {i}: {e}")),
        }
    }
    if out.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(out)
}
