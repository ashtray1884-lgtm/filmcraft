//! The R3D SDK backend: [`SdkRuntime`] / [`SdkClip`] over the C bridge in `shim.cpp`.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

use crate::braw::{AudioInfo, Changes, Clip, ClipInfo, Kind, Picture, Runtime, Scale, Setting};
use filmcraft_time::FrameRate;

#[repr(C)]
#[derive(Clone, Copy)]
struct FcR3dInfo {
    width: u32,
    height: u32,
    frames: u64,
    fps: f32,
    fps_num: u32,
    fps_den: u32,
    audio_channels: u32,
    audio_rate: u32,
    audio_samples: u64,
    default_version: i32,
    minimum_version: i32,
    camera: [c_char; 64],
    timecode: [c_char; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Settings {
    pub version: i32,
    pub kelvin: f32,
    pub tint: f32,
    pub exposure_adjust: f32,
    pub exposure_compensation: f32,
    pub iso: u32,
    pub gamma: i32,
    pub color_space: i32,
    pub pipeline: i32,
    pub tone_map: i32,
    pub roll_off: i32,
    pub hdr_peak_nits: u32,
    pub cdl_saturation: f32,
    pub cdl: [f32; 9],
    pub cdl_enabled: i32,
    pub lens_correction: i32,
    pub chroma_nr: i32,
    pub maskline: i32,
    pub flashing_pixel: i32,
    pub saturation: f32,
    pub contrast: f32,
    pub brightness: f32,
    pub drx: f32,
    pub shadow: f32,
    pub flut: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Limits {
    kelvin: [f32; 2],
    tint: [f32; 2],
    exposure_adjust: [f32; 2],
    exposure: [f32; 2],
    hdr_nits: [u32; 2],
    cdl_slope: [f32; 2],
    cdl_offset: [f32; 2],
    cdl_power: [f32; 2],
    saturation: [f32; 2],
    contrast: [f32; 2],
    brightness: [f32; 2],
    drx: [f32; 2],
    shadow: [f32; 2],
    flut: [f32; 2],
}

unsafe extern "C" {
    fn fc_r3d_init(lib_dir: *const c_char, flags: u32) -> c_int;
    fn fc_r3d_version() -> *const c_char;
    fn fc_r3d_open(path: *const c_char, status: *mut c_int) -> *mut c_void;
    fn fc_r3d_close(h: *mut c_void);
    fn fc_r3d_info(h: *mut c_void, out: *mut FcR3dInfo) -> c_int;
    fn fc_r3d_settings(h: *mut c_void, recorded: c_int, out: *mut Settings) -> c_int;
    fn fc_r3d_choice(kind: c_int, index: usize, value: *mut i32, label: *mut *const c_char) -> usize;
    fn fc_r3d_limits(out: *mut Limits);
    fn fc_r3d_decode(h: *mut c_void, frame: u64, divisor: c_int, settings: *const Settings, out: *mut c_void, out_bytes: usize) -> c_int;
    fn fc_r3d_audio(h: *mut c_void, start: u64, n: usize, out: *mut f32) -> i64;
}

/// `InitializeSdk` status names (R3DSDKDefinitions.h).
fn init_status(s: i32) -> &'static str {
    match s {
        1 => "a library was not loaded",
        2 => "REDR3D was not found next to FilmCraft",
        3 => "REDCuda was not found",
        4 => "REDOpenCL was not found",
        5 => "REDDecoder was not found",
        6 => "RED's libraries do not match the SDK FilmCraft was built with",
        7..=10 | 18 => "one of RED's libraries is not valid",
        11..=14 | 19 => "one of RED's libraries failed to start",
        15 => "the library folder is not valid",
        20 => "Metal is not available",
        21 => "CUDA is not available",
        _ => "internal error",
    }
}

fn load_status(s: i32) -> &'static str {
    match s {
        1 => "the clip was not found",
        2 => "the clip could not be opened",
        3 => "the file is not an R3D clip",
        4 => "the clip has no frames",
        5 => "out of memory",
        8 => "RED's decoder is not initialised",
        _ => "unknown error",
    }
}

fn decode_status(s: i32) -> &'static str {
    match s {
        1 => "output buffer invalid",
        3 => "frame out of range",
        4 => "invalid parameter",
        5 => "dropped frame",
        6 => "corrupt frame",
        7 => "out of memory",
        9 => "no clip open",
        10 => "the frame could not be read from disk",
        11 => "pixel type not supported",
        14 => "clip format not supported for this decode",
        15 => "parameter not supported by RED's libraries",
        _ => "decode failed",
    }
}

