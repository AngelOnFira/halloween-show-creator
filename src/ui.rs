//! The editor UI, drawn as a bevy_egui overlay. Ported from the original eframe
//! app; the only structural change is that panels attach to an `egui::Context`
//! (`.show(ctx, …)`) instead of a root `Ui`. Pure timeline logic lives in
//! `logic.rs` and is reused unchanged.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::audio::{self, has_playable_audio, AudioPlayback, UploadPhase, UploadState};
use crate::conn::{ConnResource, ConnState};
use crate::logic::{expand_held, fold_keyframes, short_id, state_label};
use crate::module_bindings::*;
use crate::state::{AppState, Playback};
use spacetimedb_sdk::{DbContext, Table};

const COL_ON: Color32 = Color32::from_rgb(255, 206, 84);
const COL_OFF: Color32 = Color32::from_rgb(38, 38, 48);
const COL_KEYFRAME: Color32 = Color32::from_rgb(255, 255, 255);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(90, 200, 250);
const COL_BEAT: Color32 = Color32::from_rgb(80, 160, 150);
const COL_DOWNBEAT: Color32 = Color32::from_rgb(120, 230, 200);

/// The single egui system (runs in `EguiPrimaryContextPass`).
pub fn ui_system(
    mut contexts: EguiContexts,
    conn: NonSend<ConnResource>,
    mut app: ResMut<AppState>,
    mut playback: ResMut<Playback>,
    upload: NonSend<UploadState>,
    audio: NonSend<AudioPlayback>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let guard = conn.state.borrow();
    match &*guard {
        ConnState::Connecting => {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting to SpacetimeDB…");
                });
            });
        }
        ConnState::Failed(e) => {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(Color32::LIGHT_RED, format!("Connection failed:\n{e}"));
                });
            });
        }
        ConnState::Connected(c) => {
            ui_connected(ctx, c, &mut app, &mut playback, &upload, &audio);
        }
    }
    Ok(())
}

fn ui_connected(
    ctx: &egui::Context,
    conn: &DbConnection,
    app: &mut AppState,
    playback: &mut Playback,
    upload: &UploadState,
    audio: &AudioPlayback,
) {
    let me = conn.try_identity();

    let mut projects: Vec<Project> = conn
        .db()
        .project()
        .iter()
        .filter(|p| me.as_ref() == Some(&p.owner))
        .collect();
    projects.sort_by_key(|p| p.id);

    egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if app.open_project.is_some() {
                if ui.button("← Projects").clicked() {
                    app.open_project = None;
                    app.history_pos = None;
                }
                ui.separator();
            }
            ui.heading("🎚 Light Show Editor");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("id {}", short_id(me.as_ref())));
                ui.colored_label(Color32::from_rgb(120, 220, 120), "● live");
            });
        });
    });

    let open = app
        .open_project
        .and_then(|pid| projects.iter().find(|p| p.id == pid).cloned());

    match open {
        Some(project) => editor_ui(ctx, conn, app, playback, &project, upload, audio),
        None => {
            app.open_project = None;
            project_list_ui(ctx, app, &projects, upload);
        }
    }
}

