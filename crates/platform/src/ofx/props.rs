//! Property sets and the property suite, and the registry of live handles.
//!
//! Every handle FilmCraft gives a plug-in (property sets, effects, parameters, clips, images,
//! memory, mutexes) is registered here with its kind, and every call that receives one checks it
//! first: a stale or foreign handle is refused with `kOfxStatErrBadHandle` instead of being
//! dereferenced.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::{Mutex, PoisonError, RwLock};

use super::ffi::{self, OfxStatus, Raw};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Props,
    Effect,
    ParamSet,
    Param,
    Clip,
    Memory,
    Mutex,
}

static LIVE: RwLock<Option<HashMap<usize, Kind>>> = RwLock::new(None);

pub fn register<T>(p: *const T, k: Kind) {
    let mut g = LIVE.write().unwrap_or_else(PoisonError::into_inner);
    g.get_or_insert_with(HashMap::new).insert(p as usize, k);
}

pub fn unregister<T>(p: *const T) {
    let mut g = LIVE.write().unwrap_or_else(PoisonError::into_inner);
    if let Some(m) = g.as_mut() {
        m.remove(&(p as usize));
    }
}

/// Whether `h` is a live handle of kind `k`.
pub fn is<T>(h: *const T, k: Kind) -> bool {
    !h.is_null() && LIVE.read().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|m| m.get(&(h as usize)) == Some(&k))
}

/// The object behind handle `h` of kind `k`, if it is live.
///
/// Objects are only freed by FilmCraft at defined points (instance destruction, image release),
/// so a handle checked here stays valid for the call that received it.
pub fn get<'a, T>(h: Raw, k: Kind) -> Option<&'a T> {
    if !is(h as *const T, k) {
        return None;
    }
    // SAFETY: `h` was registered as a live object of type `T` for kind `k` (every kind maps to one
    // type) and is unregistered before it is freed.
    Some(unsafe { &*(h as *const T) })
}

/// A property value: a fixed type and one or more elements.
#[derive(Clone, Debug)]
pub enum Val {
    Ptr(Vec<usize>),
    Str(Vec<CString>),
    Dbl(Vec<f64>),
    Int(Vec<i32>),
}

impl Val {
    fn len(&self) -> usize {
        match self {
            Val::Ptr(v) => v.len(),
            Val::Str(v) => v.len(),
            Val::Dbl(v) => v.len(),
            Val::Int(v) => v.len(),
        }
    }
}

/// A property set (`OfxPropertySetHandle`).
#[derive(Debug, Default)]
pub struct PropSet {
    map: Mutex<HashMap<String, Val>>,
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap_or_default()
}

