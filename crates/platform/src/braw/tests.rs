use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use filmcraft_frame::PixelData;
use filmcraft_media::{FrameRequest, MediaError, MediaSource, SharedReader};
use filmcraft_time::FrameRate;

use super::fake::{FakeRuntime, frame_rgb, sample};
use super::*;

fn audio48() -> Option<AudioInfo> {
    Some(AudioInfo { sample_rate: 48_000, channels: 2, bits_per_sample: 24, sample_count: 48_000 * 2 })
}

fn source(rt: &FakeRuntime) -> BrawSource {
    open_with(rt, "A001_C001.braw", Path::new("/media/A001_C001.braw"), Some(10_000_000)).unwrap()
}

#[test]
fn media_info_comes_from_the_clip() {
    let rt = FakeRuntime::new(6144, 3456, FrameRate::FPS_23_976, 48, audio48());
    let s = source(&rt);
    let i = s.info();
    let v = i.video.as_ref().unwrap();
    assert_eq!((v.width, v.height, v.frame_rate), (6144, 3456, FrameRate::FPS_23_976));
    assert_eq!(v.codec, "Blackmagic RAW (URSA Test)");
    assert_eq!(i.container, "Blackmagic RAW");
    assert_eq!(i.duration, FrameRate::FPS_23_976.tick_of(48));
    assert_eq!(i.start_timecode, Some(86_400));
    assert_eq!(i.audio_streams.len(), 1);
    assert_eq!((i.audio_streams[0].sample_rate, i.audio_streams[0].channels), (48_000, 2));
    assert!(v.bitrate.is_some_and(|b| b > 0));
}

#[test]
fn frames_map_time_to_the_right_index_and_colour() {
    let rate = FrameRate::FPS_24;
    let rt = FakeRuntime::new(64, 32, rate, 10, None);
    let s = source(&rt);
    for idx in [0u64, 1, 7, 9] {
        let f = s.video_frame(FrameRequest::full(rate.tick_of(idx as i64))).unwrap();
        assert_eq!((f.width, f.height), (64, 32));
        assert_eq!(f.pts, rate.tick_of(idx as i64));
        // round-trips through the Y'CbCr planes to the colour the decoder produced
        let rgba = f.to_rgba8();
        let want = frame_rgb(idx).map(|v| (v as f32 / 65_535.0 * 255.0).round() as i32);
        let got = [rgba[0] as i32, rgba[1] as i32, rgba[2] as i32];
        // to_rgba8 goes through linear light and back: allow a couple of code values
        for k in 0..3 {
            assert!((got[k] - want[k]).abs() <= 2, "frame {idx} channel {k}: {got:?} vs {want:?}");
        }
    }
    // past the end holds the last frame instead of failing
    let f = s.video_frame(FrameRequest::full(rate.tick_of(500))).unwrap();
    assert_eq!(f.pts, rate.tick_of(9));
}

