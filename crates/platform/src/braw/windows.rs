//! Blackmagic RAW on Windows: Blackmagic's installed runtime, driven through COM.
//!
//! Nothing of Blackmagic's is compiled in. At the first `.braw` import:
//!
//! 1. **Find the runtime.** `FILMCRAFT_BRAW_RUNTIME` (a DLL or its folder), the COM classes the
//!    Blackmagic RAW installer registered (`HKEY_CLASSES_ROOT\CLSID\…\InprocServer32`), then
//!    `BlackmagicRawAPI.dll` under `%ProgramFiles%\Blackmagic Design`.
//! 2. **Read its type information.** The runtime DLL describes its interfaces (a COM type
//!    library). Every interface ID, every method's vtable slot and every parameter type FilmCraft
//!    uses is taken from there, by name, and checked against what this module passes: the
//!    interfaces have changed between SDK versions (methods were added mid-interface), so slots
//!    are never assumed. A method that is missing or has different parameters is reported, not
//!    called.
//! 3. **Create the factory** without COM activation (no apartment rules, no proxies): the DLL's
//!    `CreateBlackmagicRawFactoryInstance` export when it has one, else its class factory
//!    (`DllGetClassObject`), then `QueryInterface` for the factory interface the type library
//!    describes, so the object and the slots always come from the same runtime.
//!
//! Decoding follows the SDK's documented flow: one codec per clip with our callback object, the
//! CPU pipeline; read job → frame (resource format RGB 16-bit, resolution scale) → decode and
//! process job → processed image, each step completed with `FlushJobs`. Calls on a clip are
//! serialised by a mutex; the SDK runs its own worker threads.
//!
//! [`check_runtime`] reports all of this in words (the CLI's `braw-check`), which is how a user's machine
//! tells us what it has.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, IClassFactory, ITypeInfo, ITypeLib, TKIND_ALIAS, TKIND_COCLASS, TKIND_ENUM, TKIND_INTERFACE, TYPEDESC,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW};
use windows::Win32::System::Ole::{LoadTypeLibEx, REGKIND_NONE};
use windows::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW};
use windows::Win32::System::Variant::{
    VARENUM, VT_BSTR, VT_ERROR, VT_HRESULT, VT_I4, VT_I8, VT_INT, VT_PTR, VT_R4, VT_UI4, VT_UI8, VT_UINT, VT_UNKNOWN, VT_USERDEFINED,
};
use windows::core::{BSTR, GUID, HSTRING, Interface, PCWSTR, s};

use super::{AudioInfo, Changes, Clip, ClipInfo, FORMAT_RGB_U16, Kind, Picture, Runtime, Scale, Setting, fourcc, pcm_to_f32, rate_from_f32, to_picture};
use serde_json::{Value, json};

type Raw = *mut c_void;

const PIPELINE_CPU: u32 = fourcc(b"cpub");

fn scale_code(s: Scale) -> u32 {
    match s {
        Scale::Full => fourcc(b"full"),
        Scale::Half => fourcc(b"half"),
        Scale::Quarter => fourcc(b"qrtr"),
        Scale::Eighth => fourcc(b"eith"),
    }
}

fn hr_text(hr: i32) -> String {
    format!("HRESULT 0x{:08X}", hr as u32)
}

// ---------------------------------------------------------------------------------------------
// Type information

/// A parameter as the calling convention sees it (x64 / ARM64: what matters is pointer, 32-bit
/// integer, 64-bit integer or float).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arg {
    Ptr,
    W32,
    W64,
    F32,
    Other(u16),
}

#[derive(Clone, Debug)]
struct Method {
    slot: usize,
    args: Vec<Arg>,
    /// Returns HRESULT (methods) or nothing (callbacks).
    returns_hresult: bool,
}

#[derive(Clone, Debug, Default)]
struct Iface {
    iid: GUID,
    /// Vtable size in slots (including IUnknown's three).
    slots: usize,
    methods: HashMap<String, Method>,
}

impl Iface {
    /// The slot of `name`, if its parameters are exactly `args` (and it returns HRESULT when
    /// `hresult`).
    fn slot(&self, iface: &str, name: &str, args: &[Arg], hresult: bool) -> Result<usize, String> {
        let m = self.methods.get(name).ok_or_else(|| format!("{iface}::{name} is not in this runtime's type information"))?;
        if m.args != args || m.returns_hresult != hresult {
            return Err(format!("{iface}::{name} has parameters {:?} in this runtime, FilmCraft expects {args:?}", m.args));
        }
        Ok(m.slot)
    }
}

fn bstr_string(b: &BSTR) -> String {
    String::from_utf16_lossy(b)
}

/// Classify a type, resolving typedefs and enums (bounded: type libraries can be cyclic).
fn classify(ti: &ITypeInfo, td: &TYPEDESC, depth: u32) -> Arg {
    let vt: VARENUM = td.vt;
    match vt {
        VT_PTR | VT_BSTR | VT_UNKNOWN => Arg::Ptr,
        VT_I4 | VT_UI4 | VT_INT | VT_UINT | VT_HRESULT | VT_ERROR => Arg::W32,
        VT_I8 | VT_UI8 => Arg::W64,
        VT_R4 => Arg::F32,
        VT_USERDEFINED if depth < 8 => {
            // SAFETY: for VT_USERDEFINED the union holds `hreftype` (TYPEDESC contract).
            let href = unsafe { td.Anonymous.hreftype };
            // SAFETY: plain COM call on a live ITypeInfo.
            let Ok(r) = (unsafe { ti.GetRefTypeInfo(href) }) else { return Arg::Other(vt.0) };
            // SAFETY: plain COM call; the attribute block is released below.
            let Ok(attr) = (unsafe { r.GetTypeAttr() }) else { return Arg::Other(vt.0) };
            // SAFETY: GetTypeAttr succeeded, so `attr` points to a valid TYPEATTR until released.
            let a = unsafe { &*attr };
            let out = if a.typekind == TKIND_ENUM {
                Arg::W32
            } else if a.typekind == TKIND_ALIAS {
                classify(&r, &a.tdescAlias, depth + 1)
            } else {
                Arg::Other(vt.0)
            };
            // SAFETY: releases the block GetTypeAttr returned, once.
            unsafe { r.ReleaseTypeAttr(attr) };
            out
        }
        _ => Arg::Other(vt.0),
    }
}

fn read_iface(ti: &ITypeInfo) -> Result<Iface, String> {
    // SAFETY: plain COM call; released at the end of this function.
    let attr = unsafe { ti.GetTypeAttr() }.map_err(|e| e.to_string())?;
    // SAFETY: GetTypeAttr succeeded; valid until ReleaseTypeAttr.
    let a = unsafe { &*attr };
    let ptr = std::mem::size_of::<usize>();
    let mut out = Iface { iid: a.guid, slots: a.cbSizeVft as usize / ptr, methods: HashMap::new() };
    for i in 0..a.cFuncs as u32 {
        // SAFETY: index below cFuncs; released below.
        let Ok(fd) = (unsafe { ti.GetFuncDesc(i) }) else { continue };
        // SAFETY: GetFuncDesc succeeded; valid until ReleaseFuncDesc.
        let f = unsafe { &*fd };
        let mut name = BSTR::new();
        // SAFETY: out-pointer to a local BSTR; the other outputs are not requested (null is allowed).
        let named = unsafe { ti.GetDocumentation(f.memid, Some(&mut name as *mut BSTR), None, std::ptr::null_mut(), None) }.is_ok();
        let n = f.cParams.max(0) as usize;
        let args = (0..n)
            .map(|k| {
                // SAFETY: lprgelemdescParam holds cParams elements.
                let ed = unsafe { &*f.lprgelemdescParam.add(k) };
                classify(ti, &ed.tdesc, 0)
            })
            .collect();
        let ret = f.elemdescFunc.tdesc.vt;
        if named && f.oVft >= 0 {
            out.methods.insert(bstr_string(&name), Method { slot: f.oVft as usize / ptr, args, returns_hresult: ret == VT_HRESULT });
        }
        // SAFETY: releases the block GetFuncDesc returned, once.
        unsafe { ti.ReleaseFuncDesc(fd) };
    }
    // SAFETY: releases the block GetTypeAttr returned, once.
    unsafe { ti.ReleaseTypeAttr(attr) };
    Ok(out)
}