/// Where RED's dynamic libraries are: `FILMCRAFT_R3D_LIBS`, else next to the executable.
fn lib_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FILMCRAFT_R3D_LIBS").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."))
}

struct Loaded {
    version: String,
    dir: PathBuf,
}

/// RED's SDK, initialised at the first R3D import (once per process; RED requires it to be
/// initialised before any clip and never again).
#[derive(Default)]
pub struct SdkRuntime {
    loaded: OnceLock<Result<Arc<Loaded>, String>>,
}

impl SdkRuntime {
    fn get(&self) -> Result<Arc<Loaded>, String> {
        self.loaded
            .get_or_init(|| {
                let dir = lib_dir();
                // the SDK wants the folder with a trailing separator
                let mut s = dir.to_string_lossy().to_string();
                if !s.ends_with(std::path::MAIN_SEPARATOR) {
                    s.push(std::path::MAIN_SEPARATOR);
                }
                let c = CString::new(s).map_err(|_| "bad library path".to_string())?;
                // SAFETY: a NUL-terminated path; CPU decoding (no optional GPU components).
                let st = unsafe { fc_r3d_init(c.as_ptr(), 0) };
                // SAFETY: the SDK returns a static NUL-terminated string (also after a failed init).
                let version = unsafe { CStr::from_ptr(fc_r3d_version()) }.to_string_lossy().into_owned();
                if st != 0 {
                    return Err(format!("{} (status {st}; {})", init_status(st), dir.display()));
                }
                log::info!("R3D SDK: {version}");
                Ok(Arc::new(Loaded { version, dir }))
            })
            .clone()
    }
}

impl Runtime for SdkRuntime {
    fn name(&self) -> String {
        "RED's R3D decoder".into()
    }
    fn open(&self, path: &Path) -> Result<Arc<dyn Clip>, String> {
        self.get()?;
        SdkClip::open(path).map(|c| Arc::new(c) as Arc<dyn Clip>)
    }
    fn decoding_on(&self) -> Option<String> {
        self.loaded.get().and_then(|l| l.as_ref().ok()).map(|_| "CPU (RED's multithreaded decoder)".to_string())
    }
}

pub fn report(rt: &dyn Runtime) -> String {
    let _ = rt;
    let r = SdkRuntime::default();
    match r.get() {
        Ok(l) => format!("RED R3D: OK, {} ({})", l.version, l.dir.display()),
        Err(e) => format!("RED R3D: NOT usable: {e}"),
    }
}

struct Handle(*mut c_void);
// SAFETY: R3DSDK::Clip is thread-safe for decoding and reading (no LoadFrom/Close after open);
// audio is serialised inside the bridge.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from fc_r3d_open and is closed once.
        unsafe { fc_r3d_close(self.0) };
    }
}

/// How a setting maps onto [`Settings`].
#[derive(Clone, Copy, Debug)]
enum Field {
    Iso,
    Kelvin,
    Tint,
    Exposure,
    ColorSpace,
    Gamma,
    Pipeline,
    ToneMap,
    RollOff,
    HdrNits,
    CdlEnabled,
    CdlSaturation,
    Cdl(usize),
    Lens,
    ChromaNr,
    Maskline,
    Flashing,
    Saturation,
    Contrast,
    Brightness,
    Drx,
    Shadow,
    Flut,
}

