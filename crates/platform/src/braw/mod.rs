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
    /// Decode frame `index` (0-based) at `scale`.
    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String>;
    /// `frames` sample frames from `start`, interleaved, as f32 in -1..1. Fewer at the end.
    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String>;
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
        Ok(Self { clip, info, last: Mutex::new(None) })
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
    for (i, px) in p.rgb.chunks_exact(3).enumerate() {
        let (Some(dy), Some(db), Some(dr)) = (y.get_mut(i), cb.get_mut(i), cr.get_mut(i)) else { break };
        let [r, g, b] = [px[0], px[1], px[2]].map(|v| v as f32 / MAX);
        let [yy, u, v] = rgb_to_ycbcr(r, g, b, Matrix::Bt709);
        *dy = q(yy * MAX);
        *db = q(u * MAX + MID);
        *dr = q(v * MAX + MID);
    }
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
        let pic = self.clip.decode(index, scale).map_err(|e| MediaError::Decode(format!("{}: frame {index}: {e}", self.info.name)))?;
        let frame = Arc::new(picture_to_frame(&pic).map_err(|e| MediaError::Decode(format!("{}: frame {index}: {e}", self.info.name)))?.with_pts(pts));
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((index, scale, frame.clone()));
        Ok(frame)
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

/// An in-memory [`Runtime`] for tests: clips are generated, not read.
#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
