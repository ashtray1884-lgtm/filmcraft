//! VST3 hosting against real plug-ins: set `FILMCRAFT_VST3_TEST_PLUGINS` to a folder of `.vst3`
//! bundles (the `vst3` crate's `gain` example; Steinberg's SDK samples such as AGain). Without it
//! the tests skip (no plug-in binaries are committed).
#![cfg(all(any(windows, target_os = "linux"), target_arch = "x86_64"))]

use std::path::PathBuf;

use filmcraft_platform::vst::catalog::{PluginInfo, Scanner, describe_bundle, effect_defs, find_bundles, start_with};
use filmcraft_platform::vst::fx::{STATE, decode_state, encode_state};
use filmcraft_project::ParamValue;

fn bundles() -> Option<(PathBuf, Vec<PathBuf>)> {
    let dir = PathBuf::from(std::env::var_os("FILMCRAFT_VST3_TEST_PLUGINS")?);
    let b = find_bundles(&dir);
    Some((dir, b))
}

fn tone(n: usize) -> Vec<Vec<f32>> {
    let l: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    vec![l.clone(), l]
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

#[test]
fn vst3_real_plugins() {
    let Some((dir, list)) = bundles() else {
        eprintln!("FILMCRAFT_VST3_TEST_PLUGINS not set: skipped");
        return;
    };
    assert!(!list.is_empty(), "no bundles in {}", dir.display());
    let mut infos: Vec<PluginInfo> = Vec::new();
    for b in &list {
        let v = describe_bundle(b).unwrap_or_else(|e| panic!("{}: {e}", b.display()));
        for i in &v {
            eprintln!(
                "{} by {} [{}] params {:?} editor {} in {} out {}",
                i.name,
                i.vendor,
                i.sub_categories,
                i.params.iter().map(|p| (&p.title, &p.units, p.plain_min, p.plain_max)).collect::<Vec<_>>(),
                i.has_editor,
                i.inputs,
                i.outputs
            );
        }
        infos.extend(v);
    }
    let defs = effect_defs(&infos);
    assert_eq!(defs.len(), infos.len());

    // through FilmCraft: the scan registers the effects and the audio host
    let data = std::env::temp_dir().join(format!("fc-vst3-{}", std::process::id()));
    start_with(Some(data.clone()), Scanner::InProcess, vec![dir.clone()]).expect("first start").join().unwrap();
    assert!(data.join("vst3-plugins.json").is_file());
    for d in &defs {
        let def = filmcraft_project::find_effect(d.id).expect("registered");
        assert_eq!(def.kind, filmcraft_project::EffectKind::Audio);
        assert!(filmcraft_render::audio_fx::supported(def.id), "usable on clips and inserts");
        let e = def.instance();
        let mut fx = filmcraft_render::audio_fx::configured(&e, filmcraft_time::Tick::ZERO, 48_000).unwrap_or_else(|| panic!("{} starts", def.name));
        let mut sig = tone(4800);
        {
            let (l, r) = sig.split_at_mut(1);
            fx.process(&mut [&mut l[0][..], &mut r[0][..]]);
        }
        assert!(sig.iter().flatten().all(|v| v.is_finite()), "{}", def.name);
        eprintln!("{}: rms {:.3} after processing (input {:.3})", def.name, rms(&sig[0]), rms(&tone(4800)[0]));
    }

    // FilmCraft's default parameters leave each plug-in as it starts on its own (deterministic
    // plug-ins only: two plain runs must agree first). At 44.1 kHz, so FilmCraft makes new
    // instances rather than reusing the ones above (reused ones keep LFO phases running).
    let raw = |i: &PluginInfo| -> f32 {
        let m = filmcraft_platform::vst::module::open(std::path::Path::new(&i.bundle)).unwrap();
        let cid = filmcraft_platform::vst::module::cid_from_hex(&i.cid).unwrap();
        let mut inst = filmcraft_platform::vst::module::Instance::new(&m, &cid, 44_100.0, 2, 4096).unwrap();
        let ch = vst3::ComWrapper::new(filmcraft_platform::vst::host::Changes::default());
        let mut sc = filmcraft_platform::vst::module::Scratch::default();
        let mut out = 0.0;
        for _ in 0..10 {
            let mut sig = tone(4800);
            let (l, r) = sig.split_at_mut(1);
            inst.process(&mut [&mut l[0][..], &mut r[0][..]], &ch, &mut sc);
            out = rms(&l[0]);
        }
        out
    };
    for i in &infos {
        let (a, b) = (raw(i), raw(i));
        if (a - b).abs() > 1e-6 {
            continue;
        }
        let def = filmcraft_project::find_effect(&format!("vst3:{}", i.cid)).unwrap();
        let mut fx = filmcraft_render::audio_fx::configured(&def.instance(), filmcraft_time::Tick::ZERO, 44_100).unwrap();
        let mut got = 0.0;
        for _ in 0..10 {
            let mut sig = tone(4800);
            let (l, r) = sig.split_at_mut(1);
            fx.process(&mut [&mut l[0][..], &mut r[0][..]]);
            got = rms(&l[0]);
        }
        assert!((got - a).abs() <= 0.02 * a.max(got) + 1e-4, "{}: {got} through FilmCraft, {a} on its own", i.name);
    }

    // the gain example: its parameter reaches the processor
    if let Some(g) = defs.iter().find(|d| d.name.to_ascii_lowercase().starts_with("gain") && d.params.len() == 1) {
        let def = filmcraft_project::find_effect(g.id).unwrap();
        let pid = def.params[0].id;
        let mut out = Vec::new();
        for level in [1.0, 0.25] {
            let mut e = def.instance();
            e.params.get_mut(pid).unwrap().value = ParamValue::Float(level);
            let mut fx = filmcraft_render::audio_fx::configured(&e, filmcraft_time::Tick::ZERO, 48_000).unwrap();
            let mut sig = tone(2400);
            let (l, r) = sig.split_at_mut(1);
            fx.process(&mut [&mut l[0][..], &mut r[0][..]]);
            out.push(rms(&l[0]));
        }
        assert!((out[1] / out[0] - 0.25).abs() < 0.02, "gain 1 → {}, gain 0.25 → {}", out[0], out[1]);
        // a saved state is restored into a new processor (here the plug-in's own default state)
        let mut e = def.instance();
        e.params.insert(STATE.into(), filmcraft_project::Param::new(ParamValue::Text(encode_state(&[], &[]))));
        assert!(filmcraft_render::audio_fx::configured(&e, filmcraft_time::Tick::ZERO, 44_100).is_some());
    }
    assert_eq!(decode_state(&encode_state(b"abc", b"")), Some((b"abc".to_vec(), vec![])));
    let _ = std::fs::remove_dir_all(&data);
}
