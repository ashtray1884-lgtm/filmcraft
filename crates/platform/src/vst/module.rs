//! VST3 modules (the plug-in binaries) and plug-in instances: component, audio processor and edit
//! controller, wired the way the VST3 SDK's host documentation describes.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

use super::host::{Changes, Handler, MemStream, NoEvents, host_context, string8, string128};

/// A loaded module (never unloaded: plug-ins may keep threads and windows alive).
pub struct Module {
    _lib: libloading::Library,
    pub factory: ComPtr<IPluginFactory>,
}

// SAFETY: the factory is free-threaded per the VST3 SDK (hosts create instances from any thread).
unsafe impl Send for Module {}
// SAFETY: as above.
unsafe impl Sync for Module {}

static MODULES: Mutex<Option<HashMap<String, Arc<Module>>>> = Mutex::new(None);

/// The binary inside a `.vst3` bundle for this system, or the path itself (a single-file
/// `.vst3` on Windows).
pub fn binary_of(bundle: &Path) -> Option<std::path::PathBuf> {
    if bundle.is_file() {
        return Some(bundle.to_path_buf());
    }
    let name = bundle.file_stem()?.to_string_lossy().to_string();
    let (dir, ext) = if cfg!(windows) { ("x86_64-win", "vst3") } else { ("x86_64-linux", "so") };
    let p = bundle.join("Contents").join(dir).join(format!("{name}.{ext}"));
    if p.is_file() {
        return Some(p);
    }
    std::fs::read_dir(bundle.join("Contents").join(dir)).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)))
}

/// Load the module of `bundle` once per process.
pub fn open(bundle: &Path) -> Result<Arc<Module>, String> {
    let key = bundle.to_string_lossy().to_string();
    let mut g = MODULES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(m) = g.as_ref().and_then(|m| m.get(&key)) {
        return Ok(m.clone());
    }
    let bin = binary_of(bundle).ok_or_else(|| format!("no plug-in binary in {}", bundle.display()))?;
    // SAFETY: loading a plug-in runs its initialisers; that is what hosting a plug-in means. Its
    // own folder is searched for its dependencies on Windows.
    let lib = unsafe {
        #[cfg(windows)]
        {
            libloading::os::windows::Library::load_with_flags(&bin, 0x0000_0008).map(libloading::Library::from)
        }
        #[cfg(not(windows))]
        {
            libloading::Library::new(&bin)
        }
    }
    .map_err(|e| format!("could not load {}: {e}", bin.display()))?;
    // module entry: InitDll (Windows) / ModuleEntry (Linux), before anything else
    #[cfg(windows)]
    // SAFETY: the VST3 module entry signature (`bool InitDll()`).
    if let Ok(init) = unsafe { lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") } {
        // SAFETY: as above.
        if !unsafe { init() } {
            return Err("the plug-in's InitDll failed".into());
        }
    }
    #[cfg(not(windows))]
    // SAFETY: the VST3 module entry signature (`bool ModuleEntry(void*)`).
    if let Ok(init) = unsafe { lib.get::<unsafe extern "system" fn(*mut c_void) -> bool>(b"ModuleEntry\0") } {
        // SAFETY: as above; the handle argument is optional for plug-ins built with the SDK.
        if !unsafe { init(std::ptr::null_mut()) } {
            return Err("the plug-in's ModuleEntry failed".into());
        }
    }
    // SAFETY: `IPluginFactory* GetPluginFactory()`.
    let get = unsafe { lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0") }
        .map_err(|_| "not a VST3 plug-in (no GetPluginFactory)".to_string())?;
    // SAFETY: as above; the returned factory carries a reference for us.
    let factory = unsafe { ComPtr::from_raw(get()) }.ok_or("the plug-in has no factory")?;
    if let Some(f3) = factory.cast::<IPluginFactory3>() {
        // SAFETY: our host context lives for the process.
        let _ = unsafe { f3.setHostContext(host_context()) };
    }
    let m = Arc::new(Module { _lib: lib, factory });
    g.get_or_insert_with(HashMap::new).insert(key, m.clone());
    Ok(m)
}

/// One class in a module.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub cid: [u8; 16],
    pub category: String,
    pub name: String,
    pub sub_categories: String,
    pub vendor: String,
    pub version: String,
}

