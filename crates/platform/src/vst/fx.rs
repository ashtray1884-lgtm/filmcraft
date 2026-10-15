//! VST3 plug-ins as FilmCraft audio effects: the [`filmcraft_render::external::AudioPlugins`]
//! host and the processor it makes ([`PluginFx`]), which FilmCraft's clip chains and mixer
//! inserts drive like their own effects.
//!
//! Parameters are FilmCraft parameters `p<id>` in the plug-in's own units (the controller
//! converts plain ↔ normalized), switches and short lists as toggles and menus. The plug-in's
//! full state (presets, settings that are not parameters) is saved in the effect as `__state`
//! and restored whenever a processor is made. Instances are pooled per plug-in, sample rate and
//! channel count, since seeking makes fresh chains often and plug-ins are slow to create.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_audio_dsp::{AudioEffect, ParamSpec, Unit};
use filmcraft_project::{EffectInstance, ParamValue};
use vst3::ComWrapper;

use super::catalog::PluginInfo;
use super::host::Changes;
use super::module::{Instance, Scratch, cid_from_hex, open};

/// The FilmCraft parameter id of plug-in parameter `id`.
pub fn param_id(id: u32) -> String {
    format!("p{id}")
}

/// The effect parameter holding the plug-in's saved state.
pub const STATE: &str = "__state";

#[derive(Clone, Copy, Debug, PartialEq)]
enum PKind {
    Plain,
    /// The plug-in's normalized value itself (its conversions do not invert).
    Normalized,
    Toggle,
    Steps(i32),
}

/// Per plug-in class: parameter specs (leaked once) and their mapping.
struct ClassSpecs {
    specs: &'static [ParamSpec],
    map: Vec<(&'static str, u32, PKind)>,
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

pub(crate) fn kind_of(p: &super::module::ParamInfo) -> (bool, i32) {
    (p.steps == 1, if (2..=64).contains(&p.steps) && !p.choices.is_empty() { p.steps } else { 0 })
}

fn specs_for(info: &PluginInfo) -> ClassSpecs {
    let mut specs = Vec::new();
    let mut map = Vec::new();
    for p in info.params.iter().filter(|p| p.visible()) {
        let id = leak(param_id(p.id));
        let name = leak(p.title.clone());
        let (toggle, steps) = kind_of(p);
        if toggle {
            specs.push(ParamSpec::toggle(id, name, p.default >= 0.5));
            map.push((id, p.id, PKind::Toggle));
        } else if steps > 0 {
            let choices: &'static [&'static str] = Box::leak(p.choices.iter().map(|c| leak(c.clone())).collect::<Vec<_>>().into_boxed_slice());
            specs.push(ParamSpec::choice(id, name, choices, (p.default * steps as f64).round() as usize));
            map.push((id, p.id, PKind::Steps(steps)));
        } else if !p.plain_ok {
            specs.push(ParamSpec::new(id, name, 0.0, 1.0, p.default as f32, Unit::None));
            map.push((id, p.id, PKind::Normalized));
        } else {
            let (lo, hi) = if p.plain_min <= p.plain_max { (p.plain_min, p.plain_max) } else { (p.plain_max, p.plain_min) };
            specs.push(ParamSpec::new(id, name, lo as f32, hi as f32, p.plain_default as f32, Unit::None));
            map.push((id, p.id, PKind::Plain));
        }
    }
    ClassSpecs { specs: Box::leak(specs.into_boxed_slice()), map }
}

type PoolKey = (String, u32, usize);

/// Pooled instances with the saved state they were made from (`None`: the plug-in's own start
/// state). An instance is only reused for the same state: some plug-ins do not restore their own
/// state exactly, so restoring into a reused instance could change their sound.
type Pool = Arc<Mutex<HashMap<PoolKey, Vec<(Instance, Option<u64>)>>>>;

/// The audio plug-in host.
pub struct Host {
    infos: Mutex<HashMap<String, PluginInfo>>,
    specs: Mutex<HashMap<String, Arc<ClassSpecs>>>,
    pool: Pool,
    failed: Mutex<HashMap<String, String>>,
}

fn state_hash(s: &(Vec<u8>, Vec<u8>)) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

impl Host {
    pub fn new() -> Host {
        Host {
            infos: Mutex::new(HashMap::new()),
            specs: Mutex::new(HashMap::new()),
            pool: Arc::new(Mutex::new(HashMap::new())),
            failed: Mutex::new(HashMap::new()),
        }
    }

