//! Blackmagic RAW (`.braw`) through Blackmagic's own runtime.
//!
//! Blackmagic RAW has no public bitstream specification, so FilmCraft cannot decode it itself
//! (the clean-room rule). Instead, where Blackmagic's free runtime is installed (it comes with the
//! Blackmagic RAW installer, the same one that adds the Premiere Pro and Avid plug-ins), a
//! [`Runtime`] backed by it decodes the clips. Nothing of Blackmagic's is linked or shipped: the
//! backend loads what the user installed, and without it a `.braw` import fails with a message
//! that says what to install.
//!
//! The pieces:
//!
//! - [`Runtime`] / [`Clip`]: the small interface a backend implements (open a clip by path, decode
//!   one frame at a resolution scale, read interleaved audio). The Windows backend talks to the
//!   runtime's COM API; tests use an in-memory one.
//! - [`BrawSource`]: a [`MediaSource`] over a [`Clip`]: media info, frames as 16-bit 4:4:4
//!   Y'CbCr (Rec.709, full range), audio resampled to the requested rate, and a one-frame cache
//!   so repeated requests (a paused monitor, thumbnails) don't decode twice.
//! - [`reader_opener`]: registered ahead of the built-in openers (a `.braw` file is a QuickTime-style
//!   container the MP4 opener would otherwise claim and fail to decode).
//!
//! Decoded pictures are display-referred Rec.709 (gamma and gamut), the clip's as-shot settings
//! otherwise; choosing the clip's log gamma and gamut for grading is a later setting.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use filmcraft_color::{ColorInfo, Matrix, Primaries, Range, Transfer, rgb_to_ycbcr};
use filmcraft_frame::{AudioBuffer, Chroma, PixelData, VideoFrame};
use filmcraft_media::{AudioStreamInfo, FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource, SharedReader, SharedSource, VideoStreamInfo};
use filmcraft_time::{FrameRate, Tick};

#[cfg(all(target_os = "windows", any(target_arch = "x86_64", target_arch = "aarch64")))]
#[allow(unsafe_code)]
pub mod windows;

/// Media-info codec name.
pub const CODEC: &str = "Blackmagic RAW";
/// Largest picture a backend may hand back (each side), against corrupt metadata.
pub const MAX_SIDE: u32 = 16_384;
/// Largest frame count trusted from clip metadata (24 hours at 120 fps).
pub const MAX_FRAMES: u64 = 24 * 3600 * 120;
/// Most audio channels trusted from clip metadata.
pub const MAX_CHANNELS: u32 = 64;
/// Most audio sample frames read from the runtime in one call.
const MAX_AUDIO_READ: usize = 1 << 20;

/// What a clip says about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipInfo {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub frame_count: u64,
    pub audio: Option<AudioInfo>,
    /// Camera model, when the clip names one (shown in media info).
    pub camera: Option<String>,
    /// Start timecode in frames at `frame_rate`, when the clip carries one.
    pub start_timecode: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    /// Sample frames in the clip.
    pub sample_count: u64,
}

/// Decode resolution. Blackmagic RAW decodes at a fraction of full size much faster than it
/// decodes full size and scales down, which keeps playback of 6K/8K clips light.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scale {
    Full,
    Half,
    Quarter,
    Eighth,
}

impl Scale {
    /// The smallest decode scale that still covers a request at `want` (1.0 = full size).
    pub fn for_request(want: f32) -> Scale {
        if !want.is_finite() || want > 0.5 {
            Scale::Full
        } else if want > 0.25 {
            Scale::Half
        } else if want > 0.125 {
            Scale::Quarter
        } else {
            Scale::Eighth
        }
    }
    pub fn divisor(self) -> u32 {
        match self {
            Scale::Full => 1,
            Scale::Half => 2,
            Scale::Quarter => 4,
            Scale::Eighth => 8,
        }
    }
}

/// A decoded picture: interleaved R'G'B' at 16 bits, Rec.709 gamma and primaries.
#[derive(Clone, Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u16>,
}