/// Everything read from the type library.
#[derive(Debug, Default)]
struct TypeInfo {
    ifaces: HashMap<String, Iface>,
    /// Coclasses: name → (CLSID, names of the interfaces they implement).
    classes: HashMap<String, (GUID, Vec<String>)>,
}

fn read_typelib(tl: &ITypeLib) -> TypeInfo {
    let mut out = TypeInfo::default();
    // SAFETY: plain COM call.
    let count = unsafe { tl.GetTypeInfoCount() };
    for i in 0..count {
        // SAFETY: index below GetTypeInfoCount.
        let Ok(kind) = (unsafe { tl.GetTypeInfoType(i) }) else { continue };
        // SAFETY: index below GetTypeInfoCount.
        let Ok(ti) = (unsafe { tl.GetTypeInfo(i) }) else { continue };
        let mut name = BSTR::new();
        // SAFETY: out-pointer to a local BSTR (MEMBERID_NIL = -1 names the type itself).
        if unsafe { ti.GetDocumentation(-1, Some(&mut name as *mut BSTR), None, std::ptr::null_mut(), None) }.is_err() {
            continue;
        }
        let name = bstr_string(&name);
        if kind == TKIND_INTERFACE {
            if let Ok(iface) = read_iface(&ti) {
                out.ifaces.insert(name, iface);
            }
        } else if kind == TKIND_COCLASS {
            // SAFETY: plain COM call; released below.
            let Ok(attr) = (unsafe { ti.GetTypeAttr() }) else { continue };
            // SAFETY: valid until ReleaseTypeAttr.
            let (clsid, n) = unsafe { ((*attr).guid, (*attr).cImplTypes) };
            let mut impls = Vec::new();
            for k in 0..n as u32 {
                // SAFETY: index below cImplTypes.
                let Ok(href) = (unsafe { ti.GetRefTypeOfImplType(k) }) else { continue };
                // SAFETY: href from GetRefTypeOfImplType.
                let Ok(r) = (unsafe { ti.GetRefTypeInfo(href) }) else { continue };
                let mut iname = BSTR::new();
                // SAFETY: out-pointer to a local BSTR.
                if unsafe { r.GetDocumentation(-1, Some(&mut iname as *mut BSTR), None, std::ptr::null_mut(), None) }.is_ok() {
                    impls.push(bstr_string(&iname));
                }
            }
            // SAFETY: releases the block GetTypeAttr returned, once.
            unsafe { ti.ReleaseTypeAttr(attr) };
            out.classes.insert(name, (clsid, impls));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Finding the runtime

fn reg_string(key: HKEY, sub: &str, value: Option<&str>) -> Option<String> {
    let sub = HSTRING::from(sub);
    let val = value.map(HSTRING::from);
    let vp = val.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer and length describe `buf`; strings are NUL-terminated HSTRINGs alive for the call.
    let r = unsafe { RegGetValueW(key, PCWSTR(sub.as_ptr()), vp, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len as *mut u32)) };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(buf.get(..n)?);
    Some(s.trim_end_matches('\0').to_string())
}

fn parse_guid(s: &str) -> Option<GUID> {
    GUID::try_from(s.trim().trim_start_matches('{').trim_end_matches('}')).ok()
}

/// COM classes whose name mentions Blackmagic RAW: (CLSID, name, DLL).
fn registered_classes() -> Vec<(GUID, String, PathBuf)> {
    let mut out = Vec::new();
    let mut key = HKEY::default();
    // SAFETY: opens a read-only handle into `key`, closed below.
    if unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, windows::core::w!("CLSID"), None, KEY_READ, &mut key) }.is_err() {
        return out;
    }
    let mut name = [0u16; 128];
    for i in 0..200_000u32 {
        let mut len = name.len() as u32;
        // SAFETY: buffer and length describe `name`.
        let r = unsafe { RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if r.is_err() {
            break;
        }
        let Some(id) = name.get(..len as usize).map(String::from_utf16_lossy) else { continue };
        let Some(label) = reg_string(key, &id, None) else { continue };
        if !label.to_ascii_lowercase().contains("blackmagicraw") && !label.to_ascii_lowercase().contains("blackmagic raw") {
            continue;
        }
        let (Some(guid), Some(dll)) = (parse_guid(&id), reg_string(key, &format!("{id}\\InprocServer32"), None)) else { continue };
        out.push((guid, label, PathBuf::from(dll)));
    }
    // SAFETY: closes the handle opened above.
    let _ = unsafe { RegCloseKey(key) };
    out
}

fn find_dlls(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 5 || out.len() > 16 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            find_dlls(&p, depth + 1, out);
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("BlackmagicRawAPI.dll")) {
            out.push(p);
        }
    }
}

/// Candidate runtime DLLs, most specific first, without duplicates.
fn candidates(notes: &mut Vec<String>) -> (Vec<PathBuf>, Vec<(GUID, String, PathBuf)>) {
    let mut dlls = Vec::new();
    if let Some(v) = std::env::var_os("FILMCRAFT_BRAW_RUNTIME") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            find_dlls(&p, 0, &mut dlls);
        } else {
            dlls.push(p);
        }
        notes.push(format!("FILMCRAFT_BRAW_RUNTIME → {:?}", dlls));
    }
    let classes = registered_classes();
    for (g, label, dll) in &classes {
        notes.push(format!("registered class {label} {g:?} → {}", dll.display()));
        dlls.push(dll.clone());
    }
    // the Blackmagic RAW installer's folder, and the copy the Premiere Pro plug-in may carry
    for (var, sub) in [
        ("ProgramFiles", "Blackmagic Design"),
        ("ProgramW6432", "Blackmagic Design"),
        ("CommonProgramFiles", "Blackmagic Design"),
        ("CommonProgramFiles", "Adobe"),
    ] {
        if let Some(root) = std::env::var_os(var) {
            find_dlls(&PathBuf::from(root).join(sub), 0, &mut dlls);
        }
    }
    let mut seen = Vec::new();
    dlls.retain(|p| {
        let k = p.to_string_lossy().to_ascii_lowercase();
        let new = !seen.contains(&k) && p.is_file();
        seen.push(k);
        new
    });
    (dlls, classes)
}

// ---------------------------------------------------------------------------------------------
// Raw COM calls

/// The function in vtable slot `slot` of `obj`.
///
/// # Safety
/// `obj` is a live COM interface pointer whose vtable has more than `slot` entries.
unsafe fn vfn(obj: Raw, slot: usize) -> *const c_void {
    // SAFETY: per the contract, `obj` points at a vtable pointer, and the vtable has `slot`.
    unsafe {
        let vt = *(obj as *const *const *const c_void);
        *vt.add(slot)
    }
}