pub fn cid_hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn cid_from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Module {
    pub fn classes(&self) -> Vec<ClassInfo> {
        let f2 = self.factory.cast::<IPluginFactory2>();
        let mut vendor = String::new();
        // SAFETY: zeroed POD out-structs for the factory's info calls.
        let mut fi: PFactoryInfo = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { self.factory.getFactoryInfo(&mut fi) } == kResultOk {
            vendor = string8(&fi.vendor);
        }
        // SAFETY: the factory's own count.
        let n = unsafe { self.factory.countClasses() }.clamp(0, 4096);
        let mut out = Vec::new();
        for i in 0..n {
            if let Some(f2) = &f2 {
                // SAFETY: as above.
                let mut c: PClassInfo2 = unsafe { std::mem::zeroed() };
                // SAFETY: index below the count; a valid out-struct.
                if unsafe { f2.getClassInfo2(i, &mut c) } == kResultOk {
                    let v = string8(&c.vendor);
                    out.push(ClassInfo {
                        cid: c.cid.map(|b| b as u8),
                        category: string8(&c.category),
                        name: string8(&c.name),
                        sub_categories: string8(&c.subCategories),
                        vendor: if v.is_empty() { vendor.clone() } else { v },
                        version: string8(&c.version),
                    });
                    continue;
                }
            }
            // SAFETY: as above.
            let mut c: PClassInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; a valid out-struct.
            if unsafe { self.factory.getClassInfo(i, &mut c) } == kResultOk {
                out.push(ClassInfo {
                    cid: c.cid.map(|b| b as u8),
                    category: string8(&c.category),
                    name: string8(&c.name),
                    sub_categories: String::new(),
                    vendor: vendor.clone(),
                    version: String::new(),
                });
            }
        }
        out
    }

    fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = std::ptr::null_mut();
        let iid = I::IID;
        // SAFETY: class and interface ids are 16 bytes; `obj` is our out-pointer.
        let r = unsafe { self.factory.createInstance(cid.as_ptr() as *const c_char, iid.as_ptr() as *const c_char, &mut obj) };
        if r != kResultOk {
            return None;
        }
        // SAFETY: on success the factory returned an `I` with a reference for us.
        unsafe { ComPtr::from_raw(obj as *mut I) }
    }
}

/// One parameter of a plug-in, as its controller describes it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub steps: i32,
    pub default: f64,
    pub flags: i32,
    /// Plain values at normalized 0, 1 and the default (as the controller converts them).
    pub plain_min: f64,
    pub plain_max: f64,
    pub plain_default: f64,
    /// Step names, for lists with a few steps.
    pub choices: Vec<String>,
    /// The controller converts plain values back to normalized ones exactly: FilmCraft shows
    /// the plug-in's units. Otherwise the parameter is shown 0…1 as the plug-in stores it.
    #[serde(default)]
    pub plain_ok: bool,
}

impl ParamInfo {
    pub fn visible(&self) -> bool {
        let f = self.flags;
        f & ParameterInfo_::ParameterFlags_::kIsHidden == 0
            && f & ParameterInfo_::ParameterFlags_::kIsReadOnly == 0
            && f & ParameterInfo_::ParameterFlags_::kIsBypass == 0
            && f & ParameterInfo_::ParameterFlags_::kIsProgramChange == 0
    }
}

/// Speaker arrangement for `channels` interleaved FilmCraft channels.
pub fn arrangement(channels: usize) -> SpeakerArrangement {
    match channels {
        1 => SpeakerArr::kMono,
        6 => SpeakerArr::k51,
        _ => SpeakerArr::kStereo,
    }
}

fn channels_of(a: SpeakerArrangement) -> usize {
    a.count_ones() as usize
}

/// A running plug-in: initialised, buses arranged, active and processing.
pub struct Instance {
    pub component: ComPtr<IComponent>,
    pub processor: ComPtr<IAudioProcessor>,
    pub controller: Option<ComPtr<IEditController>>,
    separate: bool,
    points: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    pub handler: ComWrapper<Handler>,
    /// Channels of every audio input / output bus (the first is FilmCraft's).
    pub ins: Vec<usize>,
    pub outs: Vec<usize>,
    pub max_block: usize,
    pub sample_rate: f64,
    _module: Arc<Module>,
}

// SAFETY: VST3 objects are used by one thread at a time here (an instance belongs to one audio
// chain, or to the editor, never both); the SDK allows calls from any thread in that case.
unsafe impl Send for Instance {}

