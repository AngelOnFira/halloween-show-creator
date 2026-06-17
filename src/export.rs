//! "Export show": assemble the open project into a rusty-halloween show `.zip`
//! entirely client-side, then trigger a browser download.
//!
//! The output mirrors the on-disk packages under `rusty-halloween/shows/<name>/`
//! so an exported show plays in rusty-halloween (`UnloadedShow::load_show_file`,
//! `rusty-halloween/src/show/show.rs`) and round-trips back through our own
//! importer (`tools/show-seeder`). This module is the inverse of that importer.
//!
//! A package is one `<show_name>/` folder containing:
//!   - `instructions-exported.json` — sparse keyframe grid, keyed by ms-as-string
//!   - `metadata.json`              — title / duration / tempo / beats
//!   - `<show_name>.<ext>`          — the original audio bytes
//!
//! Everything is read from the already-replicated SpacetimeDB client cache; no
//! server round-trip is involved.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use spacetimedb_sdk::{DbContext, Table};

// Glob to pull in the per-table accessor traits (`SongTableAccess`, …) that
// provide `conn.db().song()` etc., alongside the row types themselves.
use crate::module_bindings::*;
use crate::patterns::PATTERN_NAMES;

/// Reader device-count caps (rusty-halloween `MAX_*`). Lights MUST be clamped:
/// the reader indexes `lights[index-1]` after an `index <= MAX_LIGHTS` check, so
/// `light-8` would panic.
const MAX_LIGHTS: u32 = 7;
const MAX_LASERS: u8 = 5;
const MAX_TURRETS: u8 = 4;

/// The fully-gathered, ready-to-zip payload for one project.
pub struct ExportData {
    /// Sanitized folder + zip base name.
    pub show_name: String,
    /// Audio file extension (e.g. `mp3`).
    pub audio_ext: String,
    pub audio_bytes: Vec<u8>,
    pub instructions: Value,
    pub metadata: Value,
}

/// Gather the open project, assemble the zip, and (on wasm) trigger a download.
/// Returns the zip filename on success, or a user-facing message on failure.
pub fn export_open_project(conn: &DbConnection, project: &Project) -> Result<String, String> {
    let data = gather(conn, project)?;
    let zip = build_zip(&data)?;
    let filename = format!("{}.zip", data.show_name);
    #[cfg(target_arch = "wasm32")]
    trigger_download(&zip, &filename)?;
    // On native (dev `cargo check`/window) there is no browser to hand the bytes
    // to; building the zip is still exercised so the codepath stays compiled.
    #[cfg(not(target_arch = "wasm32"))]
    let _ = zip;
    Ok(filename)
}

/// Read everything for `project` out of the client cache and turn it into the
/// two JSON documents plus the reassembled audio.
fn gather(conn: &DbConnection, project: &Project) -> Result<ExportData, String> {
    let pid = project.id;

    // --- Song is mandatory: both the audio file and the frame<->ms timing need it.
    let song: Song = conn
        .db()
        .song()
        .iter()
        .find(|s| s.project_id == pid)
        .ok_or("This project has no song yet. Upload audio before exporting.")?;
    if !song.complete || song.chunks_received < song.num_chunks {
        return Err("Audio is still uploading — wait until it finishes, then export.".into());
    }
    if song.bpm <= 0.0 {
        return Err("Song has no tempo, so timing can't be computed.".into());
    }

    // --- Reassemble audio (mirrors audio::ensure_audio_buffer).
    let mut chunks: Vec<SongChunk> = conn
        .db()
        .song_chunk()
        .iter()
        .filter(|ch| ch.song_id == song.id)
        .collect();
    if chunks.len() as u32 != song.num_chunks {
        return Err("Audio is still downloading to this client. Try again in a moment.".into());
    }
    chunks.sort_by_key(|ch| ch.idx);
    let mut audio_bytes = Vec::with_capacity(song.byte_len as usize);
    for ch in &chunks {
        audio_bytes.extend_from_slice(&ch.data);
    }

    // --- Frame -> ms is the exact inverse of the importer's
    //     `frame = round(ms / (30000/bpm))` (tools/show-seeder/src/main.rs).
    let bpm = song.bpm as f64;
    let first_beat_ms = song.first_beat_ms as f64;
    let frame_to_ms = move |f: u32| -> i64 {
        (first_beat_ms + (f as f64) * 30_000.0 / bpm).round() as i64
    };

    // --- Lights: fold the append-only edit log into explicit keyframes.
    let edits: Vec<Edit> = conn
        .db()
        .edit_log()
        .iter()
        .filter(|e| e.project_id == pid)
        .collect();
    let light_kf = crate::logic::fold_keyframes(&edits, project.head_seq);

    // --- Rich fixtures.
    let lasers: Vec<LaserKeyframe> = conn
        .db()
        .laser_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    let projectors: Vec<ProjectorKeyframe> = conn
        .db()
        .projector_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    let turrets: Vec<TurretKeyframe> = conn
        .db()
        .turret_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();

    let instructions =
        build_instructions(&light_kf, &lasers, &projectors, &turrets, &frame_to_ms);

    let show_name = sanitize_name(&project.name);
    let audio_ext = audio_ext(&song);
    let metadata = build_metadata(&show_name, &project.name, &song);

    Ok(ExportData {
        show_name,
        audio_ext,
        audio_bytes,
        instructions,
        metadata,
    })
}

