//! Rendering FilmCraft frames through plug-ins: the [`filmcraft_render::external::External`]
//! FilmCraft's renderer calls for `ofx:` effects and `ofxt:` transitions.
//!
//! Each plug-in is loaded in-process at its first render (the scan that listed it ran in a
//! separate process). Instances are pooled per plug-in and context: a render takes an idle one
//! (or creates one), sets every parameter from the clip's effect at the frame's time (animated
//! parameters answer other times from FilmCraft's keyframes), hands over the images and calls
//! the render action. Plug-ins that declare themselves unsafe to render concurrently are
//! rendered one frame at a time.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_project::{EffectInstance, ParamValue};
use filmcraft_render::external::{ExtCtx, ExtImage, External};
use filmcraft_time::Tick;

use super::catalog::{Layout, layout};
use super::ffi;
use super::host::{AtTime, Depth, Effect, ImageBuf, PV, RenderJob};
use super::plugin::{Plugin, PluginInfo, last_message, open_binary};
use super::props::PropSet;

/// A loaded plug-in with its instance pool.
struct Loaded {
    plugin: Plugin,
    info: PluginInfo,
    depth: Depth,
    /// Idle instances by context.
    idle: Mutex<HashMap<String, Vec<Box<Effect>>>>,
    /// Held while rendering a plug-in that is not thread safe.
    serial: Mutex<()>,
}

/// The plug-in host FilmCraft's renderer calls.
pub struct Host {
    infos: Mutex<HashMap<String, PluginInfo>>,
    loaded: Mutex<HashMap<String, Result<Arc<Loaded>, String>>>,
    /// Effects whose failure has been logged (once each).
    logged: Mutex<std::collections::HashSet<String>>,
}

impl Host {
    pub fn new() -> Host {
        Host { infos: Mutex::new(HashMap::new()), loaded: Mutex::new(HashMap::new()), logged: Mutex::new(Default::default()) }
    }