macro_rules! vcall {
    ($obj:expr, $slot:expr, fn($($t:ty),*) -> $r:ty, $($a:expr),*) => {{
        // SAFETY: the slot was looked up by name in the runtime's own type information and its
        // parameter kinds checked against these argument types (Api::load), and `$obj` is a live
        // interface pointer of that interface.
        unsafe {
            let f = std::mem::transmute::<*const c_void, unsafe extern "system" fn(Raw $(, $t)*) -> $r>(vfn($obj, $slot));
            f($obj $(, $a)*)
        }
    }};
}

/// An owned interface pointer (released on drop).
struct Com(Raw);
// SAFETY: the Blackmagic RAW runtime is free-threaded (its objects are used from its own worker
// threads); FilmCraft serialises its calls per clip with a mutex.
unsafe impl Send for Com {}
// SAFETY: as above.
unsafe impl Sync for Com {}
impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = vcall!(self.0, 2, fn() -> u32,);
        }
    }
}

fn check(hr: i32, what: &str) -> Result<(), String> {
    if hr < 0 { Err(format!("{what} failed ({})", hr_text(hr))) } else { Ok(()) }
}

fn com_init() {
    thread_local!(static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    INIT.with(|i| {
        if !i.get() {
            // SAFETY: initialises COM for this thread; an existing apartment (RPC_E_CHANGED_MODE) is fine.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            i.set(true);
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The interface table: every slot FilmCraft calls, verified

use Arg::{Ptr as P, W32, W64};

#[derive(Debug)]
struct Api {
    factory_iid: GUID,
    f_create_codec: usize,
    c_open_clip: usize,
    c_set_callback: usize,
    c_prepare_pipeline: usize,
    c_flush: usize,
    clip_width: usize,
    clip_height: usize,
    clip_rate: usize,
    clip_count: usize,
    clip_read_job: usize,
    frame_scale: usize,
    frame_format: usize,
    frame_process_job: usize,
    job_submit: usize,
    img_width: usize,
    img_height: usize,
    img_resource: usize,
    img_size: usize,
    img_format: usize,
    cb_iid: GUID,
    cb_slots: usize,
    cb_read: usize,
    cb_process: usize,
    audio: Option<AudioApi>,
    /// Clip and frame processing attributes (Source Settings), when the runtime describes them.
    proc: Option<ProcApi>,
}

/// One of the two processing-attribute interfaces (clip or frame): the same five methods.
#[derive(Debug)]
struct AttrApi {
    get: usize,
    set: usize,
    range: usize,
    list: usize,
    iso: Option<usize>,
}

#[derive(Debug)]
struct ProcApi {
    clip_clone: usize,
    frame_clone: usize,
    clip: AttrApi,
    frame: AttrApi,
}

#[derive(Debug)]
struct AudioApi {
    iid: GUID,
    bits: usize,
    channels: usize,
    rate: usize,
    count: usize,
    samples: usize,
}

impl Api {
    fn load(ti: &TypeInfo, notes: &mut Vec<String>) -> Result<Api, String> {
        let get = |n: &str| ti.ifaces.get(n).ok_or_else(|| format!("{n} is not in this runtime's type information"));
        let (f, c, cl, fr, j, im, cb) = (
            get("IBlackmagicRawFactory")?,
            get("IBlackmagicRaw")?,
            get("IBlackmagicRawClip")?,
            get("IBlackmagicRawFrame")?,
            get("IBlackmagicRawJob")?,
            get("IBlackmagicRawProcessedImage")?,
            get("IBlackmagicRawCallback")?,
        );
        let api = Api {
            factory_iid: f.iid,
            f_create_codec: f.slot("IBlackmagicRawFactory", "CreateCodec", &[P], true)?,
            c_open_clip: c.slot("IBlackmagicRaw", "OpenClip", &[P, P], true)?,
            c_set_callback: c.slot("IBlackmagicRaw", "SetCallback", &[P], true)?,
            c_prepare_pipeline: c.slot("IBlackmagicRaw", "PreparePipeline", &[W32, P, P, P], true)?,
            c_flush: c.slot("IBlackmagicRaw", "FlushJobs", &[], true)?,
            clip_width: cl.slot("IBlackmagicRawClip", "GetWidth", &[P], true)?,
            clip_height: cl.slot("IBlackmagicRawClip", "GetHeight", &[P], true)?,
            clip_rate: cl.slot("IBlackmagicRawClip", "GetFrameRate", &[P], true)?,
            clip_count: cl.slot("IBlackmagicRawClip", "GetFrameCount", &[P], true)?,
            clip_read_job: cl.slot("IBlackmagicRawClip", "CreateJobReadFrame", &[W64, P], true)?,
            frame_scale: fr.slot("IBlackmagicRawFrame", "SetResolutionScale", &[W32], true)?,
            frame_format: fr.slot("IBlackmagicRawFrame", "SetResourceFormat", &[W32], true)?,
            frame_process_job: fr.slot("IBlackmagicRawFrame", "CreateJobDecodeAndProcessFrame", &[P, P, P], true)?,
            job_submit: j.slot("IBlackmagicRawJob", "Submit", &[], true)?,
            img_width: im.slot("IBlackmagicRawProcessedImage", "GetWidth", &[P], true)?,
            img_height: im.slot("IBlackmagicRawProcessedImage", "GetHeight", &[P], true)?,
            img_resource: im.slot("IBlackmagicRawProcessedImage", "GetResource", &[P], true)?,
            img_size: im.slot("IBlackmagicRawProcessedImage", "GetResourceSizeBytes", &[P], true)?,
            img_format: im.slot("IBlackmagicRawProcessedImage", "GetResourceFormat", &[P], true)?,
            cb_iid: cb.iid,
            cb_slots: cb.slots,
            cb_read: cb.slot("IBlackmagicRawCallback", "ReadComplete", &[P, W32, P], false)?,
            cb_process: cb.slot("IBlackmagicRawCallback", "ProcessComplete", &[P, W32, P], false)?,
            audio: None,
            proc: None,
        };
        if api.cb_slots <= api.cb_read.max(api.cb_process) || api.cb_slots > 256 {
            return Err(format!("IBlackmagicRawCallback has an unexpected size ({} slots)", api.cb_slots));
        }
        // the callback's other methods return nothing, so FilmCraft can answer them with one
        // no-op (x64 / ARM64 callers clean up their own arguments); check that they do
        for (n, m) in &cb.methods {
            if m.returns_hresult {
                return Err(format!("IBlackmagicRawCallback::{n} returns a value in this runtime; FilmCraft only implements void callbacks"));
            }
        }
        let audio = ti.ifaces.get("IBlackmagicRawClipAudio").map(|a| -> Result<AudioApi, String> {
            let n = "IBlackmagicRawClipAudio";
            Ok(AudioApi {
                iid: a.iid,
                bits: a.slot(n, "GetAudioBitDepth", &[P], true)?,
                channels: a.slot(n, "GetAudioChannelCount", &[P], true)?,
                rate: a.slot(n, "GetAudioSampleRate", &[P], true)?,
                count: a.slot(n, "GetAudioSampleCount", &[P], true)?,
                samples: a.slot(n, "GetAudioSamples", &[W64, P, W32, W32, P, P], true)?,
            })
        });
        let attr = |i: &Iface, n: &str, what: &str| -> Result<AttrApi, String> {
            Ok(AttrApi {
                get: i.slot(n, &format!("Get{what}Attribute"), &[W32, P], true)?,
                set: i.slot(n, &format!("Set{what}Attribute"), &[W32, P], true)?,
                range: i.slot(n, &format!("Get{what}AttributeRange"), &[W32, P, P, P], true)?,
                list: i.slot(n, &format!("Get{what}AttributeList"), &[W32, P, P, P], true)?,
                iso: i.slot(n, "GetISOList", &[P, P, P], true).ok(),
            })
        };
        let proc = (|| -> Result<ProcApi, String> {
            let (ca, fa) = (get("IBlackmagicRawClipProcessingAttributes")?, get("IBlackmagicRawFrameProcessingAttributes")?);
            Ok(ProcApi {
                clip_clone: cl.slot("IBlackmagicRawClip", "CloneClipProcessingAttributes", &[P], true)?,
                frame_clone: fr.slot("IBlackmagicRawFrame", "CloneFrameProcessingAttributes", &[P], true)?,
                clip: attr(ca, "IBlackmagicRawClipProcessingAttributes", "Clip")?,
                frame: attr(fa, "IBlackmagicRawFrameProcessingAttributes", "Frame")?,
            })
        })();
        let api = match proc {
            Ok(pr) => Api { proc: Some(pr), ..api },
            Err(e) => {
                notes.push(format!("source settings off: {e}"));
                api
            }
        };
        let api = match audio {
            Some(Ok(a)) => Api { audio: Some(a), ..api },
            Some(Err(e)) => {
                notes.push(format!("audio off: {e}"));
                api
            }
            None => {
                notes.push("audio off: IBlackmagicRawClipAudio is not in the type information".into());
                api
            }
        };
        Ok(api)
    }
}

// ---------------------------------------------------------------------------------------------
// Our callback object

#[derive(Default)]
struct CbState {
    /// ReadComplete: result and the frame (AddRef'd, released by the taker).
    read: Option<(i32, usize)>,
    /// ProcessComplete: result and the copied picture (width, height, format, bytes).
    image: Option<Result<(u32, u32, u32, Vec<u8>), String>>,
}

#[repr(C)]
struct Callback {
    /// Must stay the first field: the object pointer the runtime holds points at it.
    vtbl: *const *const c_void,
    table: Box<[*const c_void]>,
    img: [usize; 5], // width, height, resource, size, format slots
    /// The callback interface this object implements (from the type information): the only one,
    /// besides IUnknown, it says yes to, so a runtime asking for a newer callback version (with
    /// methods this table lacks) is told no rather than calling past its end.
    iid: GUID,
    state: Mutex<CbState>,
}

unsafe extern "system" fn cb_query(this: Raw, iid: *const GUID, out: *mut Raw) -> i32 {
    if out.is_null() {
        return 0x8000_4003_u32 as i32; // E_POINTER
    }
    // SAFETY: `this` is our Callback; `iid` is the caller's interface ID (null-checked).
    let ok = !iid.is_null() && unsafe { *iid == (*(this as *const Callback)).iid || *iid == windows::core::IUnknown::IID };
    // SAFETY: `out` is a non-null out-pointer from the caller.
    unsafe { *out = if ok { this } else { std::ptr::null_mut() } };
    if ok { 0 } else { 0x8000_4002_u32 as i32 } // S_OK / E_NOINTERFACE
}
unsafe extern "system" fn cb_addref(_this: Raw) -> u32 {
    1 // owned by FilmCraft for the codec's lifetime
}
unsafe extern "system" fn cb_noop(_this: Raw) {}

unsafe extern "system" fn cb_read(this: Raw, _job: Raw, hr: i32, frame: Raw) {
    // SAFETY: `this` is the Callback we registered (its first field is the vtable pointer).
    let me = unsafe { &*(this as *const Callback) };
    if !frame.is_null() {
        let _ = vcall!(frame, 1, fn() -> u32,);
    }
    let mut st = me.state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, old)) = st.read.replace((hr, frame as usize))
        && old != 0
    {
        let _ = vcall!(old as Raw, 2, fn() -> u32,);
    }
}

unsafe extern "system" fn cb_process(this: Raw, _job: Raw, hr: i32, image: Raw) {
    // SAFETY: as in cb_read.
    let me = unsafe { &*(this as *const Callback) };
    let result = if hr < 0 || image.is_null() {
        Err(format!("decode failed ({})", hr_text(hr)))
    } else {
        let [sw, sh, sres, ssize, sfmt] = me.img;
        let (mut w, mut h, mut size, mut fmt, mut res) = (0u32, 0u32, 0u32, 0u32, std::ptr::null_mut::<c_void>());
        let ok = vcall!(image, sw, fn(*mut u32) -> i32, &mut w) >= 0
            && vcall!(image, sh, fn(*mut u32) -> i32, &mut h) >= 0
            && vcall!(image, ssize, fn(*mut u32) -> i32, &mut size) >= 0
            && vcall!(image, sfmt, fn(*mut u32) -> i32, &mut fmt) >= 0
            && vcall!(image, sres, fn(*mut Raw) -> i32, &mut res) >= 0;
        if !ok || res.is_null() || size == 0 || size > 1 << 31 {
            Err("the runtime returned no picture".into())
        } else {
            // SAFETY: GetResource returned a CPU buffer of GetResourceSizeBytes bytes, valid
            // while the processed image is (for the duration of this callback).
            let bytes = unsafe { std::slice::from_raw_parts(res as *const u8, size as usize) }.to_vec();
            Ok((w, h, fmt, bytes))
        }
    };
    me.state.lock().unwrap_or_else(PoisonError::into_inner).image = Some(result);
}

impl Callback {
    fn new(api: &Api) -> Box<Callback> {
        let mut table = vec![cb_noop as *const c_void; api.cb_slots].into_boxed_slice();
        let set = |t: &mut [*const c_void], i: usize, f: *const c_void| {
            if let Some(s) = t.get_mut(i) {
                *s = f;
            }
        };
        set(&mut table, 0, cb_query as *const c_void);
        set(&mut table, 1, cb_addref as *const c_void);
        set(&mut table, 2, cb_addref as *const c_void);
        set(&mut table, api.cb_read, cb_read as *const c_void);
        set(&mut table, api.cb_process, cb_process as *const c_void);
        let mut cb = Box::new(Callback {
            vtbl: std::ptr::null(),
            table,
            img: [api.img_width, api.img_height, api.img_resource, api.img_size, api.img_format],
            iid: api.cb_iid,
            state: Mutex::default(),
        });
        cb.vtbl = cb.table.as_ptr();
        cb
    }
    fn as_raw(&self) -> Raw {
        self as *const Callback as Raw
    }
}

// SAFETY: the vtable pointer points into `table`, owned by the same box; state is behind a mutex.
unsafe impl Send for Callback {}
// SAFETY: as above.
unsafe impl Sync for Callback {}

// ---------------------------------------------------------------------------------------------
// Runtime and clips

struct Loaded {
    api: Api,
    factory: Com,
    dll: PathBuf,
    notes: Vec<String>,
}

fn load() -> Result<Loaded, String> {
    com_init();
    let mut notes = Vec::new();
    let (dlls, classes) = candidates(&mut notes);
    if dlls.is_empty() {
        return Err("the Blackmagic RAW runtime (BlackmagicRawAPI.dll) was not found. Install Blackmagic RAW from \
                    blackmagicdesign.com (the installer that adds the Premiere Pro plug-in), or set FILMCRAFT_BRAW_RUNTIME \
                    to its folder"
            .into());
    }
    let mut errors = Vec::new();
    for dll in &dlls {
        match load_from(dll, &classes, &mut notes) {
            Ok((api, factory)) => return Ok(Loaded { api, factory, dll: dll.clone(), notes }),
            Err(e) => errors.push(format!("{}: {e}", dll.display())),
        }
    }
    Err(format!("the Blackmagic RAW runtime could not be used: {}", errors.join("; ")))
}

fn load_from(dll: &Path, classes: &[(GUID, String, PathBuf)], notes: &mut Vec<String>) -> Result<(Api, Com), String> {
    let wide = HSTRING::from(dll.as_os_str());
    // SAFETY: loads a DLL by absolute path, searching its own folder for its dependencies. It is
    // never unloaded: the runtime's threads may outlive any one clip.
    let module: HMODULE = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) }
        .map_err(|e| format!("could not load it ({e})"))?;
    // SAFETY: reads the type library resource of the file; nothing is registered.
    let tl = unsafe { LoadTypeLibEx(PCWSTR(wide.as_ptr()), REGKIND_NONE) }.map_err(|e| format!("it has no type information ({e})"))?;
    let ti = read_typelib(&tl);
    notes.push(format!("{}: {} interfaces, {} classes in its type information", dll.display(), ti.ifaces.len(), ti.classes.len()));
    let api = Api::load(&ti, notes)?;
    let factory = create_factory(module, &api, &ti, classes, dll, notes)?;
    Ok((api, factory))
}

fn query(obj: Raw, iid: &GUID) -> Result<Com, String> {
    let mut out: Raw = std::ptr::null_mut();
    let hr = vcall!(obj, 0, fn(*const GUID, *mut Raw) -> i32, iid, &mut out);
    check(hr, "QueryInterface")?;
    if out.is_null() { Err("QueryInterface returned nothing".into()) } else { Ok(Com(out)) }
}

fn create_factory(module: HMODULE, api: &Api, ti: &TypeInfo, classes: &[(GUID, String, PathBuf)], dll: &Path, notes: &mut Vec<String>) -> Result<Com, String> {
    // 1. the exported entry point (as on macOS and Linux)
    // SAFETY: looks up an export by NUL-terminated name.
    if let Some(f) = unsafe { GetProcAddress(module, s!("CreateBlackmagicRawFactoryInstance")) } {
        // SAFETY: the export takes no arguments and returns an interface pointer (SDK manual).
        let create = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn() -> Raw>(f) };
        // SAFETY: as above.
        let raw = unsafe { create() };
        if !raw.is_null() {
            let first = Com(raw);
            notes.push("factory: CreateBlackmagicRawFactoryInstance".into());
            return query(first.0, &api.factory_iid);
        }
    }
    // 2. the class factory, for the coclass implementing IBlackmagicRawFactory
    let mut clsids: Vec<GUID> = ti.classes.values().filter(|(_, impls)| impls.iter().any(|i| i == "IBlackmagicRawFactory")).map(|(g, _)| *g).collect();
    for (g, _, d) in classes {
        if d.to_string_lossy().eq_ignore_ascii_case(&dll.to_string_lossy()) && !clsids.contains(g) {
            clsids.push(*g);
        }
    }
    // SAFETY: looks up an export by NUL-terminated name.
    let Some(get) = (unsafe { GetProcAddress(module, s!("DllGetClassObject")) }) else {
        return Err("it exports neither CreateBlackmagicRawFactoryInstance nor DllGetClassObject".into());
    };
    // SAFETY: DllGetClassObject's documented signature.
    let get = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(*const GUID, *const GUID, *mut Raw) -> i32>(get) };
    let mut last = String::from("no factory class found");
    for clsid in clsids {
        let mut cf: Raw = std::ptr::null_mut();
        // SAFETY: valid GUID pointers and out-pointer.
        let hr = unsafe { get(&clsid, &IClassFactory::IID, &mut cf) };
        if hr < 0 || cf.is_null() {
            last = format!("DllGetClassObject {clsid:?}: {}", hr_text(hr));
            continue;
        }
        let cf = Com(cf);
        let mut out: Raw = std::ptr::null_mut();
        // IClassFactory::CreateInstance is slot 3 (IUnknown, then CreateInstance, LockServer)
        let hr = vcall!(cf.0, 3, fn(Raw, *const GUID, *mut Raw) -> i32, std::ptr::null_mut(), &api.factory_iid, &mut out);
        if hr >= 0 && !out.is_null() {
            notes.push(format!("factory: class {clsid:?}"));
            return Ok(Com(out));
        }
        last = format!("CreateInstance {clsid:?}: {}", hr_text(hr));
    }
    Err(last)
}

