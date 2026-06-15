//! Light-show timeline editor — SpacetimeDB module.
//!
//! Design: every change a user makes is stored as an immutable, append-only
//! `Edit` row (event sourcing). The current state of a project is the fold of
//! all of its edits, so we get "perfect version control" for free: nothing is
//! ever mutated or deleted, and any past version can be reconstructed by
//! folding the edits up to a chosen sequence number (time travel).
//!
//! Lights are on/off and use *keyframe + hold* semantics: an `Edit` places a
//! keyframe `(light, frame) -> state` and that state holds on the timeline
//! until the next keyframe for the same light. `state` is:
//!   0 = Off keyframe, 1 = On keyframe, 2 = Clear (remove the keyframe here).

use spacetimedb::{Identity, ReducerContext, Table, Timestamp};

/// One light show. Owned by the identity that created it.
#[spacetimedb::table(accessor = project, public)]
pub struct Project {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// Creator. The client filters projects to the connected identity.
    pub owner: Identity,
    pub name: String,
    pub num_lights: u32,
    pub num_frames: u32,
    pub created_at: Timestamp,
    /// Highest `seq` issued for this project == number of edits. Lets us assign
    /// a monotonic per-project sequence without scanning the edit log.
    pub head_seq: u64,
}

/// An immutable, append-only edit. The complete history of a project.
#[spacetimedb::table(accessor = edit_log, public)]
pub struct Edit {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub project_id: u64,
    /// Monotonic 1-based sequence within the project (its position in history).
    pub seq: u64,
    pub author: Identity,
    pub created_at: Timestamp,
    pub light: u32,
    pub frame: u32,
    /// 0 = Off keyframe, 1 = On keyframe, 2 = Clear (remove keyframe).
    pub state: u8,
}

#[spacetimedb::reducer(init)]
pub fn init(_ctx: &ReducerContext) {}

/// Create a new, empty light-show project owned by the caller.
#[spacetimedb::reducer]
pub fn create_project(
    ctx: &ReducerContext,
    name: String,
    num_lights: u32,
    num_frames: u32,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Project name cannot be empty".to_string());
    }
    let num_lights = num_lights.clamp(1, 512);
    let num_frames = num_frames.clamp(1, 100_000);
    ctx.db.project().insert(Project {
        id: 0,
        owner: ctx.sender(),
        name: name.to_string(),
        num_lights,
        num_frames,
        created_at: ctx.timestamp,
        head_seq: 0,
    });
    Ok(())
}

/// Append a keyframe edit. This is the only way project content ever changes,
/// which is what makes the edit log a complete, replayable history.
#[spacetimedb::reducer]
pub fn append_edit(
    ctx: &ReducerContext,
    project_id: u64,
    light: u32,
    frame: u32,
    state: u8,
) -> Result<(), String> {
    let Some(project) = ctx.db.project().id().find(project_id) else {
        return Err("Project not found".to_string());
    };
    if project.owner != ctx.sender() {
        return Err("You do not own this project".to_string());
    }
    if light >= project.num_lights {
        return Err("light index out of range".to_string());
    }
    if frame >= project.num_frames {
        return Err("frame index out of range".to_string());
    }
    if state > 2 {
        return Err("state must be 0 (off), 1 (on) or 2 (clear)".to_string());
    }

    let seq = project.head_seq + 1;
    ctx.db.edit_log().insert(Edit {
        id: 0,
        project_id,
        seq,
        author: ctx.sender(),
        created_at: ctx.timestamp,
        light,
        frame,
        state,
    });
    ctx.db.project().id().update(Project {
        head_seq: seq,
        ..project
    });
    Ok(())
}