    /// Make plug-ins known (from the scan); later versions of an id replace earlier ones.
    pub fn add(&self, list: &[PluginInfo]) {
        let mut g = self.infos.lock().unwrap_or_else(PoisonError::into_inner);
        for i in list {
            let newer = g.get(&i.id).is_none_or(|o| (i.major, i.minor) >= (o.major, o.minor));
            if newer {
                g.insert(i.id.clone(), i.clone());
            }
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Loaded>, String> {
        if let Some(r) = self.loaded.lock().unwrap_or_else(PoisonError::into_inner).get(id) {
            return r.clone();
        }
        let info =
            self.infos.lock().unwrap_or_else(PoisonError::into_inner).get(id).cloned().ok_or_else(|| format!("the OpenFX plug-in {id} is not installed"))?;
        let r = load(&info).map(Arc::new);
        if let Err(e) = &r {
            log::warn!("OpenFX: {} ({id}) could not be loaded: {e}", info.label);
        }
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner).insert(id.to_string(), r.clone());
        r
    }

    fn note(&self, key: &str, e: &str) {
        if self.logged.lock().unwrap_or_else(PoisonError::into_inner).insert(key.to_string()) {
            log::warn!("OpenFX: {key}: {e}");
        }
    }
}

impl Default for Host {
    fn default() -> Self {
        Host::new()
    }
}

fn load(info: &PluginInfo) -> Result<Loaded, String> {
    let bin = open_binary(std::path::Path::new(&info.binary))?;
    let plugin = Plugin::load(bin, info.index)?;
    if plugin.id != info.id {
        return Err(format!("the binary now holds {} at index {}, not {}", plugin.id, info.index, info.id));
    }
    let has = |d: &str| info.depths.is_empty() || info.depths.iter().any(|x| x == d);
    let depth = if has(ffi::BitDepthFloat) {
        Depth::Float
    } else if has(ffi::BitDepthShort) {
        Depth::Short
    } else {
        Depth::Byte
    };
    Ok(Loaded { plugin, info: info.clone(), depth, idle: Mutex::new(HashMap::new()), serial: Mutex::new(()) })
}

// ---------------------------------------------------------------------------------------------
// Pixels

/// FilmCraft's picture (top row first) → a plug-in image (bottom row first) at `depth`.
pub fn to_buf(img: &ExtImage, depth: Depth) -> Option<ImageBuf> {
    use rayon::prelude::*;
    let buf = ImageBuf::new(img.w, img.h, depth)?;
    let row = img.w * 4;
    if img.px.len() < row * img.h || row == 0 {
        return (img.w == 0 || img.h == 0).then_some(buf);
    }
    let bpp = depth.bytes();
    let out = buf.bytes_mut();
    // destination rows bottom-up: destination row `dy` comes from source row h-1-dy
    out.par_chunks_mut(row * bpp).enumerate().for_each(|(dy, dst)| {
        let y = img.h - 1 - dy;
        let Some(src) = img.px.get(y * row..(y + 1) * row) else { return };
        match depth {
            Depth::Float => {
                for (o, v) in dst.chunks_exact_mut(4).zip(src) {
                    o.copy_from_slice(&v.to_ne_bytes());
                }
            }
            Depth::Short => {
                for (o, v) in dst.chunks_exact_mut(2).zip(src) {
                    o.copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_ne_bytes());
                }
            }
            Depth::Byte => {
                for (o, v) in dst.iter_mut().zip(src) {
                    *o = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
        }
    });
    Some(buf)
}

/// A plug-in image → FilmCraft's picture (top row first); non-finite values become 0.
pub fn from_buf(buf: &ImageBuf) -> ExtImage {
    use rayon::prelude::*;
    let (w, h) = (buf.w, buf.h);
    let mut px = vec![0f32; w * h * 4];
    let bytes = buf.bytes();
    let bpp = buf.depth.bytes();
    let row = w * 4;
    if row == 0 {
        return ExtImage { w, h, px };
    }
    px.par_chunks_mut(row).enumerate().for_each(|(y, dst)| {
        let sy = h - 1 - y;
        let Some(src) = bytes.get(sy * row * bpp..(sy + 1) * row * bpp) else { return };
        match buf.depth {
            Depth::Float => {
                for (o, b) in dst.iter_mut().zip(src.chunks_exact(4)) {
                    let v = f32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
                    *o = if v.is_finite() { v } else { 0.0 };
                }
            }
            Depth::Short => {
                for (o, b) in dst.iter_mut().zip(src.chunks_exact(2)) {
                    *o = u16::from_ne_bytes([b[0], b[1]]) as f32 / 65535.0;
                }
            }
            Depth::Byte => {
                for (o, &b) in dst.iter_mut().zip(src) {
                    *o = b as f32 / 255.0;
                }
            }
        }
    });
    ExtImage { w, h, px }
}

// ---------------------------------------------------------------------------------------------
// Parameters

fn cs(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap_or_default()
}

fn round_i(v: f64) -> i32 {
    if v.is_finite() { v.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32 } else { 0 }
}

/// The OpenFX value of parameter `pi` from FilmCraft's effect at media time `t`.
fn value_of(pi: &super::plugin::ParamInfo, e: &EffectInstance, t: Tick) -> PV {
    let def = |i: usize| pi.default.get(i).copied().unwrap_or(0.0);
    let num = |id: &str, i: usize| e.param(id).map(|p| p.value_at(t)).and_then(|v| v.as_f64()).unwrap_or(def(i));
    match layout(pi) {
        Layout::Float => PV::D(vec![num(&pi.name, 0)]),
        Layout::Int => PV::I(vec![round_i(num(&pi.name, 0))]),
        Layout::Bool => PV::I(vec![e.param(&pi.name).and_then(|p| p.value.as_bool()).map_or(round_i(def(0)), i32::from)]),
        Layout::Choice => PV::I(vec![match e.param(&pi.name).map(|p| &p.value) {
            Some(ParamValue::Choice(c)) => (*c).min(i32::MAX as u32) as i32,
            _ => round_i(def(0)),
        }]),
        Layout::StrChoice => {
            let idx = match e.param(&pi.name).map(|p| &p.value) {
                Some(ParamValue::Choice(c)) => *c as usize,
                _ => pi.choice_enums.iter().position(|x| *x == pi.default_str).unwrap_or(0),
            };
            let s = pi.choice_enums.get(idx).or(pi.choices.get(idx)).cloned().unwrap_or_else(|| pi.default_str.clone());
            PV::S(cs(&s))
        }
        Layout::Color(n) => {
            let c = e.param(&pi.name).and_then(|p| p.value_at(t).as_color()).map(|c| c.map(|x| x as f64)).unwrap_or([
                def(0),
                def(1),
                def(2),
                if n == 4 { def(3) } else { 1.0 },
            ]);
            PV::D(c.get(..n).unwrap_or(&c).to_vec())
        }
        Layout::Point => {
            let v = e.param(&pi.name).and_then(|p| p.value_at(t).as_vec2());
            PV::D(match v {
                Some(v) if v.x.is_finite() && v.y.is_finite() => vec![v.x, v.y],
                _ => vec![def(0), def(1)],
            })
        }
        Layout::Split { dims, int } => {
            let v: Vec<f64> = (0..dims).map(|i| num(&format!("{}#{i}", pi.name), i)).collect();
            if int { PV::I(v.into_iter().map(round_i).collect()) } else { PV::D(v) }
        }
        Layout::Text => PV::S(cs(match e.param(&pi.name).map(|p| &p.value) {
            Some(ParamValue::Text(s)) => s,
            _ => &pi.default_str,
        })),
        Layout::Hidden => {
            if super::host::is_string_type(&pi.ty) {
                PV::S(cs(&pi.default_str))
            } else {
                match super::host::shape(&pi.ty) {
                    Some((true, n)) => PV::D((0..n).map(def).collect()),
                    Some((false, n)) => PV::I((0..n).map(|i| round_i(def(i))).collect()),
                    None => PV::None,
                }
            }
        }
    }
}

/// Set every parameter of `inst` from `e` at the frame being rendered; animated ones also answer
/// other times (frames relative to `frame`) from the keyframes.
fn set_params(inst: &Effect, params: &[super::plugin::ParamInfo], e: &EffectInstance, t: Tick, frame: f64, fps: f64, transition: Option<f64>) {
    let list = inst.params.list.lock().unwrap_or_else(PoisonError::into_inner);
    let by_name: HashMap<&str, &super::plugin::ParamInfo> = params.iter().map(|p| (p.name.as_str(), p)).collect();
    for p in list.iter() {
        if let Some(tp) = transition
            && p.name == ffi::TransitionParam
        {
            *p.val.lock().unwrap_or_else(PoisonError::into_inner) = PV::D(vec![tp]);
            *p.at_time.lock().unwrap_or_else(PoisonError::into_inner) = None;
            continue;
        }
        let Some(pi) = by_name.get(p.name.as_str()) else { continue };
        if shape(&p.ty).is_none() && !super::host::is_string_type(&p.ty) {
            continue;
        }
        let v = value_of(pi, e, t);
        *p.val.lock().unwrap_or_else(PoisonError::into_inner) = v;
        let animated = e.params.iter().any(|(k, fp)| (k == &pi.name || k.starts_with(&format!("{}#", pi.name))) && fp.is_animated());
        let f: Option<AtTime> = if animated {
            let (pi, e) = ((*pi).clone(), e.clone());
            let fps = if fps > 0.0 { fps } else { 30.0 };
            Some(Arc::new(move |f: f64| value_of(&pi, &e, t + Tick::from_seconds_f64((f - frame) / fps))))
        } else {
            None
        };
        *p.at_time.lock().unwrap_or_else(PoisonError::into_inner) = f;
    }
}

use super::host::shape;

// ---------------------------------------------------------------------------------------------
// Rendering

struct Job<'a> {
    now: f64,
    fps: f64,
    duration: f64,
    depth: Depth,
    output: Arc<ImageBuf>,
    inputs: HashMap<&'static str, Arc<ImageBuf>>,
    frames: Option<&'a (dyn Fn(f64) -> Option<ExtImage> + Sync)>,
    other: Mutex<HashMap<u64, Option<Arc<ImageBuf>>>>,
}

impl RenderJob for Job<'_> {
    fn image(&self, clip: &str, time: f64) -> Option<Arc<ImageBuf>> {
        if clip == ffi::OutputClip {
            return Some(self.output.clone());
        }
        let cur = self.inputs.get(clip)?;
        if (time - self.now).abs() < 1e-3 {
            return Some(cur.clone());
        }
        // another frame (temporal plug-ins): only the main source has them
        if clip != ffi::SourceClip {
            return Some(cur.clone());
        }
        let frames = self.frames?;
        let key = time.to_bits();
        if let Some(v) = self.other.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
            return v.clone();
        }
        let dt = (time - self.now) / self.fps.max(1.0);
        let got = frames(dt).and_then(|f| to_buf(&f, self.depth)).map(Arc::new);
        let mut g = self.other.lock().unwrap_or_else(PoisonError::into_inner);
        if g.len() < 64 {
            g.insert(key, got.clone());
        }
        got
    }
    fn time(&self) -> f64 {
        self.now
    }
    fn duration(&self) -> f64 {
        self.duration
    }
}