/// A choice list from the SDK: (value, label).
fn choices(kind: c_int) -> Vec<(i32, String)> {
    // SAFETY: a count query (null outputs are allowed by the bridge).
    let n = unsafe { fc_r3d_choice(kind, 0, std::ptr::null_mut(), std::ptr::null_mut()) }.min(256);
    (0..n)
        .map(|i| {
            let (mut v, mut l) = (0i32, std::ptr::null::<c_char>());
            // SAFETY: index below the count; out-pointers to locals.
            unsafe { fc_r3d_choice(kind, i, &mut v, &mut l) };
            let label = if l.is_null() {
                v.to_string()
            } else {
                // SAFETY: the SDK's static label strings.
                unsafe { CStr::from_ptr(l) }.to_string_lossy().into_owned()
            };
            (v, label)
        })
        .collect()
}

/// Gamma curves and colour spaces only the legacy pipeline offers (R3DSDKDefinitions.h: "only
/// available with color version 2, otherwise ignored"), hidden for IPP2 clips; and the reverse.
const LEGACY_GAMMA: [i32; 7] = [2, 3, 7, 10, 11, 12, 9];
const LEGACY_COLOR: [i32; 8] = [2, 3, 6, 8, 9, 11, 12, 0];
const IPP2_ONLY_COLOR: [i32; 1] = [18];

const LENS: [(i32, &str); 3] = [(0, "As Shot"), (1, "On"), (2, "Off")];
const FLASHING: [(i32, &str); 4] = [(0, "Off"), (0x4650_4D44, "Mild"), (0x4650_4D4D, "Medium"), (0x4650_5347, "Strong")];

