//! VST3 audio plug-in hosting: the effects editors already own (FabFilter, iZotope, Waves,
//! Soundtoys, Valhalla, Universal Audio, Plugin Alliance and others) as FilmCraft audio effects,
//! on clips and as mixer track inserts.
//!
//! - [`catalog`]: finding and scanning plug-ins (in a child process) and listing them.
//! - [`module`]: loading a module, creating and running instances.
//! - [`host`]: the objects FilmCraft hands plug-ins (host application, streams, parameter
//!   changes, messages).
//! - [`fx`]: instances as FilmCraft audio effects, with their full state saved in the project.
//!
//! Interfaces come from the `vst3` crate (MIT OR Apache-2.0), generated from Steinberg's VST 3
//! SDK headers (MIT since VST 3.8). VST2 plug-ins are not hosted. A plug-in is foreign code in
//! FilmCraft's process: scanning is isolated, processing is not (as in every host).

// Host objects are boxed COM objects whose addresses plug-ins keep.
#![allow(clippy::vec_box)]

pub mod catalog;
#[cfg(any(windows, filmcraft_wincheck))]
pub mod editor;
pub mod fx;
pub mod host;
pub mod module;

/// The audio plug-in host, once started.
pub(crate) static HOST: std::sync::OnceLock<std::sync::Arc<fx::Host>> = std::sync::OnceLock::new();

pub use catalog::{scan_and_report, scan_main, start, start_with};
