//! Effects and transitions implemented outside FilmCraft: installed OpenFX plug-ins, hosted by
//! `filmcraft_platform::ofx` (the only crate allowed foreign code), which registers itself here
//! with [`set_external`].
//!
//! Plug-ins see the picture the way editors' plug-ins expect it: RGBA, premultiplied, colour on
//! the display curve (as FilmCraft's own grading effects see it), top row first here (the host
//! flips rows for OpenFX's bottom-up images). A plug-in effect that is missing (not installed, or
//! not scanned yet) or fails leaves the picture as it was: the project keeps the effect and its
//! settings, and renders it again once the plug-in is there.

use std::sync::{Arc, OnceLock};

use filmcraft_project::EffectInstance;
use filmcraft_time::Tick;
use rayon::prelude::*;

use crate::effects::{FxCtx, dec, enc};
use crate::image::Image;

/// A picture handed to a plug-in: RGBA premultiplied, display-encoded, `w × h`, top row first.
#[derive(Clone, Debug, Default)]
pub struct ExtImage {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

/// What a plug-in render needs besides the picture.
pub struct ExtCtx<'a> {
    pub effect: &'a EffectInstance,
    /// Media time keyframes are evaluated at now.
    pub t: Tick,
    /// Frames since the clip's start (OpenFX time).
    pub frame: f64,
    pub fps: f64,
    /// The clip's length in frames.
    pub duration: f64,
    /// Working pixels per full-resolution pixel (½ at half-resolution playback).
    pub render_scale: f32,
    /// Full-resolution size of the picture (OpenFX's project size).
    pub full_size: (u32, u32),
    /// The same layer `dt` seconds from now (effects before this one applied), for plug-ins that
    /// read other frames (noise reduction, motion blur).
    pub frames: Option<&'a (dyn Fn(f64) -> Option<ExtImage> + Sync)>,
}

/// The plug-in host.
pub trait External: Send + Sync {
    /// Process `img` in place with effect `cx.effect`.
    fn effect(&self, img: &mut ExtImage, cx: &ExtCtx) -> Result<(), String>;
    /// Transition `cx.effect` from `a` to `b` at progress `p` (0..1).
    fn transition(&self, a: &ExtImage, b: &ExtImage, p: f32, cx: &ExtCtx) -> Result<ExtImage, String>;
}

static EXTERNAL: OnceLock<Arc<dyn External>> = OnceLock::new();

/// Install the plug-in host (once; later calls are ignored).
pub fn set_external(x: Arc<dyn External>) {
    let _ = EXTERNAL.set(x);
}

fn external() -> Option<&'static Arc<dyn External>> {
    EXTERNAL.get()
}

/// Linear premultiplied → display-encoded premultiplied.
pub fn to_ext(img: &Image) -> ExtImage {
    let mut px = img.px.clone();
    px.par_chunks_mut(4).for_each(|p| {
        let a = p[3];
        if a > 0.0 {
            let c = enc([p[0] / a, p[1] / a, p[2] / a]);
            p[0] = c[0] * a;
            p[1] = c[1] * a;
            p[2] = c[2] * a;
        } else {
            p[0] = 0.0;
            p[1] = 0.0;
            p[2] = 0.0;
            p[3] = 0.0;
        }
    });
    ExtImage { w: img.w, h: img.h, px }
}

/// Display-encoded premultiplied → linear premultiplied, into `img` (sizes must match).
fn from_ext(x: &ExtImage, img: &mut Image) -> bool {
    if x.w != img.w || x.h != img.h || x.px.len() != img.px.len() {
        return false;
    }
    img.px.par_chunks_mut(4).zip(x.px.par_chunks(4)).for_each(|(o, p)| {
        let a = if p[3].is_finite() { p[3].clamp(0.0, 1.0) } else { 0.0 };
        if a > 0.0 {
            let c = dec([p[0] / a, p[1] / a, p[2] / a]);
            o[0] = c[0] * a;
            o[1] = c[1] * a;
            o[2] = c[2] * a;
            o[3] = a;
        } else {
            o.copy_from_slice(&[0.0; 4]);
        }
    });
    true
}

/// Effects panel / render: apply plug-in effect `e`. Missing host or failure: unchanged.
pub(crate) fn apply_effect(img: &mut Image, e: &EffectInstance, cx: &FxCtx) {
    let Some(x) = external() else { return };
    let fps = cx.env.map_or(30.0, |v| v.frame_rate()).max(1.0);
    let duration = cx.env.map_or(0.0, |v| v.clip_seconds() * fps);
    let scale = if cx.px_scale.is_finite() && cx.px_scale > 0.0 { cx.px_scale } else { 1.0 };
    let full = ((img.w as f32 / scale).round().max(1.0) as u32, (img.h as f32 / scale).round().max(1.0) as u32);
    let frames = |dt: f64| -> Option<ExtImage> { cx.env.and_then(|env| env.frame(dt, e)).filter(|f| f.w == img.w && f.h == img.h).map(|f| to_ext(&f)) };
    let ctx = ExtCtx {
        effect: e,
        t: cx.t,
        frame: (cx.seconds * fps).max(0.0),
        fps,
        duration,
        render_scale: scale,
        full_size: full,
        frames: if cx.env.is_some() { Some(&frames) } else { None },
    };
    let mut ext = to_ext(img);
    if x.effect(&mut ext, &ctx).is_ok() {
        from_ext(&ext, img);
    }
}

