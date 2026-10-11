//! Blackmagic RAW end to end through the engine, with a generated stand-in for Blackmagic's
//! runtime: import a `.braw` file, cut it into a sequence, render, export picture and sound, save
//! and reopen the project, and fail cleanly when the runtime is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use filmcraft_engine::Session;
use filmcraft_platform::braw::{self, AudioInfo, Clip, ClipInfo, Missing, Picture, Runtime, Scale};
use filmcraft_time::FrameRate;
use serde_json::{Value, json};

const RATE: FrameRate = FrameRate::FPS_24;

struct Fake;
struct FakeClip(ClipInfo);

/// Frame `i` is a flat colour whose red channel steps with `i`.
fn red(i: u64) -> u16 {
    (i * 6_000).min(65_535) as u16
}

impl Runtime for Fake {
    fn name(&self) -> String {
        "test runtime".into()
    }
    fn open(&self, _path: &Path) -> Result<Arc<dyn Clip>, String> {
        let audio = Some(AudioInfo { sample_rate: 48_000, channels: 2, bits_per_sample: 24, sample_count: 48_000 * 2 });
        Ok(Arc::new(FakeClip(ClipInfo {
            width: 640,
            height: 360,
            frame_rate: RATE,
            frame_count: 48,
            audio,
            camera: Some("Test Camera".into()),
            start_timecode: None,
        })))
    }
}

impl Clip for FakeClip {
    fn info(&self) -> &ClipInfo {
        &self.0
    }
    fn decode(&self, index: u64, scale: Scale) -> Result<Picture, String> {
        let (w, h) = (self.0.width.div_ceil(scale.divisor()), self.0.height.div_ceil(scale.divisor()));
        Ok(Picture { width: w, height: h, rgb: [red(index), 20_000, 40_000].repeat((w * h) as usize) })
    }
    fn audio(&self, start: u64, frames: usize) -> Result<Vec<f32>, String> {
        let end = (start + frames as u64).min(48_000 * 2);
        Ok((start..end)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
                [v, v]
            })
            .collect())
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("filmcraft-braw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().to_string()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn v1(s: &mut Session) -> Vec<Value> {
    let q = s.execute("sequence.inspect", json!({})).unwrap();
    q["video"][0]["items"].as_array().cloned().unwrap_or_default()
}

/// Mean linear red of a rendered program frame.
fn mean_red(s: &Session, frame: i64) -> f32 {
    let img = s.try_render_program_at(0.25, RATE.tick_of(frame)).unwrap();
    let px = &img.px;
    let n = px.len() / 4;
    px.chunks_exact(4).map(|p| p[0]).sum::<f32>() / n as f32
}

#[test]
fn braw_imports_edits_renders_exports_and_reopens() {
    filmcraft_codecs::register_reader_opener(braw::reader_opener);
    let dir = Scratch::new("e2e");
    let clip = dir.path("A001_C003.braw");
    std::fs::write(&clip, b"not read by the opener: the runtime opens the path").unwrap();

    // 1. without a runtime: a clear error, no crash, nothing imported
    braw::set_missing(Missing::NotInstalled("BlackmagicRawAPI.dll not found".into()));
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name": "BRAW", "fps": 24.0, "width": 640, "height": 360})).unwrap();
    let r = s.execute("file.import", json!({"paths": [clip]}));
    let msg = match r {
        Err(e) => e.to_string(),
        Ok(v) => v["errors"].to_string(),
    };
    assert!(msg.contains("Blackmagic RAW installer"), "{msg}");

    // 2. with the runtime: import, probe info
    braw::set_runtime(Arc::new(Fake));
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name": "BRAW", "fps": 24.0, "width": 640, "height": 360})).unwrap();
    let imp = s.execute("file.import", json!({"paths": [clip]})).unwrap();
    assert_eq!(imp["errors"], json!([]), "{imp}");
    let item = imp["items"][0].as_u64().unwrap();

    // 3. edit: place, razor at frame 12, ripple-delete the head
    let placed = s.execute("timeline.place", json!({"item": item, "track": "V1", "frame": 0})).unwrap();
    assert_eq!(placed["clips"].as_array().unwrap().len(), 2, "video and its audio: {placed}");
    s.execute("sequence.addEdit", json!({"frame": 12})).unwrap();
    let items = v1(&mut s);
    assert_eq!(items.len(), 2, "{items:?}");
    let head = items[0]["clip"].as_u64().unwrap();
    s.execute("edit.rippleDelete", json!({"clips": [head]})).unwrap();
    let items = v1(&mut s);
    assert_eq!((items.len(), items[0]["startFrame"].as_i64(), items[0]["sourceIn"].as_i64()), (1, Some(0), Some(RATE.tick_of(12).0)));

    // 4. render: sequence frame 0 now shows source frame 12, frame 10 shows source frame 22
    let (r0, r10) = (mean_red(&s, 0), mean_red(&s, 10));
    let lin = |i: u64| filmcraft_color::srgb_to_linear(red(i) as f32 / 65_535.0);
    assert!((r0 - lin(12)).abs() < 0.01, "{r0} vs {}", lin(12));
    assert!((r10 - lin(22)).abs() < 0.01, "{r10} vs {}", lin(22));

    // 5. export picture (ProRes) and sound (WAV)
    let mov = s.execute("file.exportMedia", json!({"path": dir.path("out.mov"), "format": "prores", "range": "entire", "wait": true})).unwrap();
    let mov = mov["path"].as_str().unwrap().to_string();
    assert!(std::fs::metadata(&mov).unwrap().len() > 10_000, "{mov}");
    let wav = s.execute("file.exportMedia", json!({"path": dir.path("out.wav"), "format": "wav", "range": "entire", "wait": true})).unwrap();
    let wav = std::fs::read(wav["path"].as_str().unwrap()).unwrap();
    let pcm = wav.get(44..).unwrap_or_default();
    let loud = pcm.chunks_exact(2).filter(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs() > 1_000).count();
    assert!(loud > pcm.len() / 8, "the clip's tone is in the mix ({loud} loud samples)");

    // 6. save, reopen, still renders
    let proj = dir.path("braw.fcproj");
    s.execute("file.save", json!({"path": proj})).unwrap();
    let mut s2 = Session::default();
    s2.execute("file.open", json!({"path": proj})).unwrap();
    assert!((mean_red(&s2, 10) - lin(22)).abs() < 0.01);
}