impl PropSet {
    /// A new, registered set (unregistered when dropped).
    pub fn new() -> Box<PropSet> {
        let b = Box::new(PropSet::default());
        register(&*b as *const PropSet, Kind::Props);
        b
    }
    pub fn raw(&self) -> Raw {
        self as *const PropSet as Raw
    }
    pub fn copy_from(&self, other: &PropSet) {
        let src = other.map.lock().unwrap_or_else(PoisonError::into_inner).clone();
        self.map.lock().unwrap_or_else(PoisonError::into_inner).extend(src);
    }
    pub fn set(&self, name: &str, v: Val) {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_string(), v);
    }
    pub fn s(&self, name: &str, v: &str) {
        self.set(name, Val::Str(vec![cstring(v)]));
    }
    pub fn ss(&self, name: &str, v: &[&str]) {
        self.set(name, Val::Str(v.iter().map(|s| cstring(s)).collect()));
    }
    pub fn i(&self, name: &str, v: i32) {
        self.set(name, Val::Int(vec![v]));
    }
    pub fn is_(&self, name: &str, v: &[i32]) {
        self.set(name, Val::Int(v.to_vec()));
    }
    pub fn d(&self, name: &str, v: f64) {
        self.set(name, Val::Dbl(vec![v]));
    }
    pub fn ds(&self, name: &str, v: &[f64]) {
        self.set(name, Val::Dbl(v.to_vec()));
    }
    pub fn p(&self, name: &str, v: Raw) {
        self.set(name, Val::Ptr(vec![v as usize]));
    }
    pub fn get(&self, name: &str) -> Option<Val> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }
    pub fn str_(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            Val::Str(v) => v.first().map(|s| s.to_string_lossy().into_owned()),
            _ => None,
        }
    }
    pub fn strs(&self, name: &str) -> Vec<String> {
        match self.get(name) {
            Some(Val::Str(v)) => v.iter().map(|s| s.to_string_lossy().into_owned()).collect(),
            _ => Vec::new(),
        }
    }
    pub fn int(&self, name: &str) -> Option<i32> {
        self.ints(name).first().copied()
    }
    pub fn ints(&self, name: &str) -> Vec<i32> {
        match self.get(name) {
            Some(Val::Int(v)) => v,
            Some(Val::Dbl(v)) => v.iter().map(|d| if d.is_finite() { d.clamp(i32::MIN as f64, i32::MAX as f64) as i32 } else { 0 }).collect(),
            _ => Vec::new(),
        }
    }
    pub fn dbl(&self, name: &str) -> Option<f64> {
        self.dbls(name).first().copied()
    }
    pub fn dbls(&self, name: &str) -> Vec<f64> {
        match self.get(name) {
            Some(Val::Dbl(v)) => v,
            Some(Val::Int(v)) => v.iter().map(|&i| i as f64).collect(),
            _ => Vec::new(),
        }
    }
    pub fn ptr(&self, name: &str) -> Option<usize> {
        match self.get(name)? {
            Val::Ptr(v) => v.first().copied(),
            _ => None,
        }
    }

    // ---- the suite's operations ----

    fn put<T>(&self, name: &str, index: c_int, make: impl Fn(T) -> Val, merge: impl FnOnce(&mut Val, usize) -> bool, v: T) -> OfxStatus {
        let Ok(i) = usize::try_from(index) else { return ffi::ERR_BAD_INDEX };
        if i > 4096 {
            return ffi::ERR_BAD_INDEX;
        }
        let mut m = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        match m.get_mut(name) {
            Some(cur) => {
                if merge(cur, i) {
                    ffi::OK
                } else {
                    ffi::ERR_VALUE
                }
            }
            None => {
                // a property the host did not create: element 0 makes it; a later element is
                // written into a new property padded with defaults
                let mut val = make(v);
                if i > 0 {
                    let pad = |val: &mut Val| match val {
                        Val::Ptr(a) => a.insert(0, 0),
                        Val::Str(a) => a.insert(0, CString::default()),
                        Val::Dbl(a) => a.insert(0, 0.0),
                        Val::Int(a) => a.insert(0, 0),
                    };
                    for _ in 0..i {
                        pad(&mut val);
                    }
                }
                m.insert(name.to_string(), val);
                ffi::OK
            }
        }
    }

    fn set_at_str(&self, name: &str, index: c_int, v: CString) -> OfxStatus {
        let c = v.clone();
        self.put(
            name,
            index,
            |v: CString| Val::Str(vec![v]),
            |cur, i| match cur {
                Val::Str(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, CString::default());
                    }
                    s[i] = c;
                    true
                }
                _ => false,
            },
            v,
        )
    }
    fn set_at_dbl(&self, name: &str, index: c_int, v: f64) -> OfxStatus {
        self.put(
            name,
            index,
            |v: f64| Val::Dbl(vec![v]),
            |cur, i| match cur {
                Val::Dbl(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, 0.0);
                    }
                    s[i] = v;
                    true
                }
                Val::Int(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, 0);
                    }
                    s[i] = if v.is_finite() { v.clamp(i32::MIN as f64, i32::MAX as f64) as i32 } else { 0 };
                    true
                }
                _ => false,
            },
            v,
        )
    }
    fn set_at_int(&self, name: &str, index: c_int, v: i32) -> OfxStatus {
        self.put(
            name,
            index,
            |v: i32| Val::Int(vec![v]),
            |cur, i| match cur {
                Val::Int(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, 0);
                    }
                    s[i] = v;
                    true
                }
                Val::Dbl(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, 0.0);
                    }
                    s[i] = v as f64;
                    true
                }
                _ => false,
            },
            v,
        )
    }
    fn set_at_ptr(&self, name: &str, index: c_int, v: usize) -> OfxStatus {
        self.put(
            name,
            index,
            |v: usize| Val::Ptr(vec![v]),
            |cur, i| match cur {
                Val::Ptr(s) => {
                    if s.len() <= i {
                        s.resize(i + 1, 0);
                    }
                    s[i] = v;
                    true
                }
                _ => false,
            },
            v,
        )
    }
}

impl Drop for PropSet {
    fn drop(&mut self) {
        unregister(self as *const PropSet);
    }
}

