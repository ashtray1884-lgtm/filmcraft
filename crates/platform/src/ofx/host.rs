//! The host side of OpenFX: the host property set, `fetchSuite`, image effects (descriptors and
//! instances), parameters, clips and images, and the image-effect, parameter, memory,
//! multi-thread, message, progress and time-line suites.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

use super::ffi::{self, OfxStatus, Raw};
use super::props::{Kind, PropSet, Val, get, guard, register, unregister};

/// A NUL-terminated string from a plug-in.
///
/// # Safety
/// `p` is null or NUL-terminated.
pub unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

// ---------------------------------------------------------------------------------------------
// Objects

/// A parameter's current value.
#[derive(Clone, Debug, PartialEq)]
pub enum PV {
    D(Vec<f64>),
    I(Vec<i32>),
    S(CString),
    None,
}

/// How a parameter type stores its value: (doubles?, dimension); strings are `S`.
pub fn shape(ty: &str) -> Option<(bool, usize)> {
    Some(match ty {
        ffi::ParamTypeDouble => (true, 1),
        ffi::ParamTypeDouble2D => (true, 2),
        ffi::ParamTypeDouble3D => (true, 3),
        ffi::ParamTypeRGB => (true, 3),
        ffi::ParamTypeRGBA => (true, 4),
        ffi::ParamTypeInteger | ffi::ParamTypeBoolean | ffi::ParamTypeChoice => (false, 1),
        ffi::ParamTypeInteger2D => (false, 2),
        ffi::ParamTypeInteger3D => (false, 3),
        _ => return None,
    })
}

pub fn is_string_type(ty: &str) -> bool {
    matches!(ty, ffi::ParamTypeString | ffi::ParamTypeCustom | ffi::ParamTypeStrChoice)
}

/// The value at another time, for animated parameters (set by the renderer for one render).
pub type AtTime = Arc<dyn Fn(f64) -> PV + Send + Sync>;

pub struct Param {
    pub name: String,
    pub ty: String,
    pub props: Box<PropSet>,
    pub val: Mutex<PV>,
    pub at_time: Mutex<Option<AtTime>>,
}

impl Param {
    pub fn new(name: &str, ty: &str) -> Box<Param> {
        let props = PropSet::new();
        props.s(ffi::PropType, ffi::TypeParameter);
        props.s(ffi::PropName, name);
        props.s(ffi::ParamPropType, ty);
        props.s(ffi::PropLabel, name);
        props.i(ffi::ParamPropAnimates, if is_string_type(ty) || ty == ffi::ParamTypeBoolean || ty == ffi::ParamTypeChoice { 0 } else { 1 });
        props.i(ffi::ParamPropSecret, 0);
        props.i(ffi::ParamPropEnabled, 1);
        props.i(ffi::ParamPropEvaluateOnChange, 1);
        props.i(ffi::ParamPropPersistant, 1);
        props.i(ffi::ParamPropCanUndo, 1);
        props.i(ffi::ParamPropPluginMayWrite, 0);
        props.i(ffi::ParamPropIsAnimating, 0);
        props.s(ffi::ParamPropHint, "");
        props.s(ffi::ParamPropScriptName, name);
        props.s(ffi::ParamPropParent, "");
        props.p(ffi::ParamPropDataPtr, std::ptr::null_mut());
        if let Some((dbl, n)) = shape(ty) {
            if dbl {
                props.ds(ffi::ParamPropDefault, &vec![0.0; n]);
                props.ds(ffi::ParamPropMin, &vec![-f64::MAX; n]);
                props.ds(ffi::ParamPropMax, &vec![f64::MAX; n]);
                props.ds(ffi::ParamPropDisplayMin, &vec![-f64::MAX; n]);
                props.ds(ffi::ParamPropDisplayMax, &vec![f64::MAX; n]);
                props.d(ffi::ParamPropIncrement, 1.0);
                props.i(ffi::ParamPropDigits, 2);
                props.s(ffi::ParamPropDoubleType, "OfxParamDoubleTypePlain");
            } else {
                props.is_(ffi::ParamPropDefault, &vec![0; n]);
                props.is_(ffi::ParamPropMin, &vec![i32::MIN; n]);
                props.is_(ffi::ParamPropMax, &vec![i32::MAX; n]);
                props.is_(ffi::ParamPropDisplayMin, &vec![i32::MIN; n]);
                props.is_(ffi::ParamPropDisplayMax, &vec![i32::MAX; n]);
            }
        } else if is_string_type(ty) {
            props.s(ffi::ParamPropDefault, "");
            props.s(ffi::ParamPropStringMode, "OfxParamStringIsSingleLine");
        }
        if ty == ffi::ParamTypeChoice || ty == ffi::ParamTypeStrChoice {
            props.set(ffi::ParamPropChoiceOption, Val::Str(Vec::new()));
            props.set(ffi::ParamPropChoiceEnum, Val::Str(Vec::new()));
            props.set(ffi::ParamPropChoiceOrder, Val::Int(Vec::new()));
        }
        if ty == ffi::ParamTypeGroup {
            props.i(ffi::ParamPropGroupOpen, 1);
        }
        if ty == ffi::ParamTypePage {
            props.set(ffi::ParamPropPageChild, Val::Str(Vec::new()));
        }
        let b = Box::new(Param { name: name.to_string(), ty: ty.to_string(), props, val: Mutex::new(PV::None), at_time: Mutex::new(None) });
        register(&*b as *const Param, Kind::Param);
        b
    }

    /// The default value from the descriptor's properties.
    pub fn default_value(&self) -> PV {
        if let Some((dbl, n)) = shape(&self.ty) {
            if dbl {
                let mut v = self.props.dbls(ffi::ParamPropDefault);
                v.resize(n, 0.0);
                return PV::D(v);
            }
            let mut v = self.props.ints(ffi::ParamPropDefault);
            v.resize(n, 0);
            return PV::I(v);
        }
        if is_string_type(&self.ty) {
            return PV::S(CString::new(self.props.str_(ffi::ParamPropDefault).unwrap_or_default().replace('\0', " ")).unwrap_or_default());
        }
        PV::None
    }

    /// A copy for an instance (properties copied, value at its default).
    pub fn instantiate(&self) -> Box<Param> {
        let p = Param::new(&self.name, &self.ty);
        p.props.copy_from(&self.props);
        p.props.s(ffi::PropType, ffi::TypeParameterInstance);
        *p.val.lock().unwrap_or_else(PoisonError::into_inner) = self.default_value();
        p
    }

    pub fn raw(&self) -> Raw {
        self as *const Param as Raw
    }

    pub fn value(&self) -> PV {
        self.val.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn value_at(&self, t: f64) -> PV {
        let f = self.at_time.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match f {
            Some(f) => f(t),
            None => self.value(),
        }
    }
}

impl Drop for Param {
    fn drop(&mut self) {
        unregister(self as *const Param);
    }
}

pub struct ParamSet {
    pub props: Box<PropSet>,
    pub list: Mutex<Vec<Box<Param>>>,
}

impl ParamSet {
    pub fn new() -> Box<ParamSet> {
        let b = Box::new(ParamSet { props: PropSet::new(), list: Mutex::new(Vec::new()) });
        register(&*b as *const ParamSet, Kind::ParamSet);
        b
    }
    pub fn raw(&self) -> Raw {
        self as *const ParamSet as Raw
    }
    /// The parameter named `name` (its address stays put: parameters are boxed).
    pub fn find(&self, name: &str) -> Option<Raw> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|p| p.name == name).map(|p| p.raw())
    }
}

