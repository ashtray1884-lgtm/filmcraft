//! A VST3 plug-in's own window (Windows).
//!
//! Each open editor runs on a thread of its own, the plug-in's UI thread: a separate instance of
//! the plug-in (component and controller, the clip's state and parameters restored) whose view is
//! attached to a native window. Knobs turned in the window come back as parameter updates in
//! FilmCraft's units (so they are saved in the project, undoable and keyframable like any other
//! parameter); every half second after a change, and when the window closes, the plug-in's full
//! state is read back too (settings that are not parameters, presets loaded in the window).
//! Parameters changed in Effect Controls are sent to the window.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::catalog::PluginInfo;
use super::fx::{encode_state, kind_of, param_id};
use super::host::Changes;
use super::module::{Instance, Scratch, cid_from_hex, open};

/// What an open editor reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// FilmCraft parameter `p<id>` now has this value (plug-in units; index for lists, 0/1 for
    /// switches).
    Param(String, f64),
    /// The plug-in's full state (`__state` text).
    State(String),
    /// The window closed (with the reason, if it failed).
    Closed(Option<String>),
}

struct Shared {
    updates: Mutex<Vec<Update>>,
    inbox: Mutex<Vec<(String, f64)>>,
    hwnd: AtomicUsize,
    open: AtomicBool,
}

/// A handle on an open editor window.
pub struct Editor {
    shared: Arc<Shared>,
}

impl Editor {
    /// Updates since the last call.
    pub fn poll(&self) -> Vec<Update> {
        std::mem::take(&mut *self.shared.updates.lock().unwrap_or_else(PoisonError::into_inner))
    }
    pub fn is_open(&self) -> bool {
        self.shared.open.load(Ordering::Relaxed)
    }
    /// Show a parameter changed elsewhere in the window.
    pub fn set_param(&self, id: &str, value: f64) {
        self.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner).push((id.to_string(), value));
        let h = self.shared.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: posting to a window handle is safe even if the window has gone.
            unsafe { PostMessageW(h as HWND, WM_APP, 0, 0) };
        }
    }
    pub fn close(&self) {
        let h = self.shared.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: as above.
            unsafe { PostMessageW(h as HWND, WM_CLOSE, 0, 0) };
        }
    }
    /// Bring the window to the front.
    pub fn focus(&self) {
        let h = self.shared.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: as above.
            unsafe {
                ShowWindow(h as HWND, SW_SHOW);
                SetForegroundWindow(h as HWND);
            }
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Copy)]
enum PKind {
    Plain,
    /// The plug-in's normalized value itself (its conversions do not invert).
    Normalized,
    Toggle,
    Steps(i32),
}

/// Plug-in parameter id ↔ FilmCraft id and kind.
fn kinds(info: &PluginInfo) -> HashMap<u32, (String, PKind)> {
    info.params
        .iter()
        .filter(|p| p.visible())
        .map(|p| {
            let (toggle, steps) = kind_of(p);
            let k = if toggle {
                PKind::Toggle
            } else if steps > 0 {
                PKind::Steps(steps)
            } else if p.plain_ok {
                PKind::Plain
            } else {
                PKind::Normalized
            };
            (p.id, (param_id(p.id), k))
        })
        .collect()
}

fn to_normalized(inst: &Instance, pid: u32, kind: PKind, v: f64) -> f64 {
    match kind {
        PKind::Toggle => (v >= 0.5) as i32 as f64,
        PKind::Steps(n) => (v.round() / n as f64).clamp(0.0, 1.0),
        PKind::Plain => inst.to_normalized(pid, v),
        PKind::Normalized => v.clamp(0.0, 1.0),
    }
}

fn from_normalized(c: &ComPtr<IEditController>, pid: u32, kind: PKind, norm: f64) -> f64 {
    match kind {
        PKind::Toggle => (norm >= 0.5) as i32 as f64,
        PKind::Steps(n) => (norm * n as f64).round(),
        // SAFETY: plain conversion on the controller (on its UI thread).
        PKind::Plain => unsafe { c.normalizedParamToPlain(pid, norm) },
        PKind::Normalized => norm,
    }
}