/// A property name from the plug-in.
///
/// # Safety
/// `p` is null or a NUL-terminated string (OpenFX contract).
unsafe fn name<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated per the contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Run a suite function body, turning a panic into `kOfxStatErrUnknown` (unwinding into a plug-in
/// is undefined).
pub fn guard(f: impl FnOnce() -> OfxStatus) -> OfxStatus {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(ffi::ERR_UNKNOWN)
}

macro_rules! with_props {
    ($h:expr, $p:expr, |$ps:ident, $n:ident| $body:expr) => {
        guard(|| {
            let Some($ps) = get::<PropSet>($h, Kind::Props) else { return ffi::ERR_BAD_HANDLE };
            // SAFETY: OpenFX passes property names as NUL-terminated strings.
            let Some($n) = (unsafe { name($p) }) else { return ffi::ERR_UNKNOWN };
            $body
        })
    };
}

unsafe extern "C" fn set_pointer(h: Raw, p: *const c_char, i: c_int, v: Raw) -> OfxStatus {
    with_props!(h, p, |ps, n| ps.set_at_ptr(n, i, v as usize))
}
unsafe extern "C" fn set_string(h: Raw, p: *const c_char, i: c_int, v: *const c_char) -> OfxStatus {
    with_props!(h, p, |ps, n| {
        // SAFETY: a NUL-terminated string or null (OpenFX contract).
        let s = if v.is_null() { CString::default() } else { unsafe { CStr::from_ptr(v) }.to_owned() };
        ps.set_at_str(n, i, s)
    })
}
unsafe extern "C" fn set_double(h: Raw, p: *const c_char, i: c_int, v: f64) -> OfxStatus {
    with_props!(h, p, |ps, n| ps.set_at_dbl(n, i, v))
}
unsafe extern "C" fn set_int(h: Raw, p: *const c_char, i: c_int, v: c_int) -> OfxStatus {
    with_props!(h, p, |ps, n| ps.set_at_int(n, i, v))
}

/// `count` elements at `v` (bounded), or `None` for a null array or a negative count.
///
/// # Safety
/// `v` is null or points to `count` readable elements (OpenFX contract).
unsafe fn arr<'a, T>(v: *const T, count: c_int) -> Option<&'a [T]> {
    let n = usize::try_from(count).ok().filter(|&n| n <= 4096)?;
    if v.is_null() {
        return if n == 0 { Some(&[]) } else { None };
    }
    // SAFETY: per the contract `v` holds `count` elements.
    Some(unsafe { std::slice::from_raw_parts(v, n) })
}

unsafe extern "C" fn set_pointer_n(h: Raw, p: *const c_char, c: c_int, v: *const Raw) -> OfxStatus {
    with_props!(h, p, |ps, n| {
        // SAFETY: `v` holds `c` pointers (contract).
        let Some(a) = (unsafe { arr(v, c) }) else { return ffi::ERR_BAD_INDEX };
        ps.set(n, Val::Ptr(a.iter().map(|&x| x as usize).collect()));
        ffi::OK
    })
}
unsafe extern "C" fn set_string_n(h: Raw, p: *const c_char, c: c_int, v: *const *const c_char) -> OfxStatus {
    with_props!(h, p, |ps, n| {
        // SAFETY: `v` holds `c` string pointers (contract).
        let Some(a) = (unsafe { arr(v, c) }) else { return ffi::ERR_BAD_INDEX };
        // SAFETY: each is a NUL-terminated string or null.
        let strs = a.iter().map(|&s| if s.is_null() { CString::default() } else { unsafe { CStr::from_ptr(s) }.to_owned() }).collect();
        ps.set(n, Val::Str(strs));
        ffi::OK
    })
}
unsafe extern "C" fn set_double_n(h: Raw, p: *const c_char, c: c_int, v: *const f64) -> OfxStatus {
    with_props!(h, p, |ps, n| {
        // SAFETY: `v` holds `c` doubles (contract).
        let Some(a) = (unsafe { arr(v, c) }) else { return ffi::ERR_BAD_INDEX };
        let cur_int = matches!(ps.get(n), Some(Val::Int(_)));
        ps.set(n, if cur_int { Val::Int(a.iter().map(|&d| if d.is_finite() { d as i32 } else { 0 }).collect()) } else { Val::Dbl(a.to_vec()) });
        ffi::OK
    })
}
unsafe extern "C" fn set_int_n(h: Raw, p: *const c_char, c: c_int, v: *const c_int) -> OfxStatus {
    with_props!(h, p, |ps, n| {
        // SAFETY: `v` holds `c` ints (contract).
        let Some(a) = (unsafe { arr(v, c) }) else { return ffi::ERR_BAD_INDEX };
        let cur_dbl = matches!(ps.get(n), Some(Val::Dbl(_)));
        ps.set(n, if cur_dbl { Val::Dbl(a.iter().map(|&i| i as f64).collect()) } else { Val::Int(a.to_vec()) });
        ffi::OK
    })
}

/// Element `i` of property `n` through `read` (which fills the caller's slot).
fn fetch(ps: &PropSet, n: &str, i: c_int, read: impl FnOnce(&Val, usize) -> Option<OfxStatus>) -> OfxStatus {
    let Ok(i) = usize::try_from(i) else { return ffi::ERR_BAD_INDEX };
    let m = ps.map.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(v) = m.get(n) else {
        trace_unknown(n);
        return ffi::ERR_UNKNOWN;
    };
    if i >= v.len() {
        return ffi::ERR_BAD_INDEX;
    }
    read(v, i).unwrap_or(ffi::ERR_VALUE)
}

unsafe extern "C" fn get_pointer(h: Raw, p: *const c_char, i: c_int, out: *mut Raw) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    with_props!(h, p, |ps, n| fetch(ps, n, i, |v, i| match v {
        Val::Ptr(a) => {
            // SAFETY: `out` is the caller's non-null out-pointer.
            unsafe { *out = a[i] as Raw };
            Some(ffi::OK)
        }
        _ => None,
    }))
}
unsafe extern "C" fn get_string(h: Raw, p: *const c_char, i: c_int, out: *mut *const c_char) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    with_props!(h, p, |ps, n| fetch(ps, n, i, |v, i| match v {
        Val::Str(a) => {
            // SAFETY: as above; the string lives in the set until the property is changed (the
            // lifetime OpenFX gives returned strings).
            unsafe { *out = a[i].as_ptr() };
            Some(ffi::OK)
        }
        _ => None,
    }))
}
unsafe extern "C" fn get_double(h: Raw, p: *const c_char, i: c_int, out: *mut f64) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    with_props!(h, p, |ps, n| fetch(ps, n, i, |v, i| {
        let d = match v {
            Val::Dbl(a) => a[i],
            Val::Int(a) => a[i] as f64,
            _ => return None,
        };
        // SAFETY: as above.
        unsafe { *out = d };
        Some(ffi::OK)
    }))
}
unsafe extern "C" fn get_int(h: Raw, p: *const c_char, i: c_int, out: *mut c_int) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    with_props!(h, p, |ps, n| fetch(ps, n, i, |v, i| {
        let d = match v {
            Val::Int(a) => a[i],
            Val::Dbl(a) => {
                if a[i].is_finite() {
                    a[i] as i32
                } else {
                    0
                }
            }
            _ => return None,
        };
        // SAFETY: as above.
        unsafe { *out = d };
        Some(ffi::OK)
    }))
}