/// Build the `instructions-exported.json` object: one ms-keyed entry per frame
/// that carries any keyframe, merging every device at that frame together.
fn build_instructions(
    light_kf: &std::collections::HashMap<(u32, u32), bool>,
    lasers: &[LaserKeyframe],
    projectors: &[ProjectorKeyframe],
    turrets: &[TurretKeyframe],
    frame_to_ms: impl Fn(u32) -> i64,
) -> Value {
    // Group by frame first so devices at the same frame share one ms key.
    let mut by_frame: BTreeMap<u32, Map<String, Value>> = BTreeMap::new();

    // Lights: DB light is 0-indexed -> JSON `light-{n+1}`; clamp to 1..=7.
    for (&(light, frame), &on) in light_kf {
        if light >= MAX_LIGHTS {
            continue;
        }
        by_frame
            .entry(frame)
            .or_default()
            .insert(format!("light-{}", light + 1), json!(if on { 1 } else { 0 }));
    }

    // Lasers: channel 0..=4 -> `laser-{c+1}`.
    for kf in lasers {
        if kf.channel >= MAX_LASERS {
            continue;
        }
        by_frame
            .entry(kf.frame)
            .or_default()
            .insert(format!("laser-{}", kf.channel + 1), laser_value(kf));
    }

    // Projector: only channel 0 ("lp-1"). All four DMX keys required.
    for kf in projectors {
        if kf.channel != 0 {
            continue;
        }
        by_frame.entry(kf.frame).or_default().insert(
            "lp-1".to_string(),
            json!({
                "state": kf.state,
                "gallery": kf.gallery,
                "pattern": kf.pattern,
                "colour": kf.colour,
            }),
        );
    }

    // Turrets: channel 0..=3 -> `turret-{c+1}`. All three keys required.
    for kf in turrets {
        if kf.channel >= MAX_TURRETS {
            continue;
        }
        by_frame.entry(kf.frame).or_default().insert(
            format!("turret-{}", kf.channel + 1),
            json!({
                "state": kf.state,
                "pan": kf.pan,
                "tilt": kf.tilt,
            }),
        );
    }

    // Convert frame -> ms-string key. Distinct frames can collide on the same ms
    // after rounding; merge their device maps defensively.
    let mut out = Map::new();
    for (frame, devices) in by_frame {
        let key = frame_to_ms(frame).max(0).to_string();
        match out.get_mut(&key) {
            Some(Value::Object(existing)) => {
                for (k, v) in devices {
                    existing.insert(k, v);
                }
            }
            _ => {
                out.insert(key, Value::Object(devices));
            }
        }
    }
    Value::Object(out)
}

/// One laser device entry: the number `0` for a reset/blank keyframe, or the
/// full `{config, points, hex, value}` object the reader requires.
fn laser_value(kf: &LaserKeyframe) -> Value {
    // Reset / blanked: a numeric value tells the reader (and our importer) "off".
    if !kf.enable || kf.points.is_empty() {
        return json!(0);
    }
    let points: Vec<Value> = kf
        .points
        .iter()
        .map(|p| json!([p.x, p.y, p.r, p.g, p.b]))
        .collect();
    // Pattern index -> name, hyphenated to match the existing show files (the
    // reader normalizes `-`->`_` either way). Guard a stray index so we never
    // emit a `value` the reader's lookup would reject.
    let value = PATTERN_NAMES
        .get(kf.pattern as usize)
        .copied()
        .unwrap_or("circle")
        .replace('_', "-");
    json!({
        // We don't model a per-keyframe speed profile; 1 matches the seeds.
        "config": { "speed-profile": 1, "home": false },
        "points": points,
        "hex": collapse_hex(kf.cr, kf.cg, kf.cb),
        "value": value,
    })
}