struct Meta {
    settings: Vec<(Setting, Field, Vec<(i32, String)>)>,
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn label_of(list: &[(i32, String)], v: i32) -> Value {
    list.iter().find(|(x, _)| *x == v).map_or(json!(v), |(_, l)| json!(l))
}

fn build_meta(defaults: &Settings) -> Meta {
    let mut lim = Limits::default();
    // SAFETY: an out-struct the bridge fills.
    unsafe { fc_r3d_limits(&mut lim) };
    let ipp2 = defaults.version >= 3;
    let mut out: Vec<(Setting, Field, Vec<(i32, String)>)> = Vec::new();
    let num = |key: &'static str, label: &'static str, group: &'static str, r: [f32; 2], step: f64, v: f64, note: Option<&'static str>| Setting {
        key,
        label,
        group,
        kind: Kind::Number { min: r[0] as f64, max: (r[1] as f64).max(r[0] as f64), step },
        as_shot: json!((v * 1000.0).round() / 1000.0),
        read_only: false,
        note,
    };
    let choice = |key: &'static str, label: &'static str, group: &'static str, list: &[(i32, String)], v: i32| Setting {
        key,
        label,
        group,
        kind: Kind::Choice(list.iter().map(|(_, l)| json!(l)).collect()),
        as_shot: label_of(list, v),
        read_only: false,
        note: None,
    };
    let toggle = |key: &'static str, label: &'static str, group: &'static str, v: bool| Setting {
        key,
        label,
        group,
        kind: Kind::Toggle,
        as_shot: json!(v),
        read_only: false,
        note: None,
    };

    let iso: Vec<(i32, String)> = choices(5);
    out.push((
        Setting {
            key: "iso",
            label: "ISO",
            group: "Image",
            kind: Kind::Choice(iso.iter().map(|(v, _)| json!(v)).collect()),
            as_shot: json!(defaults.iso),
            read_only: false,
            note: None,
        },
        Field::Iso,
        iso.clone(),
    ));
    out.push((num("kelvin", "Color Temp", "Image", lim.kelvin, 10.0, defaults.kelvin as f64, None), Field::Kelvin, Vec::new()));
    out.push((num("tint", "Tint", "Image", lim.tint, 0.1, defaults.tint as f64, None), Field::Tint, Vec::new()));
    if ipp2 {
        out.push((num("exposure", "Exposure", "Image", lim.exposure_adjust, 0.01, defaults.exposure_adjust as f64, None), Field::Exposure, Vec::new()));
    } else {
        out.push((num("exposure", "Exposure", "Image", lim.exposure, 0.01, defaults.exposure_compensation as f64, None), Field::Exposure, Vec::new()));
    }
    let colors: Vec<(i32, String)> =
        choices(1).into_iter().filter(|(v, _)| if ipp2 { !LEGACY_COLOR.contains(v) } else { !IPP2_ONLY_COLOR.contains(v) }).collect();
    let gammas: Vec<(i32, String)> = choices(0).into_iter().filter(|(v, _)| !ipp2 || !LEGACY_GAMMA.contains(v)).collect();
    out.push((choice("colorSpace", "Color Space", "Color", &colors, defaults.color_space), Field::ColorSpace, colors.clone()));
    out.push((choice("gamma", "Gamma Curve", "Color", &gammas, defaults.gamma), Field::Gamma, gammas.clone()));
    if ipp2 {
        let pipes = choices(2);
        out.push((choice("pipeline", "Image Pipeline", "Color", &pipes, defaults.pipeline), Field::Pipeline, pipes.clone()));
        let tones = choices(3);
        out.push((choice("toneMap", "Output Tone Map", "Output", &tones, defaults.tone_map), Field::ToneMap, tones.clone()));
        let rolls = choices(4);
        out.push((choice("highlightRollOff", "Highlight Roll-Off", "Output", &rolls, defaults.roll_off), Field::RollOff, rolls.clone()));
        let nits = [lim.hdr_nits[0] as f32, lim.hdr_nits[1] as f32];
        out.push((
            num("hdrPeakNits", "HDR Peak Nits", "Output", nits, 10.0, defaults.hdr_peak_nits as f64, Some("HDR-2084 gamma only")),
            Field::HdrNits,
            Vec::new(),
        ));
        out.push((toggle("cdlEnabled", "CDL", "CDL", defaults.cdl_enabled != 0), Field::CdlEnabled, Vec::new()));
        out.push((num("cdlSaturation", "CDL Saturation", "CDL", [0.0, 4.0], 0.01, defaults.cdl_saturation as f64, None), Field::CdlSaturation, Vec::new()));
        let names = ["Slope", "Offset", "Power"];
        let ch = ["Red", "Green", "Blue"];
        for (k, n) in names.iter().enumerate() {
            let r = [lim.cdl_slope, lim.cdl_offset, lim.cdl_power][k];
            for (c, cn) in ch.iter().enumerate() {
                let key = leak(format!("cdl{n}{cn}"));
                let label = leak(format!("{n} {cn}"));
                out.push((num(key, label, "CDL", r, 0.001, defaults.cdl[k * 3 + c] as f64, None), Field::Cdl(k * 3 + c), Vec::new()));
            }
        }
    } else {
        for (key, label, r, v, f) in [
            ("saturation", "Saturation", lim.saturation, defaults.saturation, Field::Saturation),
            ("contrast", "Contrast", lim.contrast, defaults.contrast, Field::Contrast),
            ("brightness", "Brightness", lim.brightness, defaults.brightness, Field::Brightness),
            ("drx", "DRX", lim.drx, defaults.drx, Field::Drx),
            ("shadow", "Shadow", lim.shadow, defaults.shadow, Field::Shadow),
            ("flut", "FLUT", lim.flut, defaults.flut, Field::Flut),
        ] {
            out.push((num(key, label, "Legacy Look", r, 0.01, v as f64, None), f, Vec::new()));
        }
    }
    let lens: Vec<(i32, String)> = LENS.iter().map(|(v, l)| (*v, l.to_string())).collect();
    out.push((choice("lensCorrection", "Lens Distortion Correction", "Detail", &lens, defaults.lens_correction), Field::Lens, lens.clone()));
    out.push((toggle("chromaNoiseReduction", "Chroma Noise Reduction", "Detail", defaults.chroma_nr != 0), Field::ChromaNr, Vec::new()));
    let flash: Vec<(i32, String)> = FLASHING.iter().map(|(v, l)| (*v, l.to_string())).collect();
    out.push((choice("flashingPixel", "Flashing Pixel Adjust", "Detail", &flash, defaults.flashing_pixel), Field::Flashing, flash.clone()));
    out.push((toggle("masklineAdjust", "Maskline Adjust", "Detail", defaults.maskline != 0), Field::Maskline, Vec::new()));
    Meta { settings: out }
}

