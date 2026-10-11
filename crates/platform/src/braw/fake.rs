//! A generated stand-in for Blackmagic's runtime: lets the import path, the media source and the
//! timeline be tested on any system, without the runtime or real clips.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use filmcraft_time::FrameRate;

use super::{AudioInfo, Clip, ClipInfo, Picture, Runtime, Scale};

/// Opens any path whose file name does not contain "corrupt" as a generated clip.
pub struct FakeRuntime {
    pub info: ClipInfo,
    pub decodes: Arc<AtomicUsize>,
}

impl FakeRuntime {
    pub fn new(width: u32, height: u32, frame_rate: FrameRate, frame_count: u64, audio: Option<AudioInfo>) -> Self {
        let info = ClipInfo { width, height, frame_rate, frame_count, audio, camera: Some("URSA Test".into()), start_timecode: Some(86_400) };
        Self { info, decodes: Arc::default() }
    }
}

impl Runtime for FakeRuntime {
    fn name(&self) -> String {
        "Fake Blackmagic RAW".into()
    }
    fn open(&self, path: &Path) -> Result<Arc<dyn Clip>, String> {
        if path.to_string_lossy().contains("corrupt") {
            return Err("the clip is damaged".into());
        }
        Ok(Arc::new(FakeClip { info: self.info.clone(), decodes: self.decodes.clone() }))
    }
}

pub struct FakeClip {
    pub info: ClipInfo,
    pub decodes: Arc<AtomicUsize>,
}

/// The colour frame `index` is filled with: red steps with the frame, green and blue fixed.
pub fn frame_rgb(index: u64) -> [u16; 3] {
    [((index * 4_000) % 65_536) as u16, 32_768, 8_192]
}

/// Sample `i` of channel `c` of the fake audio: a distinct ramp per channel.
pub fn sample(i: u64, c: usize) -> f32 {
    ((i % 1_000) as f32 / 1_000.0) * if c == 0 { 0.5 } else { -0.25 }
}

impl Clip for FakeClip {
    fn info(&self) -> &ClipInfo {
        &self.info
    }
    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String> {
        if index >= self.info.frame_count {
            return Err(format!("frame {index} is past the end"));
        }
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let (w, h) = (self.info.width.div_ceil(scale.divisor()), self.info.height.div_ceil(scale.divisor()));
        let px = frame_rgb(index);
        Ok(Picture { width: w, height: h, rgb: px.repeat((w * h) as usize) })
    }
    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String> {
        let Some(a) = &self.info.audio else { return Err("no audio".into()) };
        let end = (start + frames as u64).min(a.sample_count);
        let ch = a.channels as usize;
        Ok((start..end).flat_map(|i| (0..ch).map(move |c| sample(i, c))).collect())
    }
}