/// Collapse a 3-channel (0..=7) tint to a legacy `hex` string with EXACTLY one
/// `'f'` — the reader panics otherwise. Dominant channel wins (R > G > B on a
/// tie); an all-zero tint defaults to red so it stays valid.
fn collapse_hex(cr: u8, cg: u8, cb: u8) -> String {
    let (r, g, b) = (cr, cg, cb);
    if r == 0 && g == 0 && b == 0 {
        return "f00".to_string();
    }
    if r >= g && r >= b {
        "f00".to_string()
    } else if g >= b {
        "0f0".to_string()
    } else {
        "00f".to_string()
    }
}

/// Build `metadata.json` from the song row (tempo/beats/duration).
fn build_metadata(show_name: &str, title: &str, song: &Song) -> Value {
    let bpm = song.bpm as f64;
    // beats are stored as frame indices; seconds = frame * 30/bpm (a frame is a
    // half-beat, so 30/bpm seconds, not 60/bpm).
    let beats: Vec<f64> = song
        .beats_frames
        .iter()
        .map(|&f| (f as f64) * 30.0 / bpm)
        .collect();
    json!({
        "filename": format!("songs/{0}/{0}.mp3", show_name),
        "title": title,
        "channel": "",
        "duration": song.duration_ms / 1000,
        "tempo": song.bpm,
        "beats": beats,
    })
}

/// Filesystem- and reader-safe folder/zip base name derived from the project
/// name (the reader derives the show name from the parent folder).
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "show".to_string()
    } else {
        cleaned
    }
}

/// Original audio extension, from the uploaded filename or its MIME type.
fn audio_ext(song: &Song) -> String {
    if let Some(ext) = std::path::Path::new(&song.name)
        .extension()
        .and_then(|e| e.to_str())
    {
        if !ext.is_empty() {
            return ext.to_ascii_lowercase();
        }
    }
    match song.mime.as_str() {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/aac" | "audio/mp4" => "m4a",
        _ => "mp3",
    }
    .to_string()
}