/// `FILMCRAFT_OFX_TRACE=1`: report properties plug-ins ask for that FilmCraft does not have.
fn trace_unknown(n: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("FILMCRAFT_OFX_TRACE").is_some()) {
        eprintln!("OpenFX: property {n} asked for, not set");
    }
}

/// Fill `count` slots at `out` from elements 0.. of `n`.
fn fetch_n<T: Copy>(ps: &PropSet, n: &str, count: c_int, out: *mut T, conv: impl Fn(&Val, usize) -> Option<T>) -> OfxStatus {
    let Some(k) = usize::try_from(count).ok().filter(|&k| k <= 4096) else { return ffi::ERR_BAD_INDEX };
    if out.is_null() && k > 0 {
        return ffi::ERR_BAD_HANDLE;
    }
    let m = ps.map.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(v) = m.get(n) else {
        trace_unknown(n);
        return ffi::ERR_UNKNOWN;
    };
    if k > v.len() {
        return ffi::ERR_BAD_INDEX;
    }
    for i in 0..k {
        let Some(x) = conv(v, i) else { return ffi::ERR_VALUE };
        // SAFETY: the caller's array holds `count` slots (OpenFX contract) and i < count.
        unsafe { *out.add(i) = x };
    }
    ffi::OK
}

unsafe extern "C" fn get_pointer_n(h: Raw, p: *const c_char, c: c_int, out: *mut Raw) -> OfxStatus {
    with_props!(h, p, |ps, n| fetch_n(ps, n, c, out, |v, i| match v {
        Val::Ptr(a) => a.get(i).map(|&x| x as Raw),
        _ => None,
    }))
}
unsafe extern "C" fn get_string_n(h: Raw, p: *const c_char, c: c_int, out: *mut *const c_char) -> OfxStatus {
    with_props!(h, p, |ps, n| fetch_n(ps, n, c, out, |v, i| match v {
        Val::Str(a) => a.get(i).map(|s| s.as_ptr()),
        _ => None,
    }))
}
unsafe extern "C" fn get_double_n(h: Raw, p: *const c_char, c: c_int, out: *mut f64) -> OfxStatus {
    with_props!(h, p, |ps, n| fetch_n(ps, n, c, out, |v, i| match v {
        Val::Dbl(a) => a.get(i).copied(),
        Val::Int(a) => a.get(i).map(|&x| x as f64),
        _ => None,
    }))
}
unsafe extern "C" fn get_int_n(h: Raw, p: *const c_char, c: c_int, out: *mut c_int) -> OfxStatus {
    with_props!(h, p, |ps, n| fetch_n(ps, n, c, out, |v, i| match v {
        Val::Int(a) => a.get(i).copied(),
        Val::Dbl(a) => a.get(i).map(|&x| if x.is_finite() { x as i32 } else { 0 }),
        _ => None,
    }))
}
unsafe extern "C" fn reset(h: Raw, p: *const c_char) -> OfxStatus {
    with_props!(h, p, |ps, n| if ps.get(n).is_some() { ffi::OK } else { ffi::ERR_UNKNOWN })
}
unsafe extern "C" fn get_dimension(h: Raw, p: *const c_char, out: *mut c_int) -> OfxStatus {
    if out.is_null() {
        return ffi::ERR_BAD_HANDLE;
    }
    with_props!(h, p, |ps, n| match ps.get(n) {
        Some(v) => {
            // SAFETY: the caller's non-null out-pointer.
            unsafe { *out = v.len() as c_int };
            ffi::OK
        }
        None => ffi::ERR_UNKNOWN,
    })
}