impl Instance {
    /// Create, initialise and activate class `cid` of `module` for `channels` channels.
    pub fn new(module: &Arc<Module>, cid: &[u8; 16], sample_rate: f64, channels: usize, max_block: usize) -> Result<Instance, String> {
        let component = module.create::<IComponent>(cid).ok_or("the plug-in could not be created")?;
        // SAFETY: our host context lives for the process.
        if unsafe { component.initialize(host_context()) } != kResultOk {
            return Err("the plug-in failed to initialise".into());
        }
        let processor = component.cast::<IAudioProcessor>().ok_or("not an audio processor")?;
        let handler = ComWrapper::new(Handler::default());
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => {
                let mut cid2: TUID = [0; 16];
                // SAFETY: a valid out-array.
                let ok = unsafe { component.getControllerClassId(&mut cid2) } == kResultOk;
                let cid2 = cid2.map(|b| b as u8);
                let c = if ok && cid2 != [0; 16] { module.create::<IEditController>(&cid2) } else { None };
                let c = c.filter(|c| {
                    // SAFETY: as above.
                    let r = unsafe { c.initialize(host_context()) };
                    r == kResultOk
                });
                (c, true)
            }
        };
        let mut me = Instance {
            component,
            processor,
            controller,
            separate,
            points: None,
            handler,
            ins: Vec::new(),
            outs: Vec::new(),
            max_block: max_block.clamp(32, 8192),
            sample_rate,
            _module: module.clone(),
        };
        if let Some(c) = &me.controller {
            if let Some(h) = me.handler.as_com_ref::<IComponentHandler>() {
                // SAFETY: our handler lives as long as the instance.
                let _ = unsafe { c.setComponentHandler(h.as_ptr()) };
            }
            if me.separate
                && let (Some(a), Some(b)) = (me.component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>())
            {
                // SAFETY: both connection points live as long as the instance.
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                me.points = Some((a, b));
            }
            // the controller starts from the processor's state
            let s = me.component_state();
            if !s.is_empty() {
                me.push_controller_state(&s);
            }
        }
        me.arrange(channels)?;
        me.start()?;
        Ok(me)
    }

    fn arrange(&mut self, channels: usize) -> Result<(), String> {
        let count = |dir| {
            // SAFETY: plain query.
            unsafe { self.component.getBusCount(MediaTypes_::kAudio as i32, dir) }.clamp(0, 16) as usize
        };
        let (ni, no) = (count(BusDirections_::kInput as i32), count(BusDirections_::kOutput as i32));
        if no == 0 {
            return Err("the plug-in has no audio output".into());
        }
        let current = |dir, i: usize| {
            let mut a: SpeakerArrangement = 0;
            // SAFETY: a valid out-pointer; index below the bus count.
            let ok = unsafe { self.processor.getBusArrangement(dir, i as i32, &mut a) } == kResultOk;
            if ok { a } else { SpeakerArr::kStereo }
        };
        let want = arrangement(channels);
        let mut ins: Vec<SpeakerArrangement> = (0..ni).map(|i| if i == 0 { want } else { current(BusDirections_::kInput as i32, i) }).collect();
        let mut outs: Vec<SpeakerArrangement> = (0..no).map(|i| if i == 0 { want } else { current(BusDirections_::kOutput as i32, i) }).collect();
        // SAFETY: arrays of the stated lengths.
        let ok = unsafe { self.processor.setBusArrangements(ins.as_mut_ptr(), ni as i32, outs.as_mut_ptr(), no as i32) } == kResultOk;
        if !ok {
            // take what the plug-in has (FilmCraft maps channels onto it)
            ins = (0..ni).map(|i| current(BusDirections_::kInput as i32, i)).collect();
            outs = (0..no).map(|i| current(BusDirections_::kOutput as i32, i)).collect();
        }
        self.ins = ins.iter().map(|&a| channels_of(a)).collect();
        self.outs = outs.iter().map(|&a| channels_of(a)).collect();
        // main buses on, side chains off
        for (dir, n) in [(BusDirections_::kInput as i32, ni), (BusDirections_::kOutput as i32, no)] {
            for i in 0..n {
                // SAFETY: index below the bus count.
                let _ = unsafe { self.component.activateBus(MediaTypes_::kAudio as i32, dir, i as i32, (i == 0) as TBool) };
            }
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), String> {
        // SAFETY: plain query.
        if unsafe { self.processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) } != kResultOk {
            return Err("the plug-in cannot process 32-bit audio".into());
        }
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: self.max_block as i32,
            sampleRate: self.sample_rate,
        };
        // SAFETY: a valid setup struct.
        if unsafe { self.processor.setupProcessing(&mut setup) } != kResultOk {
            return Err("the plug-in refused the sample rate or block size".into());
        }
        // SAFETY: plain state changes, in the SDK's order.
        unsafe {
            if self.component.setActive(1) != kResultOk {
                return Err("the plug-in could not be activated".into());
            }
            let _ = self.processor.setProcessing(1);
        }
        Ok(())
    }

    fn stop(&self) {
        // SAFETY: plain state changes, in the SDK's order.
        unsafe {
            let _ = self.processor.setProcessing(0);
            let _ = self.component.setActive(0);
        }
    }

    /// Clear tails and internal state (delay lines, envelopes): deactivate and activate again.
    pub fn reset(&mut self) {
        self.stop();
        let _ = self.start();
    }

    pub fn latency(&self) -> usize {
        // SAFETY: plain query.
        (unsafe { self.processor.getLatencySamples() } as usize).min(1 << 20)
    }

    pub fn tail(&self) -> usize {
        // SAFETY: plain query.
        (unsafe { self.processor.getTailSamples() } as usize).min(1 << 24)
    }

    pub fn component_state(&self) -> Vec<u8> {
        let s = ComWrapper::new(MemStream::default());
        let Some(p) = s.as_com_ref::<IBStream>() else { return Vec::new() };
        // SAFETY: our stream lives for the call.
        if unsafe { self.component.getState(p.as_ptr()) } != kResultOk {
            return Vec::new();
        }
        s.take()
    }

    pub fn controller_state(&self) -> Vec<u8> {
        let Some(c) = &self.controller else { return Vec::new() };
        let s = ComWrapper::new(MemStream::default());
        let Some(p) = s.as_com_ref::<IBStream>() else { return Vec::new() };
        // SAFETY: our stream lives for the call.
        if unsafe { c.getState(p.as_ptr()) } != kResultOk {
            return Vec::new();
        }
        s.take()
    }

    fn push_controller_state(&self, component: &[u8]) {
        let Some(c) = &self.controller else { return };
        let s = ComWrapper::new(MemStream::with(component.to_vec()));
        if let Some(p) = s.as_com_ref::<IBStream>() {
            // SAFETY: our stream lives for the call.
            let _ = unsafe { c.setComponentState(p.as_ptr()) };
        }
    }

    /// Restore a saved state (component, then controller).
    pub fn set_state(&mut self, component: &[u8], controller: &[u8]) {
        if !component.is_empty() {
            let s = ComWrapper::new(MemStream::with(component.to_vec()));
            if let Some(p) = s.as_com_ref::<IBStream>() {
                self.stop();
                // SAFETY: our stream lives for the call; the plug-in is inactive as the SDK asks.
                let _ = unsafe { self.component.setState(p.as_ptr()) };
                let _ = self.start();
            }
            self.push_controller_state(component);
        }
        if !controller.is_empty()
            && let Some(c) = &self.controller
        {
            let s = ComWrapper::new(MemStream::with(controller.to_vec()));
            if let Some(p) = s.as_com_ref::<IBStream>() {
                // SAFETY: as above.
                let _ = unsafe { c.setState(p.as_ptr()) };
            }
        }
    }

    pub fn params(&self) -> Vec<ParamInfo> {
        let Some(c) = &self.controller else { return Vec::new() };
        // SAFETY: plain query.
        let n = unsafe { c.getParameterCount() }.clamp(0, 20_000);
        let mut out = Vec::new();
        for i in 0..n {
            // SAFETY: a zeroed POD out-struct.
            let mut p: ParameterInfo = unsafe { std::mem::zeroed() };
            // SAFETY: index below the count; valid out-struct.
            if unsafe { c.getParameterInfo(i, &mut p) } != kResultOk {
                continue;
            }
            // SAFETY: plain conversions on the controller.
            let plain = |v: f64| unsafe { c.normalizedParamToPlain(p.id, v) };
            let steps = p.stepCount.max(0);
            let choices = if (2..=64).contains(&steps) {
                (0..=steps)
                    .map(|k| {
                        // SAFETY: a zeroed String128 out-parameter.
                        let mut s: String128 = unsafe { std::mem::zeroed() };
                        // SAFETY: valid out-parameter.
                        let ok = unsafe { c.getParamStringByValue(p.id, k as f64 / steps as f64, &mut s) } == kResultOk;
                        let t = if ok { string128(&s) } else { String::new() };
                        if t.trim().is_empty() { format!("{k}") } else { t }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let fin = |v: f64, d: f64| if v.is_finite() { v } else { d };
            // SAFETY: plain conversions on the controller.
            let back = |v: f64| unsafe { c.plainParamToNormalized(p.id, plain(v)) };
            let plain_ok = [0.0, 0.25, 0.5, 0.75, 1.0].iter().all(|&x| (back(x) - x).abs() < 1e-4) && (plain(1.0) - plain(0.0)).abs() > 1e-9;
            // the value the plug-in starts with (some declare a different "default")
            // SAFETY: plain query on the controller.
            let start = fin(unsafe { c.getParamNormalized(p.id) }, p.defaultNormalizedValue).clamp(0.0, 1.0);
            out.push(ParamInfo {
                id: p.id,
                title: string128(&p.title),
                units: string128(&p.units),
                steps,
                default: start,
                flags: p.flags,
                plain_min: fin(plain(0.0), 0.0),
                plain_max: fin(plain(1.0), 1.0),
                plain_default: fin(plain(start), 0.0),
                choices,
                plain_ok,
            });
        }
        out
    }

    /// Plain → normalized through the controller (identity without one).
    pub fn to_normalized(&self, id: u32, plain: f64) -> f64 {
        let v = match &self.controller {
            // SAFETY: plain conversion on the controller.
            Some(c) => unsafe { c.plainParamToNormalized(id, plain) },
            None => plain,
        };
        if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
    }

    /// Tell the controller `id` is now `norm` (the processor gets it through parameter changes).
    pub fn set_normalized(&self, id: u32, norm: f64) {
        if let Some(c) = &self.controller {
            // SAFETY: plain call on the controller.
            let _ = unsafe { c.setParamNormalized(id, norm) };
        }
    }

    /// The controller's current normalized value of `id`.
    pub fn normalized(&self, id: u32) -> Option<f64> {
        // SAFETY: plain query on the controller.
        let v = unsafe { self.controller.as_ref()?.getParamNormalized(id) };
        v.is_finite().then_some(v)
    }

    pub fn has_editor(&self) -> bool {
        let Some(c) = &self.controller else { return false };
        // SAFETY: the standard view name; the returned view carries a reference we release.
        let v = unsafe { ComPtr::<IPlugView>::from_raw(c.createView(c"editor".as_ptr())) };
        v.is_some()
    }

    /// Process `chans` in place (FilmCraft's channels on the main buses) with `changes` applied
    /// at the start of the block. Blocks longer than the maximum are split.
    pub fn process(&mut self, chans: &mut [&mut [f32]], changes: &ComWrapper<Changes>, scratch: &mut Scratch) {
        let n = chans.iter().map(|c| c.len()).min().unwrap_or(0);
        let mut done = 0;
        let mut first = true;
        while done < n {
            let len = (n - done).min(self.max_block);
            self.process_block(chans, done, len, if first { Some(changes) } else { None }, scratch);
            first = false;
            done += len;
        }
        if n == 0 && !changes.is_empty() {
            // a zero-length call still delivers parameter changes
            self.process_block(chans, 0, 0, Some(changes), scratch);
        }
        changes.clear();
    }

    fn process_block(&mut self, chans: &mut [&mut [f32]], at: usize, len: usize, changes: Option<&ComWrapper<Changes>>, s: &mut Scratch) {
        let fc = chans.len();
        // input / output buffers per bus; FilmCraft's channels go to bus 0 (mapped by index,
        // a mono plug-in gets the mix of the first two)
        s.prepare(&self.ins, &self.outs, len);
        if let Some(bus) = s.ins.first_mut() {
            let bc = bus.len();
            for (k, ch) in bus.iter_mut().enumerate() {
                if bc == 1 && fc >= 2 {
                    for (i, o) in ch.iter_mut().enumerate() {
                        *o = 0.5 * (chans[0][at + i] + chans[1][at + i]);
                    }
                } else if let Some(src) = chans.get(k) {
                    ch.copy_from_slice(&src[at..at + len]);
                }
            }
        }
        let mut in_ptrs: Vec<Vec<*mut f32>> = s.ins.iter_mut().map(|b| b.iter_mut().map(|c| c.as_mut_ptr()).collect()).collect();
        let mut out_ptrs: Vec<Vec<*mut f32>> = s.outs.iter_mut().map(|b| b.iter_mut().map(|c| c.as_mut_ptr()).collect()).collect();
        let mut ins: Vec<AudioBusBuffers> = in_ptrs
            .iter_mut()
            .map(|p| AudioBusBuffers { numChannels: p.len() as i32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: p.as_mut_ptr() } })
            .collect();
        let mut outs: Vec<AudioBusBuffers> = out_ptrs
            .iter_mut()
            .map(|p| AudioBusBuffers { numChannels: p.len() as i32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: p.as_mut_ptr() } })
            .collect();
        let input_changes = changes.and_then(|c| c.as_com_ref::<IParameterChanges>()).map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let out_changes = s.out_changes.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let events = s.events.as_com_ref::<IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
        // SAFETY: a zeroed POD context, then the fields hosts fill.
        let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
        ctx.sampleRate = self.sample_rate;
        ctx.tempo = 120.0;
        ctx.timeSigNumerator = 4;
        ctx.timeSigDenominator = 4;
        ctx.projectTimeSamples = s.position;
        ctx.continousTimeSamples = s.position;
        // FilmCraft only processes while playing or rendering: the transport runs
        ctx.state =
            (ProcessContext_::StatesAndFlags_::kPlaying | ProcessContext_::StatesAndFlags_::kTempoValid | ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: len as i32,
            numInputs: ins.len() as i32,
            numOutputs: outs.len() as i32,
            inputs: if ins.is_empty() { std::ptr::null_mut() } else { ins.as_mut_ptr() },
            outputs: outs.as_mut_ptr(),
            inputParameterChanges: input_changes,
            outputParameterChanges: out_changes,
            inputEvents: events,
            outputEvents: events,
            processContext: &mut ctx,
        };
        // SAFETY: every pointer in `data` refers to buffers and objects alive for the call, sized
        // as the bus arrangement the plug-in accepted.
        let r = unsafe { self.processor.process(&mut data) };
        s.out_changes.clear();
        s.position += len as i64;
        if r != kResultOk || len == 0 {
            return;
        }
        // back to FilmCraft's channels: bus 0 by index (a mono output fills every channel)
        if let Some(bus) = s.outs.first() {
            for (k, ch) in chans.iter_mut().enumerate() {
                let src = if bus.len() == 1 { bus.first() } else { bus.get(k) };
                if let Some(src) = src {
                    for (o, v) in ch[at..at + len].iter_mut().zip(src.iter()) {
                        *o = if v.is_finite() { *v } else { 0.0 };
                    }
                }
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stop();
        if let Some((a, b)) = self.points.take() {
            // SAFETY: the pair connected in `new`.
            unsafe {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
        }
        if let Some(c) = &self.controller {
            // SAFETY: detach our handler, then terminate (a separate controller terminates itself).
            unsafe {
                let _ = c.setComponentHandler(std::ptr::null_mut());
                if self.separate {
                    let _ = c.terminate();
                }
            }
        }
        // SAFETY: last call on the component before release.
        let _ = unsafe { self.component.terminate() };
    }
}

/// Buffers reused block after block.
pub struct Scratch {
    pub ins: Vec<Vec<Vec<f32>>>,
    pub outs: Vec<Vec<Vec<f32>>>,
    pub out_changes: ComWrapper<Changes>,
    pub events: ComWrapper<NoEvents>,
    pub position: i64,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch { ins: Vec::new(), outs: Vec::new(), out_changes: ComWrapper::new(Changes::default()), events: ComWrapper::new(NoEvents), position: 0 }
    }
}

impl Scratch {
    fn prepare(&mut self, ins: &[usize], outs: &[usize], len: usize) {
        let fit = |v: &mut Vec<Vec<Vec<f32>>>, shape: &[usize]| {
            v.resize_with(shape.len(), Vec::new);
            for (b, &n) in v.iter_mut().zip(shape) {
                b.resize_with(n, Vec::new);
                for c in b.iter_mut() {
                    c.clear();
                    c.resize(len, 0.0);
                }
            }
        };
        fit(&mut self.ins, ins);
        fit(&mut self.outs, outs);
    }
}