impl Drop for ParamSet {
    fn drop(&mut self) {
        unregister(self as *const ParamSet);
    }
}

pub struct Clip {
    pub name: String,
    pub props: Box<PropSet>,
    /// The effect (instance) the clip belongs to.
    pub effect: usize,
}

impl Clip {
    pub fn new(name: &str, effect: usize) -> Box<Clip> {
        let props = PropSet::new();
        props.s(ffi::PropType, ffi::TypeClip);
        props.s(ffi::PropName, name);
        props.s(ffi::PropLabel, name);
        props.ss(ffi::IEPropSupportedComponents, &[ffi::ComponentRGBA]);
        props.i(ffi::IEPropTemporalClipAccess, 0);
        props.i(ffi::ClipPropOptional, 0);
        props.i(ffi::ClipPropIsMask, 0);
        props.s(ffi::ClipPropFieldOrder, ffi::FieldNone);
        props.i(ffi::IEPropSupportsTiles, 1);
        let b = Box::new(Clip { name: name.to_string(), props, effect });
        register(&*b as *const Clip, Kind::Clip);
        b
    }
    pub fn raw(&self) -> Raw {
        self as *const Clip as Raw
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        unregister(self as *const Clip);
    }
}

/// Images and other frames during a render (implemented by the renderer).
pub trait RenderJob: Sync {
    /// Clip `clip` at `time` (OpenFX frames): a buffer the plug-in may read (or, for the output,
    /// write), or `None` (no such image: transparent black).
    fn image(&self, clip: &str, time: f64) -> Option<Arc<ImageBuf>>;
    fn time(&self) -> f64;
    fn duration(&self) -> f64;
}

/// An image effect: a descriptor (from describe / describe in context) or an instance.
pub struct Effect {
    pub props: Box<PropSet>,
    pub params: Box<ParamSet>,
    pub clips: Mutex<Vec<Box<Clip>>>,
    /// `&dyn RenderJob` of the render in progress (as two words), cleared afterwards.
    pub job: Mutex<Option<[usize; 2]>>,
}

impl Effect {
    pub fn new() -> Box<Effect> {
        let b = Box::new(Effect { props: PropSet::new(), params: ParamSet::new(), clips: Mutex::new(Vec::new()), job: Mutex::new(None) });
        register(&*b as *const Effect, Kind::Effect);
        b
    }
    pub fn raw(&self) -> Raw {
        self as *const Effect as Raw
    }
    pub fn clip(&self, name: &str) -> Option<Raw> {
        self.clips.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|c| c.name == name).map(|c| c.raw())
    }
    pub fn clip_names(&self) -> Vec<String> {
        self.clips.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|c| c.name.clone()).collect()
    }
    pub fn clip_props<R>(&self, name: &str, f: impl FnOnce(&PropSet) -> R) -> Option<R> {
        self.clips.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|c| c.name == name).map(|c| f(&c.props))
    }

    /// Run `f` with the current render job, if one is in progress.
    pub fn with_job<R>(&self, f: impl FnOnce(&dyn RenderJob) -> R) -> Option<R> {
        let words = (*self.job.lock().unwrap_or_else(PoisonError::into_inner))?;
        // SAFETY: the words are a `&dyn RenderJob` set by `set_job` for the duration of a render
        // (the renderer clears them before the job goes out of scope); same layout both ways.
        let job: &dyn RenderJob = unsafe { std::mem::transmute::<[usize; 2], &dyn RenderJob>(words) };
        Some(f(job))
    }

    /// Make `job` the current render job until the returned guard is dropped.
    pub fn set_job<'a>(&'a self, job: &'a dyn RenderJob) -> JobGuard<'a> {
        // SAFETY: a fat reference is two words; it is only read back by `with_job` while the guard
        // (which borrows `job`) lives.
        let words = unsafe { std::mem::transmute::<&dyn RenderJob, [usize; 2]>(job) };
        *self.job.lock().unwrap_or_else(PoisonError::into_inner) = Some(words);
        JobGuard { effect: self, _job: std::marker::PhantomData }
    }
}

pub struct JobGuard<'a> {
    effect: &'a Effect,
    _job: std::marker::PhantomData<&'a dyn RenderJob>,
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        *self.effect.job.lock().unwrap_or_else(PoisonError::into_inner) = None;
        release_images_of(self.effect as *const Effect as usize);
    }
}

impl Drop for Effect {
    fn drop(&mut self) {
        unregister(self as *const Effect);
    }
}

// ---------------------------------------------------------------------------------------------
// Images

/// Pixel depth of images exchanged with a plug-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Byte,
    Short,
    Float,
}

impl Depth {
    pub fn name(self) -> &'static str {
        match self {
            Depth::Byte => ffi::BitDepthByte,
            Depth::Short => ffi::BitDepthShort,
            Depth::Float => ffi::BitDepthFloat,
        }
    }
    pub fn bytes(self) -> usize {
        match self {
            Depth::Byte => 1,
            Depth::Short => 2,
            Depth::Float => 4,
        }
    }
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Chunk([u8; 64]);

/// RGBA pixels, rows bottom-up (OpenFX's order), 64-byte aligned, writable by the plug-in.
pub struct ImageBuf {
    pub w: usize,
    pub h: usize,
    pub depth: Depth,
    data: std::cell::UnsafeCell<Vec<Chunk>>,
}

// SAFETY: the buffer is plain bytes; concurrent writes come only from the plug-in rendering its
// output (its own business, as in every host), and FilmCraft reads it after the render returns.
unsafe impl Sync for ImageBuf {}
// SAFETY: as above.
unsafe impl Send for ImageBuf {}

impl ImageBuf {
    pub fn new(w: usize, h: usize, depth: Depth) -> Option<ImageBuf> {
        let bytes = w.checked_mul(h)?.checked_mul(4)?.checked_mul(depth.bytes())?;
        if bytes > 1 << 32 {
            return None;
        }
        Some(ImageBuf { w, h, depth, data: std::cell::UnsafeCell::new(vec![Chunk([0; 64]); bytes.div_ceil(64).max(1)]) })
    }
    pub fn row_bytes(&self) -> usize {
        self.w * 4 * self.depth.bytes()
    }
    pub fn ptr(&self) -> *mut u8 {
        // SAFETY: reading the Vec's pointer; the Vec itself is never resized.
        unsafe { (*self.data.get()).as_mut_ptr() as *mut u8 }
    }
    /// The bytes (after the plug-in has finished writing).
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the allocation holds at least w*h*4*depth bytes; only read when no render runs.
        unsafe { std::slice::from_raw_parts(self.ptr(), self.w * self.h * 4 * self.depth.bytes()) }
    }
    #[allow(clippy::mut_from_ref)]
    pub fn bytes_mut(&self) -> &mut [u8] {
        // SAFETY: as above; called by the renderer before handing the buffer to the plug-in.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.w * self.h * 4 * self.depth.bytes()) }
    }
}

