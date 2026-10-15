//! RED R3D (`.R3D`) and Nikon N-RAW (`.NEV`) through RED's R3D SDK.
//!
//! R3D has no public specification and RED's license forbids reverse engineering it, so FilmCraft
//! cannot decode it itself. Builds made with RED's free R3D SDK present (`FILMCRAFT_R3D_SDK` at
//! build time, see `build.rs`) link a small bridge (`shim.cpp`) to it and ship RED's
//! redistributable decoder libraries next to the executable, as the SDK license allows; nothing of
//! RED's is in FilmCraft's source. Other builds still claim `.R3D` files and say what is missing.
//!
//! Clips use the same machinery as Blackmagic RAW ([`crate::braw`]): reduced-resolution decodes
//! for playback, 16-bit 4:4:4 frames, audio, and Source Settings (ISO, white balance, exposure,
//! colour space, gamma, tone map, highlight roll-off, CDL, lens correction…) applied by RED's own
//! image pipeline (IPP2, or the legacy pipeline for older clips).

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use filmcraft_media::{MediaError, SharedReader, SharedSource};

use crate::braw::{self, Missing, Runtime};

#[cfg(filmcraft_r3d)]
#[allow(unsafe_code)]
pub mod sdk;

/// Media-info format name.
pub const FORMAT: &str = "RED R3D";

static RUNTIME: RwLock<Result<Arc<dyn Runtime>, Missing>> = RwLock::new(Err(Missing::NotRegistered));

pub fn set_runtime(r: Arc<dyn Runtime>) {
    *RUNTIME.write().unwrap_or_else(PoisonError::into_inner) = Ok(r);
}

pub fn set_missing(m: Missing) {
    *RUNTIME.write().unwrap_or_else(PoisonError::into_inner) = Err(m);
}

pub fn runtime() -> Result<Arc<dyn Runtime>, Missing> {
    RUNTIME.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Whether `name` is an R3D or N-RAW clip (by extension).
pub fn is_r3d(name: &str) -> bool {
    Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("r3d") || e.eq_ignore_ascii_case("nev"))
}

pub fn missing_message(name: &str, m: &Missing) -> String {
    match m {
        Missing::Platform | Missing::NotRegistered | Missing::Unfinished => {
            format!("{name}: RED R3D needs a FilmCraft build made with RED's R3D SDK (this build was made without it, or not for this system)")
        }
        Missing::NotInstalled(why) => format!("{name}: RED's R3D decoder could not be started: {why}"),
    }
}

/// Reader opener for `.R3D` / `.NEV` files (registered by [`crate::register`]).
pub fn reader_opener(name: &str, _head: &[u8], reader: &SharedReader) -> Option<filmcraft_media::Result<SharedSource>> {
    if !is_r3d(name) {
        return None;
    }
    let rt = match runtime() {
        Ok(rt) => rt,
        Err(m) => return Some(Err(MediaError::Unsupported(missing_message(name, &m)))),
    };
    let Some(path) = reader.path() else {
        return Some(Err(MediaError::Unsupported(format!("{name}: RED clips open from a file on disk"))));
    };
    Some(braw::open_with(&*rt, name, path, Some(reader.len())).map(|s| Arc::new(s) as SharedSource))
}

/// Register the opener and, in builds with the SDK, its runtime (loaded at the first R3D import).
pub fn register() {
    filmcraft_codecs::register_reader_opener(reader_opener);
    if runtime().is_ok() {
        return;
    }
    #[cfg(filmcraft_r3d)]
    set_runtime(Arc::new(sdk::SdkRuntime::default()));
    #[cfg(not(filmcraft_r3d))]
    set_missing(Missing::Platform);
}

/// `HH:MM:SS:FF` (or `.`/`;` separators) → frames at `fps` (rounded to a whole rate).
pub fn timecode_frames(tc: &str, fps: f64) -> Option<i64> {
    let parts: Vec<i64> = tc.trim().split([':', '.', ';']).map(|p| p.trim().parse::<i64>().ok()).collect::<Option<Vec<_>>>()?;
    let [h, m, s, f] = parts.as_slice() else { return None };
    let base = fps.round().max(1.0) as i64;
    if !(0..24).contains(h) || !(0..60).contains(m) || !(0..60).contains(s) || *f < 0 || *f >= base.max(1) * 2 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * base + f)
}

/// What `braw-check` style reports say (the SDK's version, or why it is missing).
pub fn report() -> String {
    match runtime() {
        Ok(rt) => {
            #[cfg(filmcraft_r3d)]
            {
                sdk::report(&*rt)
            }
            #[cfg(not(filmcraft_r3d))]
            {
                format!("RED R3D: {}", rt.name())
            }
        }
        Err(m) => format!("RED R3D: {}", missing_message("this build", &m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_and_timecode() {
        assert!(is_r3d("A001_C002_0101AB_001.R3D") && is_r3d("clip.nev") && !is_r3d("clip.braw") && !is_r3d("r3d"));
        assert_eq!(timecode_frames("01:00:00:00", 23.976), Some(86_400));
        assert_eq!(timecode_frames("00:00:01:12", 24.0), Some(36));
        assert_eq!(timecode_frames("00:00:01.12", 59.94), Some(60 + 12), "high frame rates use a dot");
        assert_eq!(timecode_frames("99:00:00:00", 24.0), None);
        assert_eq!(timecode_frames("garbage", 24.0), None);
        assert_eq!(timecode_frames("00:00:00:00:00", 24.0), None);
    }
}
