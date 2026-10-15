//! RED R3D through RED's R3D SDK (builds made with `FILMCRAFT_R3D_SDK`). `FILMCRAFT_R3D_LIBS`
//! names RED's redistributable libraries (the SDK's `Redistributable/<os>`); set
//! `FILMCRAFT_R3D_TEST_CLIP` to an `.R3D` file to also decode one (no RED media is committed).
#![cfg(filmcraft_r3d)]

use std::path::Path;

use filmcraft_media::{FrameRequest, MediaSource};
use filmcraft_platform::braw::Runtime;
use filmcraft_platform::r3d::sdk::SdkRuntime;
use serde_json::json;

#[test]
fn r3d_sdk_starts_refuses_bad_files_and_decodes_a_clip() {
    if std::env::var_os("FILMCRAFT_R3D_LIBS").is_none() {
        eprintln!("FILMCRAFT_R3D_LIBS not set: skipped");
        return;
    }
    let rt = SdkRuntime::default();
    // not a clip: a clean error, not a crash
    let dir = std::env::temp_dir().join(format!("fc-r3d-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bogus = dir.join("A001_C001_0101AA_001.R3D");
    std::fs::write(&bogus, vec![0x5Au8; 4096]).unwrap();
    let e = rt.open(&bogus).err().expect("garbage is refused");
    assert!(e.contains("not an R3D") || e.contains("could not be opened"), "{e}");
    assert!(rt.open(&dir.join("missing.R3D")).is_err());
    assert!(rt.decoding_on().is_some(), "the SDK started");
    let _ = std::fs::remove_dir_all(&dir);

    let Some(clip_path) = std::env::var_os("FILMCRAFT_R3D_TEST_CLIP") else {
        eprintln!("FILMCRAFT_R3D_TEST_CLIP not set: decode skipped");
        return;
    };
    let path = Path::new(&clip_path);
    let src = filmcraft_platform::braw::open_with(&rt, "test.R3D", path, None).expect("opens");
    let info = src.info().clone();
    let v = info.video.clone().unwrap();
    eprintln!("{} {}x{} {:?} {}", v.codec, v.width, v.height, v.frame_rate, info.duration.seconds());
    assert!(v.codec.starts_with("RED R3D"));
    for scale in [1.0f32, 0.5, 0.25, 0.125] {
        let t = std::time::Instant::now();
        let f = src.video_frame(FrameRequest { time: filmcraft_time::Tick::ZERO, scale }).expect("decodes");
        eprintln!("scale {scale}: {}x{} in {:?}", f.width, f.height, t.elapsed());
        assert!(f.width >= v.width / 9 && f.width <= v.width);
    }
    let sc = src.source_settings().expect("settings");
    let keys: Vec<&str> = sc["settings"].as_array().unwrap().iter().filter_map(|s| s["key"].as_str()).collect();
    for k in ["iso", "kelvin", "tint", "exposure", "colorSpace", "gamma"] {
        assert!(keys.contains(&k), "{k} in {keys:?}");
    }
    assert_eq!(sc["title"], "RED R3D");
    // a change reaches RED's pipeline: +2 stops is brighter
    let bright = src.with_source_settings(&json!({"exposure": 2.0})).unwrap();
    let mean = |s: &dyn MediaSource| {
        let f = s.video_frame(FrameRequest { time: filmcraft_time::Tick::ZERO, scale: 0.125 }).unwrap();
        let px = f.to_rgba8();
        px.iter().map(|&b| b as f64).sum::<f64>() / px.len().max(1) as f64
    };
    let (a, b) = (mean(&src), mean(&*bright));
    assert!(b > a + 1.0, "exposure +2: {a} → {b}");
    if info.has_audio() {
        let buf = src.audio(0, 4800, 48_000).unwrap();
        assert!(buf.peaks().iter().all(|p| p.is_finite()));
    }
}