/// The Windows [`Runtime`]: loads Blackmagic's runtime at the first use.
#[derive(Default)]
pub struct WinRuntime {
    loaded: OnceLock<Result<Arc<Loaded>, String>>,
}

impl WinRuntime {
    fn get(&self) -> Result<Arc<Loaded>, String> {
        self.loaded.get_or_init(|| load().map(Arc::new)).clone()
    }
}

impl Runtime for WinRuntime {
    fn name(&self) -> String {
        "Blackmagic RAW runtime".into()
    }
    fn open(&self, path: &Path) -> Result<Arc<dyn Clip>, String> {
        let rt = self.get()?;
        WinClip::open(rt, path).map(|c| Arc::new(c) as Arc<dyn Clip>)
    }
}

struct Inner {
    // drop order: clip and audio, then codec (after detaching the callback), then the callback
    audio: Option<Com>,
    clip: Com,
    codec: Com,
    cb: Box<Callback>,
}

struct WinClip {
    rt: Arc<Loaded>,
    info: ClipInfo,
    inner: Mutex<Inner>,
    /// Source Settings read from the runtime at first use (empty: none).
    settings: OnceLock<Vec<(Setting, Meta)>>,
}

impl Drop for WinClip {
    fn drop(&mut self) {
        com_init();
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        let api = &self.rt.api;
        let _ = vcall!(inner.codec.0, api.c_flush, fn() -> i32,);
        let _ = vcall!(inner.codec.0, api.c_set_callback, fn(Raw) -> i32, std::ptr::null_mut());
        let mut st = inner.cb.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, f)) = st.read.take()
            && f != 0
        {
            let _ = vcall!(f as Raw, 2, fn() -> u32,);
        }
    }
}

