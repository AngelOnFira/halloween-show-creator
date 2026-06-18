//! Song upload + beat detection + synced playback.
//!
//! Flow (all browser-side; nothing heavy ever runs in a reducer):
//!   1. User picks an audio file (`rfd`).
//!   2. We decode it with the browser's Web Audio `decodeAudioData` and run a
//!      pure-Rust spectral-flux beat detector on the PCM.
//!   3. Beats (mapped to timeline frames) + the raw bytes are uploaded to
//!      SpacetimeDB: `begin_song_upload` then `append_song_chunk` per 256 KiB
//!      chunk. Progress is read back from the `song` row's `chunks_received`.
//!   4. For playback we reassemble the chunks, decode once, and drive the
//!      timeline playhead from the Web Audio clock so lights lock to the music.
//!
//! The decode/playback bits are wasm-only (`web-sys`); native builds get no-op
//! stubs so `cargo check` stays green. Beat detection is cross-platform.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bevy::prelude::*;

use crate::conn::{ConnResource, ConnState};
use crate::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};

/// Must match `CHUNK_SIZE` in the SpacetimeDB module.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Decoded analysis result handed from the async pick/decode task to the
/// upload-driver system. The timeline length is derived here from the song:
/// `num_frames = 2 * num_beats` (one on-beat frame + one off-beat frame each).
pub struct AudioAnalysis {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub name: String,
    pub duration_ms: u32,
    /// On-beat frame indices (even: 0, 2, 4, …).
    pub beats_frames: Vec<u32>,
    pub bpm: f32,
    /// Half-beat frame rate, `bpm / 30`.
    pub fps_used: f32,
    /// `2 * num_beats`, clamped to the project limit.
    pub num_frames: u32,
    pub first_beat_ms: u32,
}

#[derive(Clone, PartialEq, Default)]
pub enum UploadPhase {
    #[default]
    Idle,
    Picking,
    Analyzing,
    CreatingProject,
    Beginning,
    Sending {
        song_id: u64,
        total: u32,
    },
    Done,
    Error(String),
}

/// Upload state machine. `NonSend` (holds `Rc`s shared with the async task).
pub struct UploadState {
    pub phase: Rc<RefCell<UploadPhase>>,
    /// Written by the async decode task, drained by `drive_upload`.
    pub incoming: Rc<RefCell<Option<AudioAnalysis>>>,
    /// The analysis whose bytes are currently being chunk-uploaded.
    pub pending: Option<AudioAnalysis>,
    /// The project created for this song (0 until `create_project` lands).
    pub project_id: Cell<u64>,
    /// Name + light count for the project being created.
    pub pending_name: RefCell<String>,
    pub pending_lights: Cell<u32>,
}

impl Default for UploadState {
    fn default() -> Self {
        Self {
            phase: Rc::new(RefCell::new(UploadPhase::Idle)),
            incoming: Rc::new(RefCell::new(None)),
            pending: None,
            project_id: Cell::new(0),
            pending_name: RefCell::new(String::new()),
            pending_lights: Cell::new(8),
        }
    }
}

impl UploadState {
    pub fn phase(&self) -> UploadPhase {
        self.phase.borrow().clone()
    }
    pub fn is_busy(&self) -> bool {
        !matches!(
            self.phase(),
            UploadPhase::Idle | UploadPhase::Done | UploadPhase::Error(_)
        )
    }
}

/// Startup: insert the audio `NonSend` resources.
pub fn setup_audio(world: &mut World) {
    world.insert_non_send_resource(UploadState::default());
    world.insert_non_send_resource(AudioPlayback::default());
}

// ---------------------------------------------------------------------------
// Beat detection (cross-platform): spectral-flux onset envelope → tempo via
// autocorrelation → a regular beat grid (phase-aligned to the onsets).
// ---------------------------------------------------------------------------