fn project_list_ui(
    ctx: &egui::Context,
    app: &mut AppState,
    projects: &[Project],
    upload: &UploadState,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("New light show — built around a song").strong());
            ui.weak("Upload a song; beat detection sets the timeline to its length (2 frames per beat: on-beat & off-beat).");
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut app.new_name);
                ui.label("Lights");
                ui.add(egui::DragValue::new(&mut app.new_lights).range(1..=64));
            });
            ui.horizontal(|ui| {
                match upload.phase() {
                    UploadPhase::Idle | UploadPhase::Done | UploadPhase::Error(_) => {
                        if ui.button("🎵 Choose song & create").clicked() {
                            let name = app.new_name.trim().to_string();
                            if name.is_empty() {
                                app.last_error = Some("Name cannot be empty".to_string());
                            } else {
                                app.last_error = None;
                                audio::trigger_upload(upload, name, app.new_lights);
                            }
                        }
                    }
                    UploadPhase::Picking => {
                        ui.add_enabled(false, egui::Button::new("Choosing file…"));
                    }
                    UploadPhase::Analyzing => {
                        ui.add(egui::Spinner::new());
                        ui.label("Decoding & detecting beats…");
                    }
                    UploadPhase::CreatingProject | UploadPhase::Beginning => {
                        ui.add(egui::Spinner::new());
                        ui.label("Creating project…");
                    }
                    UploadPhase::Sending { .. } => {
                        ui.add(egui::Spinner::new());
                        ui.label("Uploading song…");
                    }
                }
            });
            if let UploadPhase::Error(e) = upload.phase() {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        });

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Your light shows").strong());
        ui.separator();

        if projects.is_empty() {
            ui.weak("None yet — create one above.");
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for p in projects {
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() {
                        app.open_project = Some(p.id);
                        app.current_frame = 0;
                        app.history_pos = None;
                    }
                    ui.label(egui::RichText::new(&p.name).strong());
                    ui.weak(format!(
                        "{} lights · {} beats · {} edits",
                        p.num_lights,
                        p.num_frames / 2,
                        p.head_seq
                    ));
                });
            }
        });

        if let Some(e) = &app.last_error {
            ui.add_space(8.0);
            ui.colored_label(Color32::LIGHT_RED, e);
        }
    });
}