/// Transitions: plug-in transition `e` from `a` to `b` at `p`; `None` when it cannot render.
pub(crate) fn apply_transition(e: &EffectInstance, a: &Image, b: &Image, p: f32, scale: f32) -> Option<Image> {
    let x = external()?;
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let ctx = ExtCtx {
        effect: e,
        t: Tick::ZERO,
        frame: 0.0,
        fps: 30.0,
        duration: 1.0,
        render_scale: scale,
        full_size: ((a.w as f32 / scale).round().max(1.0) as u32, (a.h as f32 / scale).round().max(1.0) as u32),
        frames: None,
    };
    let out = x.transition(&to_ext(a), &to_ext(b), p, &ctx).ok()?;
    let mut img = Image::new(a.w, a.h);
    from_ext(&out, &mut img).then_some(img)
}

// ---------------------------------------------------------------------------------------------
// Audio plug-ins (VST3)

/// The audio plug-in host: makes a processor for a plug-in effect (its saved state restored),
/// which FilmCraft then drives like its own effects (parameters by id, blocks in place).
pub trait AudioPlugins: Send + Sync {
    fn create(&self, effect: &EffectInstance, sample_rate: u32, channels: usize) -> Option<Box<dyn filmcraft_audio_dsp::AudioEffect>>;
    /// Seconds of audio a fresh chain runs before the requested range (tails, smoothing).
    fn preroll(&self, effect: &EffectInstance) -> f64 {
        let _ = effect;
        0.5
    }
}

static AUDIO: OnceLock<Arc<dyn AudioPlugins>> = OnceLock::new();

/// Install the audio plug-in host (once).
pub fn set_audio_plugins(x: Arc<dyn AudioPlugins>) {
    let _ = AUDIO.set(x);
}

/// A processor for audio plug-in effect `e`, if the host is installed and the plug-in loads.
pub fn create_audio_plugin(e: &EffectInstance, sample_rate: u32, channels: usize) -> Option<Box<dyn filmcraft_audio_dsp::AudioEffect>> {
    AUDIO.get()?.create(e, sample_rate, channels)
}

pub fn audio_plugin_preroll(e: &EffectInstance) -> f64 {
    AUDIO.get().map_or(0.0, |a| a.preroll(e))
}

// ---------------------------------------------------------------------------------------------
// Audio plug-in windows

/// What an open plug-in window reports.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowUpdate {
    /// A parameter of the effect changed in the window (FilmCraft id and value; 0/1 for
    /// switches, an index for lists).
    Param(String, f64),
    /// The plug-in's full state, for the effect's `__state` parameter.
    State(String),
    /// The window closed (with the reason if it could not open or failed).
    Closed(Option<String>),
}

/// An open plug-in window.
pub trait PluginWindow: Send {
    fn poll(&self) -> Vec<WindowUpdate>;
    fn is_open(&self) -> bool;
    fn set_param(&self, id: &str, value: f64);
    fn focus(&self);
    fn close(&self);
}

/// Opens plug-ins' own windows (installed by the plug-in host where it can).
pub trait PluginWindows: Send + Sync {
    fn has_window(&self, effect_id: &str) -> bool;
    fn open(&self, effect_id: &str, state: Option<&str>, params: Vec<(String, f64)>) -> Result<Box<dyn PluginWindow>, String>;
}

static WINDOWS: OnceLock<Arc<dyn PluginWindows>> = OnceLock::new();

pub fn set_plugin_windows(x: Arc<dyn PluginWindows>) {
    let _ = WINDOWS.set(x);
}

/// Whether effect `effect_id` is a plug-in with a window FilmCraft can open here.
pub fn plugin_has_window(effect_id: &str) -> bool {
    WINDOWS.get().is_some_and(|w| w.has_window(effect_id))
}

/// Open plug-in effect `effect_id`'s window with its saved state and parameter values restored.
pub fn open_plugin_window(effect_id: &str, state: Option<&str>, params: Vec<(String, f64)>) -> Result<Box<dyn PluginWindow>, String> {
    WINDOWS.get().ok_or("plug-in windows are not available on this system")?.open(effect_id, state, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_round_trips_through_the_plugin_encoding() {
        let mut img = Image::filled(2, 1, [0.18, 0.5, 1.0, 1.0]);
        img.px[4..8].copy_from_slice(&[0.1, 0.05, 0.0, 0.5]);
        let ext = to_ext(&img);
        assert!((ext.px[0] - filmcraft_color::linear_to_srgb(0.18)).abs() < 1e-5);
        let mut back = Image::new(2, 1);
        assert!(from_ext(&ext, &mut back));
        for (a, b) in img.px.iter().zip(&back.px) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
        // a wrong size is refused, not read out of bounds
        assert!(!from_ext(&ExtImage { w: 1, h: 1, px: vec![0.0; 4] }, &mut back));
    }
}
