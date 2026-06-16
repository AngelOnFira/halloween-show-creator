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

use spacetimedb::{Identity, ReducerContext, SpacetimeType, Table, Timestamp};

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
    /// Seeded sample show: visible to *everyone* (not just the owner) and shown
    /// read-only unless forked. Normal user projects are `false`.
    /// Defaulted so the column can be added to an existing DB without wiping it.
    #[default(false)]
    pub is_template: bool,
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

// ---------------------------------------------------------------------------
// Rich fixtures imported from legacy shows: lasers, DMX gobo projectors, and
// DMX moving-head turrets. Unlike on/off lights, these aren't part of the
// event-sourced `Edit` log — they are stored as direct keyframe rows that the
// client renders with *hold* semantics (the latest keyframe at or before the
// playhead frame, per channel, wins; an "off" keyframe blanks the channel).
//
// They are seeded with a show and copied on fork, but are *view-only* in v1
// (there is no in-editor authoring UI for them yet).
// ---------------------------------------------------------------------------

/// One vertex of a laser's projected path. `x`/`y` are the galvo coordinates in
/// the legacy 0..=300 space; `r`/`g`/`b` are 3-bit (0..=7) per-point colour.
#[derive(SpacetimeType, Clone)]
pub struct LaserPoint {
    pub x: i16,
    pub y: i16,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A laser projector's state at a frame. `channel` 0..=4 is the laser index.
#[spacetimedb::table(accessor = laser_kf, public)]
pub struct LaserKeyframe {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub project_id: u64,
    pub frame: u32,
    pub channel: u8,
    /// False = laser blanked at this keyframe (an off/reset keyframe).
    pub enable: bool,
    /// Legacy pattern id (0..=38); informational — `points` carry the geometry.
    pub pattern: u8,
    pub points: Vec<LaserPoint>,
}

/// A DMX gobo projector's state at a frame. `channel` 0 (one projector, `lp-1`).
/// Values are raw DMX bytes (0..=255).
#[spacetimedb::table(accessor = projector_kf, public)]
pub struct ProjectorKeyframe {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub project_id: u64,
    pub frame: u32,
    pub channel: u8,
    pub state: u8,
    pub gallery: u8,
    pub pattern: u8,
    pub colour: u8,
}

/// A DMX moving-head turret's state at a frame. `channel` 0..=3 is the turret
/// index. Values are raw DMX bytes (0..=255).
#[spacetimedb::table(accessor = turret_kf, public)]
pub struct TurretKeyframe {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub project_id: u64,
    pub frame: u32,
    pub channel: u8,
    pub state: u8,
    pub pan: u8,
    pub tilt: u8,
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
        is_template: false,
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

// ---------------------------------------------------------------------------
// Audio: per-project song + beat grid, with the raw audio stored chunked.
//
// The song is *mutable per-project metadata* (NOT part of the event-sourced
// `Edit` history): the raw bytes are megabytes, so folding them into the replay
// log would wreck the "replay is cheap" property. Light keyframes baked onto
// beats are still normal `append_edit` calls, so they remain undoable.
//
// The audio bytes are split into ordered `SongChunk` rows. Chunking keeps every
// reducer call and every subscription row push well under SpacetimeDB's 32 MiB
// WebSocket message ceiling, and lets the client show an upload progress bar.
// ---------------------------------------------------------------------------

/// Max bytes per audio chunk (kept far below the 32 MiB WS message limit).
const CHUNK_SIZE: usize = 256 * 1024;
/// Reject songs larger than this so we stay clear of the message ceiling.
const MAX_SONG_BYTES: u64 = 24 * 1024 * 1024;

/// One song per project (replaced on re-upload). Small: metadata + beat grid.
#[spacetimedb::table(accessor = song, public)]
pub struct Song {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub project_id: u64,
    pub owner: Identity,
    /// Original filename.
    pub name: String,
    /// e.g. "audio/mpeg".
    pub mime: String,
    /// Total assembled byte length (integrity check).
    pub byte_len: u64,
    pub num_chunks: u32,
    /// Upload progress, bumped by `append_song_chunk` (drives the client's bar).
    pub chunks_received: u32,
    pub duration_ms: u32,
    /// Half-beat frame rate (`bpm/30`) used to map seconds <-> frames.
    pub fps_used: f32,
    /// Detected beat positions, as timeline frame indices (the even frames:
    /// 0, 2, 4, …, since each beat spans two half-beat frames).
    pub beats_frames: Vec<u32>,
    /// Estimated global tempo (0.0 if unknown).
    pub bpm: f32,
    /// Time of the first detected beat (audio playback phase offset): frame `f`
    /// corresponds to `first_beat_ms/1000 + f * (30/bpm)` seconds of audio.
    pub first_beat_ms: u32,
    /// True once every chunk is present and the total length matches `byte_len`.
    pub complete: bool,
    pub created_at: Timestamp,
}

/// Raw audio bytes, one row per ordered chunk. Subscribed on-demand only (the
/// client scopes the query to the open project).
#[spacetimedb::table(accessor = song_chunk, public)]
pub struct SongChunk {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub song_id: u64,
    /// Denormalized so clients can scope the subscription by project.
    pub project_id: u64,
    pub idx: u32,
    pub data: Vec<u8>,
}

/// Delete every song (and its chunks) belonging to a project.
fn delete_project_songs(ctx: &ReducerContext, project_id: u64) {
    let song_ids: Vec<u64> = ctx
        .db
        .song()
        .project_id()
        .filter(project_id)
        .map(|s| s.id)
        .collect();
    for sid in song_ids {
        let chunk_ids: Vec<u64> = ctx
            .db
            .song_chunk()
            .song_id()
            .filter(sid)
            .map(|c| c.id)
            .collect();
        for cid in chunk_ids {
            ctx.db.song_chunk().id().delete(cid);
        }
        ctx.db.song().id().delete(sid);
    }
}

/// Replace the project's song with a fresh, empty (incomplete) one. The client
/// then streams the bytes with `append_song_chunk`. Beats are detected
/// client-side and passed in here as frame indices.
#[spacetimedb::reducer]
pub fn begin_song_upload(
    ctx: &ReducerContext,
    project_id: u64,
    name: String,
    mime: String,
    byte_len: u64,
    num_chunks: u32,
    duration_ms: u32,
    fps_used: f32,
    beats_frames: Vec<u32>,
    bpm: f32,
    first_beat_ms: u32,
) -> Result<(), String> {
    let Some(project) = ctx.db.project().id().find(project_id) else {
        return Err("Project not found".to_string());
    };
    if project.owner != ctx.sender() {
        return Err("You do not own this project".to_string());
    }
    if byte_len == 0 {
        return Err("Audio file is empty".to_string());
    }
    if byte_len > MAX_SONG_BYTES {
        return Err(format!(
            "Song too large: {byte_len} bytes (max {MAX_SONG_BYTES})"
        ));
    }
    let expected = byte_len.div_ceil(CHUNK_SIZE as u64) as u32;
    if num_chunks != expected {
        return Err(format!("num_chunks {num_chunks} != expected {expected}"));
    }
    // Keep only beats that fall inside the timeline.
    let mut beats_frames: Vec<u32> = beats_frames
        .into_iter()
        .filter(|&f| f < project.num_frames)
        .collect();
    beats_frames.sort_unstable();
    beats_frames.dedup();

    delete_project_songs(ctx, project_id);
    ctx.db.song().insert(Song {
        id: 0,
        project_id,
        owner: ctx.sender(),
        name,
        mime,
        byte_len,
        num_chunks,
        chunks_received: 0,
        duration_ms,
        fps_used,
        beats_frames,
        bpm,
        first_beat_ms,
        complete: false,
        created_at: ctx.timestamp,
    });
    Ok(())
}

/// Append one audio chunk to an in-progress song upload.
#[spacetimedb::reducer]
pub fn append_song_chunk(
    ctx: &ReducerContext,
    song_id: u64,
    idx: u32,
    data: Vec<u8>,
) -> Result<(), String> {
    let Some(mut song) = ctx.db.song().id().find(song_id) else {
        return Err("Song not found".to_string());
    };
    if song.owner != ctx.sender() {
        return Err("You do not own this song".to_string());
    }
    if data.len() > CHUNK_SIZE {
        return Err("Chunk exceeds CHUNK_SIZE".to_string());
    }
    if idx >= song.num_chunks {
        return Err("Chunk index out of range".to_string());
    }
    // Idempotency: ignore a chunk we already have (so a retry is harmless).
    if ctx.db.song_chunk().song_id().filter(song_id).any(|c| c.idx == idx) {
        return Ok(());
    }

    let project_id = song.project_id;
    ctx.db.song_chunk().insert(SongChunk {
        id: 0,
        song_id,
        project_id,
        idx,
        data,
    });

    song.chunks_received += 1;
    if song.chunks_received >= song.num_chunks {
        let total: u64 = ctx
            .db
            .song_chunk()
            .song_id()
            .filter(song_id)
            .map(|c| c.data.len() as u64)
            .sum();
        song.complete = total == song.byte_len;
    }
    ctx.db.song().id().update(song);
    Ok(())
}

/// Remove the project's song and all its chunks.
#[spacetimedb::reducer]
pub fn delete_song(ctx: &ReducerContext, project_id: u64) -> Result<(), String> {
    let Some(project) = ctx.db.project().id().find(project_id) else {
        return Err("Project not found".to_string());
    };
    if project.owner != ctx.sender() {
        return Err("You do not own this project".to_string());
    }
    delete_project_songs(ctx, project_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Seeding & forking.
//
// Seeded "sample" shows are imported once from the legacy rusty-halloween shows
// by a host-side tool (see tools/show-seeder). Reducers can't return the new
// project's id, so the seeder addresses a template by its (unique) name;
// `seed_project` makes re-runs idempotent by wiping any existing template with
// the same name first. `fork_project` lets any user turn a (read-only) template
// into an editable copy they own.
// ---------------------------------------------------------------------------

/// Find the template project with a given name (there is at most one).
fn find_template(ctx: &ReducerContext, name: &str) -> Option<Project> {
    ctx.db
        .project()
        .iter()
        .find(|p| p.is_template && p.name == name)
}

/// Delete a project's edit log and all fixture keyframes (not the project row,
/// not its song — use `delete_project_songs` for that).
fn delete_project_children(ctx: &ReducerContext, project_id: u64) {
    let edit_ids: Vec<u64> = ctx
        .db
        .edit_log()
        .project_id()
        .filter(project_id)
        .map(|e| e.id)
        .collect();
    for id in edit_ids {
        ctx.db.edit_log().id().delete(id);
    }
    let laser_ids: Vec<u64> = ctx
        .db
        .laser_kf()
        .project_id()
        .filter(project_id)
        .map(|r| r.id)
        .collect();
    for id in laser_ids {
        ctx.db.laser_kf().id().delete(id);
    }
    let projector_ids: Vec<u64> = ctx
        .db
        .projector_kf()
        .project_id()
        .filter(project_id)
        .map(|r| r.id)
        .collect();
    for id in projector_ids {
        ctx.db.projector_kf().id().delete(id);
    }
    let turret_ids: Vec<u64> = ctx
        .db
        .turret_kf()
        .project_id()
        .filter(project_id)
        .map(|r| r.id)
        .collect();
    for id in turret_ids {
        ctx.db.turret_kf().id().delete(id);
    }
}

/// Create (or replace) a seeded template project, addressed by its `name`.
#[spacetimedb::reducer]
pub fn seed_project(
    ctx: &ReducerContext,
    name: String,
    num_lights: u32,
    num_frames: u32,
) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name is required".to_string());
    }
    let num_lights = num_lights.clamp(1, 512);
    let num_frames = num_frames.clamp(1, 100_000);
    // Idempotency: drop any existing template with this name and all its content.
    if let Some(existing) = find_template(ctx, &name) {
        delete_project_children(ctx, existing.id);
        delete_project_songs(ctx, existing.id);
        ctx.db.project().id().delete(existing.id);
    }
    ctx.db.project().insert(Project {
        id: 0,
        owner: ctx.sender(),
        name,
        num_lights,
        num_frames,
        created_at: ctx.timestamp,
        head_seq: 0,
        is_template: true,
    });
    Ok(())
}

/// One light keyframe for `seed_light_edits`.
#[derive(SpacetimeType)]
pub struct LightEditInput {
    pub light: u32,
    pub frame: u32,
    /// 0 = Off, 1 = On, 2 = Clear (same as `append_edit`).
    pub state: u8,
}

/// Bulk-append light keyframes to a template's edit log, assigning monotonic
/// `seq` and bumping `head_seq` once.
#[spacetimedb::reducer]
pub fn seed_light_edits(
    ctx: &ReducerContext,
    name: String,
    edits: Vec<LightEditInput>,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    let mut seq = project.head_seq;
    for e in edits {
        if e.light >= project.num_lights || e.frame >= project.num_frames || e.state > 2 {
            continue;
        }
        seq += 1;
        ctx.db.edit_log().insert(Edit {
            id: 0,
            project_id: project.id,
            seq,
            author: ctx.sender(),
            created_at: ctx.timestamp,
            light: e.light,
            frame: e.frame,
            state: e.state,
        });
    }
    ctx.db.project().id().update(Project {
        head_seq: seq,
        ..project
    });
    Ok(())
}

/// One laser keyframe for `seed_laser_keyframes`.
#[derive(SpacetimeType)]
pub struct LaserKeyframeInput {
    pub frame: u32,
    pub channel: u8,
    pub enable: bool,
    pub pattern: u8,
    pub points: Vec<LaserPoint>,
}

#[spacetimedb::reducer]
pub fn seed_laser_keyframes(
    ctx: &ReducerContext,
    name: String,
    rows: Vec<LaserKeyframeInput>,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    for r in rows {
        ctx.db.laser_kf().insert(LaserKeyframe {
            id: 0,
            project_id: project.id,
            frame: r.frame,
            channel: r.channel,
            enable: r.enable,
            pattern: r.pattern,
            points: r.points,
        });
    }
    Ok(())
}

/// One projector keyframe for `seed_projector_keyframes`.
#[derive(SpacetimeType)]
pub struct ProjectorKeyframeInput {
    pub frame: u32,
    pub channel: u8,
    pub state: u8,
    pub gallery: u8,
    pub pattern: u8,
    pub colour: u8,
}

#[spacetimedb::reducer]
pub fn seed_projector_keyframes(
    ctx: &ReducerContext,
    name: String,
    rows: Vec<ProjectorKeyframeInput>,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    for r in rows {
        ctx.db.projector_kf().insert(ProjectorKeyframe {
            id: 0,
            project_id: project.id,
            frame: r.frame,
            channel: r.channel,
            state: r.state,
            gallery: r.gallery,
            pattern: r.pattern,
            colour: r.colour,
        });
    }
    Ok(())
}

/// One turret keyframe for `seed_turret_keyframes`.
#[derive(SpacetimeType)]
pub struct TurretKeyframeInput {
    pub frame: u32,
    pub channel: u8,
    pub state: u8,
    pub pan: u8,
    pub tilt: u8,
}

#[spacetimedb::reducer]
pub fn seed_turret_keyframes(
    ctx: &ReducerContext,
    name: String,
    rows: Vec<TurretKeyframeInput>,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    for r in rows {
        ctx.db.turret_kf().insert(TurretKeyframe {
            id: 0,
            project_id: project.id,
            frame: r.frame,
            channel: r.channel,
            state: r.state,
            pan: r.pan,
            tilt: r.tilt,
        });
    }
    Ok(())
}

/// Begin a template's song upload (addressed by the template `name`). Mirrors
/// `begin_song_upload` but resolves the project by name instead of id.
#[spacetimedb::reducer]
pub fn seed_song(
    ctx: &ReducerContext,
    name: String,
    song_name: String,
    mime: String,
    byte_len: u64,
    num_chunks: u32,
    duration_ms: u32,
    fps_used: f32,
    beats_frames: Vec<u32>,
    bpm: f32,
    first_beat_ms: u32,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    if byte_len == 0 {
        return Err("Audio file is empty".to_string());
    }
    if byte_len > MAX_SONG_BYTES {
        return Err(format!(
            "Song too large: {byte_len} bytes (max {MAX_SONG_BYTES})"
        ));
    }
    let expected = byte_len.div_ceil(CHUNK_SIZE as u64) as u32;
    if num_chunks != expected {
        return Err(format!("num_chunks {num_chunks} != expected {expected}"));
    }
    let mut beats_frames: Vec<u32> = beats_frames
        .into_iter()
        .filter(|&f| f < project.num_frames)
        .collect();
    beats_frames.sort_unstable();
    beats_frames.dedup();

    delete_project_songs(ctx, project.id);
    ctx.db.song().insert(Song {
        id: 0,
        project_id: project.id,
        owner: ctx.sender(),
        name: song_name,
        mime,
        byte_len,
        num_chunks,
        chunks_received: 0,
        duration_ms,
        fps_used,
        beats_frames,
        bpm,
        first_beat_ms,
        complete: false,
        created_at: ctx.timestamp,
    });
    Ok(())
}

/// Append one audio chunk to a template's in-progress song (by template `name`).
#[spacetimedb::reducer]
pub fn seed_song_chunk(
    ctx: &ReducerContext,
    name: String,
    idx: u32,
    data: Vec<u8>,
) -> Result<(), String> {
    let Some(project) = find_template(ctx, &name) else {
        return Err("template not found".to_string());
    };
    let Some(mut song) = ctx.db.song().project_id().filter(project.id).next() else {
        return Err("song upload not begun".to_string());
    };
    if data.len() > CHUNK_SIZE {
        return Err("Chunk exceeds CHUNK_SIZE".to_string());
    }
    if idx >= song.num_chunks {
        return Err("Chunk index out of range".to_string());
    }
    if ctx.db.song_chunk().song_id().filter(song.id).any(|c| c.idx == idx) {
        return Ok(());
    }
    let project_id = project.id;
    ctx.db.song_chunk().insert(SongChunk {
        id: 0,
        song_id: song.id,
        project_id,
        idx,
        data,
    });
    song.chunks_received += 1;
    if song.chunks_received >= song.num_chunks {
        let total: u64 = ctx
            .db
            .song_chunk()
            .song_id()
            .filter(song.id)
            .map(|c| c.data.len() as u64)
            .sum();
        song.complete = total == song.byte_len;
    }
    ctx.db.song().id().update(song);
    Ok(())
}

/// Copy a project (template or otherwise) into a fresh, editable project owned
/// by the caller: its edit log, all fixture keyframes, and its song + chunks.
#[spacetimedb::reducer]
pub fn fork_project(ctx: &ReducerContext, project_id: u64) -> Result<(), String> {
    let Some(src) = ctx.db.project().id().find(project_id) else {
        return Err("Project not found".to_string());
    };
    let new = ctx.db.project().insert(Project {
        id: 0,
        owner: ctx.sender(),
        name: format!("{} (copy)", src.name),
        num_lights: src.num_lights,
        num_frames: src.num_frames,
        created_at: ctx.timestamp,
        head_seq: src.head_seq,
        is_template: false,
    });
    let nid = new.id;

    let edits: Vec<Edit> = ctx.db.edit_log().project_id().filter(project_id).collect();
    for e in edits {
        ctx.db.edit_log().insert(Edit {
            id: 0,
            project_id: nid,
            seq: e.seq,
            author: ctx.sender(),
            created_at: ctx.timestamp,
            light: e.light,
            frame: e.frame,
            state: e.state,
        });
    }
    let lasers: Vec<LaserKeyframe> = ctx.db.laser_kf().project_id().filter(project_id).collect();
    for r in lasers {
        ctx.db.laser_kf().insert(LaserKeyframe {
            id: 0,
            project_id: nid,
            frame: r.frame,
            channel: r.channel,
            enable: r.enable,
            pattern: r.pattern,
            points: r.points,
        });
    }
    let projectors: Vec<ProjectorKeyframe> =
        ctx.db.projector_kf().project_id().filter(project_id).collect();
    for r in projectors {
        ctx.db.projector_kf().insert(ProjectorKeyframe {
            id: 0,
            project_id: nid,
            frame: r.frame,
            channel: r.channel,
            state: r.state,
            gallery: r.gallery,
            pattern: r.pattern,
            colour: r.colour,
        });
    }
    let turrets: Vec<TurretKeyframe> =
        ctx.db.turret_kf().project_id().filter(project_id).collect();
    for r in turrets {
        ctx.db.turret_kf().insert(TurretKeyframe {
            id: 0,
            project_id: nid,
            frame: r.frame,
            channel: r.channel,
            state: r.state,
            pan: r.pan,
            tilt: r.tilt,
        });
    }
    if let Some(song) = ctx.db.song().project_id().filter(project_id).next() {
        let new_song = ctx.db.song().insert(Song {
            id: 0,
            project_id: nid,
            owner: ctx.sender(),
            name: song.name,
            mime: song.mime,
            byte_len: song.byte_len,
            num_chunks: song.num_chunks,
            chunks_received: song.chunks_received,
            duration_ms: song.duration_ms,
            fps_used: song.fps_used,
            beats_frames: song.beats_frames,
            bpm: song.bpm,
            first_beat_ms: song.first_beat_ms,
            complete: song.complete,
            created_at: ctx.timestamp,
        });
        let new_song_id = new_song.id;
        let chunks: Vec<SongChunk> = ctx.db.song_chunk().song_id().filter(song.id).collect();
        for c in chunks {
            ctx.db.song_chunk().insert(SongChunk {
                id: 0,
                song_id: new_song_id,
                project_id: nid,
                idx: c.idx,
                data: c.data,
            });
        }
    }
    Ok(())
}