/// Returns `(beat_times_seconds, estimated_bpm)`.
pub fn detect_beats(pcm: &[f32], sample_rate: f32) -> (Vec<f32>, f32) {
    use rustfft::num_complex::Complex;

    const WIN: usize = 1024;
    const HOP: usize = 512;
    if pcm.len() < WIN * 2 || sample_rate <= 0.0 {
        return (Vec::new(), 0.0);
    }

    // Hann window.
    let hann: Vec<f32> = (0..WIN)
        .map(|n| {
            let s = (std::f32::consts::PI * n as f32 / (WIN as f32 - 1.0)).sin();
            s * s
        })
        .collect();

    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(WIN);
    let nbins = WIN / 2;
    let mut prev = vec![0f32; nbins];
    let mut buf = vec![Complex::new(0.0f32, 0.0f32); WIN];
    let mut flux: Vec<f32> = Vec::new();

    let mut pos = 0usize;
    while pos + WIN <= pcm.len() {
        for i in 0..WIN {
            buf[i] = Complex::new(pcm[pos + i] * hann[i], 0.0);
        }
        fft.process(&mut buf);
        let mut f = 0f32;
        for k in 0..nbins {
            let mag = buf[k].norm();
            let d = mag - prev[k];
            if d > 0.0 {
                f += d;
            }
            prev[k] = mag;
        }
        flux.push(f);
        pos += HOP;
    }
    if flux.len() < 4 {
        return (Vec::new(), 0.0);
    }

    // Normalize the onset envelope.
    let maxf = flux.iter().cloned().fold(0f32, f32::max);
    if maxf > 0.0 {
        for v in flux.iter_mut() {
            *v /= maxf;
        }
    }

    let frame_rate = sample_rate / HOP as f32; // onset frames per second

    // Tempo: autocorrelation peak within 60–180 BPM.
    let lag_min = ((60.0 * frame_rate / 180.0).round() as usize).max(1);
    let lag_max = (((60.0 * frame_rate / 60.0).round()) as usize)
        .max(lag_min + 1)
        .min(flux.len() - 1);
    let mut best_lag = lag_min;
    let mut best_corr = -1.0f32;
    for lag in lag_min..=lag_max {
        let mut s = 0f32;
        for i in lag..flux.len() {
            s += flux[i] * flux[i - lag];
        }
        if s > best_corr {
            best_corr = s;
            best_lag = lag;
        }
    }
    if best_corr <= 0.0 {
        return (Vec::new(), 0.0);
    }
    let bpm = 60.0 * frame_rate / best_lag as f32;

    // Phase: best offset in [0, period) maximizing pulse-train energy.
    let period = best_lag;
    let mut best_off = 0usize;
    let mut best_sum = -1.0f32;
    for off in 0..period {
        let mut s = 0f32;
        let mut k = off;
        while k < flux.len() {
            s += flux[k];
            k += period;
        }
        if s > best_sum {
            best_sum = s;
            best_off = off;
        }
    }

    // Emit a regular beat grid across the whole track.
    let mut beats = Vec::new();
    let mut k = best_off;
    while k < flux.len() {
        beats.push(k as f32 / frame_rate);
        k += period;
    }
    (beats, bpm)
}

// ---------------------------------------------------------------------------
// Upload state machine (cross-platform — only talks to SpacetimeDB).
// ---------------------------------------------------------------------------

