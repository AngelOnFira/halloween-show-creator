//! Pure timeline logic — independent of the UI framework. Folds the append-only
//! edit log into the held on/off grid (keyframe + hold semantics). Reused
//! verbatim from the original eframe spike.

use std::collections::HashMap;

use spacetimedb_sdk::Identity;

use crate::module_bindings::{Edit, LightEditInput, TurretKeyframe};

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

/// Hermite smoothstep on `[0,1]` (clamped): eases in and out, so interpolated
/// motion starts and ends gently like a real moving head.
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A turret's interpolated aim at a point in time. `pan`/`tilt` are continuous
/// DMX-byte values (0..=255); `on` is whether the head is lit.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TurretPose {
    pub pan: f32,
    pub tilt: f32,
    pub on: bool,
}

/// The eased aim of turret `channel` at continuous frame-time `t` (frame units,
/// e.g. `current_frame + sub_frame_fraction`).
///
/// Finds the bracketing keyframes — `prev` (latest with `frame <= t`) and `next`
/// (earliest with `frame > t`) — and smoothsteps pan/tilt between them so the head
/// sweeps continuously instead of snapping. Hold semantics match the rest of the
/// editor: before the first keyframe the channel is off (`None`); after the last
/// (or with a single keyframe) it holds that pose. `on` follows the *prev*
/// keyframe's state byte, and a head that is off does not animate (it rests on
/// `prev`). DMX bytes are linear (no angle wraparound), so a plain lerp is correct.
pub fn turret_pose_at(rows: &[TurretKeyframe], channel: u8, t: f32) -> Option<TurretPose> {
    let mut prev: Option<&TurretKeyframe> = None; // latest frame <= t
    let mut next: Option<&TurretKeyframe> = None; // earliest frame > t
    for r in rows {
        if r.channel != channel {
            continue;
        }
        if (r.frame as f32) <= t {
            if prev.is_none_or(|p| r.frame >= p.frame) {
                prev = Some(r);
            }
        } else if next.is_none_or(|n| r.frame < n.frame) {
            next = Some(r);
        }
    }
    let prev = prev?; // before the first keyframe: off
    let on = prev.state > 0;
    let (pan, tilt) = match next {
        // Tween only while the head is on and the bracket spans >0 frames.
        Some(n) if on && n.frame > prev.frame => {
            let span = (n.frame - prev.frame) as f32;
            let s = smoothstep((t - prev.frame as f32) / span);
            (
                prev.pan as f32 + (n.pan as f32 - prev.pan as f32) * s,
                prev.tilt as f32 + (n.tilt as f32 - prev.tilt as f32) * s,
            )
        }
        _ => (prev.pan as f32, prev.tilt as f32),
    };
    Some(TurretPose { pan, tilt, on })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tk(channel: u8, frame: u32, state: u8, pan: u8, tilt: u8) -> TurretKeyframe {
        TurretKeyframe {
            id: 0,
            project_id: 0,
            frame,
            channel,
            state,
            pan,
            tilt,
        }
    }

    #[test]
    fn smoothstep_endpoints_and_clamp() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5); // symmetric inflection
        assert_eq!(smoothstep(-1.0), 0.0); // clamps below
        assert_eq!(smoothstep(2.0), 1.0); // clamps above
    }

    #[test]
    fn turret_pose_before_first_is_off() {
        let rows = [tk(0, 10, 1, 100, 100)];
        assert_eq!(turret_pose_at(&rows, 0, 5.0), None);
    }

    #[test]
    fn turret_pose_after_last_holds() {
        let rows = [tk(0, 10, 1, 40, 60)];
        let p = turret_pose_at(&rows, 0, 100.0).unwrap();
        assert_eq!((p.pan, p.tilt, p.on), (40.0, 60.0, true));
    }

    #[test]
    fn turret_pose_single_keyframe_holds() {
        let rows = [tk(0, 0, 1, 200, 50)];
        let p = turret_pose_at(&rows, 0, 7.0).unwrap();
        assert_eq!((p.pan, p.tilt), (200.0, 50.0));
    }

    #[test]
    fn turret_pose_midpoint_is_eased() {
        // 0..100 over frames 0..10; at the exact midpoint smoothstep(0.5)=0.5.
        let rows = [tk(0, 0, 1, 0, 0), tk(0, 10, 1, 100, 200)];
        let p = turret_pose_at(&rows, 0, 5.0).unwrap();
        assert!((p.pan - 50.0).abs() < 1e-4);
        assert!((p.tilt - 100.0).abs() < 1e-4);
        // A quarter in, easing keeps it below the linear value.
        let q = turret_pose_at(&rows, 0, 2.5).unwrap();
        assert!(q.pan < 25.0, "eased quarter pan {} should trail linear", q.pan);
    }

    #[test]
    fn turret_pose_off_prev_does_not_animate() {
        let rows = [tk(0, 0, 0, 10, 10), tk(0, 10, 1, 250, 250)];
        let p = turret_pose_at(&rows, 0, 5.0).unwrap();
        assert!(!p.on);
        assert_eq!((p.pan, p.tilt), (10.0, 10.0)); // rests on prev, no tween
    }

    #[test]
    fn turret_pose_isolates_channels() {
        let rows = [tk(0, 0, 1, 0, 0), tk(1, 0, 1, 255, 255)];
        assert_eq!(turret_pose_at(&rows, 1, 0.0).unwrap().pan, 255.0);
    }
}