fn editor_ui(
    ctx: &egui::Context,
    conn: &DbConnection,
    app: &mut AppState,
    playback: &mut Playback,
    project: &Project,
    upload: &UploadState,
    audio: &AudioPlayback,
) {
    let nl = project.num_lights;
    let nf = project.num_frames;
    if app.current_frame >= nf {
        app.current_frame = nf.saturating_sub(1);
    }

    let mut edits: Vec<Edit> = conn
        .db()
        .edit_log()
        .iter()
        .filter(|e| e.project_id == project.id)
        .collect();
    edits.sort_by_key(|e| e.seq);

    let head = project.head_seq;
    let cutoff = app.history_pos.unwrap_or(head).min(head);
    let viewing_history = app.history_pos.is_some_and(|p| p < head);

    let keyframes = fold_keyframes(&edits, cutoff);
    let held = expand_held(&keyframes, nl, nf);

    // The project's song (may still be uploading). Beats/markers come from the
    // stored on-beat frame list; playback needs a decoded buffer (`audio_ready`).
    let song: Option<Song> = conn.db().song().iter().find(|s| s.project_id == project.id);
    let beats: Vec<u32> = song.as_ref().map(|s| s.beats_frames.clone()).unwrap_or_default();
    let song_complete = song.as_ref().map(|s| s.complete).unwrap_or(false);
    let song_id = song.as_ref().map(|s| s.id);
    let audio_ready = has_playable_audio(audio, if song_complete { song_id } else { None });
    let nbeats = nf / 2;

    // ---- Transport + song info (one half-beat per frame) ----
    egui::TopBottomPanel::top("framebar").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(&project.name).strong());
            ui.weak(format!("{nl} lights · {nbeats} beats"));
            if let Some(s) = &song {
                ui.separator();
                ui.weak(format!(
                    "♪ {} · {} · {:.0} BPM",
                    s.name,
                    fmt_ms(s.duration_ms),
                    s.bpm
                ));
                if !s.complete {
                    if let UploadPhase::Sending { .. } = upload.phase() {
                        let total = s.num_chunks.max(1);
                        ui.add(
                            egui::ProgressBar::new(s.chunks_received as f32 / total as f32)
                                .desired_width(140.0)
                                .text(format!("uploading {}/{}", s.chunks_received, total)),
                        );
                    } else {
                        ui.weak("(uploading…)");
                    }
                }
            }
        });

        ui.horizontal(|ui| {
            let play_label = if playback.playing { "⏸" } else { "▶" };
            if ui.button(play_label).on_hover_text("Play / pause").clicked() {
                playback.playing = !playback.playing;
            }
            if ui.small_button("⏮").on_hover_text("Previous beat").clicked() {
                if let Some(f) = neighbor_beat(&beats, app.current_frame, false) {
                    app.current_frame = f;
                    playback.playing = false;
                }
            }
            if ui.small_button("⏭").on_hover_text("Next beat").clicked() {
                if let Some(f) = neighbor_beat(&beats, app.current_frame, true) {
                    app.current_frame = f;
                    playback.playing = false;
                }
            }
            ui.label("Beat");
            ui.add(
                egui::Slider::new(&mut app.current_frame, 0..=nf.saturating_sub(1)).show_value(false),
            );
            ui.monospace(beat_label(app.current_frame));
            ui.checkbox(&mut playback.looping, "loop");
            if audio_ready {
                ui.colored_label(COL_DOWNBEAT, "♪ synced");
            } else if song_complete {
                ui.weak("decoding audio…");
            }
        });

        let cf = app.current_frame;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Toggle lights at beat {}:", beat_label(cf)));
            for l in 0..nl {
                let on = held[l as usize][cf as usize];
                let is_kf = keyframes.contains_key(&(l, cf));
                let mut btn = egui::Button::new(format!("{l}{}", if is_kf { "•" } else { "" }))
                    .min_size(Vec2::new(34.0, 28.0));
                if on {
                    btn = btn.fill(COL_ON);
                }
                if ui.add(btn).clicked() && !viewing_history {
                    let new_state = if on { 0u8 } else { 1u8 };
                    send_edit(conn, app, project.id, l, cf, new_state);
                }
            }
        });

        // ---- Auto-generate on beats ----
        if !beats.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label("Auto-generate:");
                egui::ComboBox::from_id_salt("autogen_pattern")
                    .selected_text(pattern_label(app.autogen_pattern))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut app.autogen_pattern, 0, pattern_label(0));
                        ui.selectable_value(&mut app.autogen_pattern, 1, pattern_label(1));
                        ui.selectable_value(&mut app.autogen_pattern, 2, pattern_label(2));
                    });
                let gen =
                    ui.add_enabled(!viewing_history, egui::Button::new("✨ Generate on beats"));
                if gen.clicked() {
                    let pat = app.autogen_pattern;
                    autogen_on_beats(conn, app, project, &beats, pat);
                }
            });
        }
        ui.add_space(2.0);
    });

    // ---- History / time-travel panel ----
    egui::SidePanel::right("history")
        .resizable(true)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("History (time travel)").strong());
            ui.separator();

            let mut pos = app.history_pos.unwrap_or(head);
            let resp = ui.add_enabled(head > 0, egui::Slider::new(&mut pos, 0..=head).text("edit"));
            if resp.changed() {
                app.history_pos = if pos >= head { None } else { Some(pos) };
            }

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(viewing_history, egui::Button::new("⟲ Live"))
                    .clicked()
                {
                    app.history_pos = None;
                }
                if ui
                    .add_enabled(viewing_history, egui::Button::new("Restore this version"))
                    .on_hover_text("Append edits so the live show matches this point in history")
                    .clicked()
                {
                    restore_to(conn, app, project, &edits, cutoff);
                    app.history_pos = None;
                }
            });
            if viewing_history {
                ui.colored_label(
                    Color32::from_rgb(240, 200, 80),
                    format!("Viewing edit {cutoff}/{head} (read-only)"),
                );
            } else {
                ui.weak(format!("At latest ({head} edits)"));
            }

            ui.separator();
            ui.label("Recent edits:");
            egui::ScrollArea::vertical().show(ui, |ui| {
                for e in edits.iter().rev().take(40) {
                    let marker = if e.seq == cutoff { "▶ " } else { "  " };
                    let txt = format!(
                        "{marker}#{}  L{} F{}  {}",
                        e.seq,
                        e.light,
                        e.frame,
                        state_label(e.state)
                    );
                    if ui.selectable_label(e.seq == cutoff, txt).clicked() {
                        app.history_pos = if e.seq >= head { None } else { Some(e.seq) };
                    }
                }
            });
        });

    // ---- Timeline grid (bottom panel; the 3D viewport shows through the
    // uncovered center of the screen) ----
    egui::TopBottomPanel::bottom("timeline")
        .resizable(true)
        .default_height(230.0)
        .show(ctx, |ui| {
            ui.weak("Each column is a half-beat — teal lines mark the beat (the column after each line is the off-beat). Click a cell to cycle none → on → off.");
            draw_frame_grid(ui, conn, app, playback, project, &held, &keyframes, &beats, viewing_history);
            if let Some(e) = &app.last_error {
                ui.add_space(6.0);
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        });
}

