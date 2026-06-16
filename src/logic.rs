//! Pure timeline logic — independent of the UI framework. Folds the append-only
//! edit log into the held on/off grid (keyframe + hold semantics). Reused
//! verbatim from the original eframe spike.

use std::collections::HashMap;

use spacetimedb_sdk::Identity;

use crate::module_bindings::{Edit, LightEditInput};

/// Apply optimistic (not-yet-acknowledged) light edits on top of a folded
/// keyframe map, so the UI and 3D view react instantly. `state` matches `Edit`:
/// 0 = off, 1 = on, 2 = clear.
pub fn apply_pending(kf: &mut HashMap<(u32, u32), bool>, pending: &[LightEditInput]) {
    for e in pending {
        match e.state {
            0 => {
                kf.insert((e.light, e.frame), false);
            }
            1 => {
                kf.insert((e.light, e.frame), true);
            }
            _ => {
                kf.remove(&(e.light, e.frame));
            }
        }
    }
}

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

/// Generic "held" fold for a fixture keyframe table: for each channel in
/// `0..channels`, return the keyframe in effect at `frame` (the latest row for
/// that channel whose own frame is `<= frame`), or `None` if there isn't one
/// yet. Used for lasers / projectors / turrets, which (unlike lights) are stored
/// as direct keyframe rows rather than an event-sourced log.
pub fn fold_fixtures<T: Clone>(
    rows: &[T],
    frame: u32,
    channels: usize,
    channel_of: impl Fn(&T) -> u8,
    frame_of: impl Fn(&T) -> u32,
) -> Vec<Option<T>> {
    let mut best: Vec<Option<(u32, T)>> = (0..channels).map(|_| None).collect();
    for r in rows {
        let c = channel_of(r) as usize;
        if c >= channels {
            continue;
        }
        let f = frame_of(r);
        if f > frame {
            continue;
        }
        match &best[c] {
            Some((bf, _)) if *bf >= f => {}
            _ => best[c] = Some((f, r.clone())),
        }
    }
    best.into_iter().map(|o| o.map(|(_, t)| t)).collect()
}

/// Expand fixture keyframe rows into, per channel, a per-frame held on/off row
/// plus the frames carrying an explicit keyframe (for the timeline dots). Same
/// hold semantics as lights: a keyframe holds until the next one; before the
/// first keyframe the channel is off. `active` decides whether a keyframe turns
/// the channel "on" (e.g. a laser is on when enabled with points; a DMX fixture
/// when its state byte is non-zero). Returns one `(held, keyframe_frames)` per
/// channel `0..channels`.
pub fn expand_fixture_tracks<T>(
    rows: &[T],
    channels: usize,
    nf: u32,
    channel_of: impl Fn(&T) -> u8,
    frame_of: impl Fn(&T) -> u32,
    active: impl Fn(&T) -> bool,
) -> Vec<(Vec<bool>, Vec<u32>)> {
    let mut per: Vec<Vec<(u32, bool)>> = (0..channels).map(|_| Vec::new()).collect();
    for r in rows {
        let c = channel_of(r) as usize;
        if c < channels {
            per[c].push((frame_of(r), active(r)));
        }
    }
    per.iter_mut()
        .map(|kfs| {
            kfs.sort_by_key(|x| x.0);
            let kf_frames: Vec<u32> = kfs.iter().map(|x| x.0).collect();
            let mut held = vec![false; nf as usize];
            let mut cur = false;
            let mut idx = 0;
            for (f, cell) in held.iter_mut().enumerate() {
                while idx < kfs.len() && kfs[idx].0 as usize == f {
                    cur = kfs[idx].1;
                    idx += 1;
                }
                *cell = cur;
            }
            (held, kf_frames)
        })
        .collect()
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
