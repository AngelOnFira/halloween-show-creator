//! ECS resources holding the editor's state (everything that isn't the
//! SpacetimeDB connection, which is a `NonSend` resource in `conn.rs`).

use std::collections::HashMap;

use bevy::prelude::*;

use crate::module_bindings::{
    LaserKeyframe, LaserKeyframeInput, LightEditInput, ProjectorKeyframe, ProjectorKeyframeInput,
    TurretKeyframe, TurretKeyframeInput,
};

/// Which device category the timeline shows. `All` stacks every device row;
/// the others narrow to a single category.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineFilter {
    #[default]
    All,
    Lights,
    Lasers,
    Turrets,
    Projector,
}

/// A rectangular timeline selection, in the unified row space
/// (`0..num_lights` are light rows, the rest are fixture tracks) × frames.
/// Bounds are inclusive.
#[derive(Clone, Copy)]
pub struct GridSelection {
    pub row_min: u32,
    pub row_max: u32,
    pub frame_min: u32,
    pub frame_max: u32,
}

/// A copied timeline region, held in memory for paste / duplicate. Offsets are
/// relative to the selection's top-left: lights carry a `(light, frame)` offset,
/// fixtures carry a frame offset but keep their physical `channel`. Lost on
/// reload — the persistent counterpart is a saved blueprint.
#[derive(Clone)]
pub struct Clipboard {
    /// Light rows / frames spanned (for the status label and blueprint dims).
    pub light_span: u32,
    pub frame_span: u32,
    /// The selection's first light row, so a plain paste lands on the same rows.
    pub src_light_min: u32,
    pub lights: Vec<LightEditInput>,
    pub lasers: Vec<LaserKeyframeInput>,
    pub projectors: Vec<ProjectorKeyframeInput>,
    pub turrets: Vec<TurretKeyframeInput>,
}

impl Clipboard {
    pub fn is_empty(&self) -> bool {
        self.lights.is_empty()
            && self.lasers.is_empty()
            && self.projectors.is_empty()
            && self.turrets.is_empty()
    }

    /// Light edits placed at `(base_light, base_frame)`.
    pub fn light_rows(&self, base_light: u32, base_frame: u32) -> Vec<LightEditInput> {
        self.lights
            .iter()
            .map(|e| LightEditInput {
                light: base_light + e.light,
                frame: base_frame + e.frame,
                state: e.state,
            })
            .collect()
    }

    pub fn laser_rows(&self, base_frame: u32) -> Vec<LaserKeyframeInput> {
        self.lasers
            .iter()
            .map(|r| LaserKeyframeInput {
                frame: base_frame + r.frame,
                channel: r.channel,
                enable: r.enable,
                pattern: r.pattern,
                points: r.points.clone(),
            })
            .collect()
    }

    pub fn projector_rows(&self, base_frame: u32) -> Vec<ProjectorKeyframeInput> {
        self.projectors
            .iter()
            .map(|r| ProjectorKeyframeInput {
                frame: base_frame + r.frame,
                channel: r.channel,
                state: r.state,
                gallery: r.gallery,
                pattern: r.pattern,
                colour: r.colour,
            })
            .collect()
    }

    pub fn turret_rows(&self, base_frame: u32) -> Vec<TurretKeyframeInput> {
        self.turrets
            .iter()
            .map(|r| TurretKeyframeInput {
                frame: base_frame + r.frame,
                channel: r.channel,
                state: r.state,
                pan: r.pan,
                tilt: r.tilt,
            })
            .collect()
    }

    /// The clipboard's light pattern as `held[light_offset][frame_offset]`, for
    /// rendering the ghost and computing overwrite stamps.
    pub fn light_held(&self) -> Vec<Vec<bool>> {
        let mut kf: HashMap<(u32, u32), bool> = HashMap::new();
        for e in &self.lights {
            match e.state {
                0 => {
                    kf.insert((e.light, e.frame), false);
                }
                1 => {
                    kf.insert((e.light, e.frame), true);
                }
                _ => {}
            }
        }
        crate::logic::expand_held(&kf, self.light_span, self.frame_span)
    }

    /// Distinct fixture channels the clipboard touches, for clearing the
    /// destination before an overwrite stamp.
    pub fn fixture_channels(&self) -> (Vec<u8>, Vec<u8>, bool) {
        let mut lasers: Vec<u8> = self.lasers.iter().map(|r| r.channel).collect();
        lasers.sort_unstable();
        lasers.dedup();
        let mut turrets: Vec<u8> = self.turrets.iter().map(|r| r.channel).collect();
        turrets.sort_unstable();
        turrets.dedup();
        (lasers, turrets, !self.projectors.is_empty())
    }
}

