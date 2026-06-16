//! One-shot host tool: import the legacy rusty-halloween shows into SpacetimeDB
//! as read-only "sample" template projects.
//!
//! The legacy `instructions-exported.json` is already a sparse half-beat keyframe
//! grid (timestamps step in `30000/bpm` ms; each frame lists only the fixtures
//! that changed), which maps almost 1:1 onto this editor's keyframe model. We
//! quantise each timestamp to the editor's half-beat frame index and emit:
//!   - light on/off -> `Edit` keyframes (via `seed_light_edits`)
//!   - lasers / gobo projector / turrets -> dedicated keyframe rows
//!   - the mp3 + beat grid -> a `Song` with chunked audio
//!
//! Reducers can't return ids, so each show is addressed by a stable `source_key`
//! (its folder name); `seed_project` makes re-runs idempotent.
//!
//! Usage: `cargo run -- [shows_dir]`
//!   shows_dir defaults to `rusty-halloween/shows/test` (relative to repo root).

mod module_bindings;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use spacetimedb_sdk::{DbContext, Identity};

use module_bindings::*;

const HOST: &str = "https://maincloud.spacetimedb.com";
const DB_NAME: &str = "stdb-lightshow-spike";

/// Must match the module's `CHUNK_SIZE`.
const CHUNK_SIZE: usize = 256 * 1024;

/// Legacy laser pattern names (index == pattern id), ported verbatim from
/// rusty-halloween `src/show/show.rs`. Used only to label keyframes; the actual
/// geometry comes from each frame's `points`.
const LASER_PATTERNS: &[&str] = &[
    "bat",
    "bow",
    "bow_slow",
    "candy",
    "circle",
    "circle_slow",
    "clockwise_spiral_slow",
    "counterclockwise_spiral_slow",
    "crescent",
    "ghost",
    "gravestone_cross",
    "hexagon",
    "hexagon_slow",
    "horizontal_lines_left_to_right_slow",
    "horizontal_lines_right_to_left_slow",
    "lightning_bolt",
    "octagon",
    "octagon_slow",
    "parallelogram",
    "parallelogram_slow",
    "pentagon",
    "pentagon_slow",
    "pentagram",
    "pentagram_slow",
    "pumpkin",
    "septagon_slow",
    "square_large",
    "square_large_slow",
    "square_small",
    "square_small_slow",
    "star",
    "star_slow",
    "triangle_large",
    "triangle_large_slow",
    "triangle_small",
    "triangle_small_slow",
    "vertical_lines_bottom_to_top_slow",
    "vertical_lines_top_to_bottom_slow",
];

/// Everything we extract from one legacy show directory, ready to seed.
struct Converted {
    /// Project name; also the idempotency key the seed reducers address.
    name: String,
    num_lights: u32,
    num_frames: u32,
    bpm: f32,
    duration_ms: u32,
    beats_frames: Vec<u32>,
    lights: Vec<LightEditInput>,
    lasers: Vec<LaserKeyframeInput>,
    projectors: Vec<ProjectorKeyframeInput>,
    turrets: Vec<TurretKeyframeInput>,
    mp3_path: PathBuf,
}