/// Images handed to plug-ins: image property set → (owning effect, buffer kept alive).
static IMAGES: Mutex<Option<HashMap<usize, (usize, Box<PropSet>, Arc<ImageBuf>)>>> = Mutex::new(None);

fn image_props(buf: &Arc<ImageBuf>, scale: f64, id: u64) -> Box<PropSet> {
    let ps = PropSet::new();
    ps.s(ffi::PropType, ffi::TypeImage);
    ps.p(ffi::ImagePropData, buf.ptr() as Raw);
    let b = [0, 0, buf.w as i32, buf.h as i32];
    ps.is_(ffi::ImagePropBounds, &b);
    ps.is_(ffi::ImagePropRegionOfDefinition, &b);
    ps.i(ffi::ImagePropRowBytes, buf.row_bytes() as i32);
    ps.s(ffi::IEPropPixelDepth, buf.depth.name());
    ps.s(ffi::IEPropComponents, ffi::ComponentRGBA);
    ps.s(ffi::IEPropPreMultiplication, ffi::ImagePreMultiplied);
    ps.ds(ffi::IEPropRenderScale, &[scale, scale]);
    ps.d(ffi::ImagePropPixelAspectRatio, 1.0);
    ps.s(ffi::ImagePropField, ffi::FieldNone);
    ps.s(ffi::ImagePropUniqueIdentifier, &format!("filmcraft-{id}"));
    ps.p("OfxImageEffectPropOpenCLImage", std::ptr::null_mut());
    ps
}

fn release_images_of(effect: usize) {
    let mut g = IMAGES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(m) = g.as_mut() {
        m.retain(|_, (e, _, _)| *e != effect);
    }
}

// ---------------------------------------------------------------------------------------------
// Image effect suite

unsafe extern "C" fn ie_get_property_set(h: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(e) = get::<Effect>(h, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer from the plug-in.
        unsafe { *out = e.props.raw() };
        ffi::OK
    })
}
unsafe extern "C" fn ie_get_param_set(h: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(e) = get::<Effect>(h, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: as above.
        unsafe { *out = e.params.raw() };
        ffi::OK
    })
}
unsafe extern "C" fn ie_clip_define(h: Raw, name: *const c_char, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(e) = get::<Effect>(h, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        // SAFETY: NUL-terminated name (contract).
        let Some(n) = (unsafe { cstr(name) }) else { return ffi::ERR_UNKNOWN };
        let mut clips = e.clips.lock().unwrap_or_else(PoisonError::into_inner);
        let raw = match clips.iter().find(|c| c.name == n) {
            Some(c) => c.props.raw(),
            None => {
                let c = Clip::new(n, h as usize);
                let r = c.props.raw();
                clips.push(c);
                r
            }
        };
        if !out.is_null() {
            // SAFETY: non-null out-pointer.
            unsafe { *out = raw };
        }
        ffi::OK
    })
}
unsafe extern "C" fn ie_clip_get_handle(h: Raw, name: *const c_char, clip: *mut Raw, props: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(e) = get::<Effect>(h, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        // SAFETY: NUL-terminated name.
        let Some(n) = (unsafe { cstr(name) }) else { return ffi::ERR_UNKNOWN };
        let clips = e.clips.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(c) = clips.iter().find(|c| c.name == n) else { return ffi::ERR_UNKNOWN };
        if !clip.is_null() {
            // SAFETY: non-null out-pointers.
            unsafe { *clip = c.raw() };
        }
        if !props.is_null() {
            // SAFETY: as above.
            unsafe { *props = c.props.raw() };
        }
        ffi::OK
    })
}
unsafe extern "C" fn ie_clip_get_property_set(h: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(c) = get::<Clip>(h, Kind::Clip) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *out = c.props.raw() };
        ffi::OK
    })
}
unsafe extern "C" fn ie_clip_get_image(h: Raw, time: f64, _region: *const ffi::OfxRectD, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(c) = get::<Clip>(h, Kind::Clip) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let Some(e) = get::<Effect>(c.effect as Raw, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        let t = if time.is_finite() { time } else { 0.0 };
        let Some(Some(buf)) = e.with_job(|j| j.image(&c.name, t)) else { return ffi::FAILED };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scale = e.props.dbl(ffi::IEPropRenderScale).unwrap_or(1.0);
        let ps = image_props(&buf, scale, id);
        let raw = ps.raw();
        IMAGES.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).insert(raw as usize, (c.effect, ps, buf));
        // SAFETY: non-null out-pointer.
        unsafe { *out = raw };
        ffi::OK
    })
}
unsafe extern "C" fn ie_clip_release_image(h: Raw) -> OfxStatus {
    guard(|| {
        let removed = IMAGES.lock().unwrap_or_else(PoisonError::into_inner).as_mut().and_then(|m| m.remove(&(h as usize)));
        if removed.is_some() { ffi::OK } else { ffi::ERR_BAD_HANDLE }
    })
}
unsafe extern "C" fn ie_clip_get_rod(h: Raw, _time: f64, out: *mut ffi::OfxRectD) -> OfxStatus {
    guard(|| {
        let Some(c) = get::<Clip>(h, Kind::Clip) else { return ffi::ERR_BAD_HANDLE };
        let Some(e) = get::<Effect>(c.effect as Raw, Kind::Effect) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let size = e.props.dbls(ffi::IEPropProjectSize);
        let (w, h) = (size.first().copied().unwrap_or(1920.0), size.get(1).copied().unwrap_or(1080.0));
        // SAFETY: non-null out-pointer.
        unsafe { *out = ffi::OfxRectD { x1: 0.0, y1: 0.0, x2: w, y2: h } };
        ffi::OK
    })
}
unsafe extern "C" fn ie_abort(_h: Raw) -> c_int {
    0
}

/// Plug-in memory: a header with the size before the block (so `free` knows the layout).
const MEM_ALIGN: usize = 64;

fn mem_alloc(n: usize) -> Option<*mut u8> {
    let total = n.checked_add(MEM_ALIGN)?;
    let layout = std::alloc::Layout::from_size_align(total, MEM_ALIGN).ok()?;
    // SAFETY: a non-zero size (at least MEM_ALIGN) with a valid alignment.
    let p = unsafe { std::alloc::alloc(layout) };
    if p.is_null() {
        return None;
    }
    // SAFETY: the block holds MEM_ALIGN header bytes; the size fits in the first word.
    unsafe { (p as *mut usize).write(total) };
    // SAFETY: within the allocation.
    Some(unsafe { p.add(MEM_ALIGN) })
}

/// # Safety
/// `p` came from `mem_alloc` and is freed once.
unsafe fn mem_free(p: *mut u8) {
    // SAFETY: per the contract the header sits MEM_ALIGN bytes before `p`.
    unsafe {
        let base = p.sub(MEM_ALIGN);
        let total = (base as *const usize).read();
        if let Ok(layout) = std::alloc::Layout::from_size_align(total, MEM_ALIGN) {
            std::alloc::dealloc(base, layout);
        }
    }
}

/// Live memory blocks (plug-in memory suite and image memory), so a bad free is refused.
static BLOCKS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