/// One open clip. Implementations are shared between threads (the frame server decodes from
/// several), so they synchronise internally.
pub trait Clip: Send + Sync {
    fn info(&self) -> &ClipInfo;
    /// Decode frame `index` (0-based) at `scale`, as shot.
    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String>;
    /// `frames` sample frames from `start`, interleaved, as f32 in -1..1. Fewer at the end.
    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String>;
    /// The clip's Source Settings with their as-shot values (empty: none).
    fn settings(&self) -> Vec<Setting> {
        Vec::new()
    }
    /// Why [`Clip::settings`] is empty, for Clip ▸ Source Settings (`None`: no reason known).
    fn settings_note(&self) -> Option<String> {
        None
    }
    /// Decode with `changes` (setting key → value, already checked against [`Clip::settings`])
    /// applied over the as-shot settings.
    fn decode_with(&self, index: u64, scale: Scale, changes: &Changes) -> Result<Picture, String> {
        let _ = changes;
        self.decode(index, scale)
    }
}

/// Settings changed from as shot: key → value.
pub type Changes = serde_json::Map<String, serde_json::Value>;

/// How a setting is edited.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Number {
        min: f64,
        max: f64,
        step: f64,
    },
    Toggle,
    /// One of these values (text or numbers).
    Choice(Vec<serde_json::Value>),
}

/// One Source Setting of a clip.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    /// Stable id (`gamma`, `iso`, `saturation`…), stored in projects.
    pub key: &'static str,
    pub label: &'static str,
    /// Dialog section.
    pub group: &'static str,
    pub kind: Kind,
    pub as_shot: serde_json::Value,
    pub read_only: bool,
    /// A short note shown under the control.
    pub note: Option<&'static str>,
}

impl Setting {
    /// `value` checked against this setting: numbers clamped, choices listed, toggles boolean.
    pub fn check(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        if self.read_only {
            return None;
        }
        match &self.kind {
            Kind::Number { min, max, .. } => value.as_f64().filter(|x| x.is_finite()).map(|x| serde_json::json!(x.clamp(*min, *max))),
            Kind::Toggle => value.as_bool().map(serde_json::Value::Bool),
            Kind::Choice(opts) => opts.iter().find(|o| *o == value || (o.is_number() && value.as_f64() == o.as_f64() && value.is_number())).cloned(),
        }
    }

    /// This setting as schema JSON, with the value in effect.
    pub fn to_json(&self, value: &serde_json::Value) -> serde_json::Value {
        let mut v = serde_json::json!({
            "key": self.key, "label": self.label, "group": self.group,
            "value": value, "asShot": self.as_shot, "readOnly": self.read_only,
        });
        match &self.kind {
            Kind::Number { min, max, step } => {
                v["type"] = "number".into();
                v["min"] = (*min).into();
                v["max"] = (*max).into();
                v["step"] = (*step).into();
            }
            Kind::Toggle => v["type"] = "toggle".into(),
            Kind::Choice(o) => {
                v["type"] = "choice".into();
                v["options"] = o.clone().into();
            }
        }
        if let Some(n) = self.note {
            v["note"] = n.into();
        }
        v
    }
}

/// Keep only the changes `settings` allows (unknown keys and bad values dropped, numbers clamped).
pub fn sanitize(settings: &[Setting], changes: &serde_json::Value) -> Changes {
    let mut out = Changes::new();
    if let Some(o) = changes.as_object() {
        for (k, v) in o {
            if let Some(st) = settings.iter().find(|s| s.key == k.as_str())
                && let Some(ok) = st.check(v)
            {
                out.insert(k.clone(), ok);
            }
        }
    }
    out
}

/// A Blackmagic RAW decoder: opens clips by path.
pub trait Runtime: Send + Sync {
    /// For messages ("Blackmagic RAW 4.x").
    fn name(&self) -> String;
    fn open(&self, path: &Path) -> Result<Arc<dyn Clip>, String>;
}

/// Why no runtime is available, for the import error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// Not looked for yet ([`crate::register`] has not run).
    NotRegistered,
    /// No backend for this operating system yet.
    Platform,
    /// The backend is there but Blackmagic's runtime is not installed (or failed to load).
    NotInstalled(String),
    /// This build has the import path but not yet the binding to Blackmagic's runtime.
    Unfinished,
}