/// Apply checked `changes` (setting key → value) to `s`.
fn apply(meta: &Meta, s: &mut Settings, changes: &Changes) {
    for (st, field, list) in &meta.settings {
        let Some(v) = changes.get(st.key) else { continue };
        let f = v.as_f64().unwrap_or(0.0) as f32;
        let pick = |v: &Value| list.iter().find(|(_, l)| v.as_str() == Some(l.as_str())).map(|(x, _)| *x);
        match field {
            Field::Iso => s.iso = v.as_u64().unwrap_or(s.iso as u64).min(u32::MAX as u64) as u32,
            Field::Kelvin => s.kelvin = f,
            Field::Tint => s.tint = f,
            Field::Exposure => {
                if s.version >= 3 {
                    s.exposure_adjust = f
                } else {
                    s.exposure_compensation = f
                }
            }
            Field::ColorSpace => s.color_space = pick(v).unwrap_or(s.color_space),
            Field::Gamma => s.gamma = pick(v).unwrap_or(s.gamma),
            Field::Pipeline => s.pipeline = pick(v).unwrap_or(s.pipeline),
            Field::ToneMap => s.tone_map = pick(v).unwrap_or(s.tone_map),
            Field::RollOff => s.roll_off = pick(v).unwrap_or(s.roll_off),
            Field::HdrNits => s.hdr_peak_nits = f.max(0.0) as u32,
            Field::CdlEnabled => s.cdl_enabled = v.as_bool().unwrap_or(false) as i32,
            Field::CdlSaturation => s.cdl_saturation = f,
            Field::Cdl(i) => {
                if let Some(x) = s.cdl.get_mut(*i) {
                    *x = f
                }
            }
            Field::Lens => s.lens_correction = pick(v).unwrap_or(s.lens_correction),
            Field::ChromaNr => s.chroma_nr = v.as_bool().unwrap_or(false) as i32,
            Field::Maskline => s.maskline = v.as_bool().unwrap_or(false) as i32,
            Field::Flashing => s.flashing_pixel = pick(v).unwrap_or(s.flashing_pixel),
            Field::Saturation => s.saturation = f,
            Field::Contrast => s.contrast = f,
            Field::Brightness => s.brightness = f,
            Field::Drx => s.drx = f,
            Field::Shadow => s.shadow = f,
            Field::Flut => s.flut = f,
        }
    }
}

/// An open R3D clip.
pub struct SdkClip {
    h: Handle,
    info: ClipInfo,
    defaults: Settings,
    meta: Meta,
}

/// A 16-byte aligned output buffer.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Block([u8; 16]);