fn track_block(p: *mut u8, n: usize) {
    BLOCKS.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(HashMap::new).insert(p as usize, n);
}
fn untrack_block(p: Raw) -> bool {
    BLOCKS.lock().unwrap_or_else(PoisonError::into_inner).as_mut().and_then(|m| m.remove(&(p as usize))).is_some()
}

unsafe extern "C" fn ie_mem_alloc(_inst: Raw, n: usize, out: *mut Raw) -> OfxStatus {
    guard(|| {
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let Some(p) = mem_alloc(n) else { return ffi::ERR_MEMORY };
        track_block(p, n);
        // SAFETY: non-null out-pointer; the handle is the block itself.
        unsafe { *out = p as Raw };
        ffi::OK
    })
}
unsafe extern "C" fn ie_mem_free(h: Raw) -> OfxStatus {
    guard(|| {
        if !untrack_block(h) {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: a live block from mem_alloc, now untracked (freed once).
        unsafe { mem_free(h as *mut u8) };
        ffi::OK
    })
}
unsafe extern "C" fn ie_mem_lock(h: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let live = BLOCKS.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|m| m.contains_key(&(h as usize)));
        if !live || out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *out = h };
        ffi::OK
    })
}
unsafe extern "C" fn ie_mem_unlock(_h: Raw) -> OfxStatus {
    ffi::OK
}

pub static IMAGE_EFFECT_SUITE: ffi::ImageEffectSuiteV1 = ffi::ImageEffectSuiteV1 {
    get_property_set: ie_get_property_set,
    get_param_set: ie_get_param_set,
    clip_define: ie_clip_define,
    clip_get_handle: ie_clip_get_handle,
    clip_get_property_set: ie_clip_get_property_set,
    clip_get_image: ie_clip_get_image,
    clip_release_image: ie_clip_release_image,
    clip_get_region_of_definition: ie_clip_get_rod,
    abort: ie_abort,
    image_memory_alloc: ie_mem_alloc,
    image_memory_free: ie_mem_free,
    image_memory_lock: ie_mem_lock,
    image_memory_unlock: ie_mem_unlock,
};

// ---------------------------------------------------------------------------------------------
// Parameter suite

unsafe extern "C" fn p_define(set: Raw, ty: *const c_char, name: *const c_char, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(ps) = get::<ParamSet>(set, Kind::ParamSet) else { return ffi::ERR_BAD_HANDLE };
        // SAFETY: NUL-terminated strings (contract).
        let (Some(ty), Some(n)) = (unsafe { cstr(ty) }, unsafe { cstr(name) }) else { return ffi::ERR_UNKNOWN };
        let mut list = ps.list.lock().unwrap_or_else(PoisonError::into_inner);
        if list.iter().any(|p| p.name == n) {
            return ffi::ERR_EXISTS;
        }
        if list.len() > 10_000 {
            return ffi::ERR_MEMORY;
        }
        let p = Param::new(n, ty);
        if !out.is_null() {
            // SAFETY: non-null out-pointer.
            unsafe { *out = p.props.raw() };
        }
        list.push(p);
        ffi::OK
    })
}
unsafe extern "C" fn p_get_handle(set: Raw, name: *const c_char, out: *mut Raw, props: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(ps) = get::<ParamSet>(set, Kind::ParamSet) else { return ffi::ERR_BAD_HANDLE };
        // SAFETY: NUL-terminated string.
        let Some(n) = (unsafe { cstr(name) }) else { return ffi::ERR_UNKNOWN };
        let list = ps.list.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(p) = list.iter().find(|p| p.name == n) else { return ffi::ERR_UNKNOWN };
        if !out.is_null() {
            // SAFETY: non-null out-pointers.
            unsafe { *out = p.raw() };
        }
        if !props.is_null() {
            // SAFETY: as above.
            unsafe { *props = p.props.raw() };
        }
        ffi::OK
    })
}
unsafe extern "C" fn p_set_get_props(set: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(ps) = get::<ParamSet>(set, Kind::ParamSet) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *out = ps.props.raw() };
        ffi::OK
    })
}
unsafe extern "C" fn p_get_props(h: Raw, out: *mut Raw) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *out = p.props.raw() };
        ffi::OK
    })
}

/// Write `v` through the plug-in's out-pointers (one per component; a string is one `char*`).
///
/// # Safety
/// Each pointer the parameter type needs is a valid out-pointer of the C type for that type
/// (`double*`, `int*` or `char**`), as the OpenFX varargs contract requires.
unsafe fn write_out(v: &PV, outs: [Raw; 4]) -> OfxStatus {
    match v {
        PV::D(d) => {
            for (i, x) in d.iter().take(4).enumerate() {
                if outs[i].is_null() {
                    return ffi::ERR_BAD_HANDLE;
                }
                // SAFETY: per the contract.
                unsafe { (outs[i] as *mut f64).write_unaligned(*x) };
            }
        }
        PV::I(d) => {
            for (i, x) in d.iter().take(4).enumerate() {
                if outs[i].is_null() {
                    return ffi::ERR_BAD_HANDLE;
                }
                // SAFETY: per the contract.
                unsafe { (outs[i] as *mut c_int).write_unaligned(*x) };
            }
        }
        PV::S(s) => {
            if outs[0].is_null() {
                return ffi::ERR_BAD_HANDLE;
            }
            // SAFETY: per the contract; the string lives in the parameter until it changes.
            unsafe { (outs[0] as *mut *const c_char).write_unaligned(s.as_ptr()) };
        }
        PV::None => return ffi::ERR_UNKNOWN,
    }
    ffi::OK
}

unsafe extern "C" fn p_get_value(h: Raw, a: Raw, b: Raw, c: Raw, d: Raw) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        // SAFETY: the varargs pointers per the contract; only as many as the type has are used.
        unsafe { write_out(&p.value(), [a, b, c, d]) }
    })
}
unsafe extern "C" fn p_get_value_at_time(h: Raw, t: f64, a: Raw, b: Raw, c: Raw, d: Raw) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        // strings do not animate: their current value (whose storage outlives the call)
        let v = if is_string_type(&p.ty) { p.value() } else { p.value_at(if t.is_finite() { t } else { 0.0 }) };
        // SAFETY: as above.
        unsafe { write_out(&v, [a, b, c, d]) }
    })
}
unsafe extern "C" fn p_get_derivative(h: Raw, t: f64, a: Raw, b: Raw, c: Raw, d: Raw) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        let t = if t.is_finite() { t } else { 0.0 };
        let v = match (p.value_at(t - 0.5), p.value_at(t + 0.5)) {
            (PV::D(x), PV::D(y)) => PV::D(x.iter().zip(&y).map(|(x, y)| y - x).collect()),
            (PV::D(x), _) => PV::D(vec![0.0; x.len()]),
            _ => return ffi::ERR_UNSUPPORTED,
        };
        // SAFETY: as above.
        unsafe { write_out(&v, [a, b, c, d]) }
    })
}
unsafe extern "C" fn p_get_integral(h: Raw, t1: f64, t2: f64, a: Raw, b: Raw, c: Raw, d: Raw) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        let (t1, t2) = (if t1.is_finite() { t1 } else { 0.0 }, if t2.is_finite() { t2 } else { 0.0 });
        let steps = 16;
        let dt = (t2 - t1) / steps as f64;
        let PV::D(first) = p.value_at(t1) else { return ffi::ERR_UNSUPPORTED };
        let mut acc = vec![0.0; first.len()];
        for k in 0..steps {
            if let PV::D(v) = p.value_at(t1 + (k as f64 + 0.5) * dt) {
                for (a, x) in acc.iter_mut().zip(&v) {
                    *a += x * dt;
                }
            }
        }
        // SAFETY: as above.
        unsafe { write_out(&PV::D(acc), [a, b, c, d]) }
    })
}