/// Frame-resolution grid (one column per frame), with beat markers overlaid.
#[allow(clippy::too_many_arguments)]
fn draw_frame_grid(
    ui: &mut egui::Ui,
    conn: &DbConnection,
    app: &mut AppState,
    playback: &mut Playback,
    project: &Project,
    held: &[Vec<bool>],
    keyframes: &HashMap<(u32, u32), bool>,
    beats: &[u32],
    viewing_history: bool,
) {
    let nl = project.num_lights;
    let nf = project.num_frames;
    let cell = Vec2::new(16.0, 18.0);
    let label_w = 42.0_f32;
    let total = Vec2::new(label_w + nf as f32 * cell.x, nl as f32 * cell.y);

    egui::ScrollArea::both().show(ui, |ui| {
        let (rect, resp) = ui.allocate_exact_size(total, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let origin = rect.min;

        // Drag anywhere on the grid to scrub the playhead (and pause).
        if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                let lx = p.x - origin.x;
                if lx >= label_w {
                    let f = (((lx - label_w) / cell.x) as i64).clamp(0, nf as i64 - 1);
                    app.current_frame = f as u32;
                    playback.playing = false;
                }
            }
        }

        for l in 0..nl {
            painter.text(
                Pos2::new(origin.x + 4.0, origin.y + l as f32 * cell.y + cell.y * 0.5),
                egui::Align2::LEFT_CENTER,
                format!("L{l}"),
                egui::FontId::monospace(11.0),
                Color32::GRAY,
            );
            for f in 0..nf {
                let cmin = Pos2::new(
                    origin.x + label_w + f as f32 * cell.x,
                    origin.y + l as f32 * cell.y,
                );
                let crect = Rect::from_min_size(cmin, cell - Vec2::splat(1.0));
                let on = held[l as usize][f as usize];
                painter.rect_filled(crect, 2.0_f32, if on { COL_ON } else { COL_OFF });
                if keyframes.contains_key(&(l, f)) {
                    painter.circle_filled(crect.center(), 2.5, COL_KEYFRAME);
                }
            }
        }

        // Beat markers (thin vertical lines; downbeats brighter).
        for (i, &bf) in beats.iter().enumerate() {
            if bf < nf {
                let bx = origin.x + label_w + bf as f32 * cell.x;
                let col = if i % 4 == 0 { COL_DOWNBEAT } else { COL_BEAT };
                painter.vline(bx, rect.y_range(), Stroke::new(1.0_f32, col));
            }
        }

        let px = origin.x + label_w + app.current_frame as f32 * cell.x + cell.x * 0.5;
        painter.vline(px, rect.y_range(), Stroke::new(2.0_f32, COL_PLAYHEAD));

        if resp.clicked() && !viewing_history {
            if let Some(p) = resp.interact_pointer_pos() {
                let lx = p.x - origin.x;
                let ly = p.y - origin.y;
                if lx >= label_w {
                    let f = ((lx - label_w) / cell.x) as i64;
                    let l = (ly / cell.y) as i64;
                    if (0..nf as i64).contains(&f) && (0..nl as i64).contains(&l) {
                        let (l, f) = (l as u32, f as u32);
                        let next = match keyframes.get(&(l, f)).copied() {
                            None => 1u8,
                            Some(true) => 0u8,
                            Some(false) => 2u8,
                        };
                        send_edit(conn, app, project.id, l, f, next);
                    }
                }
            }
        }
    });
}