impl WinClip {
    fn open(rt: Arc<Loaded>, path: &Path) -> Result<WinClip, String> {
        com_init();
        let api = &rt.api;
        // created before the codec so that, on an early return, the codec is released first
        let cb = Callback::new(api);
        let mut codec: Raw = std::ptr::null_mut();
        check(vcall!(rt.factory.0, api.f_create_codec, fn(*mut Raw) -> i32, &mut codec), "CreateCodec")?;
        if codec.is_null() {
            return Err("CreateCodec returned nothing".into());
        }
        let codec = Com(codec);
        check(vcall!(codec.0, api.c_set_callback, fn(Raw) -> i32, cb.as_raw()), "SetCallback")?;
        let null = std::ptr::null_mut::<c_void>();
        check(vcall!(codec.0, api.c_prepare_pipeline, fn(u32, Raw, Raw, Raw) -> i32, PIPELINE_CPU, null, null, null), "PreparePipeline (CPU)")?;
        let _ = vcall!(codec.0, api.c_flush, fn() -> i32,);
        let name = BSTR::from(path.to_string_lossy().as_ref());
        let mut clip: Raw = std::ptr::null_mut();
        check(vcall!(codec.0, api.c_open_clip, fn(*const u16, *mut Raw) -> i32, name.as_ptr(), &mut clip), "OpenClip")?;
        if clip.is_null() {
            return Err("OpenClip returned nothing".into());
        }
        let clip = Com(clip);
        let (mut w, mut h, mut rate, mut count) = (0u32, 0u32, 0f32, 0u64);
        check(vcall!(clip.0, api.clip_width, fn(*mut u32) -> i32, &mut w), "GetWidth")?;
        check(vcall!(clip.0, api.clip_height, fn(*mut u32) -> i32, &mut h), "GetHeight")?;
        check(vcall!(clip.0, api.clip_rate, fn(*mut f32) -> i32, &mut rate), "GetFrameRate")?;
        check(vcall!(clip.0, api.clip_count, fn(*mut u64) -> i32, &mut count), "GetFrameCount")?;
        let (audio, audio_info) = match &api.audio {
            Some(a) => match query(clip.0, &a.iid) {
                Ok(au) => {
                    let (mut bits, mut ch, mut sr, mut n) = (0u32, 0u32, 0u32, 0u64);
                    let ok = vcall!(au.0, a.bits, fn(*mut u32) -> i32, &mut bits) >= 0
                        && vcall!(au.0, a.channels, fn(*mut u32) -> i32, &mut ch) >= 0
                        && vcall!(au.0, a.rate, fn(*mut u32) -> i32, &mut sr) >= 0
                        && vcall!(au.0, a.count, fn(*mut u64) -> i32, &mut n) >= 0;
                    if ok && matches!(bits, 16 | 24 | 32) && ch > 0 && sr > 0 && n > 0 {
                        (Some(au), Some(AudioInfo { sample_rate: sr, channels: ch, bits_per_sample: bits, sample_count: n }))
                    } else {
                        (None, None)
                    }
                }
                Err(_) => (None, None), // a clip without sound
            },
            None => (None, None),
        };
        let info = ClipInfo { width: w, height: h, frame_rate: rate_from_f32(rate), frame_count: count, audio: audio_info, camera: None, start_timecode: None };
        Ok(WinClip { rt: rt.clone(), info, inner: Mutex::new(Inner { audio, clip, codec, cb }), settings: OnceLock::new() })
    }
}

