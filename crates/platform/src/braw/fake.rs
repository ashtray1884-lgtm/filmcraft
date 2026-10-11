//! A generated stand-in for Blackmagic's runtime: lets the import path, the media source and the
//! timeline be tested on any system, without the runtime or real clips.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use filmcraft_time::FrameRate;

use serde_json::json;

use super::{AudioInfo, Changes, Clip, ClipInfo, Kind, Picture, Runtime, Scale, Setting};

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

/// The settings the fake clip offers (a subset of the real runtime's, with the same keys).
pub fn fake_settings() -> Vec<Setting> {
    let n = |key, label, group, min, max, step, shot: f64| Setting {
        key,
        label,
        group,
        kind: Kind::Number { min, max, step },
        as_shot: json!(shot),
        read_only: false,
        note: None,
    };
    vec![
        Setting {
            key: "gamma",
            label: "Gamma",
            group: "Color",
            kind: Kind::Choice(vec![json!("Blackmagic Design Film"), json!("Blackmagic Design Video"), json!("Blackmagic Design Custom")]),
            as_shot: json!("Blackmagic Design Film"),
            read_only: false,
            note: None,
        },
        Setting {
            key: "iso",
            label: "ISO",
            group: "Image",
            kind: Kind::Choice(vec![json!(400), json!(800), json!(1600)]),
            as_shot: json!(800),
            read_only: false,
            note: None,
        },
        n("exposure", "Exposure", "Image", -5.0, 5.0, 0.1, 0.0),
        n("saturation", "Saturation", "Custom Gamma", 0.0, 2.0, 0.01, 1.0),
        Setting {
            key: "highlightRecovery",
            label: "Highlight Recovery",
            group: "Image",
            kind: Kind::Toggle,
            as_shot: json!(false),
            read_only: false,
            note: None,
        },
        Setting {
            key: "colorScienceGen",
            label: "Color Science",
            group: "Color",
            kind: Kind::Number { min: 0.0, max: 9.0, step: 1.0 },
            as_shot: json!(5),
            read_only: true,
            note: None,
        },
    ]
}

impl Clip for FakeClip {
    fn info(&self) -> &ClipInfo {
        &self.info
    }
    fn settings(&self) -> Vec<Setting> {
        fake_settings()
    }
    /// Exposure scales the picture by 2^stops; ISO by iso/800; saturation pulls green and blue
    /// toward red. Enough to see that settings arrive.
    fn decode_with(&self, index: u64, scale: Scale, changes: &Changes) -> Result<Picture, String> {
        let mut p = self.decode(index, scale)?;
        let stops = changes.get("exposure").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let iso = changes.get("iso").and_then(|v| v.as_f64()).unwrap_or(800.0);
        let sat = changes.get("saturation").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let gain = 2f64.powf(stops) * iso / 800.0;
        for px in p.rgb.chunks_exact_mut(3) {
            let r = px[0] as f64 * gain;
            let (g, b) = (px[1] as f64 * gain, px[2] as f64 * gain);
            let mix = |c: f64| r + (c - r) * sat;
            px[0] = r.round().clamp(0.0, 65_535.0) as u16;
            px[1] = mix(g).round().clamp(0.0, 65_535.0) as u16;
            px[2] = mix(b).round().clamp(0.0, 65_535.0) as u16;
        }
        Ok(p)
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
