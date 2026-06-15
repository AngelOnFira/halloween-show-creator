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