impl WinClip {
    /// Read frame `index` (a read job, completed with FlushJobs). Called with the clip locked.
    fn read_frame(&self, inner: &Inner, index: u64) -> Result<Com, String> {
        let api = &self.rt.api;
        {
            let mut st = inner.cb.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((_, f)) = st.read.take()
                && f != 0
            {
                let _ = vcall!(f as Raw, 2, fn() -> u32,);
            }
            st.image = None;
        }
        let mut job: Raw = std::ptr::null_mut();
        check(vcall!(inner.clip.0, api.clip_read_job, fn(u64, *mut Raw) -> i32, index, &mut job), "CreateJobReadFrame")?;
        let job = Com(job);
        check(vcall!(job.0, api.job_submit, fn() -> i32,), "Submit (read)")?;
        drop(job); // the runtime holds its own reference while the job runs
        let _ = vcall!(inner.codec.0, api.c_flush, fn() -> i32,);
        let (hr, frame) = inner.cb.state.lock().unwrap_or_else(PoisonError::into_inner).read.take().ok_or("the runtime did not read the frame")?;
        let frame = Com(frame as Raw);
        check(hr, "reading the frame")?;
        if frame.0.is_null() {
            return Err("the runtime read no frame".into());
        }
        Ok(frame)
    }

    fn decode_inner(&self, index: u64, scale: Scale, changes: Option<&Changes>) -> Result<Picture, String> {
        com_init();
        let api = &self.rt.api;
        // settings are read (once) before the clip is locked for this decode
        let meta = match changes {
            Some(c) if !c.is_empty() => Some(self.settings_meta()),
            _ => None,
        };
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let frame = self.read_frame(&inner, index)?;
        check(vcall!(frame.0, api.frame_format, fn(u32) -> i32, FORMAT_RGB_U16), "SetResourceFormat")?;
        check(vcall!(frame.0, api.frame_scale, fn(u32) -> i32, scale_code(scale)), "SetResolutionScale")?;
        // Source Settings: clones of the clip's and this frame's processing attributes, changed
        let (clip_attrs, frame_attrs) = match (meta, changes, &api.proc) {
            (Some(meta), Some(changes), Some(pr)) => apply_changes(pr, &inner.clip, &frame, meta, changes),
            _ => (None, None),
        };
        let raw = |c: &Option<Com>| c.as_ref().map_or(std::ptr::null_mut(), |c| c.0);
        let mut job: Raw = std::ptr::null_mut();
        check(
            vcall!(frame.0, api.frame_process_job, fn(Raw, Raw, *mut Raw) -> i32, raw(&clip_attrs), raw(&frame_attrs), &mut job),
            "CreateJobDecodeAndProcessFrame",
        )?;
        let job = Com(job);
        check(vcall!(job.0, api.job_submit, fn() -> i32,), "Submit (decode)")?;
        drop(job);
        let _ = vcall!(inner.codec.0, api.c_flush, fn() -> i32,);
        drop(frame);
        drop((clip_attrs, frame_attrs));
        let (w, h, fmt, bytes) = inner.cb.state.lock().unwrap_or_else(PoisonError::into_inner).image.take().ok_or("the runtime did not decode the frame")??;
        to_picture(w, h, fmt, &bytes)
    }

    /// The clip's Source Settings, read from the runtime once.
    fn settings_meta(&self) -> &Vec<(Setting, Meta)> {
        self.settings.get_or_init(|| {
            let Some(pr) = &self.rt.api.proc else { return Vec::new() };
            com_init();
            let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let mut out = Vec::new();
            if let Ok(ca) = clone_attrs(inner.clip.0, pr.clip_clone) {
                for e in CATALOG.iter().filter(|e| !e.frame) {
                    if let Some(x) = describe(ca.0, &pr.clip, e) {
                        out.push(x);
                    }
                }
            }
            // per-frame settings (ISO, white balance, exposure): as shot on the first frame
            match self.read_frame(&inner, 0).and_then(|f| clone_attrs(f.0, pr.frame_clone)) {
                Ok(fa) => {
                    for e in CATALOG.iter().filter(|e| e.frame) {
                        if let Some(x) = describe(fa.0, &pr.frame, e) {
                            out.push(x);
                        }
                    }
                }
                Err(e) => log::warn!("Blackmagic RAW: frame settings unavailable: {e}"),
            }
            out.sort_by_key(|(s, _)| CATALOG.iter().position(|e| e.key == s.key).unwrap_or(usize::MAX));
            log::info!("Blackmagic RAW: {} source settings", out.len());
            out
        })
    }
}

impl Clip for WinClip {
    fn info(&self) -> &ClipInfo {
        &self.info
    }

    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String> {
        self.decode_inner(index, scale, None)
    }

    fn settings(&self) -> Vec<Setting> {
        self.settings_meta().iter().map(|(s, _)| s.clone()).collect()
    }