/// Read a new value for `p` from varargs: `int` / `double` / `char*` values in order, from
/// `next_int` (integers and pointers) and `next_dbl` (doubles).
fn read_in(p: &Param, mut next_int: impl FnMut() -> u64, mut next_dbl: impl FnMut() -> f64) -> Option<PV> {
    if let Some((dbl, n)) = shape(&p.ty) {
        return Some(if dbl { PV::D((0..n).map(|_| next_dbl()).collect()) } else { PV::I((0..n).map(|_| next_int() as u32 as i32).collect()) });
    }
    if is_string_type(&p.ty) {
        let ptr = next_int() as usize as *const c_char;
        // SAFETY: a `char*` vararg for string parameters (contract).
        let s = unsafe { cstr(ptr) }.unwrap_or_default();
        return Some(PV::S(CString::new(s).unwrap_or_default()));
    }
    None
}

fn store(p: &Param, v: Option<PV>) -> OfxStatus {
    match v {
        Some(v) => {
            *p.val.lock().unwrap_or_else(PoisonError::into_inner) = v;
            ffi::OK
        }
        None => ffi::ERR_UNSUPPORTED,
    }
}

/// Windows x64: varargs `double`s are also passed in the integer registers / stack slots, so one
/// sequence of 64-bit slots carries every value.
#[cfg(windows)]
unsafe extern "C" fn p_set_value(h: Raw, a: u64, b: u64, c: u64, d: u64) -> OfxStatus {
    guard(|| {
        let Some(p) = get::<Param>(h, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
        let slots = [a, b, c, d];
        let i = Cell::new(0usize);
        let next = || {
            let x = slots.get(i.get()).copied().unwrap_or(0);
            i.set(i.get() + 1);
            x
        };
        let v = read_in(p, next, || f64::from_bits(next()));
        store(p, v)
    })
}
#[cfg(windows)]
unsafe extern "C" fn p_set_value_at_time(h: Raw, _t: f64, a: u64, b: u64, c: u64, d: u64) -> OfxStatus {
    // FilmCraft keeps keyframes itself: a plug-in setting a value at a time sets the value
    // SAFETY: the same varargs contract as paramSetValue.
    unsafe { p_set_value(h, a, b, c, d) }
}

/// System V x86-64: integer and double varargs travel in separate registers; the assembly shims
/// below save them all and call [`p_set_regs`].
#[cfg(all(unix, target_arch = "x86_64"))]
mod sysv {
    use super::*;
    std::arch::global_asm!(
        ".globl filmcraft_ofx_set_value",
        "filmcraft_ofx_set_value:",
        "sub rsp, 120",
        "mov [rsp], rdi",
        "mov [rsp+8], rsi",
        "mov [rsp+16], rdx",
        "mov [rsp+24], rcx",
        "mov [rsp+32], r8",
        "mov [rsp+40], r9",
        "movsd [rsp+48], xmm0",
        "movsd [rsp+56], xmm1",
        "movsd [rsp+64], xmm2",
        "movsd [rsp+72], xmm3",
        "movsd [rsp+80], xmm4",
        "movsd [rsp+88], xmm5",
        "movsd [rsp+96], xmm6",
        "movsd [rsp+104], xmm7",
        "mov rdi, rsp",
        "xor esi, esi",
        "call {f}",
        "add rsp, 120",
        "ret",
        ".globl filmcraft_ofx_set_value_at_time",
        "filmcraft_ofx_set_value_at_time:",
        "sub rsp, 120",
        "mov [rsp], rdi",
        "mov [rsp+8], rsi",
        "mov [rsp+16], rdx",
        "mov [rsp+24], rcx",
        "mov [rsp+32], r8",
        "mov [rsp+40], r9",
        "movsd [rsp+48], xmm0",
        "movsd [rsp+56], xmm1",
        "movsd [rsp+64], xmm2",
        "movsd [rsp+72], xmm3",
        "movsd [rsp+80], xmm4",
        "movsd [rsp+88], xmm5",
        "movsd [rsp+96], xmm6",
        "movsd [rsp+104], xmm7",
        "mov rdi, rsp",
        "mov esi, 1",
        "call {f}",
        "add rsp, 120",
        "ret",
        f = sym p_set_regs,
    );

    unsafe extern "C" {
        pub fn filmcraft_ofx_set_value();
        pub fn filmcraft_ofx_set_value_at_time();
    }

    /// `regs`: rdi, rsi, rdx, rcx, r8, r9, then xmm0–xmm7; `at_time`: xmm0 holds the time.
    unsafe extern "C" fn p_set_regs(regs: *const u64, at_time: u32) -> OfxStatus {
        guard(|| {
            if regs.is_null() {
                return ffi::ERR_BAD_HANDLE;
            }
            // SAFETY: the shim passes its 14-slot save area.
            let r = unsafe { std::slice::from_raw_parts(regs, 14) };
            let Some(p) = get::<Param>(r[0] as Raw, Kind::Param) else { return ffi::ERR_BAD_HANDLE };
            let (mut gi, mut xi) = (1usize, 6 + at_time as usize);
            let v = read_in(
                p,
                || {
                    let x = if gi < 6 { r[gi] } else { 0 };
                    gi += 1;
                    x
                },
                || {
                    let x = if xi < 14 { r[xi] } else { 0 };
                    xi += 1;
                    f64::from_bits(x)
                },
            );
            store(p, v)
        })
    }
}

fn set_value_fn() -> *const c_void {
    #[cfg(windows)]
    {
        p_set_value as *const c_void
    }
    #[cfg(all(unix, target_arch = "x86_64"))]
    {
        sysv::filmcraft_ofx_set_value as *const c_void
    }
    #[cfg(not(any(windows, all(unix, target_arch = "x86_64"))))]
    {
        p_unsupported as *const c_void
    }
}
fn set_value_at_time_fn() -> *const c_void {
    #[cfg(windows)]
    {
        p_set_value_at_time as *const c_void
    }
    #[cfg(all(unix, target_arch = "x86_64"))]
    {
        sysv::filmcraft_ofx_set_value_at_time as *const c_void
    }
    #[cfg(not(any(windows, all(unix, target_arch = "x86_64"))))]
    {
        p_unsupported as *const c_void
    }
}
#[cfg(not(any(windows, all(unix, target_arch = "x86_64"))))]
unsafe extern "C" fn p_unsupported(_h: Raw) -> OfxStatus {
    ffi::ERR_UNSUPPORTED
}

unsafe extern "C" fn p_num_keys(h: Raw, out: *mut c_uint) -> OfxStatus {
    guard(|| {
        if get::<Param>(h, Kind::Param).is_none() || out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *out = 0 };
        ffi::OK
    })
}
unsafe extern "C" fn p_key_time(_h: Raw, _i: c_uint, _out: *mut f64) -> OfxStatus {
    ffi::ERR_BAD_INDEX
}
unsafe extern "C" fn p_key_index(_h: Raw, _t: f64, _dir: c_int, _out: *mut c_int) -> OfxStatus {
    ffi::FAILED
}
unsafe extern "C" fn p_delete_key(_h: Raw, _t: f64) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn p_delete_all_keys(_h: Raw) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn p_copy(to: Raw, from: Raw, _offset: f64, _range: *const ffi::OfxRangeD) -> OfxStatus {
    guard(|| {
        let (Some(t), Some(f)) = (get::<Param>(to, Kind::Param), get::<Param>(from, Kind::Param)) else { return ffi::ERR_BAD_HANDLE };
        if t.ty != f.ty {
            return ffi::ERR_VALUE;
        }
        let v = f.value();
        *t.val.lock().unwrap_or_else(PoisonError::into_inner) = v;
        ffi::OK
    })
}
unsafe extern "C" fn p_edit_begin(_s: Raw, _n: *const c_char) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn p_edit_end(_s: Raw) -> OfxStatus {
    ffi::OK
}

