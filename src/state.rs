//! ECS resources holding the editor's state (everything that isn't the
//! SpacetimeDB connection, which is a `NonSend` resource in `conn.rs`).

use std::collections::HashMap;

use bevy::prelude::*;

use crate::module_bindings::{LaserKeyframe, ProjectorKeyframe, TurretKeyframe};

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
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            open_project: None,
            current_frame: 0,
            history_pos: None,
            new_name: "My Light Show".to_owned(),
            new_lights: 8,
            last_error: None,
            autogen_pattern: 0,
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
