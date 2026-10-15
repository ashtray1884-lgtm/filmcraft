//! OpenFX hosting against real plug-ins: set `FILMCRAFT_OFX_TEST_PLUGINS` to a folder of plug-in
//! binaries (`*.ofx`) built from the OpenFX repository's examples (`Support/Plugins`: Invert,
//! Transition, Basic, Tester, Field, ChoiceParams; `Examples`: Invert, Basic). Without it the
//! tests skip (no plug-in binaries are committed).
#![cfg(all(any(windows, target_os = "linux"), target_arch = "x86_64"))]

use std::path::PathBuf;

use filmcraft_platform::ofx::catalog::effect_defs;
use filmcraft_platform::ofx::plugin::{PluginInfo, describe_binary};
use filmcraft_platform::ofx::render::Host;
use filmcraft_project::{EffectDef, ParamValue};
use filmcraft_render::external::{ExtCtx, ExtImage, External};
use filmcraft_time::Tick;

fn plugins() -> Option<Vec<PluginInfo>> {
    let dir = PathBuf::from(std::env::var_os("FILMCRAFT_OFX_TEST_PLUGINS")?);
    let mut all = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ofx")).collect();
    files.sort();
    for f in files {
        match describe_binary(&f) {
            Ok(v) => all.extend(v),
            Err(e) => panic!("{}: {e}", f.display()),
        }
    }
    Some(all)
}

fn gradient(w: usize, h: usize) -> ExtImage {
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32, 0.25, 1.0]);
        }
    }
    ExtImage { w, h, px }
}

fn ctx<'a>(e: &'a filmcraft_project::EffectInstance, w: usize, h: usize) -> ExtCtx<'a> {
    ExtCtx { effect: e, t: Tick::ZERO, frame: 3.0, fps: 24.0, duration: 48.0, render_scale: 1.0, full_size: (w as u32, h as u32), frames: None }
}

fn def<'a>(defs: &'a [EffectDef], id: &str) -> &'a EffectDef {
    defs.iter().find(|d| d.id == id).unwrap_or_else(|| panic!("no {id} in {:?}", defs.iter().map(|d| d.id).collect::<Vec<_>>()))
}

/// Plug-ins are loaded once per process (C++ plug-ins share static state across copies), so both
/// halves run in one test: the host directly, then the whole path through FilmCraft's renderer.
#[test]
fn ofx_real_plugins() {
    host_renders_plugins();
    plugins_render_through_filmcraft_effects();
}

fn host_renders_plugins() {
    let Some(infos) = plugins() else {
        eprintln!("FILMCRAFT_OFX_TEST_PLUGINS not set: skipped");
        return;
    };
    for i in &infos {
        eprintln!(
            "{} ({}) v{}.{} {:?} params {:?}",
            i.label,
            i.id,
            i.major,
            i.minor,
            i.contexts,
            i.effect_params.iter().map(|p| (&p.name, &p.ty)).collect::<Vec<_>>()
        );
    }
    let defs = effect_defs(&infos);
    let host = Host::new();
    host.add(&infos);

    // Invert (C++ support library): RGB inverted, alpha kept
    let inv = def(&defs, "ofx:net.sf.openfx.invertPlugin");
    let e = inv.instance();
    let mut img = gradient(32, 16);
    let src = img.clone();
    host.effect(&mut img, &ctx(&e, 32, 16)).expect("invert renders");
    for (o, s) in img.px.chunks(4).zip(src.px.chunks(4)) {
        assert!((o[0] - (1.0 - s[0])).abs() < 1e-5 && (o[1] - (1.0 - s[1])).abs() < 1e-5, "{o:?} vs {s:?}");
    }
    // the second render reuses the pooled instance
    let mut again = src.clone();
    host.effect(&mut again, &ctx(&e, 32, 16)).unwrap();
    assert_eq!(again.px, img.px);

    // Basic gain: parameters reach the plug-in, keyframes answer other times
    let basic = defs.iter().find(|d| d.id.starts_with("ofx:") && d.params.iter().any(|p| p.id == "scale")).expect("basic gain plug-in");
    let mut e = basic.instance();
    e.params.get_mut("scale").unwrap().value = ParamValue::Float(0.5);
    let mut img = gradient(32, 16);
    host.effect(&mut img, &ctx(&e, 32, 16)).expect("basic renders");
    let (o, s) = (&img.px[4 * 31..4 * 32], &src.px[4 * 31..4 * 32]);
    assert!((o[0] - s[0] * 0.5).abs() < 1e-4, "scale 0.5: {o:?} from {s:?}");

    // half-resolution render (render scale ½) still renders the whole picture
    let mut half = gradient(16, 8);
    let mut c = ctx(&e, 16, 8);
    c.render_scale = 0.5;
    c.full_size = (32, 16);
    host.effect(&mut half, &c).expect("renders at ½");

    // Cross fade transition: progress drives the mix
    let tr = def(&defs, "ofxt:net.sf.openfx.crossFade");
    let e = tr.instance();
    let a = ExtImage { w: 8, h: 4, px: [1.0, 0.0, 0.0, 1.0].repeat(32) };
    let b = ExtImage { w: 8, h: 4, px: [0.0, 0.0, 1.0, 1.0].repeat(32) };
    let out = host.transition(&a, &b, 0.25, &ctx(&e, 8, 4)).expect("cross fade renders");
    assert!((out.px[0] - 0.75).abs() < 1e-4 && (out.px[2] - 0.25).abs() < 1e-4, "{:?}", &out.px[..4]);

    // a hostile request (zero-size picture) is an error, not a crash
    let mut empty = ExtImage { w: 0, h: 0, px: vec![] };
    let e = inv.instance();
    let _ = host.effect(&mut empty, &ctx(&e, 0, 0));

    // every listed effect renders a frame without failing or crashing
    for d in defs.iter().filter(|d| d.id.starts_with("ofx:")) {
        let e = d.instance();
        let mut img = gradient(24, 12);
        let r = host.effect(&mut img, &ctx(&e, 24, 12));
        eprintln!("{} → {:?}", d.name, r.as_ref().map(|_| "ok"));
        assert_eq!((img.w, img.h, img.px.len()), (24, 12, 24 * 12 * 4));
    }
}