pub fn parameter_suite() -> &'static ffi::ParameterSuiteV1 {
    struct S(ffi::ParameterSuiteV1);
    // SAFETY: the suite holds function pointers only, shared read-only.
    unsafe impl Sync for S {}
    // SAFETY: as above.
    unsafe impl Send for S {}
    static SUITE: OnceLock<S> = OnceLock::new();
    &SUITE
        .get_or_init(|| {
            S(ffi::ParameterSuiteV1 {
                param_define: p_define,
                param_get_handle: p_get_handle,
                param_set_get_property_set: p_set_get_props,
                param_get_property_set: p_get_props,
                param_get_value: p_get_value as *const c_void,
                param_get_value_at_time: p_get_value_at_time as *const c_void,
                param_get_derivative: p_get_derivative as *const c_void,
                param_get_integral: p_get_integral as *const c_void,
                param_set_value: set_value_fn(),
                param_set_value_at_time: set_value_at_time_fn(),
                param_get_num_keys: p_num_keys,
                param_get_key_time: p_key_time,
                param_get_key_index: p_key_index,
                param_delete_key: p_delete_key,
                param_delete_all_keys: p_delete_all_keys,
                param_copy: p_copy,
                param_edit_begin: p_edit_begin,
                param_edit_end: p_edit_end,
            })
        })
        .0
}

// ---------------------------------------------------------------------------------------------
// Memory, threads, messages, progress, time line

unsafe extern "C" fn m_alloc(_h: Raw, n: usize, out: *mut Raw) -> OfxStatus {
    guard(|| {
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let Some(p) = mem_alloc(n) else { return ffi::ERR_MEMORY };
        track_block(p, n);
        // SAFETY: non-null out-pointer.
        unsafe { *out = p as Raw };
        ffi::OK
    })
}
unsafe extern "C" fn m_free(p: Raw) -> OfxStatus {
    guard(|| {
        if !untrack_block(p) {
            return ffi::ERR_BAD_HANDLE;
        }
        // SAFETY: a live block, untracked now, freed once.
        unsafe { mem_free(p as *mut u8) };
        ffi::OK
    })
}
pub static MEMORY_SUITE: ffi::MemorySuiteV1 = ffi::MemorySuiteV1 { alloc: m_alloc, free: m_free };

thread_local! {
    static THREAD: Cell<Option<(u32, bool)>> = const { Cell::new(None) };
}

struct SendPtr(Raw);
// SAFETY: the plug-in's argument for its own thread function; OpenFX hands it to every thread.
unsafe impl Send for SendPtr {}
// SAFETY: as above.
unsafe impl Sync for SendPtr {}

unsafe extern "C" fn t_multi_thread(f: Option<ffi::ThreadFunction>, n: c_uint, arg: Raw) -> OfxStatus {
    guard(|| {
        let Some(f) = f else { return ffi::ERR_BAD_HANDLE };
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get()) as u32;
        let n = if n == 0 { cores } else { n.min(256) };
        let arg = SendPtr(arg);
        if n == 1 {
            THREAD.with(|t| t.set(Some((0, false))));
            // SAFETY: the plug-in's thread function with its own argument (OpenFX contract).
            unsafe { f(0, 1, arg.0) };
            THREAD.with(|t| t.set(None));
            return ffi::OK;
        }
        let ok = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let arg = &arg;
                    std::thread::Builder::new().stack_size(8 << 20).spawn_scoped(s, move || {
                        THREAD.with(|t| t.set(Some((i, true))));
                        // SAFETY: as above, once per thread index.
                        unsafe { f(i, n, arg.0) };
                    })
                })
                .collect();
            handles.into_iter().all(|h| h.is_ok_and(|h| h.join().is_ok()))
        });
        if ok { ffi::OK } else { ffi::FAILED }
    })
}
unsafe extern "C" fn t_num_cpus(out: *mut c_uint) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    // SAFETY: non-null out-pointer.
    unsafe { *out = std::thread::available_parallelism().map_or(4, |n| n.get()) as c_uint };
    ffi::OK
}
unsafe extern "C" fn t_index(out: *mut c_uint) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    // SAFETY: non-null out-pointer.
    unsafe { *out = THREAD.with(|t| t.get()).map_or(0, |(i, _)| i) };
    ffi::OK
}
unsafe extern "C" fn t_is_spawned() -> c_int {
    THREAD.with(|t| t.get()).is_some_and(|(_, s)| s) as c_int
}

/// A recursive mutex (OpenFX mutexes may be locked again by the thread holding them).
struct RMutex {
    state: Mutex<(Option<std::thread::ThreadId>, u32)>,
    cv: Condvar,
}