fn pattern_label(p: u8) -> &'static str {
    match p {
        0 => "Strobe all",
        1 => "Chase",
        _ => "Alternate",
    }
}

/// Human label for a half-beat frame: beat number, with "½" for off-beats.
fn beat_label(frame: u32) -> String {
    let b = frame / 2;
    if frame % 2 == 0 {
        format!("{b}")
    } else {
        format!("{b}½")
    }
}

/// Format milliseconds as `m:ss`.
fn fmt_ms(ms: u32) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The previous / next beat frame relative to `f`.
fn neighbor_beat(beats: &[u32], f: u32, forward: bool) -> Option<u32> {
    if forward {
        beats.iter().copied().find(|b| *b > f)
    } else {
        beats.iter().rev().copied().find(|b| *b < f)
    }
}

/// Generate light keyframes on every detected beat, per the chosen pattern.
/// Every edit is a normal `append_edit`, so the whole batch is undoable.
fn autogen_on_beats(
    conn: &DbConnection,
    app: &mut AppState,
    project: &Project,
    beats: &[u32],
    pattern: u8,
) {
    let nl = project.num_lights;
    let nf = project.num_frames;
    for (i, &bf) in beats.iter().enumerate() {
        let next_beat = beats.get(i + 1).copied().unwrap_or(nf).min(nf);
        match pattern {
            // Strobe: every light flashes on each beat, off halfway to the next.
            0 => {
                let off_at = (bf + (next_beat.saturating_sub(bf)) / 2).min(nf.saturating_sub(1));
                for l in 0..nl {
                    send_edit(conn, app, project.id, l, bf, 1);
                    if off_at > bf {
                        send_edit(conn, app, project.id, l, off_at, 0);
                    }
                }
            }
            // Chase: one light per beat, cycling through them.
            1 => {
                let l = (i as u32) % nl;
                send_edit(conn, app, project.id, l, bf, 1);
                let off_at = next_beat.min(nf.saturating_sub(1));
                if off_at > bf {
                    send_edit(conn, app, project.id, l, off_at, 0);
                }
            }
            // Alternate: parity of (light + beat) decides on/off.
            _ => {
                for l in 0..nl {
                    let on = (l + i as u32) % 2 == 0;
                    send_edit(conn, app, project.id, l, bf, if on { 1 } else { 0 });
                }
            }
        }
    }
}

fn send_edit(conn: &DbConnection, app: &mut AppState, project_id: u64, light: u32, frame: u32, state: u8) {
    if let Err(e) = conn.reducers().append_edit(project_id, light, frame, state) {
        app.last_error = Some(format!("{e}"));
    }
}

/// Append edits so the live state's keyframes match those at `cutoff`.
fn restore_to(conn: &DbConnection, app: &mut AppState, project: &Project, edits: &[Edit], cutoff: u64) {
    let target = fold_keyframes(edits, cutoff);
    let current = fold_keyframes(edits, project.head_seq);
    let mut keys: HashSet<(u32, u32)> = current.keys().copied().collect();
    keys.extend(target.keys().copied());
    for (l, f) in keys {
        let want = target.get(&(l, f)).copied();
        let have = current.get(&(l, f)).copied();
        if want == have {
            continue;
        }
        let state = match want {
            Some(true) => 1u8,
            Some(false) => 0u8,
            None => 2u8,
        };
        send_edit(conn, app, project.id, l, f, state);
    }
}
