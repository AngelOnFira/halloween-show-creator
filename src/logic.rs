//! Pure timeline logic — independent of the UI framework. Folds the append-only
//! edit log into the held on/off grid (keyframe + hold semantics). Reused
//! verbatim from the original eframe spike.

use std::collections::HashMap;

use spacetimedb_sdk::Identity;

use crate::module_bindings::Edit;

/// Fold the append-only edit log (up to `cutoff`) into the set of explicit
/// keyframes. `true` = on keyframe, `false` = off keyframe, absent = no keyframe.
pub fn fold_keyframes(edits: &[Edit], cutoff: u64) -> HashMap<(u32, u32), bool> {
    let mut map = HashMap::new();
    for e in edits.iter().filter(|e| e.seq <= cutoff) {
        match e.state {
            0 => {
                map.insert((e.light, e.frame), false);
            }
            1 => {
                map.insert((e.light, e.frame), true);
            }
            _ => {
                map.remove(&(e.light, e.frame));
            }
        }
    }
    map
}

/// Expand keyframes into a per-cell held on/off grid (keyframe + hold).
/// `held[light][frame]`.
pub fn expand_held(keyframes: &HashMap<(u32, u32), bool>, nl: u32, nf: u32) -> Vec<Vec<bool>> {
    let mut by_light: HashMap<u32, Vec<(u32, bool)>> = HashMap::new();
    for (&(l, f), &v) in keyframes {
        by_light.entry(l).or_default().push((f, v));
    }
    for v in by_light.values_mut() {
        v.sort_by_key(|x| x.0);
    }

    let mut held = vec![vec![false; nf as usize]; nl as usize];
    for l in 0..nl {
        let kfs = by_light.get(&l);
        let mut cur = false;
        let mut idx = 0;
        for f in 0..nf {
            if let Some(kfs) = kfs {
                while idx < kfs.len() && kfs[idx].0 == f {
                    cur = kfs[idx].1;
                    idx += 1;
                }
            }
            held[l as usize][f as usize] = cur;
        }
    }
    held
}

pub fn state_label(state: u8) -> &'static str {
    match state {
        0 => "off",
        1 => "ON",
        _ => "clear",
    }
}

pub fn short_id(id: Option<&Identity>) -> String {
    match id {
        Some(i) => {
            let h = i.to_hex().to_string();
            format!("{}…", &h[..8.min(h.len())])
        }
        None => "—".to_owned(),
    }
}
