//! The host objects VST3 plug-ins receive from FilmCraft: the host application (which also makes
//! messages and attribute lists), memory streams for state, parameter changes and their queues,
//! an empty event list and the component handler that reports edits from a plug-in's editor.

// `TChar` (UTF-16 units) is `u16` on some targets of the bindings and `i16` on others.
#![allow(clippy::unnecessary_cast)]

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run a host callback body, turning a panic into `kResultFalse` (unwinding into a plug-in is
/// undefined).
pub fn guard(f: impl FnOnce() -> tresult) -> tresult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(kResultFalse)
}

/// Write `s` into a VST3 `String128` (UTF-16, NUL-terminated, truncated).
///
/// # Safety
/// `out` is null or points to a `String128`.
pub unsafe fn put_string128(out: *mut String128, s: &str) {
    if out.is_null() {
        return;
    }
    let mut v: Vec<u16> = s.encode_utf16().take(127).collect();
    v.push(0);
    // SAFETY: `out` points to 128 UTF-16 units (contract); `v` has at most 128.
    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), (*out).as_mut_ptr() as *mut u16, v.len()) };
}

/// A `String128` (or any NUL-terminated UTF-16 array) as text.
pub fn string128(s: &[TChar]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n].iter().map(|&c| c as u16).collect::<Vec<_>>())
}

/// A fixed `char8` array as text.
pub fn string8(s: &[c_char]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..n].iter().map(|&c| c as u8).collect::<Vec<_>>()).into_owned()
}

fn guid_of(p: *const TUID) -> Option<[u8; 16]> {
    if p.is_null() {
        return None;
    }
    // SAFETY: a TUID is 16 bytes (contract).
    Some(unsafe { *(p as *const [u8; 16]) })
}

/// # Safety
/// `p` is null or a NUL-terminated string.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

// ---------------------------------------------------------------------------------------------
// Host application

pub struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plug-in's String128 out-parameter.
        unsafe { put_string128(name, "FilmCraft") };
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        guard(|| {
            if obj.is_null() {
                return kInvalidArgument;
            }
            let (Some(c), Some(i)) = (guid_of(cid), guid_of(iid)) else { return kInvalidArgument };
            let out = if c == IMessage::IID && (i == IMessage::IID || i == FUnknown::IID) {
                ComWrapper::new(Message::default()).to_com_ptr::<IMessage>().map(|p| p.into_raw() as *mut c_void)
            } else if c == IAttributeList::IID && (i == IAttributeList::IID || i == FUnknown::IID) {
                ComWrapper::new(Attributes::default()).to_com_ptr::<IAttributeList>().map(|p| p.into_raw() as *mut c_void)
            } else {
                None
            };
            match out {
                Some(p) => {
                    // SAFETY: non-null out-pointer; the plug-in owns the reference.
                    unsafe { *obj = p };
                    kResultOk
                }
                None => {
                    // SAFETY: as above.
                    unsafe { *obj = std::ptr::null_mut() };
                    kNoInterface
                }
            }
        })
    }
}

/// The host context handed to `initialize` (one per process).
pub fn host_context() -> *mut FUnknown {
    static HOST: std::sync::OnceLock<HostPtr> = std::sync::OnceLock::new();
    HOST.get_or_init(|| HostPtr(ComWrapper::new(HostApp).to_com_ptr::<IHostApplication>().map_or(std::ptr::null_mut(), |p| p.into_raw() as *mut FUnknown))).0
}

struct HostPtr(*mut FUnknown);
// SAFETY: the host object is immutable and free-threaded.
unsafe impl Send for HostPtr {}
// SAFETY: as above.
unsafe impl Sync for HostPtr {}

// ---------------------------------------------------------------------------------------------
// Messages and attribute lists (plug-ins use them between their processor and controller)

#[derive(Clone, Debug)]
enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