static RUNTIME: RwLock<Result<Arc<dyn Runtime>, Missing>> = RwLock::new(Err(Missing::NotRegistered));

/// Install the runtime [`reader_opener`] decodes with (the platform backend, or a test double).
pub fn set_runtime(r: Arc<dyn Runtime>) {
    *RUNTIME.write().unwrap_or_else(PoisonError::into_inner) = Ok(r);
}

/// Record why there is no runtime (shown when a `.braw` file is imported).
pub fn set_missing(m: Missing) {
    *RUNTIME.write().unwrap_or_else(PoisonError::into_inner) = Err(m);
}

pub fn runtime() -> Result<Arc<dyn Runtime>, Missing> {
    RUNTIME.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Whether `name` is a Blackmagic RAW file (by extension: the container is QuickTime-style).
pub fn is_braw(name: &str) -> bool {
    Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("braw"))
}

/// The user-facing reason a `.braw` file cannot be opened without a runtime.
pub fn missing_message(name: &str, m: &Missing) -> String {
    match m {
        Missing::Platform => {
            format!("{name}: Blackmagic RAW is supported on Windows (through Blackmagic's free Blackmagic RAW runtime); not yet on this system")
        }
        Missing::NotInstalled(why) => format!(
            "{name}: Blackmagic RAW needs Blackmagic's free Blackmagic RAW runtime. Install it from blackmagicdesign.com \
             (the Blackmagic RAW installer, the same one that adds the Premiere Pro plug-in) and reopen the project ({why})"
        ),
        Missing::NotRegistered => format!("{name}: Blackmagic RAW support is not initialised in this build"),
        Missing::Unfinished => format!("{name}: Blackmagic RAW import is in progress: this build cannot decode it yet"),
    }
}

/// Reader opener for `.braw` files (see the module docs). Registered by [`crate::register`].
pub fn reader_opener(name: &str, _head: &[u8], reader: &SharedReader) -> Option<filmcraft_media::Result<SharedSource>> {
    if !is_braw(name) {
        return None;
    }
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(m) => return Some(Err(MediaError::Unsupported(missing_message(name, &m)))),
    };
    let Some(path) = reader.path() else {
        return Some(Err(MediaError::Unsupported(format!("{name}: Blackmagic RAW clips open from a file on disk"))));
    };
    Some(open_with(&*rt, name, path, Some(reader.len())).map(|s| Arc::new(s) as SharedSource))
}

/// Open `path` with `rt` as a [`BrawSource`].
pub fn open_with(rt: &dyn Runtime, name: &str, path: &Path, file_size: Option<u64>) -> filmcraft_media::Result<BrawSource> {
    let clip = rt.open(path).map_err(|e| MediaError::Decode(format!("{name}: {} could not open the clip: {e}", rt.name())))?;
    BrawSource::new(name, clip, file_size)
}

/// Colour of the frames [`BrawSource`] hands out.
pub const FRAME_COLOR: ColorInfo = ColorInfo { matrix: Matrix::Bt709, transfer: Transfer::Bt709, primaries: Primaries::Bt709, range: Range::Full };

/// A Blackmagic RAW clip as a media source.
pub struct BrawSource {
    clip: Arc<dyn Clip>,
    info: MediaInfo,
    /// The last frame decoded: (index, scale, frame).
    last: Mutex<Option<(u64, Scale, Arc<VideoFrame>)>>,
    /// Source Settings changed from as shot (checked against the clip's settings).
    changes: Changes,
}