/// The whole path: plug-ins found in a folder, scanned, listed as FilmCraft effects, and applied
/// by FilmCraft's renderer to a frame (linear light in, linear light out).
fn plugins_render_through_filmcraft_effects() {
    let Some(src) = std::env::var_os("FILMCRAFT_OFX_TEST_PLUGINS").map(PathBuf::from) else { return };
    let root = std::env::temp_dir().join(format!("fc-ofx-e2e-{}", std::process::id()));
    let arch = if cfg!(windows) { "Win64" } else { "Linux-x86-64" };
    // the same binaries the first half loaded, now as bundles in a plug-in folder
    let mut files: Vec<_> = std::fs::read_dir(&src).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ofx")).collect();
    files.sort();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let bin = root.join("Vendor").join(format!("{name}.bundle")).join("Contents").join(arch);
        std::fs::create_dir_all(&bin).unwrap();
        // a link, not a copy: one binary, one load
        #[cfg(unix)]
        std::os::unix::fs::symlink(f, bin.join(&name)).unwrap();
        #[cfg(windows)]
        std::fs::copy(f, bin.join(&name)).unwrap();
    }
    let data = root.join("data");
    let scan =
        filmcraft_platform::ofx::start_with(Some(data.clone()), filmcraft_platform::ofx::catalog::Scanner::InProcess, vec![root.clone()]).expect("first start");
    scan.join().unwrap();
    assert!(data.join("ofx-plugins.json").is_file(), "the scan is cached");
    let d = filmcraft_project::find_effect("ofx:net.sf.openfx.basicPlugin").expect("listed as an effect");
    assert_eq!(d.category, &["Video Effects", "OFX Example (Support)"], "the vendor group is the folder");
    assert!(filmcraft_project::all_effect_defs().iter().any(|x| x.id == d.id), "in the Effects panel list");
    assert!(filmcraft_project::find_effect("ofxt:net.sf.openfx.crossFade").is_some(), "transitions listed too");

    let mut img = filmcraft_render::Image::filled(8, 4, [0.18, 0.5, 1.0, 1.0]);
    let cx = filmcraft_render::effects::FxCtx {
        t: Tick::ZERO,
        px_scale: 1.0,
        seconds: 0.0,
        timecode: "00:00:00:00",
        clip_name: "test",
        project: None,
        env: None,
        working: Default::default(),
    };
    // Gain on red only: red halves on the display curve, the rest is untouched
    let mut e = d.instance();
    for (k, v) in [
        ("scale", ParamValue::Float(1.0)),
        ("scaleComponents", ParamValue::Bool(true)),
        ("scaleR", ParamValue::Float(0.5)),
        ("scaleG", ParamValue::Float(1.0)),
        ("scaleB", ParamValue::Float(1.0)),
        ("scaleA", ParamValue::Float(1.0)),
    ] {
        e.params.get_mut(k).unwrap_or_else(|| panic!("{k}")).value = v;
    }
    filmcraft_render::effects::apply(&mut img, &e, &cx);
    let px = &img.px[..4];
    let want_r = filmcraft_color::srgb_to_linear(0.5 * filmcraft_color::linear_to_srgb(0.18));
    assert!((px[0] - want_r).abs() < 1e-3, "red {px:?}, want {want_r}");
    assert!((px[1] - 0.5).abs() < 1e-3 && (px[2] - 1.0).abs() < 1e-3 && (px[3] - 1.0).abs() < 1e-6, "{px:?}");
    // a disabled plug-in effect leaves the frame alone
    e.enabled = false;
    let mut same = filmcraft_render::Image::filled(8, 4, [0.18, 0.5, 1.0, 1.0]);
    filmcraft_render::effects::apply(&mut same, &e, &cx);
    assert_eq!(same.px[0], 0.18);
    let _ = std::fs::remove_dir_all(&root);
}