unsafe extern "C" fn t_mutex_create(out: *mut Raw, count: c_int) -> OfxStatus {
    guard(|| {
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let me = std::thread::current().id();
        let n = count.max(0) as u32;
        let m = Box::new(RMutex { state: Mutex::new((if n > 0 { Some(me) } else { None }, n)), cv: Condvar::new() });
        let p = Box::into_raw(m);
        register(p as *const RMutex, Kind::Mutex);
        // SAFETY: non-null out-pointer.
        unsafe { *out = p as Raw };
        ffi::OK
    })
}
unsafe extern "C" fn t_mutex_destroy(h: Raw) -> OfxStatus {
    guard(|| {
        if !super::props::is(h as *const RMutex, Kind::Mutex) {
            return ffi::ERR_BAD_HANDLE;
        }
        unregister(h as *const RMutex);
        // SAFETY: created by Box::into_raw in t_mutex_create, unregistered now (freed once).
        drop(unsafe { Box::from_raw(h as *mut RMutex) });
        ffi::OK
    })
}
fn lock_mutex(h: Raw, wait: bool) -> OfxStatus {
    let Some(m) = get::<RMutex>(h, Kind::Mutex) else { return ffi::ERR_BAD_HANDLE };
    let me = std::thread::current().id();
    let mut g = m.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        match g.0 {
            None => {
                *g = (Some(me), 1);
                return ffi::OK;
            }
            Some(o) if o == me => {
                g.1 = g.1.saturating_add(1);
                return ffi::OK;
            }
            Some(_) if !wait => return ffi::FAILED,
            Some(_) => g = m.cv.wait(g).unwrap_or_else(PoisonError::into_inner),
        }
    }
}
unsafe extern "C" fn t_mutex_lock(h: Raw) -> OfxStatus {
    guard(|| lock_mutex(h, true))
}
unsafe extern "C" fn t_mutex_try_lock(h: Raw) -> OfxStatus {
    guard(|| lock_mutex(h, false))
}
unsafe extern "C" fn t_mutex_unlock(h: Raw) -> OfxStatus {
    guard(|| {
        let Some(m) = get::<RMutex>(h, Kind::Mutex) else { return ffi::ERR_BAD_HANDLE };
        let me = std::thread::current().id();
        let mut g = m.state.lock().unwrap_or_else(PoisonError::into_inner);
        if g.0 != Some(me) {
            return ffi::ERR_BAD_HANDLE;
        }
        g.1 = g.1.saturating_sub(1);
        if g.1 == 0 {
            g.0 = None;
            m.cv.notify_one();
        }
        ffi::OK
    })
}
pub static MULTI_THREAD_SUITE: ffi::MultiThreadSuiteV1 = ffi::MultiThreadSuiteV1 {
    multi_thread: t_multi_thread,
    num_cpus: t_num_cpus,
    index: t_index,
    is_spawned: t_is_spawned,
    mutex_create: t_mutex_create,
    mutex_destroy: t_mutex_destroy,
    mutex_lock: t_mutex_lock,
    mutex_unlock: t_mutex_unlock,
    mutex_try_lock: t_mutex_try_lock,
};

/// The last message a plug-in posted (shown when a render fails).
pub static LAST_MESSAGE: Mutex<Option<String>> = Mutex::new(None);

unsafe extern "C" fn msg_message(_h: Raw, ty: *const c_char, _id: *const c_char, fmt: *const c_char) -> OfxStatus {
    guard(|| {
        // SAFETY: NUL-terminated strings or null (contract).
        let (ty, text) = (unsafe { cstr(ty) }.unwrap_or(""), unsafe { cstr(fmt) }.unwrap_or(""));
        log::info!("OpenFX plug-in message ({ty}): {text}");
        if ty == ffi::MessageError || ty == ffi::MessageFatal {
            *LAST_MESSAGE.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.to_string());
        }
        if ty == ffi::MessageQuestion { ffi::REPLY_DEFAULT } else { ffi::OK }
    })
}
unsafe extern "C" fn msg_clear(_h: Raw) -> OfxStatus {
    ffi::OK
}
pub static MESSAGE_SUITE: ffi::MessageSuiteV2 = ffi::MessageSuiteV2 { message: msg_message, set_persistent: msg_message, clear_persistent: msg_clear };

unsafe extern "C" fn pr_start1(_h: Raw, _l: *const c_char) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn pr_start2(_h: Raw, _l: *const c_char, _id: *const c_char) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn pr_update(_h: Raw, _p: f64) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn pr_end(_h: Raw) -> OfxStatus {
    ffi::OK
}
pub static PROGRESS_SUITE_V1: ffi::ProgressSuiteV1 = ffi::ProgressSuiteV1 { start: pr_start1, update: pr_update, end: pr_end };
pub static PROGRESS_SUITE_V2: ffi::ProgressSuiteV2 = ffi::ProgressSuiteV2 { start: pr_start2, update: pr_update, end: pr_end };

unsafe extern "C" fn tl_get_time(h: Raw, out: *mut f64) -> OfxStatus {
    guard(|| {
        if out.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let t = get::<Effect>(h, Kind::Effect).and_then(|e| e.with_job(|j| j.time())).unwrap_or(0.0);
        // SAFETY: non-null out-pointer.
        unsafe { *out = t };
        ffi::OK
    })
}
unsafe extern "C" fn tl_goto(_h: Raw, _t: f64) -> OfxStatus {
    ffi::OK
}
unsafe extern "C" fn tl_bounds(h: Raw, a: *mut f64, b: *mut f64) -> OfxStatus {
    guard(|| {
        if a.is_null() || b.is_null() {
            return ffi::ERR_BAD_HANDLE;
        }
        let d = get::<Effect>(h, Kind::Effect).and_then(|e| e.with_job(|j| j.duration())).unwrap_or(0.0);
        // SAFETY: non-null out-pointers.
        unsafe {
            *a = 0.0;
            *b = d;
        }
        ffi::OK
    })
}
pub static TIMELINE_SUITE: ffi::TimeLineSuiteV1 = ffi::TimeLineSuiteV1 { get_time: tl_get_time, goto_time: tl_goto, get_bounds: tl_bounds };

// ---------------------------------------------------------------------------------------------
// The host

unsafe extern "C" fn fetch_suite(_host: Raw, name: *const c_char, version: c_int) -> *const c_void {
    let r = std::panic::catch_unwind(|| {
        // SAFETY: NUL-terminated suite name (contract).
        let Some(n) = (unsafe { cstr(name) }) else { return std::ptr::null() };
        match (n, version) {
            (ffi::PropertySuite, 1) => &super::props::SUITE as *const _ as *const c_void,
            (ffi::ImageEffectSuite, 1) => &IMAGE_EFFECT_SUITE as *const _ as *const c_void,
            (ffi::ParameterSuite, 1) => parameter_suite() as *const _ as *const c_void,
            (ffi::MemorySuite, 1) => &MEMORY_SUITE as *const _ as *const c_void,
            (ffi::MultiThreadSuite, 1) => &MULTI_THREAD_SUITE as *const _ as *const c_void,
            // V2 begins with V1's single function
            (ffi::MessageSuite, 1 | 2) => &MESSAGE_SUITE as *const _ as *const c_void,
            (ffi::ProgressSuite, 1) => &PROGRESS_SUITE_V1 as *const _ as *const c_void,
            (ffi::ProgressSuite, 2) => &PROGRESS_SUITE_V2 as *const _ as *const c_void,
            (ffi::TimeLineSuite, 1) => &TIMELINE_SUITE as *const _ as *const c_void,
            _ => std::ptr::null(),
        }
    });
    r.unwrap_or(std::ptr::null())
}