impl BrawSource {
    /// Validates what the clip reports (metadata from a file is never trusted blindly).
    pub fn new(name: &str, clip: Arc<dyn Clip>, file_size: Option<u64>) -> filmcraft_media::Result<Self> {
        let ci = clip.info().clone();
        let bad = |what: &str| MediaError::Decode(format!("{name}: Blackmagic RAW clip reports {what}"));
        if ci.width == 0 || ci.height == 0 || ci.width > MAX_SIDE || ci.height > MAX_SIDE {
            return Err(bad(&format!("an invalid picture size {}×{}", ci.width, ci.height)));
        }
        if ci.frame_rate.num <= 0 || ci.frame_rate.den <= 0 {
            return Err(bad("an invalid frame rate"));
        }
        if ci.frame_count == 0 || ci.frame_count > MAX_FRAMES {
            return Err(bad(&format!("{} frames", ci.frame_count)));
        }
        let frames = i64::try_from(ci.frame_count).map_err(|_| bad("too many frames"))?;
        let duration = ci.frame_rate.tick_of(frames);
        let audio_streams = match &ci.audio {
            Some(a) if a.sample_rate > 0 && a.channels > 0 && a.channels <= MAX_CHANNELS && a.sample_count > 0 => {
                vec![AudioStreamInfo { sample_rate: a.sample_rate, channels: a.channels, codec: "PCM".into(), bits_per_sample: Some(a.bits_per_sample) }]
            }
            _ => Vec::new(),
        };
        let secs = duration.seconds();
        let bitrate = file_size.filter(|_| secs > 0.0).map(|b| (b as f64 * 8.0 / secs) as u64);
        let info = MediaInfo {
            name: name.to_string(),
            kind: MediaKind::Movie,
            duration,
            video: Some(VideoStreamInfo {
                width: ci.width,
                height: ci.height,
                frame_rate: ci.frame_rate,
                par: (1, 1),
                codec: match &ci.camera {
                    Some(cam) if !cam.trim().is_empty() => format!("{CODEC} ({})", cam.trim()),
                    _ => CODEC.to_string(),
                },
                pixel_format: "RAW (decoded to RGB 16-bit)".into(),
                color: FRAME_COLOR,
                has_alpha: false,
                bitrate,
                hdr: None,
            }),
            audio_streams,
            container: CODEC.into(),
            start_timecode: ci.start_timecode,
            file_size,
        };
        Ok(Self { clip, info, last: Mutex::new(None), changes: Changes::new() })
    }

    fn frame_index(&self, t: Tick) -> u64 {
        let count = self.clip.info().frame_count;
        let f = self.info.frame_rate().frame_at(t).max(0) as u64;
        f.min(count.saturating_sub(1))
    }
}

/// 16-bit interleaved R'G'B' → 16-bit 4:4:4 Y'CbCr planes (Rec.709 matrix, full range).
pub fn picture_to_frame(p: &Picture) -> Result<VideoFrame, String> {
    if p.width == 0 || p.height == 0 || p.width > MAX_SIDE || p.height > MAX_SIDE {
        return Err(format!("decoder returned an invalid picture size {}×{}", p.width, p.height));
    }
    let n = (p.width as usize).saturating_mul(p.height as usize);
    if p.rgb.len() != n.saturating_mul(3) {
        return Err(format!("decoder returned {} samples for a {}×{} picture", p.rgb.len(), p.width, p.height));
    }
    let (mut y, mut cb, mut cr) = (vec![0u16; n], vec![0u16; n], vec![0u16; n]);
    const MAX: f32 = 65_535.0;
    const MID: f32 = 32_768.0;
    let q = |v: f32| v.round().clamp(0.0, MAX) as u16;
    // Rec.709 matrix on code values: each colour's (Y, Cb, Cr) response (the 1/65535 normalisation cancels out)
    let [kr, kg, kb] = [rgb_to_ycbcr(1.0, 0.0, 0.0, Matrix::Bt709), rgb_to_ycbcr(0.0, 1.0, 0.0, Matrix::Bt709), rgb_to_ycbcr(0.0, 0.0, 1.0, Matrix::Bt709)];
    // rows in parallel: a 6K frame is 21 million samples
    let w = p.width as usize;
    use rayon::prelude::*;
    p.rgb.par_chunks(w * 3).zip(y.par_chunks_mut(w)).zip(cb.par_chunks_mut(w)).zip(cr.par_chunks_mut(w)).for_each(|(((src, dy), db), dr)| {
        for (((px, oy), ob), or) in src.chunks_exact(3).zip(dy.iter_mut()).zip(db.iter_mut()).zip(dr.iter_mut()) {
            let [r, g, b] = [px[0] as f32, px[1] as f32, px[2] as f32];
            *oy = q(kr[0] * r + kg[0] * g + kb[0] * b);
            *ob = q(kr[1] * r + kg[1] * g + kb[1] * b + MID);
            *or = q(kr[2] * r + kg[2] * g + kb[2] * b + MID);
        }
    });
    Ok(VideoFrame {
        width: p.width,
        height: p.height,
        data: PixelData::Yuv16 { planes: [Arc::new(y), Arc::new(cb), Arc::new(cr)], chroma: Chroma::C444, bits: 16, alpha: None },
        color: FRAME_COLOR,
        par: (1, 1),
        pts: Tick::ZERO,
    })
}