/// The view's frame: resizes the window when the plug-in asks.
struct Frame {
    hwnd: AtomicUsize,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        super::host::guard(|| {
            let h = self.hwnd.load(Ordering::Relaxed);
            if h == 0 || new_size.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: the plug-in's rectangle (non-null).
            let r = unsafe { *new_size };
            let mut wr = RECT { left: 0, top: 0, right: (r.right - r.left).max(1), bottom: (r.bottom - r.top).max(1) };
            // SAFETY: plain window calls on our window, on its thread.
            unsafe {
                AdjustWindowRectEx(&mut wr, STYLE, 0, 0);
                SetWindowPos(h as HWND, std::ptr::null_mut(), 0, 0, wr.right - wr.left, wr.bottom - wr.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            // SAFETY: the view asking to be resized.
            if let Some(v) = unsafe { vst3::ComRef::<IPlugView>::from_raw(view) } {
                // SAFETY: as above.
                let _ = unsafe { v.onSize(new_size) };
            }
            kResultTrue
        })
    }
}

/// The window's state, on its thread.
struct Win {
    inst: Instance,
    view: ComPtr<IPlugView>,
    shared: Arc<Shared>,
    kinds: HashMap<u32, (String, PKind)>,
    pending: Arc<ComWrapper<Changes>>,
    dirty: Arc<AtomicBool>,
    scratch: Scratch,
}

thread_local! {
    static WIN: std::cell::RefCell<Option<Win>> = const { std::cell::RefCell::new(None) };
}

impl Win {
    /// Parameters changed in FilmCraft: into the controller (the window shows them) and the
    /// component (its state includes them).
    fn take_inbox(&mut self) {
        let items = std::mem::take(&mut *self.shared.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        let Some(c) = self.inst.controller.clone() else { return };
        for (id, v) in items {
            if let Some((&pid, &(_, k))) = self.kinds.iter().find(|(_, (fid, _))| *fid == id) {
                let n = to_normalized(&self.inst, pid, k, v);
                // SAFETY: plain call on the controller, on its UI thread.
                let _ = unsafe { c.setParamNormalized(pid, n) };
                self.pending.set(pid, n);
            }
        }
    }

    /// Bring the component up to date with the window's edits and report the full state.
    fn sync_state(&mut self) {
        let changes = ComWrapper::new(Changes::default());
        for (pid, v) in self.pending.drain() {
            changes.set(pid, v);
        }
        self.inst.process(&mut [], &changes, &mut self.scratch);
        let s = encode_state(&self.inst.component_state(), &self.inst.controller_state());
        self.shared.updates.lock().unwrap_or_else(PoisonError::into_inner).push(Update::State(s));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        WIN.with(|w| {
            let mut g = w.borrow_mut();
            let win = g.as_mut()?;
            match msg {
                WM_TIMER | WM_APP => {
                    win.take_inbox();
                    if win.dirty.swap(false, Ordering::Relaxed) {
                        win.sync_state();
                    }
                    Some(0)
                }
                WM_SIZE => {
                    // SAFETY: plain query on the view.
                    if unsafe { win.view.canResize() } == kResultTrue {
                        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                        // SAFETY: our window.
                        unsafe { GetClientRect(hwnd, &mut r) };
                        let mut vr = ViewRect { left: 0, top: 0, right: r.right - r.left, bottom: r.bottom - r.top };
                        // SAFETY: a valid rectangle.
                        let _ = unsafe { win.view.onSize(&mut vr) };
                    }
                    None
                }
                WM_CLOSE => {
                    if win.dirty.swap(false, Ordering::Relaxed) {
                        win.sync_state();
                    }
                    // SAFETY: detach the view before the window goes.
                    unsafe {
                        let _ = win.view.removed();
                        let _ = win.view.setFrame(std::ptr::null_mut());
                        DestroyWindow(hwnd);
                    }
                    Some(0)
                }
                WM_DESTROY => {
                    // SAFETY: ends this thread's message loop.
                    unsafe { PostQuitMessage(0) };
                    Some(0)
                }
                _ => None,
            }
        })
    }));
    match handled {
        Ok(Some(r)) => r,
        // SAFETY: the default handling of a message for our window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open `info`'s window with `state` and `params` (FilmCraft id → value) restored.
pub fn open_editor(info: &PluginInfo, state: Option<(Vec<u8>, Vec<u8>)>, params: Vec<(String, f64)>) -> Result<Editor, String> {
    let shared = Arc::new(Shared { updates: Mutex::new(Vec::new()), inbox: Mutex::new(Vec::new()), hwnd: AtomicUsize::new(0), open: AtomicBool::new(true) });
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let info = info.clone();
    let sh = shared.clone();
    std::thread::Builder::new()
        .name(format!("vst3-editor {}", info.name))
        .spawn(move || {
            let r = run(&info, state, params, &sh, &tx);
            if let Err(e) = &r {
                let _ = tx.send(Err(e.clone()));
            }
            sh.open.store(false, Ordering::Relaxed);
            sh.hwnd.store(0, Ordering::Relaxed);
            sh.updates.lock().unwrap_or_else(PoisonError::into_inner).push(Update::Closed(r.err()));
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(Ok(())) => Ok(Editor { shared }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the plug-in's window did not open".into()),
    }
}

fn run(
    info: &PluginInfo,
    state: Option<(Vec<u8>, Vec<u8>)>,
    params: Vec<(String, f64)>,
    shared: &Arc<Shared>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    // SAFETY: OLE on this thread (plug-in windows use drag and drop and the clipboard).
    unsafe { windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null()) };
    let m = open(Path::new(&info.bundle))?;
    let cid = cid_from_hex(&info.cid).ok_or("bad class id")?;
    let mut inst = Instance::new(&m, &cid, 48_000.0, 2, 1024)?;
    if let Some((c, k)) = &state {
        inst.set_state(c, k);
    }
    let kinds = kinds(info);
    let controller = inst.controller.clone().ok_or("the plug-in has no controller")?;
    let pending = Arc::new(ComWrapper::new(Changes::default()));
    // FilmCraft's values into the window and the component
    for (id, v) in &params {
        if let Some((&pid, &(_, k))) = kinds.iter().find(|(_, (fid, _))| fid == id) {
            let n = to_normalized(&inst, pid, k, *v);
            // SAFETY: plain call on the controller.
            let _ = unsafe { controller.setParamNormalized(pid, n) };
            pending.set(pid, n);
        }
    }
    // edits in the window → FilmCraft
    let dirty = Arc::new(AtomicBool::new(false));
    {
        let (sh, kinds, ctrl, pending, dirty) = (shared.clone(), kinds.clone(), controller.clone(), pending.clone(), dirty.clone());
        *inst.handler.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(move |pid: u32, norm: f64| {
            pending.set(pid, norm);
            dirty.store(true, Ordering::Relaxed);
            if let Some((fid, k)) = kinds.get(&pid) {
                let v = from_normalized(&ctrl, pid, *k, norm);
                if v.is_finite() {
                    sh.updates.lock().unwrap_or_else(PoisonError::into_inner).push(Update::Param(fid.clone(), v));
                }
            }
        }));
    }
    // SAFETY: the standard view name; the view comes with a reference for us.
    let view = unsafe { ComPtr::<IPlugView>::from_raw(controller.createView(c"editor".as_ptr())) }.ok_or("the plug-in has no window")?;
    // SAFETY: plain query.
    if unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) } != kResultTrue {
        return Err("the plug-in's window does not support Windows".into());
    }
    let mut vr = ViewRect { left: 0, top: 0, right: 600, bottom: 400 };
    // SAFETY: a valid rectangle.
    unsafe { view.getSize(&mut vr) };
    let (w, h) = ((vr.right - vr.left).clamp(100, 8000), (vr.bottom - vr.top).clamp(60, 8000));
    let class = wide("FilmCraftPluginWindow");
    let title = wide(&format!("{} — FilmCraft", info.name));
    // SAFETY: Win32 window creation with our class and procedure; strings live for the calls.
    let hwnd = unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // a second registration fails harmlessly (the class exists)
        RegisterClassW(&wc);
        let mut wr = RECT { left: 0, top: 0, right: w, bottom: h };
        AdjustWindowRectEx(&mut wr, STYLE, 0, 0);
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            wr.right - wr.left,
            wr.bottom - wr.top,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("could not create the plug-in window".into());
    }
    let frame = ComWrapper::new(Frame { hwnd: AtomicUsize::new(hwnd as usize) });
    if let Some(f) = frame.as_com_ref::<IPlugFrame>() {
        // SAFETY: our frame outlives the view's attachment.
        let _ = unsafe { view.setFrame(f.as_ptr()) };
    }
    // SAFETY: attach the view to our window (the plug-in creates its child window in it).
    if unsafe { view.attached(hwnd, kPlatformTypeHWND) } != kResultOk {
        // SAFETY: our window.
        unsafe { DestroyWindow(hwnd) };
        return Err("the plug-in could not open its window here".into());
    }
    shared.hwnd.store(hwnd as usize, Ordering::Relaxed);
    WIN.with(|wc| *wc.borrow_mut() = Some(Win { inst, view, shared: shared.clone(), kinds, pending, dirty, scratch: Scratch::default() }));
    // SAFETY: show our window and tick twice a second for state reads.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetTimer(hwnd, 1, 500, None);
    }
    let _ = ready.send(Ok(()));
    // SAFETY: this thread's message loop (the plug-in's UI thread).
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // the window is gone: release the view and the instance on this thread
    WIN.with(|wc| wc.borrow_mut().take());
    drop(frame);
    Ok(())
}