/// Pack the three files into an in-memory zip. Audio and JSON are stored
/// uncompressed (`Stored`): the mp3 is already compressed and STORE keeps the
/// `zip` dependency free of any native codec, so it builds cleanly for wasm.
fn build_zip(data: &ExportData) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Write};
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    let mut zip = ZipWriter::new(Cursor::new(Vec::<u8>::new()));
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let folder = &data.show_name;

    let instructions =
        serde_json::to_vec_pretty(&data.instructions).map_err(|e| e.to_string())?;
    zip.start_file(format!("{folder}/instructions-exported.json"), opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(&instructions).map_err(|e| e.to_string())?;

    let metadata = serde_json::to_vec_pretty(&data.metadata).map_err(|e| e.to_string())?;
    zip.start_file(format!("{folder}/metadata.json"), opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(&metadata).map_err(|e| e.to_string())?;

    zip.start_file(format!("{folder}/{folder}.{}", data.audio_ext), opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(&data.audio_bytes).map_err(|e| e.to_string())?;

    let cursor = zip.finish().map_err(|e| e.to_string())?;
    Ok(cursor.into_inner())
}

/// Hand the zip bytes to the browser as a download via an object-URL `<a>` click.
#[cfg(target_arch = "wasm32")]
fn trigger_download(bytes: &[u8], filename: &str) -> Result<(), String> {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use web_sys::{Blob, HtmlAnchorElement, Url};

    let array = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    array.copy_from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let blob =
        Blob::new_with_u8_array_sequence(&parts).map_err(|e| format!("blob: {e:?}"))?;
    let url =
        Url::create_object_url_with_blob(&blob).map_err(|e| format!("object url: {e:?}"))?;

    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let anchor: HtmlAnchorElement = document
        .create_element("a")
        .map_err(|e| format!("create <a>: {e:?}"))?
        .dyn_into()
        .map_err(|_| "element is not an anchor")?;
    anchor.set_href(&url);
    anchor.set_download(filename);

    // The anchor must be in the document for Chrome to reliably *finalize* the
    // download (otherwise it can stall as an `.crdownload`). Append, click,
    // then remove it again.
    let body = document.body().ok_or("no document body")?;
    body.append_child(&anchor)
        .map_err(|e| format!("append <a>: {e:?}"))?;
    anchor.click();
    let _ = body.remove_child(&anchor);

    // Revoke the object URL only *after* the browser has read the blob. Revoking
    // synchronously interrupts the transfer — the download stalls at 100% — so
    // defer it to a later task.
    let revoke = Closure::once_into_js(move || {
        let _ = Url::revoke_object_url(&url);
    });
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        revoke.unchecked_ref(),
        60_000,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_bindings::LaserPoint;
    use std::collections::HashMap;

    fn laser(enable: bool, pattern: u8, color: (u8, u8, u8), pts: usize) -> LaserKeyframe {
        LaserKeyframe {
            id: 0,
            project_id: 1,
            frame: 0,
            channel: 0,
            enable,
            pattern,
            cr: color.0,
            cg: color.1,
            cb: color.2,
            points: (0..pts)
                .map(|i| LaserPoint {
                    x: i as i16,
                    y: i as i16,
                    r: 7,
                    g: 0,
                    b: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn hex_has_exactly_one_f() {
        assert_eq!(collapse_hex(7, 0, 0), "f00");
        assert_eq!(collapse_hex(0, 5, 0), "0f0");
        assert_eq!(collapse_hex(0, 0, 3), "00f");
        assert_eq!(collapse_hex(0, 0, 0), "f00"); // all-zero stays valid
        assert_eq!(collapse_hex(4, 4, 1), "f00"); // R>=G tie -> red
        for s in [
            collapse_hex(1, 2, 3),
            collapse_hex(7, 7, 7),
            collapse_hex(0, 0, 1),
        ] {
            assert_eq!(s.len(), 3);
            assert_eq!(s.chars().filter(|&c| c == 'f').count(), 1);
            assert!(s.chars().all(|c| c == '0' || c == 'f'));
        }
    }

    #[test]
    fn disabled_or_empty_laser_is_reset_number() {
        assert_eq!(laser_value(&laser(false, 4, (7, 0, 0), 4)), json!(0));
        assert_eq!(laser_value(&laser(true, 4, (7, 0, 0), 0)), json!(0));
    }

    #[test]
    fn enabled_laser_is_object_with_required_keys() {
        let v = laser_value(&laser(true, 4, (0, 7, 0), 3));
        assert!(v.get("config").is_some());
        assert_eq!(v["points"].as_array().unwrap().len(), 3);
        assert_eq!(v["hex"], json!("0f0"));
        assert_eq!(v["value"], json!("circle")); // pattern 4 == "circle"
        assert_eq!(v["points"][0], json!([0, 0, 7, 0, 0]));
    }

    #[test]
    fn devices_at_same_frame_merge_under_one_ms_key() {
        let mut lights = HashMap::new();
        lights.insert((0u32, 2u32), true); // -> light-1
        lights.insert((6u32, 2u32), false); // -> light-7
        lights.insert((7u32, 2u32), true); // light-8: clamped out
        let lasers = vec![{
            let mut l = laser(true, 4, (7, 0, 0), 2);
            l.channel = 1;
            l.frame = 2;
            l
        }];
        // Constant frame->ms so frame 2 -> "2".
        let v = build_instructions(&lights, &lasers, &[], &[], |f| f as i64);
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 1, "all keyframes share frame 2 -> one ms key");
        let frame = obj["2"].as_object().unwrap();
        assert_eq!(frame["light-1"], json!(1));
        assert_eq!(frame["light-7"], json!(0));
        assert!(frame.get("light-8").is_none(), "light index clamped to 7");
        assert!(frame.get("laser-2").is_some());
    }

    #[test]
    fn frame_to_ms_inverts_importer_rounding() {
        let bpm = 154.0_f64;
        let period = 30_000.0 / bpm;
        let frame_to_ms = |f: u32| -> i64 { ((f as f64) * period).round() as i64 };
        let to_frame = |ms: f64| -> u32 { (ms / period).round() as u32 };
        for f in [0u32, 1, 7, 42, 1000] {
            assert_eq!(to_frame(frame_to_ms(f) as f64), f);
        }
    }

    #[test]
    fn sanitize_makes_safe_folder_names() {
        assert_eq!(sanitize_name("Spooky Scary Skeletons"), "Spooky_Scary_Skeletons");
        assert_eq!(sanitize_name("  a/b:c  "), "a_b_c");
        assert_eq!(sanitize_name("***"), "show");
    }
}