impl SdkClip {
    fn open(path: &Path) -> Result<SdkClip, String> {
        let c = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| "bad path".to_string())?;
        let mut st = 0;
        // SAFETY: a NUL-terminated path and an out-pointer.
        let h = unsafe { fc_r3d_open(c.as_ptr(), &mut st) };
        if h.is_null() {
            return Err(load_status(st).to_string());
        }
        let h = Handle(h);
        // SAFETY: a zeroed POD out-struct the bridge fills.
        let mut i: FcR3dInfo = unsafe { std::mem::zeroed() };
        // SAFETY: a live handle and our out-struct.
        if unsafe { fc_r3d_info(h.0, &mut i) } == 0 {
            return Err("the clip's properties could not be read".into());
        }
        let text = |b: &[c_char]| {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..n].iter().map(|&c| c as u8).collect::<Vec<_>>()).trim().to_string()
        };
        let frame_rate =
            if i.fps_num > 0 && i.fps_den > 0 { FrameRate { num: i.fps_num as i64, den: i.fps_den as i64 } } else { crate::braw::rate_from_f32(i.fps) };
        let fps = if frame_rate.den > 0 { frame_rate.num as f64 / frame_rate.den as f64 } else { 0.0 };
        let camera = text(&i.camera);
        let tc = text(&i.timecode);
        let audio = (i.audio_channels > 0 && i.audio_rate > 0 && i.audio_samples > 0).then_some(AudioInfo {
            sample_rate: i.audio_rate,
            channels: i.audio_channels,
            bits_per_sample: 24,
            sample_count: i.audio_samples,
        });
        let info = ClipInfo {
            width: i.width,
            height: i.height,
            frame_rate,
            frame_count: i.frames,
            audio,
            camera: (!camera.is_empty()).then(|| format!("RED {camera}").replace("RED RED", "RED")),
            start_timecode: super::timecode_frames(&tc, fps),
        };
        let mut defaults = Settings::default();
        // SAFETY: a live handle and our out-struct.
        if unsafe { fc_r3d_settings(h.0, 0, &mut defaults) } == 0 {
            return Err("the clip's settings could not be read".into());
        }
        let meta = build_meta(&defaults);
        Ok(SdkClip { h, info, defaults, meta })
    }

    fn decode_settings(&self, index: u64, scale: Scale, s: Option<&Settings>) -> Result<Picture, String> {
        let d = scale.divisor();
        let (w, h) = ((self.info.width / d).max(1), (self.info.height / d).max(1));
        let samples = (w as usize).checked_mul(h as usize).and_then(|n| n.checked_mul(3)).ok_or("picture too large")?;
        let bytes = samples * 2;
        let mut buf = vec![Block([0; 16]); bytes.div_ceil(16)];
        let p = buf.as_mut_ptr() as *mut c_void;
        let sp = s.map_or(std::ptr::null(), |s| s as *const Settings);
        // SAFETY: a live handle, a 16-byte aligned buffer of `bytes` bytes, optional settings.
        let st = unsafe { fc_r3d_decode(self.h.0, index, d as c_int, sp, p, bytes) };
        if st != 0 {
            return Err(format!("frame {index}: {}", decode_status(st)));
        }
        // SAFETY: the buffer holds `samples` u16 values (the SDK wrote w×h×3 of them).
        let rgb = unsafe { std::slice::from_raw_parts(p as *const u16, samples) }.to_vec();
        Ok(Picture { width: w, height: h, rgb })
    }
}

impl Clip for SdkClip {
    fn info(&self) -> &ClipInfo {
        &self.info
    }
    fn format(&self) -> &'static str {
        super::FORMAT
    }
    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String> {
        self.decode_settings(index, scale, None)
    }
    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String> {
        let Some(a) = &self.info.audio else { return Err("no audio".into()) };
        let n = frames.min(1 << 20);
        let mut out = vec![0f32; n * a.channels as usize];
        // SAFETY: a live handle and room for n × channels floats.
        let got = unsafe { fc_r3d_audio(self.h.0, start, n, out.as_mut_ptr()) };
        if got < 0 {
            return Err("audio could not be read".into());
        }
        out.truncate(got as usize * a.channels as usize);
        Ok(out)
    }
    fn settings(&self) -> Vec<Setting> {
        self.meta.settings.iter().map(|(s, _, _)| s.clone()).collect()
    }
    fn decode_with(&self, index: u64, scale: Scale, changes: &Changes) -> Result<Picture, String> {
        let mut s = self.defaults;
        apply(&self.meta, &mut s, changes);
        self.decode_settings(index, scale, Some(&s))
    }
}