#[test]
fn frames_are_16_bit_444() {
    let rt = FakeRuntime::new(8, 8, FrameRate::FPS_25, 2, None);
    let f = source(&rt).video_frame(FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap();
    assert!(matches!(f.data, PixelData::Yuv16 { bits: 16, chroma: filmcraft_frame::Chroma::C444, alpha: None, .. }), "{}", f.format_label());
    assert_eq!(f.color, FRAME_COLOR);
}

#[test]
fn reduced_scale_decodes_smaller_and_the_cache_serves_repeats() {
    let rt = FakeRuntime::new(6144, 3456, FrameRate::FPS_24, 4, None);
    let s = source(&rt);
    let t = FrameRate::FPS_24.tick_of(2);
    let small = s.video_frame(FrameRequest { time: t, scale: 0.2 }).unwrap();
    assert_eq!((small.width, small.height), (1536, 864));
    assert_eq!(rt.decodes.load(Ordering::Relaxed), 1);
    // same frame, same or smaller scale: cached
    s.video_frame(FrameRequest { time: t, scale: 0.2 }).unwrap();
    s.video_frame(FrameRequest { time: t, scale: 0.1 }).unwrap();
    assert_eq!(rt.decodes.load(Ordering::Relaxed), 1);
    // larger scale: decoded again at full size
    let full = s.video_frame(FrameRequest::full(t)).unwrap();
    assert_eq!((full.width, full.height), (6144, 3456));
    assert_eq!(rt.decodes.load(Ordering::Relaxed), 2);
}

#[test]
fn scale_choice() {
    assert_eq!(Scale::for_request(1.0), Scale::Full);
    assert_eq!(Scale::for_request(0.6), Scale::Full);
    assert_eq!(Scale::for_request(0.5), Scale::Half);
    assert_eq!(Scale::for_request(0.3), Scale::Half);
    assert_eq!(Scale::for_request(0.25), Scale::Quarter);
    assert_eq!(Scale::for_request(0.01), Scale::Eighth);
    assert_eq!(Scale::for_request(f32::NAN), Scale::Full);
}

#[test]
fn audio_reads_the_clip_and_resamples() {
    let rt = FakeRuntime::new(64, 32, FrameRate::FPS_24, 48, audio48());
    let s = source(&rt);
    let b = s.audio(1_000, 256, 48_000).unwrap();
    assert_eq!((b.sample_rate, b.channel_count(), b.frames()), (48_000, 2, 256));
    for i in [0usize, 100, 255] {
        assert_eq!(b.channels[0][i], sample(1_000 + i as u64, 0));
        assert_eq!(b.channels[1][i], sample(1_000 + i as u64, 1));
    }
    // half the rate: every other source sample
    let h = s.audio(500, 64, 24_000).unwrap();
    for i in 0..64 {
        assert!((h.channels[0][i] - sample(1_000 + 2 * i as u64, 0)).abs() < 1e-6);
    }
    // before the start and past the end: silence, not an error
    let pre = s.audio(-100, 50, 48_000).unwrap();
    assert!(pre.channels.iter().all(|c| c.iter().all(|v| *v == 0.0)));
    let post = s.audio(1_000_000, 50, 48_000).unwrap();
    assert!(post.channels.iter().all(|c| c.iter().all(|v| *v == 0.0)));
}

#[test]
fn no_audio_stream_is_reported_as_such() {
    let rt = FakeRuntime::new(64, 32, FrameRate::FPS_24, 4, None);
    let s = source(&rt);
    assert!(!s.info().has_audio());
    assert!(matches!(s.audio(0, 10, 48_000), Err(MediaError::NoStream("audio"))));
}

#[test]
fn hostile_metadata_is_refused_not_trusted() {
    for (w, h, rate, frames) in [
        (0, 1080, FrameRate::FPS_24, 10),
        (100_000, 1080, FrameRate::FPS_24, 10),
        (1920, 1080, FrameRate { num: 0, den: 1 }, 10),
        (1920, 1080, FrameRate { num: 24, den: -1 }, 10),
        (1920, 1080, FrameRate::FPS_24, 0),
        (1920, 1080, FrameRate::FPS_24, u64::MAX),
    ] {
        let rt = FakeRuntime::new(w, h, rate, frames, None);
        let r = open_with(&rt, "x.braw", Path::new("/x.braw"), None);
        assert!(matches!(r, Err(MediaError::Decode(_))), "{w}×{h} {rate:?} {frames}");
    }
    // absurd audio metadata: the clip opens without audio
    let rt = FakeRuntime::new(64, 32, FrameRate::FPS_24, 4, Some(AudioInfo { sample_rate: 48_000, channels: 10_000, bits_per_sample: 24, sample_count: 10 }));
    assert!(!source(&rt).info().has_audio());
}

#[test]
fn a_bad_picture_from_the_decoder_is_an_error() {
    let short = Picture { width: 4, height: 4, rgb: vec![0; 10] };
    assert!(picture_to_frame(&short).is_err());
    let zero = Picture { width: 0, height: 4, rgb: vec![] };
    assert!(picture_to_frame(&zero).is_err());
}

#[test]
fn damaged_clip_is_a_clean_error() {
    let rt = FakeRuntime::new(64, 32, FrameRate::FPS_24, 4, None);
    let e = open_with(&rt, "corrupt.braw", Path::new("/media/corrupt.braw"), None).err().unwrap();
    assert!(e.to_string().contains("could not open the clip"), "{e}");
}

/// A reader with a path but no bytes (the BRAW opener never reads through it).
struct PathReader(PathBuf);
impl filmcraft_media::ByteReader for PathReader {
    fn len(&self) -> u64 {
        1
    }
    fn read_at(&self, _: u64, _: &mut [u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn path(&self) -> Option<&Path> {
        Some(&self.0)
    }
}

#[test]
fn opener_claims_only_braw_and_explains_a_missing_runtime() {
    let _g = RUNTIME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r: SharedReader = Arc::new(PathReader("/m/clip.braw".into()));
    assert!(reader_opener("clip.mov", &[], &r).is_none());
    assert!(reader_opener("clip.mp4", &[], &r).is_none());
    for (m, needle) in
        [(Missing::Platform, "on Windows"), (Missing::NotInstalled("not found".into()), "Blackmagic RAW installer"), (Missing::Unfinished, "in progress")]
    {
        set_missing(m);
        let e = reader_opener("clip.BRAW", &[], &r).unwrap().err().unwrap();
        assert!(matches!(e, MediaError::Unsupported(_)) && e.to_string().contains(needle), "{e}");
    }
    set_runtime(Arc::new(FakeRuntime::new(64, 32, FrameRate::FPS_24, 4, None)));
    let s = reader_opener("clip.braw", &[], &r).unwrap().unwrap();
    assert_eq!(s.info().container, "Blackmagic RAW");
    // a reader with no file behind it (the web build's Blob) cannot be opened by path
    let mem: SharedReader = Arc::new(filmcraft_media::reader::MemReader(Arc::from(vec![0u8; 4])));
    assert!(reader_opener("clip.braw", &[], &mem).unwrap().is_err());
    set_missing(Missing::NotRegistered);
}

/// Tests that change the process-wide runtime run one at a time.
pub(crate) static RUNTIME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn le(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn processed_image_bytes_become_pictures() {
    // tight RGB rows
    let rgb: Vec<u16> = (0..2 * 2 * 3).map(|i| i as u16 * 1000).collect();
    let p = to_picture(2, 2, FORMAT_RGB_U16, &le(&rgb)).unwrap();
    assert_eq!((p.width, p.height, p.rgb.clone()), (2, 2, rgb.clone()));
    // RGBA: alpha dropped
    let rgba: Vec<u16> = (0..2 * 2 * 4).map(|i| i as u16).collect();
    let p = to_picture(2, 2, FORMAT_RGBA_U16, &le(&rgba)).unwrap();
    assert_eq!(p.rgb, vec![0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14]);
    // padded rows (stride 16 bytes for a 2-pixel RGB row of 12)
    let mut padded = Vec::new();
    for row in rgb.chunks(6) {
        padded.extend(le(row));
        padded.extend([0xAA; 4]);
    }
    assert_eq!(to_picture(2, 2, FORMAT_RGB_U16, &padded).unwrap().rgb, rgb);
    // refused: short buffer, wrong format, absurd size
    assert!(to_picture(2, 2, FORMAT_RGB_U16, &le(&rgb[..6])).is_err());
    assert!(to_picture(2, 2, fourcc(b"rgba"), &le(&rgb)).is_err());
    assert!(to_picture(0, 2, FORMAT_RGB_U16, &[]).is_err());
    assert!(to_picture(100_000, 1, FORMAT_RGB_U16, &[]).is_err());
}

#[test]
fn fourcc_packs_like_the_sdk() {
    assert_eq!(fourcc(b"16il"), 0x3136_696C);
    assert_eq!(FORMAT_RGB_U16, u32::from_be_bytes(*b"16il"));
}

#[test]
fn pcm_converts_at_each_depth() {
    assert_eq!(pcm_to_f32(&[0x00, 0x40, 0x00, 0xC0], 16), vec![0.5, -0.5]);
    assert_eq!(pcm_to_f32(&[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], 24), vec![0.5, -0.5]);
    assert_eq!(pcm_to_f32(&[0, 0, 0, 0x40, 0, 0, 0, 0xC0], 32), vec![0.5, -0.5]);
    assert!(pcm_to_f32(&[1, 2, 3], 12).is_empty());
    // a ragged tail is ignored, not read past
    assert_eq!(pcm_to_f32(&[0x00, 0x40, 0x01], 16).len(), 1);
}

#[test]
fn frame_rates_from_the_runtime() {
    assert_eq!(rate_from_f32(23.976), FrameRate::FPS_23_976);
    assert_eq!(rate_from_f32(23.976_025), FrameRate::FPS_23_976);
    assert_eq!(rate_from_f32(29.97), FrameRate::FPS_29_97);
    assert_eq!(rate_from_f32(59.94), FrameRate::FPS_59_94);
    assert_eq!(rate_from_f32(24.0), FrameRate::FPS_24);
    assert_eq!(rate_from_f32(25.0), FrameRate::FPS_25);
    assert_eq!(rate_from_f32(120.0), FrameRate::FPS_120);
    assert_eq!(rate_from_f32(12.5), FrameRate { num: 12_500, den: 1000 });
    assert_eq!(rate_from_f32(f32::NAN).num, 0);
    assert_eq!(rate_from_f32(-5.0).num, 0);
}

#[test]
fn source_settings_schema_and_values() {
    let rt = FakeRuntime::new(8, 8, FrameRate::FPS_24, 4, None);
    let s = source(&rt);
    let sc = s.source_settings().unwrap();
    assert_eq!(sc["kind"], "braw");
    let list = sc["settings"].as_array().unwrap();
    let get = |v: &serde_json::Value, k: &str| v["settings"].as_array().unwrap().iter().find(|e| e["key"] == k).cloned().unwrap();
    assert_eq!(list.len(), super::fake::fake_settings().len());
    assert_eq!(get(&sc, "gamma")["type"], "choice");
    assert_eq!(get(&sc, "exposure")["value"], 0.0);
    assert_eq!(get(&sc, "colorScienceGen")["readOnly"], true);
    // with settings: values in effect follow; bad keys and values are dropped, numbers clamped
    let s2 = s.with_source_settings(&serde_json::json!({"exposure": 9.0, "iso": 1600, "gamma": "nonsense", "bogus": 1, "colorScienceGen": 7})).unwrap();
    let sc2 = s2.source_settings().unwrap();
    assert_eq!(get(&sc2, "exposure")["value"], 5.0);
    assert_eq!(get(&sc2, "iso")["value"], 1600);
    assert_eq!(get(&sc2, "gamma")["value"], "Blackmagic Design Film");
    assert_eq!(get(&sc2, "colorScienceGen")["value"], 5);
}

#[test]
fn settings_reach_the_decoder() {
    let rate = FrameRate::FPS_24;
    let rt = FakeRuntime::new(8, 8, rate, 4, None);
    let s = source(&rt);
    let t = rate.tick_of(1);
    let plain = s.video_frame(FrameRequest::full(t)).unwrap().to_rgba8();
    let brighter = s.with_source_settings(&serde_json::json!({"exposure": 1.0})).unwrap().video_frame(FrameRequest::full(t)).unwrap().to_rgba8();
    assert!(brighter[1] > plain[1], "one stop up is brighter: {} vs {}", brighter[1], plain[1]);
    let grey = s.with_source_settings(&serde_json::json!({"saturation": 0.0})).unwrap().video_frame(FrameRequest::full(t)).unwrap().to_rgba8();
    assert!((grey[0] as i32 - grey[1] as i32).abs() <= 1 && (grey[1] as i32 - grey[2] as i32).abs() <= 1, "saturation 0 is grey: {:?}", &grey[..3]);
    // a clip without settings has no schema and no settings variant
    struct Plain(ClipInfo);
    impl Clip for Plain {
        fn info(&self) -> &ClipInfo {
            &self.0
        }
        fn decode(&self, _: u64, _: Scale) -> Result<Picture, String> {
            Ok(Picture { width: 1, height: 1, rgb: vec![0; 3] })
        }
        fn audio(&self, _: u64, _: usize) -> Result<Vec<f32>, String> {
            Ok(vec![])
        }
    }
    let p = BrawSource::new("p.braw", Arc::new(Plain(rt.info.clone())), None).unwrap();
    assert!(p.source_settings().is_none() && p.with_source_settings(&serde_json::json!({"exposure": 1})).is_none());
}