impl MediaSource for BrawSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        let index = self.frame_index(req.time);
        let scale = Scale::for_request(req.scale);
        let rate = self.info.frame_rate();
        let pts = rate.tick_of(i64::try_from(index).unwrap_or(i64::MAX));
        // a cached frame at this scale or larger serves the request
        if let Some((i, s, f)) = &*self.last.lock().unwrap_or_else(PoisonError::into_inner)
            && *i == index
            && s.divisor() <= scale.divisor()
        {
            return Ok(f.clone());
        }
        let pic = if self.changes.is_empty() { self.clip.decode(index, scale) } else { self.clip.decode_with(index, scale, &self.changes) }
            .map_err(|e| MediaError::Decode(format!("{}: frame {index}: {e}", self.info.name)))?;
        let frame = Arc::new(picture_to_frame(&pic).map_err(|e| MediaError::Decode(format!("{}: frame {index}: {e}", self.info.name)))?.with_pts(pts));
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((index, scale, frame.clone()));
        Ok(frame)
    }

    fn source_settings(&self) -> Option<serde_json::Value> {
        let settings = self.clip.settings();
        if settings.is_empty() {
            let why = self.clip.settings_note().unwrap_or_else(|| "Blackmagic's runtime reported no adjustable settings for this clip.".into());
            return Some(
                serde_json::json!({"kind": "braw", "title": "Blackmagic RAW", "settings": [], "message": format!("Blackmagic RAW settings are not available: {why}")}),
            );
        }
        let list: Vec<serde_json::Value> = settings.iter().map(|st| st.to_json(self.changes.get(st.key).unwrap_or(&st.as_shot))).collect();
        Some(serde_json::json!({"kind": "braw", "title": "Blackmagic RAW", "settings": list}))
    }

    fn with_source_settings(&self, settings: &serde_json::Value) -> Option<SharedSource> {
        let all = self.clip.settings();
        if all.is_empty() {
            return None;
        }
        let changes = sanitize(&all, settings);
        Some(Arc::new(BrawSource { clip: self.clip.clone(), info: self.info.clone(), last: Mutex::new(None), changes }))
    }

    fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        let Some(a) = self.clip.info().audio.clone().filter(|_| self.info.has_audio()) else {
            return Err(MediaError::NoStream("audio"));
        };
        let ch = a.channels as usize;
        let mut out = AudioBuffer::silence(sample_rate, ch, frames);
        if frames == 0 || sample_rate == 0 {
            return Ok(out);
        }
        // source frame for output frame `i` (linear interpolation when the rates differ)
        let ratio = a.sample_rate as f64 / sample_rate as f64;
        let src = |i: usize| start.saturating_add(i as i64) as f64 * ratio;
        let (lo, hi) = (src(0), src(frames - 1));
        if hi < 0.0 {
            return Ok(out);
        }
        let first = lo.max(0.0).floor() as u64;
        if first >= a.sample_count {
            return Ok(out);
        }
        let last = (hi.floor().max(0.0) as u64).saturating_add(1).min(a.sample_count - 1);
        let want = usize::try_from(last - first + 1).unwrap_or(usize::MAX);
        if want > MAX_AUDIO_READ {
            return Err(MediaError::Decode(format!("{}: audio request of {want} frames is too large", self.info.name)));
        }
        let block = self.clip.audio(first, want).map_err(|e| MediaError::Decode(format!("{}: audio: {e}", self.info.name)))?;
        let got = block.len() / ch;
        let at = |f: usize, c: usize| block.get(f * ch + c).copied().unwrap_or(0.0);
        for i in 0..frames {
            let s = src(i);
            if s < 0.0 {
                continue;
            }
            let pos = s - first as f64;
            let f0 = pos.floor() as usize;
            if f0 >= got {
                break;
            }
            let t = (pos - f0 as f64) as f32;
            let f1 = (f0 + 1).min(got - 1);
            for (c, dst) in out.channels.iter_mut().enumerate() {
                if let Some(d) = dst.get_mut(i) {
                    *d = at(f0, c) * (1.0 - t) + at(f1, c) * t;
                }
            }
        }
        Ok(out)
    }
}

