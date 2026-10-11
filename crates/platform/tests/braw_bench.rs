//! How long FilmCraft's own work on a 6K Blackmagic RAW frame takes (conversion and rendering),
//! with an instant stand-in decoder. `cargo test --release -p filmcraft-platform --test braw_bench -- --ignored --nocapture`

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use filmcraft_engine::Session;
use filmcraft_platform::braw::{self, Clip, ClipInfo, Picture, Runtime, Scale};
use filmcraft_time::FrameRate;
use serde_json::json;

struct Fake;
struct FakeClip(ClipInfo);

impl Runtime for Fake {
    fn name(&self) -> String {
        "bench".into()
    }
    fn open(&self, _: &Path) -> Result<Arc<dyn Clip>, String> {
        Ok(Arc::new(FakeClip(ClipInfo {
            width: 6144,
            height: 3456,
            frame_rate: FrameRate::FPS_24,
            frame_count: 240,
            audio: None,
            camera: None,
            start_timecode: None,
        })))
    }
}

impl Clip for FakeClip {
    fn info(&self) -> &ClipInfo {
        &self.0
    }
    fn decode(&self, i: u64, scale: Scale) -> Result<Picture, String> {
        let (w, h) = (self.0.width / scale.divisor(), self.0.height / scale.divisor());
        let v = (i as u16).wrapping_mul(97);
        Ok(Picture { width: w, height: h, rgb: [v, 20_000, 40_000].repeat((w * h) as usize) })
    }
    fn audio(&self, _: u64, _: usize) -> Result<Vec<f32>, String> {
        Err("none".into())
    }
}

#[test]
#[ignore]
fn bench_6k() {
    filmcraft_codecs::register_reader_opener(braw::reader_opener);
    braw::set_runtime(Arc::new(Fake));
    let dir = std::env::temp_dir().join(format!("fc-braw-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("A.braw");
    std::fs::write(&clip, b"x").unwrap();
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name": "S", "fps": 24.0, "width": 6144, "height": 3456})).unwrap();
    let item = s.execute("file.import", json!({"paths": [clip.to_string_lossy()]})).unwrap()["items"][0].as_u64().unwrap();
    s.execute("timeline.place", json!({"item": item, "seconds": 0.0})).unwrap();
    let src = s.media.full_res_source(&s.project, filmcraft_project::ItemId(item), &*s.services).unwrap();
    for scale in [1.0f32, 0.5, 0.25] {
        let n = 6;
        let t = Instant::now();
        for i in 0..n {
            let _ = src.video_frame(filmcraft_media::FrameRequest { time: FrameRate::FPS_24.tick_of(i), scale }).unwrap();
        }
        eprintln!("source frame at {scale}: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        let t = Instant::now();
        for i in 10..10 + n {
            let _ = s.try_render_program_at(scale, FrameRate::FPS_24.tick_of(i)).unwrap();
        }
        eprintln!("program render at {scale}: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }
}