#[derive(Default)]
pub struct Attributes {
    map: Mutex<HashMap<String, Attr>>,
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

impl Attributes {
    fn set(&self, id: AttrID, v: Attr) -> tresult {
        // SAFETY: attribute ids are NUL-terminated strings (contract).
        let Some(k) = (unsafe { cstr(id) }) else { return kInvalidArgument };
        lock(&self.map).insert(k.to_string(), v);
        kResultOk
    }
    fn get(&self, id: AttrID) -> Option<Attr> {
        // SAFETY: as above.
        let k = unsafe { cstr(id) }?;
        lock(&self.map).get(k).cloned()
    }
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        guard(|| self.set(id, Attr::Int(value)))
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        guard(|| match (self.get(id), value.is_null()) {
            (Some(Attr::Int(v)), false) => {
                // SAFETY: non-null out-pointer.
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        })
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        guard(|| self.set(id, Attr::Float(value)))
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        guard(|| match (self.get(id), value.is_null()) {
            (Some(Attr::Float(v)), false) => {
                // SAFETY: non-null out-pointer.
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        })
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        guard(|| {
            if string.is_null() {
                return kInvalidArgument;
            }
            let mut v = Vec::new();
            for i in 0..65_536 {
                // SAFETY: a NUL-terminated UTF-16 string (contract); read up to its terminator.
                let c = unsafe { *string.add(i) } as u16;
                if c == 0 {
                    break;
                }
                v.push(c);
            }
            self.set(id, Attr::Str(v))
        })
    }
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        guard(|| match self.get(id) {
            Some(Attr::Str(v)) if !string.is_null() && size_in_bytes >= 2 => {
                let cap = (size_in_bytes as usize / 2).saturating_sub(1);
                let n = v.len().min(cap);
                // SAFETY: the caller's buffer holds `size_in_bytes` bytes; we write n + 1 units.
                unsafe {
                    std::ptr::copy_nonoverlapping(v.as_ptr(), string as *mut u16, n);
                    *string.add(n) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        })
    }
    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        guard(|| {
            if data.is_null() && size_in_bytes > 0 {
                return kInvalidArgument;
            }
            let b = if size_in_bytes == 0 {
                Vec::new()
            } else {
                // SAFETY: `data` holds `size_in_bytes` bytes (contract).
                unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec()
            };
            self.set(id, Attr::Bin(b))
        })
    }
    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        guard(|| {
            if data.is_null() || size_in_bytes.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: as above.
            let Some(k) = (unsafe { cstr(id) }) else { return kInvalidArgument };
            let m = lock(&self.map);
            match m.get(k) {
                Some(Attr::Bin(b)) => {
                    // SAFETY: non-null out-pointers; the bytes live in the list until the
                    // attribute changes (the lifetime VST3 gives them).
                    unsafe {
                        *data = b.as_ptr() as *const c_void;
                        *size_in_bytes = b.len() as uint32;
                    }
                    kResultOk
                }
                _ => kResultFalse,
            }
        })
    }
}

pub struct Message {
    id: Mutex<std::ffi::CString>,
    attrs: ComWrapper<Attributes>,
}

impl Default for Message {
    fn default() -> Self {
        Message { id: Mutex::new(std::ffi::CString::default()), attrs: ComWrapper::new(Attributes::default()) }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        lock(&self.id).as_ptr()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        // SAFETY: a NUL-terminated id (contract).
        if let Some(s) = unsafe { cstr(id) } {
            *lock(&self.id) = std::ffi::CString::new(s).unwrap_or_default();
        }
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // the message keeps its own reference; the caller does not release this one
        self.attrs.as_com_ref::<IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

// ---------------------------------------------------------------------------------------------
// Memory streams (component and controller state)

#[derive(Default)]
pub struct MemStream {
    inner: Mutex<(Vec<u8>, usize)>,
}

impl Class for MemStream {
    type Interfaces = (IBStream, ISizeableStream);
}

impl MemStream {
    pub fn with(data: Vec<u8>) -> MemStream {
        MemStream { inner: Mutex::new((data, 0)) }
    }
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut lock(&self.inner).0)
    }
    pub fn rewind(&self) {
        lock(&self.inner).1 = 0;
    }
}

const MAX_STATE: usize = 256 << 20;

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_read: *mut int32) -> tresult {
        guard(|| {
            let mut g = lock(&self.inner);
            let (data, pos) = &mut *g;
            let want = num_bytes.max(0) as usize;
            let n = want.min(data.len().saturating_sub(*pos));
            if n > 0 {
                if buffer.is_null() {
                    return kInvalidArgument;
                }
                // SAFETY: the plug-in's buffer holds `num_bytes` bytes; n <= num_bytes.
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer as *mut u8, n) };
            }
            *pos += n;
            if !num_bytes_read.is_null() {
                // SAFETY: non-null out-pointer.
                unsafe { *num_bytes_read = n as int32 };
            }
            kResultOk
        })
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_bytes_written: *mut int32) -> tresult {
        guard(|| {
            let n = num_bytes.max(0) as usize;
            let mut g = lock(&self.inner);
            let (data, pos) = &mut *g;
            if *pos + n > MAX_STATE {
                return kOutOfMemory;
            }
            if n > 0 {
                if buffer.is_null() {
                    return kInvalidArgument;
                }
                if data.len() < *pos + n {
                    data.resize(*pos + n, 0);
                }
                // SAFETY: the plug-in's buffer holds `num_bytes` bytes.
                unsafe { std::ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr().add(*pos), n) };
            }
            *pos += n;
            if !num_bytes_written.is_null() {
                // SAFETY: non-null out-pointer.
                unsafe { *num_bytes_written = n as int32 };
            }
            kResultOk
        })
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        guard(|| {
            let mut g = lock(&self.inner);
            let len = g.0.len() as i64;
            let base = if mode == IBStream_::IStreamSeekMode_::kIBSeekSet as i32 {
                0
            } else if mode == IBStream_::IStreamSeekMode_::kIBSeekCur as i32 {
                g.1 as i64
            } else if mode == IBStream_::IStreamSeekMode_::kIBSeekEnd as i32 {
                len
            } else {
                return kInvalidArgument;
            };
            let p = base.saturating_add(pos).clamp(0, MAX_STATE as i64);
            g.1 = p as usize;
            if !result.is_null() {
                // SAFETY: non-null out-pointer.
                unsafe { *result = p };
            }
            kResultOk
        })
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *pos = lock(&self.inner).1 as int64 };
        kResultOk
    }
}