/// A four-character code as the SDK packs it (first character in the high byte).
pub const fn fourcc(b: &[u8; 4]) -> u32 {
    (b[0] as u32) << 24 | (b[1] as u32) << 16 | (b[2] as u32) << 8 | b[3] as u32
}
/// Interleaved RGB, 16 bits per component (`blackmagicRawResourceFormatRGBU16`).
pub const FORMAT_RGB_U16: u32 = fourcc(b"16il");
/// Interleaved RGBA, 16 bits per component.
pub const FORMAT_RGBA_U16: u32 = fourcc(b"16al");

/// The common frame rates a float from the runtime stands for.
pub fn rate_from_f32(fps: f32) -> FrameRate {
    let f = fps as f64;
    for r in [FrameRate::FPS_23_976, FrameRate::FPS_29_97, FrameRate::FPS_59_94, FrameRate::FPS_119_88, FrameRate { num: 48000, den: 1001 }] {
        if (f - r.num as f64 / r.den as f64).abs() < 0.005 {
            return r;
        }
    }
    if f.is_finite() && f > 0.0 && (f - f.round()).abs() < 0.005 {
        return FrameRate { num: f.round() as i64, den: 1 };
    }
    if f.is_finite() && f > 0.0 { FrameRate { num: (f * 1000.0).round() as i64, den: 1000 } } else { FrameRate { num: 0, den: 1 } }
}

/// Little-endian PCM → f32.
pub fn pcm_to_f32(b: &[u8], bits: u32) -> Vec<f32> {
    match bits {
        16 => b.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32_768.0).collect(),
        24 => b.chunks_exact(3).map(|s| ((i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32) / 8_388_608.0).collect(),
        32 => b.chunks_exact(4).map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0).collect(),
        _ => Vec::new(),
    }
}

/// The processed image's bytes → a [`Picture`] (RGB or RGBA 16-bit, rows possibly padded).
pub fn to_picture(w: u32, h: u32, fmt: u32, bytes: &[u8]) -> Result<Picture, String> {
    let comps = match fmt {
        FORMAT_RGB_U16 => 3usize,
        FORMAT_RGBA_U16 => 4,
        other => return Err(format!("the runtime returned resource format 0x{other:08X}, not RGB 16-bit")),
    };
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("the runtime returned a {w}×{h} picture"));
    }
    let (wu, hu) = (w as usize, h as usize);
    let row = wu * comps * 2;
    let stride = if bytes.len() >= row * hu && bytes.len().is_multiple_of(hu) { bytes.len() / hu } else { row };
    if stride < row || bytes.len() < stride * (hu - 1) + row {
        return Err(format!("the runtime returned {} bytes for a {w}×{h} picture", bytes.len()));
    }
    let mut rgb = vec![0u16; wu * hu * 3];
    use rayon::prelude::*;
    rgb.par_chunks_mut(wu * 3).enumerate().for_each(|(y, out)| {
        // in range: checked above (stride * (h - 1) + row <= len)
        let Some(line) = bytes.get(y * stride..y * stride + row) else { return };
        for (px, o) in line.chunks_exact(comps * 2).zip(out.chunks_exact_mut(3)) {
            for (c, v) in o.iter_mut().enumerate() {
                *v = u16::from_le_bytes([px[c * 2], px[c * 2 + 1]]);
            }
        }
    });
    Ok(Picture { width: w, height: h, rgb })
}

/// An in-memory [`Runtime`] for tests: clips are generated, not read.
#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