pub static SUITE: ffi::PropertySuiteV1 = ffi::PropertySuiteV1 {
    set_pointer,
    set_string,
    set_double,
    set_int,
    set_pointer_n,
    set_string_n,
    set_double_n,
    set_int_n,
    get_pointer,
    get_string,
    get_double,
    get_int,
    get_pointer_n,
    get_string_n,
    get_double_n,
    get_int_n,
    reset,
    get_dimension,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_set_get_and_refuse_bad_input() {
        let ps = PropSet::new();
        let h = ps.raw();
        let k = c"OfxPropLabel";
        // SAFETY: test calls with valid strings and out-pointers.
        unsafe {
            assert_eq!(set_string(h, k.as_ptr(), 0, c"Blur".as_ptr()), ffi::OK);
            let mut out: *const c_char = std::ptr::null();
            assert_eq!(get_string(h, k.as_ptr(), 0, &mut out), ffi::OK);
            assert_eq!(CStr::from_ptr(out).to_str().unwrap(), "Blur");
            assert_eq!(get_string(h, k.as_ptr(), 1, &mut out), ffi::ERR_BAD_INDEX);
            assert_eq!(get_string(h, c"Nope".as_ptr(), 0, &mut out), ffi::ERR_UNKNOWN);
            let mut d = 0.0;
            assert_eq!(get_double(h, k.as_ptr(), 0, &mut d), ffi::ERR_VALUE);
            // int set on a double property converts
            ps.ds("OfxImageEffectPropRenderScale", &[1.0, 1.0]);
            assert_eq!(set_int(h, c"OfxImageEffectPropRenderScale".as_ptr(), 1, 2), ffi::OK);
            assert_eq!(ps.dbls("OfxImageEffectPropRenderScale"), vec![1.0, 2.0]);
            let mut dim = 0;
            assert_eq!(get_dimension(h, c"OfxImageEffectPropRenderScale".as_ptr(), &mut dim), ffi::OK);
            assert_eq!(dim, 2);
            // a stale or foreign handle is refused, not followed
            let bogus = 0x1234usize as Raw;
            assert_eq!(set_int(bogus, k.as_ptr(), 0, 1), ffi::ERR_BAD_HANDLE);
            assert_eq!(get_string(h, k.as_ptr(), 0, std::ptr::null_mut()), ffi::ERR_BAD_HANDLE);
            let mut ints = [0; 2];
            assert_eq!(get_int_n(h, c"OfxImageEffectPropRenderScale".as_ptr(), 2, ints.as_mut_ptr()), ffi::OK);
            assert_eq!(ints, [1, 2]);
            assert_eq!(get_int_n(h, c"OfxImageEffectPropRenderScale".as_ptr(), 3, ints.as_mut_ptr()), ffi::ERR_BAD_INDEX);
        }
        let raw = ps.raw();
        drop(ps);
        assert!(!is(raw as *const PropSet, Kind::Props));
    }
}