/// Set the instance-wide properties for a render of `w × h` at `scale`.
fn setup(inst: &Effect, l: &Loaded, cx: &ExtCtx, w: usize, h: usize) {
    let (fw, fh) = (cx.full_size.0.max(1) as f64, cx.full_size.1.max(1) as f64);
    let p = &inst.props;
    p.ds(ffi::IEPropProjectSize, &[fw, fh]);
    p.ds(ffi::IEPropProjectExtent, &[fw, fh]);
    p.ds(ffi::IEPropProjectOffset, &[0.0, 0.0]);
    p.d(ffi::IEPropProjectPixelAspectRatio, 1.0);
    p.d(ffi::IEPropFrameRate, cx.fps);
    p.d(ffi::IEInstancePropEffectDuration, cx.duration.max(1.0));
    p.ds(ffi::IEPropRenderScale, &[cx.render_scale as f64, cx.render_scale as f64]);
    let _ = (w, h);
    for name in inst.clip_names() {
        inst.clip_props(&name, |c| {
            let connected = matches!(name.as_str(), ffi::OutputClip | ffi::SourceClip | ffi::SourceFromClip | ffi::SourceToClip);
            c.i(ffi::ClipPropConnected, connected as i32);
            c.s(ffi::IEPropPixelDepth, l.depth.name());
            c.s(ffi::IEPropComponents, ffi::ComponentRGBA);
            c.s(ffi::ClipPropUnmappedPixelDepth, ffi::BitDepthFloat);
            c.s(ffi::ClipPropUnmappedComponents, ffi::ComponentRGBA);
            c.s(ffi::IEPropPreMultiplication, ffi::ImagePreMultiplied);
            c.d(ffi::ImagePropPixelAspectRatio, 1.0);
            c.d(ffi::IEPropFrameRate, cx.fps);
            c.d(ffi::IEPropUnmappedFrameRate, cx.fps);
            c.ds(ffi::IEPropFrameRange, &[0.0, cx.duration.max(1.0)]);
            c.ds(ffi::IEPropUnmappedFrameRange, &[0.0, cx.duration.max(1.0)]);
            c.s(ffi::ClipPropFieldOrder, ffi::FieldNone);
            c.i(ffi::ClipPropContinuousSamples, 0);
            c.i(ffi::IEFrameVarying, 1);
        });
    }
}

