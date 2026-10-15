//! OpenFX plug-in hosting: the video effects and transitions editors already own for DaVinci
//! Resolve, Nuke, Vegas and other OpenFX hosts (Boris FX Sapphire and Continuum, Red Giant
//! Universe, FilmConvert, Neat Video, Dehancer and others) run inside FilmCraft.
//!
//! - [`catalog`]: finding, scanning (in a child process) and listing plug-ins as effects.
//! - [`plugin`]: loading a binary and describing its plug-ins.
//! - [`host`] and [`props`]: the OpenFX suites FilmCraft implements for plug-ins.
//! - [`render`]: rendering frames and transitions through them for FilmCraft's renderer.
//!
//! Supported: the Filter, General and Transition contexts; RGBA images at 32-bit float (or the
//! plug-in's best depth), any render scale, temporal access to the clip's other frames;
//! double, integer, boolean, choice, string, colour, 2D/3D parameters with keyframes. Not (yet):
//! plug-in overlays drawn on the Program monitor, custom parameter interfaces, parametric
//! (curve) parameters, GPU (OpenGL / CUDA / Metal) rendering, generators. Windows and Linux on
//! x86-64.
//!
//! A plug-in is foreign code running in FilmCraft's process, as in every OpenFX host: scanning
//! happens in a separate process, so a plug-in that crashes while loading is only skipped, but
//! one that crashes while rendering ends the session (autosave keeps the work).

// Handles given to plug-ins are object addresses, so objects are boxed and never move.
#![allow(clippy::vec_box)]

pub mod catalog;
pub mod ffi;
pub mod host;
pub mod plugin;
pub mod props;
pub mod render;

pub use catalog::{scan_and_report, scan_main, start, start_with};
