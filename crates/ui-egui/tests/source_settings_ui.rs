//! Headless UI test of Clip ▸ Source Settings for camera raw (Blackmagic RAW, with a stand-in for
//! Blackmagic's runtime): the dialog lists the clip's settings, changes apply live, Cancel puts
//! the previous settings back, OK keeps them as one undo step.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render the dialog offscreen (`source-settings.png`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_platform::braw::{self, Changes, Clip, ClipInfo, Kind, Picture, Runtime, Scale, Setting};
use filmcraft_time::FrameRate;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Fake;
struct FakeClip(ClipInfo);

impl Runtime for Fake {
    fn name(&self) -> String {
        "test runtime".into()
    }
    fn open(&self, _path: &Path) -> Result<Arc<dyn Clip>, String> {
        Ok(Arc::new(FakeClip(ClipInfo {
            width: 320,
            height: 180,
            frame_rate: FrameRate::FPS_24,
            frame_count: 24,
            audio: None,
            camera: Some("URSA Mini Pro 12K".into()),
            start_timecode: None,
        })))
    }
}

impl Clip for FakeClip {
    fn info(&self) -> &ClipInfo {
        &self.0
    }
    fn decode(&self, _: u64, scale: Scale) -> Result<Picture, String> {
        let (w, h) = (self.0.width.div_ceil(scale.divisor()), self.0.height.div_ceil(scale.divisor()));
        Ok(Picture { width: w, height: h, rgb: [30_000, 20_000, 10_000].repeat((w * h) as usize) })
    }
    fn audio(&self, _: u64, _: usize) -> Result<Vec<f32>, String> {
        Err("no audio".into())
    }
    fn settings(&self) -> Vec<Setting> {
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
                key: "colorScienceGen",
                label: "Color Science",
                group: "Color",
                kind: Kind::Number { min: 0.0, max: 9.0, step: 1.0 },
                as_shot: json!(5),
                read_only: true,
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
            n("kelvin", "Color Temp", "Image", 2000.0, 10_000.0, 50.0, 5600.0),
            n("exposure", "Exposure", "Image", -5.0, 5.0, 0.1, 0.0),
            Setting {
                key: "highlightRecovery",
                label: "Highlight Recovery",
                group: "Image",
                kind: Kind::Toggle,
                as_shot: json!(false),
                read_only: false,
                note: None,
            },
            Setting { note: Some("Used with Blackmagic Design Custom gamma"), ..n("saturation", "Saturation", "Custom Gamma", 0.0, 2.0, 0.01, 1.0) },
        ]
    }
    fn decode_with(&self, index: u64, scale: Scale, _: &Changes) -> Result<Picture, String> {
        self.decode(index, scale)
    }
}

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<PathBuf>,
}

impl Driver {
    fn new(session: Session) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        d
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }
    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }
    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }
    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }
    fn stored(&mut self, item: u64) -> Value {
        self.exec("clip.sourceSettings", json!({"item": item}))["changed"].clone()
    }
    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        self.frames(3);
        match self.harness.render() {
            Ok(img) => {
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join(format!("{name}.png"));
                img.save(&path).unwrap();
                eprintln!("snapshot: {}", path.display());
            }
            Err(e) => eprintln!("snapshot {name} skipped: {e}"),
        }
    }
}

#[test]
fn source_settings_dialog_applies_live_cancels_and_keeps() {
    filmcraft_codecs::register_reader_opener(braw::reader_opener);
    braw::set_runtime(Arc::new(Fake));
    let dir = std::env::temp_dir().join(format!("filmcraft-ui-srcset-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("A002_C017.braw");
    std::fs::write(&clip, b"x").unwrap();

    let mut d = Driver::new(Session::default());
    d.exec("file.newSequence", json!({"name": "S", "fps": 24.0, "width": 320, "height": 180}));
    let item = d.exec("file.import", json!({"paths": [clip.to_string_lossy()]}))["items"][0].as_u64().unwrap();
    d.exec("project.select", json!({"items": [item]}));

    // open: every setting has a control, read-only ones are shown as text
    d.ok("ui.menu.invoke", json!({"id": "clip.sourceSettings"}));
    d.frames(3);
    let ids = d.ids("sourceSettings.");
    for k in ["gamma", "iso", "kelvin", "exposure", "highlightRecovery", "saturation", "reset", "ok", "cancel"] {
        assert!(ids.iter().any(|i| i == &format!("sourceSettings.{k}")), "missing sourceSettings.{k} in {ids:?}");
    }
    assert!(!ids.iter().any(|i| i == "sourceSettings.colorScienceGen"), "read-only setting has no control");
    d.snapshot("source-settings");

    // a change applies right away
    d.click("sourceSettings.gamma");
    d.click("sourceSettings.gamma.Blackmagic Design Video");
    assert_eq!(d.stored(item), json!({"gamma": "Blackmagic Design Video"}));
    d.click("sourceSettings.highlightRecovery");
    assert_eq!(d.stored(item), json!({"gamma": "Blackmagic Design Video", "highlightRecovery": true}));

    // Cancel puts back what was there before the dialog
    d.click("sourceSettings.cancel");
    assert_eq!(d.stored(item), json!({}));
    assert!(d.ids("sourceSettings.").is_empty(), "dialog closed");

    // OK keeps the changes, as one undo step
    d.ok("ui.menu.invoke", json!({"id": "clip.sourceSettings"}));
    d.frames(3);
    d.click("sourceSettings.iso");
    d.click("sourceSettings.iso.1600");
    d.click("sourceSettings.gamma");
    d.click("sourceSettings.gamma.Blackmagic Design Custom");
    d.click("sourceSettings.ok");
    assert_eq!(d.stored(item), json!({"iso": 1600, "gamma": "Blackmagic Design Custom"}));
    d.exec("edit.undo", json!({}));
    assert_eq!(d.stored(item), json!({}), "both changes undone together");
    d.exec("edit.redo", json!({}));

    // a later dialog starts its own undo step; Reset to As Shot clears everything
    d.ok("ui.menu.invoke", json!({"id": "clip.sourceSettings"}));
    d.frames(3);
    d.click("sourceSettings.reset");
    assert_eq!(d.stored(item), json!({}));
    d.click("sourceSettings.ok");
    d.exec("edit.undo", json!({}));
    assert_eq!(d.stored(item), json!({"iso": 1600, "gamma": "Blackmagic Design Custom"}), "undoing the reset keeps the earlier dialog's changes");
    let _ = std::fs::remove_dir_all(&dir);
}