fn main() -> Result<()> {
    let shows_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rusty-halloween/shows/test"));

    println!("Reading shows from {}", shows_dir.display());
    let mut shows: Vec<Converted> = Vec::new();
    for entry in std::fs::read_dir(&shows_dir)
        .with_context(|| format!("reading shows dir {}", shows_dir.display()))?
    {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        match convert_show(&path) {
            Ok(c) => {
                println!(
                    "  parsed {:<55} {} lights, {} laser, {} proj, {} turret keyframes, {} frames",
                    c.name,
                    c.lights.len(),
                    c.lasers.len(),
                    c.projectors.len(),
                    c.turrets.len(),
                    c.num_frames,
                );
                shows.push(c);
            }
            Err(e) => println!("  SKIP {}: {e:#}", path.display()),
        }
    }
    if shows.is_empty() {
        bail!("no shows found under {}", shows_dir.display());
    }
    shows.sort_by(|a, b| a.name.cmp(&b.name));

    println!("\nConnecting to {DB_NAME} at {HOST} …");
    let conn = connect()?;

    for show in &shows {
        seed_show(&conn, show)?;
    }

    println!("\nDone. Seeded {} sample shows.", shows.len());
    conn.disconnect().ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

fn convert_show(dir: &Path) -> Result<Converted> {
    let meta: Value = read_json(&dir.join("metadata.json"))?;
    let bpm = meta["tempo"]
        .as_f64()
        .ok_or_else(|| anyhow!("metadata.json: missing tempo"))? as f32;
    if bpm <= 0.0 {
        bail!("non-positive tempo");
    }
    let duration_s = meta["duration"].as_f64().unwrap_or(0.0);
    let frame_period_ms = 30_000.0 / bpm as f64;
    let to_frame = |ms: f64| -> u32 { (ms / frame_period_ms).round().max(0.0) as u32 };

    let instructions: Value = read_json(&dir.join("instructions-exported.json"))?;
    let obj = instructions
        .as_object()
        .ok_or_else(|| anyhow!("instructions-exported.json: not an object"))?;

    let mut lights = Vec::new();
    let mut lasers = Vec::new();
    let mut projectors = Vec::new();
    let mut turrets = Vec::new();
    let mut max_frame = 0u32;

    for (ts, frame) in obj {
        if ts == "song" {
            continue;
        }
        let ms: f64 = match ts.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let f = to_frame(ms);
        max_frame = max_frame.max(f);
        let Some(devices) = frame.as_object() else {
            continue;
        };

        for (dev, val) in devices {
            if let Some(n) = dev.strip_prefix("light-") {
                if let Ok(idx) = n.parse::<u32>() {
                    let on = val.as_f64().unwrap_or(0.0) > 0.0;
                    lights.push(LightEditInput {
                        light: idx - 1,
                        frame: f,
                        state: if on { 1 } else { 0 },
                    });
                }
            } else if let Some(n) = dev.strip_prefix("laser-") {
                if let Ok(idx) = n.parse::<u8>() {
                    if let Some(kf) = parse_laser(f, idx - 1, val) {
                        lasers.push(kf);
                    }
                }
            } else if let Some(n) = dev.strip_prefix("lp-") {
                if let Ok(idx) = n.parse::<u8>() {
                    projectors.push(ProjectorKeyframeInput {
                        frame: f,
                        channel: idx - 1,
                        state: u8field(val, "state"),
                        gallery: u8field(val, "gallery"),
                        pattern: u8field(val, "pattern"),
                        colour: u8field(val, "colour"),
                    });
                }
            } else if let Some(n) = dev.strip_prefix("turret-") {
                if let Ok(idx) = n.parse::<u8>() {
                    turrets.push(TurretKeyframeInput {
                        frame: f,
                        channel: idx - 1,
                        state: u8field(val, "state"),
                        pan: u8field(val, "pan"),
                        tilt: u8field(val, "tilt"),
                    });
                }
            }
        }
    }

    let dur_frames = to_frame(duration_s * 1000.0) + 1;
    let num_frames = dur_frames.max(max_frame + 1).clamp(1, 100_000);

    let beats_frames: Vec<u32> = meta["beats"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.as_f64())
                .map(|s| to_frame(s * 1000.0))
                .filter(|&f| f < num_frames)
                .collect()
        })
        .unwrap_or_default();

    let mp3_path = find_mp3(dir)?;

    Ok(Converted {
        name: prettify(dir),
        num_lights: 7,
        num_frames,
        bpm,
        duration_ms: (duration_s * 1000.0) as u32,
        beats_frames,
        lights,
        lasers,
        projectors,
        turrets,
        mp3_path,
    })
}

/// Parse one `laser-N` value into a keyframe. Numeric value = blank/reset
/// keyframe; an object with a `config` carries a drawn path. An object without
/// `config` is ignored (matches the legacy parser).
fn parse_laser(frame: u32, channel: u8, val: &Value) -> Option<LaserKeyframeInput> {
    if val.is_number() {
        return Some(LaserKeyframeInput {
            frame,
            channel,
            enable: false,
            pattern: 0,
            points: Vec::new(),
        });
    }
    val.get("config")?; // skip objects without a config (as legacy does)

    let points = val
        .get("points")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    let get = |i: usize| a.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
                    Some(LaserPoint {
                        x: get(0) as i16,
                        y: get(1) as i16,
                        r: get(2).clamp(0, 255) as u8,
                        g: get(3).clamp(0, 255) as u8,
                        b: get(4).clamp(0, 255) as u8,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let pattern = val
        .get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.replace('-', "_"))
        .and_then(|name| LASER_PATTERNS.iter().position(|&p| p == name))
        .unwrap_or(0) as u8;

    Some(LaserKeyframeInput {
        frame,
        channel,
        enable: true,
        pattern,
        points,
    })
}

fn u8field(val: &Value, key: &str) -> u8 {
    val.get(key).and_then(|v| v.as_u64()).unwrap_or(0).min(255) as u8
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?)
}

fn find_mp3(dir: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().and_then(|e| e.to_str()) == Some("mp3") {
            return Ok(p);
        }
    }
    bail!("no .mp3 in {}", dir.display())
}