/// The plug-in windows FilmCraft's interface opens.
pub struct Windows;

impl filmcraft_render::external::PluginWindow for Editor {
    fn poll(&self) -> Vec<filmcraft_render::external::WindowUpdate> {
        use filmcraft_render::external::WindowUpdate as W;
        Editor::poll(self)
            .into_iter()
            .map(|u| match u {
                Update::Param(k, v) => W::Param(k, v),
                Update::State(s) => W::State(s),
                Update::Closed(e) => W::Closed(e),
            })
            .collect()
    }
    fn is_open(&self) -> bool {
        Editor::is_open(self)
    }
    fn set_param(&self, id: &str, value: f64) {
        Editor::set_param(self, id, value);
    }
    fn focus(&self) {
        Editor::focus(self);
    }
    fn close(&self) {
        Editor::close(self);
    }
}

impl filmcraft_render::external::PluginWindows for Windows {
    fn has_window(&self, effect_id: &str) -> bool {
        effect_id.strip_prefix("vst3:").and_then(|c| super::HOST.get().and_then(|h| h.info(c))).is_some_and(|i| i.has_editor)
    }
    fn open(&self, effect_id: &str, state: Option<&str>, params: Vec<(String, f64)>) -> Result<Box<dyn filmcraft_render::external::PluginWindow>, String> {
        let cid = effect_id.strip_prefix("vst3:").ok_or("not an audio plug-in")?;
        let info = super::HOST.get().and_then(|h| h.info(cid)).ok_or("the plug-in is not installed")?;
        let st = state.and_then(super::fx::decode_state);
        open_editor(&info, st, params).map(|e| Box::new(e) as Box<dyn filmcraft_render::external::PluginWindow>)
    }
}