    fn decode_with(&self, index: u64, scale: Scale, changes: &Changes) -> Result<Picture, String> {
        self.decode_inner(index, scale, Some(changes))
    }

    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String> {
        com_init();
        let (Some(a), Some(ai)) = (&self.rt.api.audio, &self.info.audio) else { return Err("no audio".into()) };
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(au) = &inner.audio else { return Err("no audio".into()) };
        let ch = ai.channels as usize;
        let bytes_per = (ai.bits_per_sample / 8) as usize;
        let want = frames.min(1 << 20);
        let mut buf = vec![0u8; want.saturating_mul(ch).saturating_mul(bytes_per)];
        let (mut got, mut got_bytes) = (0u32, 0u32);
        let start_i = i64::try_from(start).map_err(|_| "audio position out of range")?;
        let hr = vcall!(
            au.0,
            a.samples,
            fn(i64, *mut c_void, u32, u32, *mut u32, *mut u32) -> i32,
            start_i,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            want as u32,
            &mut got,
            &mut got_bytes
        );
        check(hr, "GetAudioSamples")?;
        let used = (got_bytes as usize).min(buf.len());
        Ok(pcm_to_f32(buf.get(..used).unwrap_or_default(), ai.bits_per_sample))
    }
}

// ---------------------------------------------------------------------------------------------
// Source Settings: processing attributes

/// A setting FilmCraft offers when the clip has it: attribute four-character code from the SDK
/// manual's BlackmagicRawClipProcessingAttribute / BlackmagicRawFrameProcessingAttribute tables.
struct Entry {
    key: &'static str,
    label: &'static str,
    group: &'static str,
    attr: u32,
    /// A frame processing attribute (ISO, white balance, exposure) rather than a clip one.
    frame: bool,
    /// An on/off setting (stored by the runtime as a number or boolean).
    toggle: bool,
    note: Option<&'static str>,
}

const CUSTOM_NOTE: Option<&str> = Some("Applies with Blackmagic Design Custom gamma (chosen automatically when changed)");

const fn entry(key: &'static str, label: &'static str, group: &'static str, code: &[u8; 4], frame: bool, toggle: bool, note: Option<&'static str>) -> Entry {
    Entry { key, label, group, attr: fourcc(code), frame, toggle, note }
}

static CATALOG: [Entry; 21] = [
    entry("iso", "ISO", "Image", b"fiso", true, false, None),
    entry("kelvin", "Color Temp", "Image", b"wbkv", true, false, None),
    entry("tint", "Tint", "Image", b"wbtn", true, false, None),
    entry("exposure", "Exposure", "Image", b"expo", true, false, None),
    entry("highlightRecovery", "Highlight Recovery", "Image", b"hlry", false, true, None),
    entry("gamma", "Gamma", "Color", b"gama", false, false, None),
    entry("gamut", "Gamut", "Color", b"gamt", false, false, None),
    entry("colorScienceGen", "Color Science", "Color", b"csgn", false, false, None),
    entry("gamutCompression", "Gamut Compression", "Color", b"gace", false, true, None),
    entry("contrast", "Contrast", "Custom Gamma", b"tcon", false, false, CUSTOM_NOTE),
    entry("saturation", "Saturation", "Custom Gamma", b"tsat", false, false, None),
    entry("midpoint", "Midpoint", "Custom Gamma", b"tmid", false, false, None),
    entry("highlights", "Highlight Rolloff", "Custom Gamma", b"thih", false, false, None),
    entry("shadows", "Shadow Rolloff", "Custom Gamma", b"tsha", false, false, None),
    entry("blackLevel", "Black Level", "Custom Gamma", b"tblk", false, false, None),
    entry("whiteLevel", "White Level", "Custom Gamma", b"twit", false, false, None),
    entry("videoBlackLevel", "Video Black Level", "Custom Gamma", b"tvbl", false, true, None),
    entry("lutMode", "3D LUT", "LUT", b"lutm", false, false, None),
    entry("lensShading", "Lens Shading Correction", "Lens", b"shde", false, true, None),
    entry("lensDistortion", "Lens Distortion Correction", "Lens", b"dice", false, true, None),
    entry("chromaticAberration", "Chromatic Aberration Correction", "Lens", b"cace", false, true, None),
];

/// Changing these switches the gamma to Blackmagic Design Custom (as in DaVinci Resolve).
const TONE_KEYS: [&str; 8] = ["contrast", "saturation", "midpoint", "highlights", "shadows", "blackLevel", "whiteLevel", "videoBlackLevel"];

/// How to write a setting back: its attribute and the VARIANT type the runtime reported.
#[derive(Clone, Copy, Debug)]
struct Meta {
    attr: u32,
    frame: bool,
    vt: u16,
}

/// A Windows VARIANT (24 bytes on x64 and ARM64: type, three reserved words, 16-byte payload),
/// cleared with VariantClear when dropped.
#[repr(C)]
#[derive(Default)]
struct Var {
    vt: u16,
    reserved: [u16; 3],
    data: [u64; 2],
}

impl Drop for Var {
    fn drop(&mut self) {
        if self.vt != 0 {
            // SAFETY: a VARIANT the runtime filled in or we built (BSTRs owned by it); cleared once.
            let _ = unsafe { windows::Win32::System::Variant::VariantClear((self as *mut Var).cast()) };
            self.vt = 0;
        }
    }
}

const VT_I2_: u16 = 2;
const VT_I4_: u16 = 3;
const VT_R4_: u16 = 4;
const VT_R8_: u16 = 5;
const VT_BSTR_: u16 = 8;
const VT_BOOL_: u16 = 11;
const VT_UI1_: u16 = 17;
const VT_UI2_: u16 = 18;
const VT_UI4_: u16 = 19;
const VT_INT_: u16 = 22;
const VT_UINT_: u16 = 23;