/// Human-friendly project name from the folder name.
fn prettify(dir: &Path) -> String {
    dir.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("show")
        .replace('_', " ")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Seeding (reducer calls, each awaited so ordering is guaranteed)
// ---------------------------------------------------------------------------

fn seed_show(conn: &DbConnection, s: &Converted) -> Result<()> {
    println!("\nSeeding {} …", s.name);
    let key = s.name.clone();

    await_reducer("seed_project", |tx| {
        conn.reducers()
            .seed_project_then(key.clone(), s.num_lights, s.num_frames, reducer_cb(tx))
    })?;

    await_reducer("seed_light_edits", |tx| {
        conn.reducers()
            .seed_light_edits_then(key.clone(), s.lights.clone(), reducer_cb(tx))
    })?;

    await_reducer("seed_laser_keyframes", |tx| {
        conn.reducers()
            .seed_laser_keyframes_then(key.clone(), s.lasers.clone(), reducer_cb(tx))
    })?;

    await_reducer("seed_projector_keyframes", |tx| {
        conn.reducers().seed_projector_keyframes_then(
            key.clone(),
            s.projectors.clone(),
            reducer_cb(tx),
        )
    })?;

    await_reducer("seed_turret_keyframes", |tx| {
        conn.reducers()
            .seed_turret_keyframes_then(key.clone(), s.turrets.clone(), reducer_cb(tx))
    })?;

    // Audio.
    let bytes = std::fs::read(&s.mp3_path)
        .with_context(|| format!("reading {}", s.mp3_path.display()))?;
    let byte_len = bytes.len() as u64;
    let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE).collect();
    let num_chunks = chunks.len() as u32;
    let fps_used = s.bpm / 30.0;

    await_reducer("seed_song", |tx| {
        conn.reducers().seed_song_then(
            key.clone(),
            s.mp3_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("audio.mp3")
                .to_string(),
            "audio/mpeg".to_string(),
            byte_len,
            num_chunks,
            s.duration_ms,
            fps_used,
            s.beats_frames.clone(),
            s.bpm,
            0, // first_beat_ms: frame f maps linearly to f * (30000/bpm) ms of audio
            reducer_cb(tx),
        )
    })?;

    for (idx, chunk) in chunks.iter().enumerate() {
        let data = chunk.to_vec();
        await_reducer("seed_song_chunk", |tx| {
            conn.reducers()
                .seed_song_chunk_then(key.clone(), idx as u32, data.clone(), reducer_cb(tx))
        })?;
    }
    println!(
        "  uploaded {} ({:.1} MiB in {} chunks)",
        s.mp3_path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        byte_len as f64 / (1024.0 * 1024.0),
        num_chunks,
    );
    Ok(())
}

type ReducerResult = Result<Result<(), String>, spacetimedb_sdk::__codegen::InternalError>;

/// Build a reducer-completion callback that forwards the flattened result.
fn reducer_cb(
    tx: mpsc::Sender<Result<(), String>>,
) -> impl FnOnce(&ReducerEventContext, ReducerResult) + Send + 'static {
    move |_ctx, res| {
        let flat = match res {
            Ok(inner) => inner,
            Err(e) => Err(format!("internal: {e}")),
        };
        let _ = tx.send(flat);
    }
}

/// Invoke a reducer (via the supplied closure) and block until it completes.
fn await_reducer<F>(label: &str, call: F) -> Result<()>
where
    F: FnOnce(mpsc::Sender<Result<(), String>>) -> spacetimedb_sdk::Result<()>,
{
    let (tx, rx) = mpsc::channel();
    call(tx).with_context(|| format!("sending {label}"))?;
    match rx.recv_timeout(Duration::from_secs(300)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => bail!("{label} failed: {e}"),
        Err(e) => bail!("{label} timed out: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

fn connect() -> Result<DbConnection> {
    let (tx, rx) = mpsc::channel::<Result<Identity, String>>();
    let on_ok = tx.clone();
    let conn = DbConnection::builder()
        .with_uri(HOST)
        .with_database_name(DB_NAME)
        .on_connect(move |_conn, identity, _token| {
            let _ = on_ok.send(Ok(identity));
        })
        .on_connect_error(move |_ctx, err| {
            let _ = tx.send(Err(err.to_string()));
        })
        .build()
        .context("building connection")?;

    conn.run_threaded();

    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(identity)) => {
            println!("Connected as {}", identity.to_hex());
            Ok(conn)
        }
        Ok(Err(e)) => bail!("connect error: {e}"),
        Err(e) => bail!("connection timed out: {e}"),
    }
}