impl ISizeableStreamTrait for MemStream {
    unsafe fn getStreamSize(&self, size: *mut int64) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: non-null out-pointer.
        unsafe { *size = lock(&self.inner).0.len() as int64 };
        kResultOk
    }
    unsafe fn setStreamSize(&self, size: int64) -> tresult {
        guard(|| {
            let n = size.clamp(0, MAX_STATE as i64) as usize;
            let mut g = lock(&self.inner);
            g.0.resize(n, 0);
            g.1 = g.1.min(n);
            kResultOk
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter changes

#[derive(Default)]
pub struct Queue {
    pub id: ParamID,
    pub points: Mutex<Vec<(i32, f64)>>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        lock(&self.points).len() as int32
    }
    unsafe fn getPoint(&self, index: int32, sample_offset: *mut int32, value: *mut ParamValue) -> tresult {
        guard(|| {
            let p = lock(&self.points);
            let Some(&(o, v)) = usize::try_from(index).ok().and_then(|i| p.get(i)) else { return kResultFalse };
            if sample_offset.is_null() || value.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: non-null out-pointers.
            unsafe {
                *sample_offset = o;
                *value = v;
            }
            kResultTrue
        })
    }
    unsafe fn addPoint(&self, sample_offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        guard(|| {
            let mut p = lock(&self.points);
            if p.len() >= 4096 {
                return kResultFalse;
            }
            p.push((sample_offset, value));
            if !index.is_null() {
                // SAFETY: non-null out-pointer.
                unsafe { *index = p.len() as int32 - 1 };
            }
            kResultOk
        })
    }
}

/// Parameter changes for one process call: input changes FilmCraft makes, or output changes a
/// plug-in reports (read and discarded).
#[derive(Default)]
pub struct Changes {
    queues: Mutex<Vec<ComWrapper<Queue>>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl Changes {
    /// Set `id` to `value` at the start of the block.
    pub fn set(&self, id: ParamID, value: f64) {
        let mut q = lock(&self.queues);
        if let Some(e) = q.iter().find(|e| e.id == id) {
            *lock(&e.points) = vec![(0, value)];
            return;
        }
        q.push(ComWrapper::new(Queue { id, points: Mutex::new(vec![(0, value)]) }));
    }
    pub fn clear(&self) {
        lock(&self.queues).clear();
    }
    pub fn is_empty(&self) -> bool {
        lock(&self.queues).is_empty()
    }
    /// Every (id, last value) and clear.
    pub fn drain(&self) -> Vec<(ParamID, f64)> {
        let mut q = lock(&self.queues);
        let out = q.iter().filter_map(|e| lock(&e.points).last().map(|&(_, v)| (e.id, v))).collect();
        q.clear();
        out
    }
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        lock(&self.queues).len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        let q = lock(&self.queues);
        usize::try_from(index).ok().and_then(|i| q.get(i)).and_then(|e| e.as_com_ref::<IParamValueQueue>()).map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: non-null in-pointer.
        let id = unsafe { *id };
        let mut q = lock(&self.queues);
        let i = match q.iter().position(|e| e.id == id) {
            Some(i) => i,
            None => {
                if q.len() >= 4096 {
                    return std::ptr::null_mut();
                }
                q.push(ComWrapper::new(Queue { id, points: Mutex::new(Vec::new()) }));
                q.len() - 1
            }
        };
        if !index.is_null() {
            // SAFETY: non-null out-pointer.
            unsafe { *index = i as int32 };
        }
        q.get(i).and_then(|e| e.as_com_ref::<IParamValueQueue>()).map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

/// No MIDI or note events: FilmCraft hosts audio effects.
#[derive(Default)]
pub struct NoEvents;

impl Class for NoEvents {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for NoEvents {
    unsafe fn getEventCount(&self) -> int32 {
        0
    }
    unsafe fn getEvent(&self, _index: int32, _e: *mut Event) -> tresult {
        kResultFalse
    }
    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        kResultOk
    }
}

// ---------------------------------------------------------------------------------------------
// Component handler (edits made in a plug-in's own window)

pub type EditSink = Arc<dyn Fn(ParamID, f64) + Send + Sync>;

#[derive(Default)]
pub struct Handler {
    pub sink: Mutex<Option<EditSink>>,
    /// Set when the plug-in asks the host to re-read its state or latency.
    pub restart: std::sync::atomic::AtomicI32,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        guard(|| {
            let sink = lock(&self.sink).clone();
            if let Some(s) = sink {
                s(id, value);
            }
            kResultOk
        })
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart.fetch_or(flags, std::sync::atomic::Ordering::Relaxed);
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    #[test]
    fn streams_read_write_seek() {
        let s = ComWrapper::new(MemStream::default());
        let p: ComPtr<IBStream> = s.to_com_ptr().unwrap();
        let mut data = *b"hello";
        let mut n = 0;
        // SAFETY: test calls with valid buffers.
        unsafe {
            assert_eq!(p.write(data.as_mut_ptr() as *mut c_void, 5, &mut n), kResultOk);
            assert_eq!(n, 5);
            let mut r = 0i64;
            assert_eq!(p.seek(1, IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut r), kResultOk);
            let mut out = [0u8; 8];
            assert_eq!(p.read(out.as_mut_ptr() as *mut c_void, 8, &mut n), kResultOk);
            assert_eq!((&out[..n as usize], n), (&b"ello"[..], 4));
            // reading past the end gives 0 bytes, not garbage
            assert_eq!(p.read(out.as_mut_ptr() as *mut c_void, 8, &mut n), kResultOk);
            assert_eq!(n, 0);
            // a null buffer is refused
            assert_eq!(p.write(std::ptr::null_mut(), 4, &mut n), kInvalidArgument);
        }
        assert_eq!(s.take(), b"hello");
    }

    #[test]
    fn host_makes_messages_with_attributes() {
        let host = host_context();
        assert!(!host.is_null());
        // SAFETY: our own host object.
        let app = unsafe { vst3::ComRef::<IHostApplication>::from_raw(host as *mut IHostApplication) }.unwrap();
        let mut obj: *mut c_void = std::ptr::null_mut();
        let mut cid = tuid(IMessage::IID);
        let mut iid = tuid(IMessage::IID);
        // SAFETY: valid ids and out-pointer.
        assert_eq!(unsafe { app.createInstance(&mut cid, &mut iid, &mut obj) }, kResultOk);
        // SAFETY: a message we just created, owned here.
        let msg = unsafe { ComPtr::<IMessage>::from_raw(obj as *mut IMessage) }.unwrap();
        // SAFETY: valid strings.
        unsafe {
            msg.setMessageID(c"meter".as_ptr());
            assert_eq!(CStr::from_ptr(msg.getMessageID()).to_str().unwrap(), "meter");
            let attrs = vst3::ComRef::<IAttributeList>::from_raw(msg.getAttributes()).unwrap();
            assert_eq!(attrs.setFloat(c"level".as_ptr(), -6.0), kResultOk);
            let mut v = 0.0;
            assert_eq!(attrs.getFloat(c"level".as_ptr(), &mut v), kResultOk);
            assert_eq!(v, -6.0);
            assert_eq!(attrs.getInt(c"level".as_ptr(), std::ptr::null_mut()), kResultFalse);
        }
    }

    fn tuid(g: [u8; 16]) -> TUID {
        g.map(|b| b as c_char)
    }
}