/// f32 values shown without float noise (0.1 rather than 0.10000000149).
fn tidy(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn var_to_json(v: &Var) -> Option<Value> {
    let d = v.data[0];
    Some(match v.vt {
        VT_UI1_ => json!(d as u8),
        VT_I2_ => json!(d as u16 as i16),
        VT_UI2_ => json!(d as u16),
        VT_I4_ | VT_INT_ => json!(d as u32 as i32),
        VT_UI4_ | VT_UINT_ => json!(d as u32),
        VT_R4_ => json!(tidy(f32::from_bits(d as u32) as f64)),
        VT_R8_ => json!(f64::from_bits(d)),
        VT_BOOL_ => json!(d as u16 != 0),
        VT_BSTR_ => {
            let p = d as usize as *const u16;
            if p.is_null() {
                json!("")
            } else {
                // SAFETY: a BSTR: its byte length is the u32 before the first character.
                let s = unsafe {
                    let bytes = *(p as *const u32).sub(1) as usize;
                    String::from_utf16_lossy(std::slice::from_raw_parts(p, (bytes / 2).min(1 << 16)))
                };
                json!(s)
            }
        }
        _ => return None,
    })
}

/// `value` as a VARIANT of type `vt` (the type the runtime uses for the setting).
fn json_to_var(value: &Value, vt: u16) -> Option<Var> {
    let num = value.as_f64().or_else(|| value.as_bool().map(|b| if b { 1.0 } else { 0.0 }));
    let mut v = Var { vt, ..Var::default() };
    v.data[0] = match vt {
        VT_UI1_ => num?.round().clamp(0.0, 255.0) as u8 as u64,
        VT_I2_ => num?.round().clamp(-32_768.0, 32_767.0) as i16 as u16 as u64,
        VT_UI2_ => num?.round().clamp(0.0, 65_535.0) as u16 as u64,
        VT_I4_ | VT_INT_ => num?.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32 as u64,
        VT_UI4_ | VT_UINT_ => num?.round().clamp(0.0, u32::MAX as f64) as u32 as u64,
        VT_R4_ => (num? as f32).to_bits() as u64,
        VT_R8_ => num?.to_bits(),
        VT_BOOL_ => {
            if num? != 0.0 {
                0xFFFF
            } else {
                0
            }
        }
        VT_BSTR_ => BSTR::from(value.as_str()?).into_raw() as usize as u64,
        _ => return None,
    };
    Some(v)
}

fn clone_attrs(obj: Raw, slot: usize) -> Result<Com, String> {
    let mut out: Raw = std::ptr::null_mut();
    check(vcall!(obj, slot, fn(*mut Raw) -> i32, &mut out), "clone processing attributes")?;
    if out.is_null() { Err("no processing attributes".into()) } else { Ok(Com(out)) }
}

/// A catalog entry as this clip has it: current value, options or range, read-only flag.
fn describe(attrs: Raw, a: &AttrApi, e: &Entry) -> Option<(Setting, Meta)> {
    let mut cur = Var::default();
    if vcall!(attrs, a.get, fn(u32, *mut Var) -> i32, e.attr, &mut cur) < 0 {
        return None; // not something this clip or camera has
    }
    let shot = var_to_json(&cur)?;
    let meta = Meta { attr: e.attr, frame: e.frame, vt: cur.vt };
    let mut ro: i32 = 0;
    // a list of values (gamma, gamut, ISO…)
    let mut n: u32 = 0;
    let mut kind = None;
    if !e.toggle
        && vcall!(attrs, a.list, fn(u32, *mut Var, *mut u32, *mut i32) -> i32, e.attr, std::ptr::null_mut(), &mut n, &mut ro) >= 0
        && (1..=512).contains(&n)
    {
        let mut arr: Vec<Var> = (0..n).map(|_| Var::default()).collect();
        let mut got = n;
        if vcall!(attrs, a.list, fn(u32, *mut Var, *mut u32, *mut i32) -> i32, e.attr, arr.as_mut_ptr(), &mut got, &mut ro) >= 0 {
            let mut opts: Vec<Value> = arr.iter().take(got.min(n) as usize).filter_map(var_to_json).collect();
            if !opts.is_empty() {
                if !opts.contains(&shot) {
                    opts.insert(0, shot.clone());
                }
                kind = Some(Kind::Choice(opts));
            }
        }
    }
    // ISO on the frame interface: its own list call
    if kind.is_none()
        && e.key == "iso"
        && let Some(slot) = a.iso
    {
        let mut n: u32 = 0;
        if vcall!(attrs, slot, fn(*mut u32, *mut u32, *mut i32) -> i32, std::ptr::null_mut(), &mut n, &mut ro) >= 0 && (1..=512).contains(&n) {
            let mut arr = vec![0u32; n as usize];
            let mut got = n;
            if vcall!(attrs, slot, fn(*mut u32, *mut u32, *mut i32) -> i32, arr.as_mut_ptr(), &mut got, &mut ro) >= 0 {
                let mut opts: Vec<Value> = arr.iter().take(got.min(n) as usize).map(|v| json!(v)).collect();
                if !opts.is_empty() {
                    if !opts.contains(&shot) && shot.is_number() {
                        opts.insert(0, shot.clone());
                    }
                    kind = Some(Kind::Choice(opts));
                }
            }
        }
    }
    // a range (white balance, exposure, tone curve) or an on/off
    if kind.is_none() {
        let (mut lo, mut hi) = (Var::default(), Var::default());
        let ranged = vcall!(attrs, a.range, fn(u32, *mut Var, *mut Var, *mut i32) -> i32, e.attr, &mut lo, &mut hi, &mut ro) >= 0;
        let (lo, hi) = (var_to_json(&lo).and_then(|v| v.as_f64()), var_to_json(&hi).and_then(|v| v.as_f64()));
        kind = Some(if e.toggle {
            Kind::Toggle
        } else if let (true, Some(lo), Some(hi)) = (ranged, lo, hi) {
            let int = !matches!(cur.vt, VT_R4_ | VT_R8_);
            Kind::Number { min: lo.min(hi), max: hi.max(lo), step: if int { 1.0 } else { 0.01 } }
        } else {
            ro = 1; // nothing to choose from: show it, don't edit it
            Kind::Number { min: 0.0, max: 0.0, step: 1.0 }
        });
    }
    let kind = kind?;
    let as_shot = match kind {
        Kind::Toggle => json!(shot.as_bool().unwrap_or_else(|| shot.as_f64().unwrap_or(0.0) != 0.0)),
        _ => shot,
    };
    Some((Setting { key: e.key, label: e.label, group: e.group, kind, as_shot, read_only: ro != 0, note: e.note }, meta))
}

/// Clones of the clip's and the frame's processing attributes with `changes` set (`None` for a
/// side nothing changed on). A value the runtime refuses is logged and skipped.
fn apply_changes(pr: &ProcApi, clip: &Com, frame: &Com, meta: &[(Setting, Meta)], changes: &Changes) -> (Option<Com>, Option<Com>) {
    let mut changes = changes.clone();
    // tone curve settings only act with Blackmagic Design Custom gamma: switch to it, as Resolve does
    if changes.keys().any(|k| TONE_KEYS.contains(&k.as_str()))
        && !changes.contains_key("gamma")
        && let Some((st, _)) = meta.iter().find(|(s, _)| s.key == "gamma")
        && let Kind::Choice(opts) = &st.kind
        && let Some(custom) = opts.iter().find(|o| o.as_str().is_some_and(|t| t.contains("Custom")))
    {
        changes.insert("gamma".into(), custom.clone());
    }
    let (mut ca, mut fa): (Option<Com>, Option<Com>) = (None, None);
    // gamma and gamut first: the other settings are relative to them
    let mut keys: Vec<&String> = changes.keys().collect();
    keys.sort_by_key(|k| match k.as_str() {
        "gamma" => 0,
        "gamut" => 1,
        _ => 2,
    });
    for k in keys {
        let (Some(v), Some((_, m))) = (changes.get(k), meta.iter().find(|(s, _)| s.key == k.as_str())) else { continue };
        let (target, api, slot, owner) = if m.frame { (&mut fa, &pr.frame, pr.frame_clone, frame) } else { (&mut ca, &pr.clip, pr.clip_clone, clip) };
        if target.is_none() {
            *target = clone_attrs(owner.0, slot).ok();
        }
        let (Some(t), Some(mut var)) = (target.as_ref(), json_to_var(v, m.vt)) else { continue };
        let hr = vcall!(t.0, api.set, fn(u32, *mut Var) -> i32, m.attr, &mut var);
        if hr < 0 {
            log::warn!("Blackmagic RAW: {k} = {v} refused ({})", hr_text(hr));
        }
    }
    (ca, fa)
}

/// What this machine has, in words (`filmcraft-cli braw-check`).
pub fn check_runtime() -> String {
    com_init();
    let mut lines = Vec::new();
    match load() {
        Ok(l) => {
            lines.push(format!("Blackmagic RAW runtime: OK ({})", l.dll.display()));
            lines.extend(l.notes.iter().map(|n| format!("  {n}")));
            lines.push(format!("  audio: {}", if l.api.audio.is_some() { "available" } else { "off" }));
        }
        Err(e) => lines.push(format!("Blackmagic RAW runtime: NOT usable: {e}")),
    }
    lines.join("\n")
}