struct HostBox(Box<ffi::OfxHost>, #[allow(dead_code)] Box<PropSet>);
// SAFETY: the host struct is created once and only read (by plug-ins) afterwards.
unsafe impl Send for HostBox {}
// SAFETY: as above.
unsafe impl Sync for HostBox {}

/// The `OfxHost` handed to every plug-in.
pub fn host() -> *mut ffi::OfxHost {
    static HOST: OnceLock<HostBox> = OnceLock::new();
    let h = HOST.get_or_init(|| {
        let ps = PropSet::new();
        ps.s(ffi::PropType, ffi::TypeImageEffectHost);
        ps.s(ffi::PropName, "com.storytold.FilmCraft");
        ps.s(ffi::PropLabel, "FilmCraft");
        ps.is_(ffi::PropVersion, &[0, 6, 0]);
        ps.s(ffi::PropVersionLabel, "0.6");
        ps.is_(ffi::PropAPIVersion, &[1, 4]);
        ps.i(ffi::IEHostPropIsBackground, 0);
        ps.i(ffi::IEPropSupportsOverlays, 0);
        ps.i(ffi::IEPropSupportsMultiResolution, 1);
        ps.i(ffi::IEPropSupportsTiles, 0);
        ps.i(ffi::IEPropTemporalClipAccess, 1);
        ps.ss(ffi::IEPropSupportedComponents, &[ffi::ComponentRGBA]);
        ps.ss(ffi::IEPropSupportedContexts, &[ffi::ContextFilter, ffi::ContextGeneral, ffi::ContextTransition]);
        ps.ss(ffi::IEPropSupportedPixelDepths, &[ffi::BitDepthFloat, ffi::BitDepthShort, ffi::BitDepthByte]);
        ps.i(ffi::IEPropSupportsMultipleClipDepths, 0);
        ps.i(ffi::IEPropSupportsMultipleClipPARs, 0);
        ps.i(ffi::IEPropSetableFrameRate, 0);
        ps.i(ffi::IEPropSetableFielding, 0);
        ps.i(ffi::IEInstancePropSequentialRender, 0);
        ps.s(ffi::IEPropOpenGLRenderSupported, "false");
        for k in [
            "OfxImageEffectPropCudaRenderSupported",
            "OfxImageEffectPropCudaStreamSupported",
            "OfxImageEffectPropMetalRenderSupported",
            "OfxImageEffectPropOpenCLRenderSupported",
        ] {
            ps.s(k, "false");
        }
        ps.i(ffi::IEPropRenderQualityDraft, 0);
        ps.i("OfxParamHostPropSupportsStrChoiceAnimation", 0);
        ps.s(ffi::IEHostPropNativeOrigin, ffi::HostNativeOriginBottomLeft);
        ps.i(ffi::ParamHostPropSupportsCustomInteract, 0);
        ps.i(ffi::ParamHostPropSupportsStringAnimation, 0);
        ps.i(ffi::ParamHostPropSupportsChoiceAnimation, 0);
        ps.i(ffi::ParamHostPropSupportsBooleanAnimation, 0);
        ps.i(ffi::ParamHostPropSupportsCustomAnimation, 0);
        ps.i(ffi::ParamHostPropSupportsParametricAnimation, 0);
        ps.i(ffi::ParamHostPropSupportsStrChoice, 1);
        ps.i(ffi::ParamHostPropMaxParameters, -1);
        ps.i(ffi::ParamHostPropMaxPages, 0);
        ps.is_(ffi::ParamHostPropPageRowColumnCount, &[0, 0]);
        ps.p(ffi::PropHostOSHandle, std::ptr::null_mut());
        let raw = ps.raw();
        HostBox(Box::new(ffi::OfxHost { host: raw, fetch_suite }), ps)
    });
    &*h.0 as *const ffi::OfxHost as *mut ffi::OfxHost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suites_are_offered_by_name_and_version() {
        let h = host();
        // SAFETY: our own host struct.
        let f = unsafe { (*h).fetch_suite };
        for (n, v) in [("OfxPropertySuite", 1), ("OfxImageEffectSuite", 1), ("OfxParameterSuite", 1), ("OfxMultiThreadSuite", 1), ("OfxMessageSuite", 2)] {
            let c = CString::new(n).unwrap();
            // SAFETY: valid name string.
            assert!(!unsafe { f(std::ptr::null_mut(), c.as_ptr(), v) }.is_null(), "{n}");
        }
        // SAFETY: as above.
        assert!(unsafe { f(std::ptr::null_mut(), c"OfxInteractSuite".as_ptr(), 1) }.is_null());
        // SAFETY: a null name is refused, not read
        assert!(unsafe { f(std::ptr::null_mut(), std::ptr::null(), 1) }.is_null());
    }

    #[test]
    fn parameter_values_round_trip_and_bad_handles_are_refused() {
        let set = ParamSet::new();
        let mut props: Raw = std::ptr::null_mut();
        // SAFETY: valid strings and out-pointer.
        assert_eq!(unsafe { p_define(set.raw(), c"OfxParamTypeRGBA".as_ptr(), c"tint".as_ptr(), &mut props) }, ffi::OK);
        // SAFETY: as above.
        assert_eq!(unsafe { p_define(set.raw(), c"OfxParamTypeRGBA".as_ptr(), c"tint".as_ptr(), &mut props) }, ffi::ERR_EXISTS);
        let h = set.find("tint").unwrap();
        // SAFETY: test-only access to our own parameter.
        let p = get::<Param>(h, Kind::Param).unwrap();
        *p.val.lock().unwrap() = PV::D(vec![0.1, 0.2, 0.3, 1.0]);
        let mut c = [0.0f64; 4];
        let ptrs = c.as_mut_ptr();
        // SAFETY: four double out-pointers for an RGBA parameter.
        let st = unsafe { p_get_value(h, ptrs as Raw, ptrs.add(1) as Raw, ptrs.add(2) as Raw, ptrs.add(3) as Raw) };
        assert_eq!(st, ffi::OK);
        assert_eq!(c, [0.1, 0.2, 0.3, 1.0]);
        // SAFETY: a bogus handle is refused before anything is written
        assert_eq!(unsafe { p_get_value(0x10 as Raw, ptrs as Raw, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, ffi::ERR_BAD_HANDLE);
        // a missing out-pointer is refused
        // SAFETY: null pointers are checked.
        assert_eq!(unsafe { p_get_value(h, ptrs as Raw, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, ffi::ERR_BAD_HANDLE);
    }

    #[test]
    fn recursive_mutexes_lock_twice_on_one_thread() {
        let mut m: Raw = std::ptr::null_mut();
        // SAFETY: valid out-pointer; handles from create.
        unsafe {
            assert_eq!(t_mutex_create(&mut m, 0), ffi::OK);
            assert_eq!(t_mutex_lock(m), ffi::OK);
            assert_eq!(t_mutex_lock(m), ffi::OK);
            let mm = m as usize;
            let other = std::thread::spawn(move || t_mutex_try_lock(mm as Raw)).join().unwrap();
            assert_eq!(other, ffi::FAILED);
            assert_eq!(t_mutex_unlock(m), ffi::OK);
            assert_eq!(t_mutex_unlock(m), ffi::OK);
            assert_eq!(t_mutex_destroy(m), ffi::OK);
            assert_eq!(t_mutex_destroy(m), ffi::ERR_BAD_HANDLE);
        }
    }

    #[test]
    fn plugin_memory_is_tracked() {
        let mut p: Raw = std::ptr::null_mut();
        // SAFETY: valid out-pointer; frees of our blocks.
        unsafe {
            assert_eq!(m_alloc(std::ptr::null_mut(), 1000, &mut p), ffi::OK);
            assert_eq!(p as usize % 64, 0);
            std::ptr::write_bytes(p as *mut u8, 7, 1000);
            assert_eq!(m_free(p), ffi::OK);
            assert_eq!(m_free(p), ffi::ERR_BAD_HANDLE);
        }
    }
}