pub fn drive_upload(
    mut up: NonSendMut<UploadState>,
    conn: NonSend<ConnResource>,
    mut app: ResMut<crate::state::AppState>,
) {
    let guard = conn.state.borrow();
    let ConnState::Connected(c) = &*guard else {
        return;
    };

    match up.phase() {
        // Decode finished: create the song-backed project (length = 2*beats).
        UploadPhase::Analyzing => {
            let Some(analysis) = up.incoming.borrow_mut().take() else {
                return;
            };
            let name = up.pending_name.borrow().clone();
            let lights = up.pending_lights.get();
            if let Err(e) = c
                .reducers()
                .create_project(name, lights, analysis.num_frames)
            {
                *up.phase.borrow_mut() = UploadPhase::Error(format!("{e}"));
                return;
            }
            up.project_id.set(0);
            up.pending = Some(analysis);
            *up.phase.borrow_mut() = UploadPhase::CreatingProject;
        }
        // Find the freshly created project, open it, and begin the upload.
        UploadPhase::CreatingProject => {
            let name = up.pending_name.borrow().clone();
            let Some(pending) = up.pending.as_ref() else {
                *up.phase.borrow_mut() = UploadPhase::Idle;
                return;
            };
            let me = c.try_identity();
            let project = c
                .db()
                .project()
                .iter()
                .filter(|p| {
                    Some(p.owner) == me
                        && p.name == name
                        && p.num_frames == pending.num_frames
                        && p.head_seq == 0
                })
                .max_by_key(|p| p.id);
            let Some(project) = project else {
                return; // not replicated yet
            };
            up.project_id.set(project.id);
            app.open_project = Some(project.id);
            app.current_frame = 0;
            app.history_pos = None;

            let byte_len = pending.bytes.len() as u64;
            let num_chunks = pending.bytes.len().div_ceil(CHUNK_SIZE).max(1) as u32;
            if let Err(e) = c.reducers().begin_song_upload(
                project.id,
                pending.name.clone(),
                pending.mime.clone(),
                byte_len,
                num_chunks,
                pending.duration_ms,
                pending.fps_used,
                pending.beats_frames.clone(),
                pending.bpm,
                pending.first_beat_ms,
            ) {
                *up.phase.borrow_mut() = UploadPhase::Error(format!("{e}"));
                return;
            }
            *up.phase.borrow_mut() = UploadPhase::Beginning;
        }
        UploadPhase::Beginning => {
            let project_id = up.project_id.get();
            let Some(pending) = up.pending.as_ref() else {
                *up.phase.borrow_mut() = UploadPhase::Idle;
                return;
            };
            let byte_len = pending.bytes.len() as u64;
            // The freshly-created (incomplete) song row for our project.
            let Some(song) = c
                .db()
                .song()
                .iter()
                .find(|s| s.project_id == project_id && s.byte_len == byte_len && !s.complete)
            else {
                return; // row not replicated yet; try again next frame
            };
            let song_id = song.id;
            let total = song.num_chunks;
            // Fire all chunks; the WebSocket is ordered and reliable, and each
            // chunk is far below the 32 MiB message limit.
            for idx in 0..total {
                let start = idx as usize * CHUNK_SIZE;
                let end = ((idx as usize + 1) * CHUNK_SIZE).min(pending.bytes.len());
                let chunk = pending.bytes[start..end].to_vec();
                let _ = c.reducers().append_song_chunk(song_id, idx, chunk);
            }
            *up.phase.borrow_mut() = UploadPhase::Sending { song_id, total };
        }
        UploadPhase::Sending { song_id, .. } => {
            match c.db().song().iter().find(|s| s.id == song_id) {
                Some(song) if song.complete => {
                    up.pending = None;
                    *up.phase.borrow_mut() = UploadPhase::Done;
                }
                None => {
                    up.pending = None;
                    *up.phase.borrow_mut() =
                        UploadPhase::Error("song removed during upload".to_string());
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Begin a pick → decode → analyze → create-project → upload cycle. The song
/// defines the new project: its length is `2 * num_beats` half-beat frames.
pub fn trigger_upload(up: &UploadState, name: String, num_lights: u32) {
    if up.is_busy() {
        return;
    }
    *up.pending_name.borrow_mut() = name;
    up.pending_lights.set(num_lights);
    up.project_id.set(0);
    *up.phase.borrow_mut() = UploadPhase::Picking;
    #[cfg(target_arch = "wasm32")]
    spawn_pick(up.phase.clone(), up.incoming.clone());
    #[cfg(not(target_arch = "wasm32"))]
    {
        *up.phase.borrow_mut() = UploadPhase::Error("audio upload is browser-only".to_string());
    }
}

/// Drive the playhead tempo from the open project's song: one frame is one
/// half-beat, so `fps = bpm / 30` (stored as `fps_used`). Replaces the old
/// user-set fps control.
pub fn sync_tempo(
    conn: NonSend<ConnResource>,
    app: Res<crate::state::AppState>,
    mut pb: ResMut<crate::state::Playback>,
) {
    let fps = {
        let guard = conn.state.borrow();
        let ConnState::Connected(c) = &*guard else {
            return;
        };
        let Some(pid) = app.open_project else {
            return;
        };
        let found = c
            .db()
            .song()
            .iter()
            .find(|s| s.project_id == pid)
            .map(|s| s.fps_used);
        found
    };
    if let Some(f) = fps {
        if f > 0.0 {
            pb.fps = f;
        }
    }
}

// ---------------------------------------------------------------------------
// wasm: file pick, Web Audio decode, and synced playback.
// ---------------------------------------------------------------------------

/// Upper bound on timeline frames (matches the module's `num_frames` clamp).
const MAX_FRAMES: u32 = 100_000;

#[cfg(target_arch = "wasm32")]
fn spawn_pick(phase: Rc<RefCell<UploadPhase>>, incoming: Rc<RefCell<Option<AudioAnalysis>>>) {
    wasm_bindgen_futures::spawn_local(async move {
        let file = rfd::AsyncFileDialog::new()
            .add_filter("audio", &["mp3", "wav", "ogg", "m4a", "flac", "aac"])
            .pick_file()
            .await;
        let Some(file) = file else {
            *phase.borrow_mut() = UploadPhase::Idle;
            return;
        };
        let name = file.file_name();
        let bytes = file.read().await;
        *phase.borrow_mut() = UploadPhase::Analyzing;
        match analyze(&bytes).await {
            Ok(analysis) => {
                let mut analysis = analysis;
                analysis.bytes = bytes;
                analysis.mime = guess_mime(&name);
                analysis.name = name;
                *incoming.borrow_mut() = Some(analysis);
                // phase stays Analyzing; drive_upload drains `incoming`.
            }
            Err(e) => *phase.borrow_mut() = UploadPhase::Error(e),
        }
    });
}

/// Decode the song, detect beats, and derive the half-beat timeline.
#[cfg(target_arch = "wasm32")]
async fn analyze(bytes: &[u8]) -> Result<AudioAnalysis, String> {
    let ctx = web_sys::AudioContext::new().map_err(|e| format!("AudioContext: {e:?}"))?;
    let buffer = decode_with_ctx(&ctx, bytes).await?;
    let _ = ctx.close();

    let sample_rate = buffer.sample_rate();
    let duration_ms = (buffer.duration() * 1000.0) as u32;
    let pcm = mono_pcm(&buffer)?;
    let (beat_secs, bpm) = detect_beats(&pcm, sample_rate);
    if beat_secs.is_empty() || bpm <= 0.0 {
        return Err("No beats detected in this audio".to_string());
    }
    Ok(build_analysis(duration_ms, &beat_secs, bpm))
}

/// Turn detected beats into the half-beat timeline definition.
pub fn build_analysis(duration_ms: u32, beat_secs: &[f32], bpm: f32) -> AudioAnalysis {
    let num_beats = beat_secs.len() as u32;
    // Two half-beat frames per beat; clamp to the module limit.
    let num_frames = (2 * num_beats).clamp(1, MAX_FRAMES);
    let n_on = (num_frames / 2).min(num_beats);
    let beats_frames: Vec<u32> = (0..n_on).map(|k| 2 * k).collect();
    let first_beat_ms = beat_secs.first().map(|s| (s * 1000.0) as u32).unwrap_or(0);
    let fps_used = if bpm > 0.0 { bpm / 30.0 } else { 1.0 };
    AudioAnalysis {
        bytes: Vec::new(),
        mime: String::new(),
        name: String::new(),
        duration_ms,
        beats_frames,
        bpm,
        fps_used,
        num_frames,
        first_beat_ms,
    }
}

#[cfg(target_arch = "wasm32")]
async fn decode_with_ctx(
    ctx: &web_sys::AudioContext,
    bytes: &[u8],
) -> Result<web_sys::AudioBuffer, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    // decodeAudioData detaches the ArrayBuffer, so hand it a fresh copy.
    let u8arr = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    u8arr.copy_from(bytes);
    let arraybuf = u8arr.buffer();
    let promise = ctx
        .decode_audio_data(&arraybuf)
        .map_err(|e| format!("decode_audio_data: {e:?}"))?;
    let decoded = JsFuture::from(promise)
        .await
        .map_err(|e| format!("decode failed: {e:?}"))?;
    decoded
        .dyn_into::<web_sys::AudioBuffer>()
        .map_err(|_| "decoded value was not an AudioBuffer".to_string())
}

#[cfg(target_arch = "wasm32")]
fn mono_pcm(buffer: &web_sys::AudioBuffer) -> Result<Vec<f32>, String> {
    let len = buffer.length() as usize;
    let nch = buffer.number_of_channels();
    let mut mono = vec![0f32; len];
    for ch in 0..nch {
        let data = buffer
            .get_channel_data(ch)
            .map_err(|e| format!("get_channel_data: {e:?}"))?;
        for (i, s) in data.iter().enumerate() {
            if i < len {
                mono[i] += *s;
            }
        }
    }
    if nch > 0 {
        let inv = 1.0 / nch as f32;
        for s in mono.iter_mut() {
            *s *= inv;
        }
    }
    Ok(mono)
}

#[cfg(target_arch = "wasm32")]
fn guess_mime(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" | "aac" => "audio/aac",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ----- Playback -----------------------------------------------------------

#[cfg(target_arch = "wasm32")]
#[derive(Default)]
pub struct AudioPlayback {
    pub ctx: Option<web_sys::AudioContext>,
    pub buffer: Option<web_sys::AudioBuffer>,
    pub buffer_song_id: Option<u64>,
    pub decoding_song_id: Option<u64>,
    pub incoming_buffer: Rc<RefCell<Option<web_sys::AudioBuffer>>>,
    pub source: Option<web_sys::AudioBufferSourceNode>,
    pub playing: bool,
    pub start_ctx_time: f64,
    pub start_frame: u32,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct AudioPlayback;

/// Returns whether a decoded, playable buffer is available for the open
/// project's song (used by the UI to enable the audio transport).
#[cfg(target_arch = "wasm32")]
pub fn has_playable_audio(audio: &AudioPlayback, song_id: Option<u64>) -> bool {
    audio.buffer.is_some() && audio.buffer_song_id == song_id && song_id.is_some()
}
#[cfg(not(target_arch = "wasm32"))]
pub fn has_playable_audio(_audio: &AudioPlayback, _song_id: Option<u64>) -> bool {
    false
}

/// Reassemble the open project's song chunks and decode them into a playable
/// `AudioBuffer` (once per song).
#[cfg(target_arch = "wasm32")]
pub fn ensure_audio_buffer(
    mut audio: NonSendMut<AudioPlayback>,
    conn: NonSend<ConnResource>,
    app: Res<crate::state::AppState>,
) {
    // Move a finished decode into place (drop the RefMut before mutating self).
    let decoded = audio.incoming_buffer.borrow_mut().take();
    if let Some(buf) = decoded {
        audio.buffer = Some(buf);
        audio.buffer_song_id = audio.decoding_song_id.take();
    }

    let guard = conn.state.borrow();
    let ConnState::Connected(c) = &*guard else {
        return;
    };
    let Some(pid) = app.open_project else {
        return;
    };
    let Some(song) = c
        .db()
        .song()
        .iter()
        .find(|s| s.project_id == pid && s.complete)
    else {
        return;
    };

    if audio.buffer_song_id == Some(song.id) || audio.decoding_song_id == Some(song.id) {
        return; // already have it / decoding it
    }

    // Gather and order all chunks for this song.
    let mut chunks: Vec<SongChunk> = c
        .db()
        .song_chunk()
        .iter()
        .filter(|ch| ch.song_id == song.id)
        .collect();
    if chunks.len() as u32 != song.num_chunks {
        return; // chunks still replicating
    }
    chunks.sort_by_key(|ch| ch.idx);
    let mut bytes = Vec::with_capacity(song.byte_len as usize);
    for ch in &chunks {
        bytes.extend_from_slice(&ch.data);
    }

    // Lazily create the persistent playback context, then decode on it.
    if audio.ctx.is_none() {
        match web_sys::AudioContext::new() {
            Ok(ctx) => audio.ctx = Some(ctx),
            Err(e) => {
                log::error!("AudioContext: {e:?}");
                return;
            }
        }
    }
    let ctx = audio.ctx.clone().unwrap();
    audio.decoding_song_id = Some(song.id);
    let sink = audio.incoming_buffer.clone();
    wasm_bindgen_futures::spawn_local(async move {
        match decode_with_ctx(&ctx, &bytes).await {
            Ok(buf) => *sink.borrow_mut() = Some(buf),
            Err(e) => log::error!("playback decode: {e}"),
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn ensure_audio_buffer() {}

/// Drive the playhead from the audio clock while playing, so lights lock to the
/// music. When audio is off/absent, `Playback.audio_driven` stays false and the
/// real-time `scene::playback_advance` runs instead.
#[cfg(target_arch = "wasm32")]
pub fn audio_playback_sync(
    mut audio: NonSendMut<AudioPlayback>,
    conn: NonSend<ConnResource>,
    mut app: ResMut<crate::state::AppState>,
    mut pb: ResMut<crate::state::Playback>,
) {
    pb.audio_driven = false;

    // The open project's complete song: its id and first-beat phase offset.
    let (song_id, first_beat_secs) = {
        let guard = conn.state.borrow();
        match &*guard {
            ConnState::Connected(c) => app
                .open_project
                .and_then(|pid| {
                    c.db()
                        .song()
                        .iter()
                        .find(|s| s.project_id == pid && s.complete)
                        .map(|s| (Some(s.id), s.first_beat_ms as f64 / 1000.0))
                })
                .unwrap_or((None, 0.0)),
            _ => (None, 0.0),
        }
    };

    let available = has_playable_audio(&audio, song_id);
    if !available {
        if audio.playing {
            stop_source(&mut audio);
        }
        return;
    }

    let want = pb.playing;
    if want && !audio.playing {
        start_source(&mut audio, app.current_frame, pb.fps, first_beat_secs);
    } else if !want && audio.playing {
        stop_source(&mut audio);
    }

    if audio.playing {
        pb.audio_driven = true;
        if let Some(ctx) = &audio.ctx {
            let elapsed = ctx.current_time() - audio.start_ctx_time;
            // Keep the unrounded position so the sub-frame remainder can drive
            // smooth fixture interpolation (see `PlayheadTime`).
            let frame_f = audio.start_frame as f64 + elapsed * pb.fps as f64;
            let frame = frame_f.floor();
            let nf = {
                let guard = conn.state.borrow();
                match &*guard {
                    ConnState::Connected(c) => app
                        .open_project
                        .and_then(|pid| c.db().project().iter().find(|p| p.id == pid))
                        .map(|p| p.num_frames)
                        .unwrap_or(1),
                    _ => 1,
                }
            };
            if frame >= nf as f64 {
                if pb.looping {
                    // Restart the source from the top for a seamless loop.
                    stop_source(&mut audio);
                    app.current_frame = 0;
                    pb.audio_fraction = 0.0;
                    start_source(&mut audio, 0, pb.fps, first_beat_secs);
                } else {
                    app.current_frame = nf.saturating_sub(1);
                    pb.audio_fraction = 0.0;
                    pb.playing = false;
                    stop_source(&mut audio);
                }
            } else {
                app.current_frame = frame as u32;
                pb.audio_fraction = frame_f.fract() as f32;
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn start_source(audio: &mut AudioPlayback, frame: u32, fps: f32, first_beat_secs: f64) {
    let (Some(ctx), Some(buffer)) = (audio.ctx.clone(), audio.buffer.clone()) else {
        return;
    };
    let _ = ctx.resume(); // browsers start contexts suspended until a gesture
    let Ok(src) = ctx.create_buffer_source() else {
        return;
    };
    src.set_buffer(Some(&buffer));
    let _ = src.connect_with_audio_node(&ctx.destination());
    // Frame f maps to audio time = first_beat + f * half_beat (half_beat = 1/fps).
    let offset = (first_beat_secs + frame as f64 / fps.max(0.1) as f64).max(0.0);
    let _ = src.start_with_when_and_grain_offset(0.0, offset);
    audio.source = Some(src);
    audio.playing = true;
    audio.start_ctx_time = ctx.current_time();
    audio.start_frame = frame;
}

#[cfg(target_arch = "wasm32")]
#[allow(deprecated)]
fn stop_source(audio: &mut AudioPlayback) {
    if let Some(src) = audio.source.take() {
        let _ = src.stop();
    }
    audio.playing = false;
}

#[cfg(not(target_arch = "wasm32"))]
pub fn audio_playback_sync() {}