    pub fn add(&self, list: &[PluginInfo]) {
        let mut g = self.infos.lock().unwrap_or_else(PoisonError::into_inner);
        for i in list {
            g.insert(i.cid.clone(), i.clone());
        }
    }

    pub fn info(&self, cid: &str) -> Option<PluginInfo> {
        self.infos.lock().unwrap_or_else(PoisonError::into_inner).get(cid).cloned()
    }

    fn specs(&self, info: &PluginInfo) -> Arc<ClassSpecs> {
        let mut g = self.specs.lock().unwrap_or_else(PoisonError::into_inner);
        g.entry(info.cid.clone()).or_insert_with(|| Arc::new(specs_for(info))).clone()
    }

    /// A ready instance of `info` for `state` (pooled with the same state, or new).
    fn instance(&self, info: &PluginInfo, sr: u32, ch: usize, state: Option<(Vec<u8>, Vec<u8>)>) -> Result<(Instance, Option<u64>), String> {
        let key = (info.cid.clone(), sr, ch);
        let origin = state.as_ref().map(state_hash);
        let pooled = {
            let mut g = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
            g.get_mut(&key).and_then(|v| v.iter().position(|(_, o)| *o == origin).map(|i| v.swap_remove(i)))
        };
        if let Some((mut inst, o)) = pooled {
            inst.reset();
            return Ok((inst, o));
        }
        let m = open(Path::new(&info.bundle))?;
        let cid = cid_from_hex(&info.cid).ok_or("bad class id")?;
        let mut inst = Instance::new(&m, &cid, sr as f64, ch, 4096)?;
        if let Some((c, k)) = &state {
            inst.set_state(c, k);
        }
        Ok((inst, origin))
    }
}

impl Default for Host {
    fn default() -> Self {
        Host::new()
    }
}

/// `__state` text → (component state, controller state).
pub fn decode_state(s: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let rest = s.strip_prefix("vst3:1:")?;
    let (a, b) = rest.split_once(':').unwrap_or((rest, ""));
    Some((b64_decode(a)?, b64_decode(b)?))
}

pub fn encode_state(component: &[u8], controller: &[u8]) -> String {
    format!("vst3:1:{}:{}", b64_encode(component), b64_encode(controller))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0;
    for ch in s.bytes() {
        if ch == b'=' {
            break;
        }
        let v = B64.iter().position(|&c| c == ch)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

impl filmcraft_render::external::AudioPlugins for Host {
    fn create(&self, e: &EffectInstance, sample_rate: u32, channels: usize) -> Option<Box<dyn AudioEffect>> {
        let cid = e.effect.strip_prefix("vst3:")?;
        let info = self.info(cid)?;
        if self.failed.lock().unwrap_or_else(PoisonError::into_inner).contains_key(cid) {
            return None;
        }
        let state = match e.params.get(STATE).map(|p| &p.value) {
            Some(ParamValue::Text(s)) => decode_state(s),
            _ => None,
        };
        let ch = channels.clamp(1, 8);
        match self.instance(&info, sample_rate, ch, state) {
            Ok((inst, origin)) => {
                let specs = self.specs(&info);
                Some(Box::new(PluginFx {
                    inst: Some(inst),
                    origin,
                    key: (info.cid.clone(), sample_rate, ch),
                    pool: self.pool.clone(),
                    specs,
                    values: HashMap::new(),
                    changes: ComWrapper::new(Changes::default()),
                    scratch: Scratch::default(),
                }))
            }
            Err(err) => {
                log::warn!("VST3: {} could not be started: {err}", info.name);
                self.failed.lock().unwrap_or_else(PoisonError::into_inner).insert(cid.to_string(), err);
                None
            }
        }
    }

    fn preroll(&self, e: &EffectInstance) -> f64 {
        // the plug-in's own tail, within reason
        e.effect.strip_prefix("vst3:").and_then(|c| self.info(c)).map_or(0.5, |i| (i.tail_seconds + 0.1).clamp(0.1, 4.0))
    }
}

/// One plug-in instance in an audio chain.
pub struct PluginFx {
    inst: Option<Instance>,
    origin: Option<u64>,
    key: PoolKey,
    pool: Pool,
    specs: Arc<ClassSpecs>,
    values: HashMap<&'static str, f32>,
    changes: ComWrapper<Changes>,
    scratch: Scratch,
}

impl AudioEffect for PluginFx {
    fn id(&self) -> &'static str {
        "plugin"
    }
    fn params(&self) -> &'static [ParamSpec] {
        self.specs.specs
    }
    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(&(sid, pid, kind)) = self.specs.map.iter().find(|(s, _, _)| *s == id) else { return false };
        let v = if value.is_finite() { value } else { 0.0 };
        let first = self.values.insert(sid, v).is_none();
        // the first value for a parameter: when the plug-in already has it (its start or saved
        // state, which may hold in-between values FilmCraft's switch or list cannot show), leave it
        if first && let Some(cur) = self.inst.as_ref().and_then(|i| i.normalized(pid)) {
            let same = match kind {
                PKind::Toggle => (cur >= 0.5) == (v >= 0.5),
                PKind::Steps(n) => (cur * n as f64).round() == (v as f64).round(),
                PKind::Normalized => (cur - v as f64).abs() < 1e-6,
                PKind::Plain => self.inst.as_ref().is_some_and(|i| (i.to_normalized(pid, v as f64) - cur).abs() < 1e-6),
            };
            if same {
                return true;
            }
        }
        let norm = match kind {
            PKind::Toggle => (v >= 0.5) as i32 as f64,
            PKind::Steps(n) => (v.round() as f64 / n as f64).clamp(0.0, 1.0),
            PKind::Plain => self.inst.as_ref().map_or(0.0, |i| i.to_normalized(pid, v as f64)),
            PKind::Normalized => (v as f64).clamp(0.0, 1.0),
        };
        self.changes.set(pid, norm);
        // the controller follows, so the instance's own view of its values stays right when it
        // is reused
        if let Some(i) = &self.inst {
            i.set_normalized(pid, norm);
        }
        true
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.values.get(id).copied()
    }
    fn reset(&mut self) {
        if let Some(i) = &mut self.inst {
            i.reset();
        }
    }
    fn process(&mut self, channels: &mut [&mut [f32]]) {
        if let Some(i) = &mut self.inst {
            i.process(channels, &self.changes, &mut self.scratch);
        }
    }
    fn latency(&self) -> usize {
        self.inst.as_ref().map_or(0, Instance::latency)
    }
}

impl Drop for PluginFx {
    fn drop(&mut self) {
        let Some(inst) = self.inst.take() else { return };
        let mut g = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
        let v = g.entry(self.key.clone()).or_default();
        if v.len() < 4 {
            v.push((inst, self.origin));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_text_round_trips() {
        for (a, b) in [(vec![], vec![]), (vec![1u8, 2, 3, 250], vec![9u8]), ((0..=255u8).collect(), vec![0u8, 0])] {
            let s = encode_state(&a, &b);
            assert_eq!(decode_state(&s), Some((a, b)));
        }
        assert_eq!(decode_state("nonsense"), None);
        assert_eq!(decode_state("vst3:1:@@@@:"), None, "not base64");
    }
}