/// What an in-progress pointer drag on the timeline grid is doing. Most kinds
/// only update a live preview; the edit is committed once on release.
#[derive(Clone)]
pub enum DragKind {
    /// Scrubbing the playhead from the beat-ruler strip.
    Scrub,
    /// Drawing a marquee; `ghost` = true for Ctrl+drag (copy-to-ghost on release).
    Marquee { ghost: bool },
    /// Painting a light row "on" from `start` to `cur` (inclusive).
    Paint { light: u32, start: u32, cur: u32 },
    /// Resizing the light bar originally spanning `[bar_start, bar_end]`.
    Resize {
        light: u32,
        bar_start: u32,
        bar_end: u32,
        drag_left: bool,
        cur: u32,
    },
    /// Moving the current selection; delta = `cur - grab`.
    Move {
        orig: GridSelection,
        grab_row: u32,
        grab_frame: u32,
        cur_row: u32,
        cur_frame: u32,
    },
}

/// UI / editor state — a direct lift of the original `LightShowApp` fields.
#[derive(Resource)]
pub struct AppState {
    /// Currently opened project id (None => project picker screen).
    pub open_project: Option<u64>,
    pub current_frame: u32,
    /// Time-travel cursor: `None` = live, `Some(seq)` = viewing after edit `seq`.
    pub history_pos: Option<u64>,
    pub new_name: String,
    pub new_lights: u32,
    pub last_error: Option<String>,
    /// Auto-generate pattern: 0 = strobe all, 1 = chase, 2 = alternate.
    pub autogen_pattern: u8,
    /// Viewport camera orbit azimuth, in degrees (0–360). Driven by both the
    /// topbar slider and click-drag on the 3D scene; read by `scene::orbit_camera`.
    pub camera_angle: f32,
    /// Finalized Factorio-style marquee selection on the timeline (Shift+drag),
    /// or `None` when nothing is selected.
    pub selection: Option<GridSelection>,
    /// Anchor cell `(row, frame)` of an in-progress Shift+drag marquee.
    pub sel_anchor: Option<(u32, u32)>,
    /// In-memory copied region for paste / duplicate (lost on reload).
    pub clipboard: Option<Clipboard>,
    /// Name field for saving the current selection as a blueprint.
    pub blueprint_name: String,
    /// Which device category the timeline currently shows.
    pub timeline_filter: TimelineFilter,
    /// Blueprint id whose paste preview ghost is shown at the playhead.
    pub active_blueprint: Option<u64>,
    /// In-progress grid drag (transient; reset on release).
    pub drag: Option<DragKind>,
    /// A region "in hand" as a white ghost (from Ctrl+drag); click stamps it.
    pub ghost: Option<Clipboard>,
    /// Optimistic light edits sent but not yet echoed by the backend, applied on
    /// top of the authoritative fold so edits appear instantly. Cleared once our
    /// own edits past `pending_base` have all landed.
    pub pending: Vec<LightEditInput>,
    /// `head_seq` at the moment the current pending batch began.
    pub pending_base: u64,
    /// Project the pending batch belongs to (cleared on project switch).
    pub pending_project: u64,
    /// Cached `head_seq` of the open project (set each frame by the UI).
    pub cur_head: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            open_project: None,
            current_frame: 0,
            history_pos: None,
            new_name: "My Light Show".to_owned(),
            new_lights: 7,
            last_error: None,
            autogen_pattern: 0,
            camera_angle: 0.0,
            selection: None,
            sel_anchor: None,
            clipboard: None,
            blueprint_name: "Snippet".to_owned(),
            timeline_filter: TimelineFilter::default(),
            active_blueprint: None,
            drag: None,
            ghost: None,
            pending: Vec::new(),
            pending_base: 0,
            pending_project: 0,
            cur_head: 0,
        }
    }
}

/// Playback clock state. `fps` is client-side (a UI control), not persisted.
#[derive(Resource)]
pub struct Playback {
    pub playing: bool,
    pub fps: f32,
    pub looping: bool,
    pub accumulator: f32,
    /// Set each frame by `audio::audio_playback_sync` when the playhead is being
    /// driven by the audio clock; suppresses the real-time `playback_advance`.
    pub audio_driven: bool,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            playing: false,
            fps: 30.0,
            looping: true,
            accumulator: 0.0,
            audio_driven: false,
        }
    }
}

/// The held on/off grid for the open project at the current history cutoff,
/// recomputed once per frame so both the UI and the 3D apply system read the
/// same data. `held[light][frame]`.
#[derive(Resource, Default)]
pub struct HeldGrid {
    /// Explicit keyframes (on/off) at the current cutoff.
    pub keyframes: HashMap<(u32, u32), bool>,
    pub held: Vec<Vec<bool>>,
    pub nl: u32,
    pub nf: u32,
    /// `project.head_seq` of the open project (for the history slider range).
    pub head: u64,
    /// Whether we are viewing a past version (read-only).
    pub viewing_history: bool,
}

/// The rich fixtures (laser / gobo projector / turret) in effect at the current
/// playhead frame, recomputed once per frame from the keyframe tables (held
/// semantics: latest keyframe at or before the playhead, per channel). Read by
/// the 3D render systems. Indexed by channel.
#[derive(Resource, Default)]
pub struct FixtureGrid {
    pub lasers: Vec<Option<LaserKeyframe>>,
    pub projectors: Vec<Option<ProjectorKeyframe>>,
    pub turrets: Vec<Option<TurretKeyframe>>,
}
