//! The editor UI, drawn as a bevy_egui overlay. Ported from the original eframe
//! app; the only structural change is that panels attach to an `egui::Context`
//! (`.show(ctx, …)`) instead of a root `Ui`. Pure timeline logic lives in
//! `logic.rs` and is reused unchanged.

use std::collections::HashSet;

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

use crate::conn::{ConnResource, ConnState};
use crate::logic::{expand_held, fold_keyframes, short_id, state_label};
use crate::module_bindings::*;
use crate::state::{AppState, Playback};
use spacetimedb_sdk::{DbContext, Table};

const COL_ON: Color32 = Color32::from_rgb(255, 206, 84);
const COL_OFF: Color32 = Color32::from_rgb(38, 38, 48);
const COL_KEYFRAME: Color32 = Color32::from_rgb(255, 255, 255);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(90, 200, 250);

/// The single egui system (runs in `EguiPrimaryContextPass`).
pub fn ui_system(
    mut contexts: EguiContexts,
    conn: NonSend<ConnResource>,
    mut app: ResMut<AppState>,
    mut playback: ResMut<Playback>,
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
            ui_connected(ctx, c, &mut app, &mut playback);
        }
    }
    Ok(())
}

fn ui_connected(ctx: &egui::Context, conn: &DbConnection, app: &mut AppState, playback: &mut Playback) {
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
        Some(project) => editor_ui(ctx, conn, app, playback, &project),
        None => {
            app.open_project = None;
            project_list_ui(ctx, conn, app, &projects);
        }
    }
}

fn project_list_ui(ctx: &egui::Context, conn: &DbConnection, app: &mut AppState, projects: &[Project]) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label("New project");
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut app.new_name);
            });
            ui.horizontal(|ui| {
                ui.label("Lights");
                ui.add(egui::DragValue::new(&mut app.new_lights).range(1..=64));
                ui.label("Frames");
                ui.add(egui::DragValue::new(&mut app.new_frames).range(1..=2000));
                if ui.button("➕ Create").clicked() {
                    if let Err(e) =
                        conn.reducers()
                            .create_project(app.new_name.clone(), app.new_lights, app.new_frames)
                    {
                        app.last_error = Some(format!("{e}"));
                    }
                }
            });
        });

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Your projects").strong());
        ui.separator();

        if projects.is_empty() {
            ui.weak("No projects yet — create one above. (New projects appear here in real time.)");
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
                        "{} lights × {} frames · {} edits",
                        p.num_lights, p.num_frames, p.head_seq
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

    // ---- Frame scrubber + per-frame light toggles ----
    egui::TopBottomPanel::top("framebar").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&project.name).strong());
            ui.weak(format!("({nl} lights × {nf} frames)"));
        });
        ui.horizontal(|ui| {
            let play_label = if playback.playing { "⏸" } else { "▶" };
            if ui.button(play_label).on_hover_text("Play / pause").clicked() {
                playback.playing = !playback.playing;
            }
            ui.label("Frame");
            ui.add(egui::Slider::new(&mut app.current_frame, 0..=nf.saturating_sub(1)));
            ui.separator();
            ui.label("fps");
            ui.add(egui::DragValue::new(&mut playback.fps).range(1.0..=60.0));
            ui.checkbox(&mut playback.looping, "loop");
        });
        let cf = app.current_frame;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Lights on at frame {cf}:"));
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
        ui.weak("Grid: filled = on. White dot = keyframe. Click a cell to cycle none → on → off.  (3D viewport above shows the lights.)");
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

        if let Some(e) = &app.last_error {
            ui.add_space(6.0);
            ui.colored_label(Color32::LIGHT_RED, e);
        }
    });
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