/// Ask the plug-in for its clip preferences once (FilmCraft keeps RGBA at the chosen depth; some
/// plug-ins set up internal state here).
fn clip_preferences(l: &Loaded, inst: &Effect, fps: f64) {
    let out = PropSet::new();
    for name in inst.clip_names() {
        out.s(&format!("OfxImageClipPropComponents_{name}"), ffi::ComponentRGBA);
        out.s(&format!("OfxImageClipPropDepth_{name}"), l.depth.name());
        out.d(&format!("OfxImageClipPropPAR_{name}"), 1.0);
    }
    out.d(ffi::IEPropFrameRate, fps);
    out.s(ffi::ClipPropFieldOrder, ffi::FieldNone);
    out.s(ffi::IEPropPreMultiplication, ffi::ImagePreMultiplied);
    out.i(ffi::ClipPropContinuousSamples, 0);
    out.i(ffi::IEFrameVarying, 1);
    let _ = l.plugin.call(ffi::ActionGetClipPreferences, inst.raw(), None, Some(&out));
}

/// Render once with an instance of `ctx`: `inputs` by clip name; returns the output picture.
fn run(
    l: &Loaded,
    ctx: &str,
    params: &[super::plugin::ParamInfo],
    cx: &ExtCtx,
    inputs: Vec<(&'static str, &ExtImage)>,
    transition: Option<f64>,
) -> Result<ExtImage, String> {
    let (w, h) = inputs.first().map(|(_, i)| (i.w, i.h)).ok_or("no input")?;
    let depth = l.depth;
    let mut bufs = HashMap::new();
    for (name, img) in &inputs {
        bufs.insert(*name, Arc::new(to_buf(img, depth).ok_or("picture too large")?));
    }
    let output = Arc::new(ImageBuf::new(w, h, depth).ok_or("picture too large")?);
    let inst = {
        let mut idle = l.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.get_mut(ctx).and_then(Vec::pop)
    };
    let inst = match inst {
        Some(i) => i,
        None => {
            let i = l.plugin.instantiate(ctx, |i| setup(i, l, cx, w, h))?;
            clip_preferences(l, &i, cx.fps);
            i
        }
    };
    setup(&inst, l, cx, w, h);
    set_params(&inst, params, cx.effect, cx.t, cx.frame, cx.fps, transition);
    let _serial = (l.info.safety == ffi::RenderUnsafe).then(|| l.serial.lock().unwrap_or_else(PoisonError::into_inner));
    let job = Job {
        now: cx.frame,
        fps: cx.fps,
        duration: cx.duration,
        depth,
        output: output.clone(),
        inputs: bufs,
        frames: cx.frames,
        other: Mutex::new(HashMap::new()),
    };
    let result = {
        let _guard = inst.set_job(&job);
        render_actions(l, &inst, &job, w, h, cx.render_scale as f64)
    };
    // back to the pool (a few per context), or destroyed
    {
        let mut idle = l.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let v = idle.entry(ctx.to_string()).or_default();
        if v.len() < 6 {
            v.push(inst);
        } else {
            drop(idle);
            l.plugin.destroy(inst);
        }
    }
    result?;
    Ok(from_buf(&output))
}

/// CPU rendering: the GPU properties plug-ins check in render arguments, all off.
fn cpu_only(p: &PropSet) {
    for k in ["OfxImageEffectPropOpenGLEnabled", "OfxImageEffectPropCudaEnabled", "OfxImageEffectPropMetalEnabled", "OfxImageEffectPropOpenCLEnabled"] {
        p.i(k, 0);
    }
    for k in ["OfxImageEffectPropCudaStream", "OfxImageEffectPropMetalCommandQueue", "OfxImageEffectPropOpenCLCommandQueue", "OfxImageEffectPropOpenCLImage"] {
        p.p(k, std::ptr::null_mut());
    }
}

fn render_actions(l: &Loaded, inst: &Effect, job: &Job, w: usize, h: usize, scale: f64) -> Result<(), String> {
    let t = job.now;
    let window = [0, 0, w as i32, h as i32];
    let begin = PropSet::new();
    begin.ds(ffi::IEPropFrameRange, &[t, t]);
    begin.d(ffi::IEPropFrameStep, 1.0);
    begin.i(ffi::PropIsInteractive, 0);
    begin.ds(ffi::IEPropRenderScale, &[scale, scale]);
    begin.i(ffi::IEPropSequentialRenderStatus, 0);
    begin.i(ffi::IEPropInteractiveRenderStatus, 0);
    cpu_only(&begin);
    let _ = l.plugin.call(ffi::ActionBeginSequenceRender, inst.raw(), Some(&begin), None);
    // an identity effect (nothing to do at these settings): pass the named clip through
    let ident_in = PropSet::new();
    ident_in.d(ffi::PropTime, t);
    ident_in.s(ffi::IEPropFieldToRender, ffi::FieldNone);
    ident_in.is_(ffi::IEPropRenderWindow, &window);
    ident_in.ds(ffi::IEPropRenderScale, &[scale, scale]);
    let ident_out = PropSet::new();
    ident_out.s(ffi::PropName, "");
    ident_out.d(ffi::PropTime, t);
    let st = l.plugin.call(ffi::ActionIsIdentity, inst.raw(), Some(&ident_in), Some(&ident_out));
    let mut result = Ok(());
    let identity = if ffi::ran(st) { ident_out.str_(ffi::PropName).filter(|n| !n.is_empty()) } else { None };
    match identity.and_then(|n| job.image(&n, ident_out.dbl(ffi::PropTime).unwrap_or(t))) {
        Some(src) => job.output.bytes_mut().copy_from_slice(src.bytes()),
        None => {
            let args = PropSet::new();
            args.d(ffi::PropTime, t);
            args.s(ffi::IEPropFieldToRender, ffi::FieldNone);
            args.is_(ffi::IEPropRenderWindow, &window);
            args.ds(ffi::IEPropRenderScale, &[scale, scale]);
            args.i(ffi::IEPropSequentialRenderStatus, 0);
            args.i(ffi::IEPropInteractiveRenderStatus, 0);
            args.i(ffi::IEPropRenderQualityDraft, 0);
            cpu_only(&args);
            let st = l.plugin.call(ffi::ActionRender, inst.raw(), Some(&args), None);
            if !ffi::not_failed(st) {
                result = Err(format!("the plug-in could not render this frame (status {st}){}", last_message()));
            }
        }
    }
    let _ = l.plugin.call(ffi::ActionEndSequenceRender, inst.raw(), Some(&begin), None);
    result
}

impl External for Host {
    fn effect(&self, img: &mut ExtImage, cx: &ExtCtx) -> Result<(), String> {
        let id = cx.effect.effect.strip_prefix("ofx:").ok_or("not a plug-in effect")?;
        let l = self.get(id)?;
        let ctx = l.info.effect_context.clone().ok_or("the plug-in has no effect context")?;
        let params = l.info.effect_params.clone();
        let src = std::mem::take(img);
        let r = run(&l, &ctx, &params, cx, vec![(ffi::SourceClip, &src)], None);
        match r {
            Ok(out) if out.w == src.w && out.h == src.h => {
                *img = out;
                Ok(())
            }
            Ok(_) => {
                *img = src;
                Err("the plug-in returned a different size".into())
            }
            Err(e) => {
                *img = src;
                self.note(&l.info.label, &e);
                Err(e)
            }
        }
    }

    fn transition(&self, a: &ExtImage, b: &ExtImage, p: f32, cx: &ExtCtx) -> Result<ExtImage, String> {
        let id = cx.effect.effect.strip_prefix("ofxt:").ok_or("not a plug-in transition")?;
        let l = self.get(id)?;
        let params = l.info.transition_params.clone();
        let r = run(&l, ffi::ContextTransition, &params, cx, vec![(ffi::SourceFromClip, a), (ffi::SourceToClip, b)], Some(p.clamp(0.0, 1.0) as f64));
        if let Err(e) = &r {
            self.note(&l.info.label, e);
        }
        r
    }
}
