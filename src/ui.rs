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
use crate::logic::{
    apply_pending, expand_fixture_tracks, expand_held, fold_keyframes, short_id, state_label,
};
use crate::module_bindings::*;
use crate::state::{
    AppState, Clipboard, DragKind, FixtureEditor, FixtureKind, GridSelection, PendingFixture,
    Playback, TimelineFilter, Viewport3dRect,
};
use spacetimedb_sdk::{DbContext, Table};

const COL_ON: Color32 = Color32::from_rgb(255, 206, 84);
const COL_OFF: Color32 = Color32::from_rgb(38, 38, 48);
const COL_KEYFRAME: Color32 = Color32::from_rgb(255, 255, 255);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(90, 200, 250);
const COL_BEAT: Color32 = Color32::from_rgb(80, 160, 150);
const COL_DOWNBEAT: Color32 = Color32::from_rgb(120, 230, 200);
// "On" colours for the read-only fixture rows (lasers / gobo projector / turrets)
// shown beneath the light rows in the timeline.
const COL_LASER: Color32 = Color32::from_rgb(120, 230, 120);
const COL_PROJ: Color32 = Color32::from_rgb(220, 130, 235);
const COL_TURRET: Color32 = Color32::from_rgb(120, 180, 250);
// Ghost overlay (paste preview / in-hand region): white, so it reads clearly
// against the coloured machine rows.
const COL_GHOST_FILL: Color32 = Color32::from_rgba_premultiplied(150, 150, 150, 150);
const COL_GHOST_EDGE: Color32 = Color32::from_rgb(245, 245, 245);

/// A blueprint's light pattern, ready to ghost onto the grid at `base_frame`.
struct BlueprintPreview {
    base_frame: u32,
    num_frames: u32,
    /// `held[light][frame]` for the blueprint's own (normalized) frames.
    held: Vec<Vec<bool>>,
}

/// A read-only timeline row for a non-light fixture channel (laser / projector /
/// turret): its per-frame held on/off state and the frames carrying a keyframe.
struct FixtureTrack {
    label: String,
    held: Vec<bool>,
    keyframes: Vec<u32>,
    on_color: Color32,
    kind: FixtureKind,
    channel: u8,
}

/// Fixed device counts: every project shows the same rig regardless of whether
/// a given channel currently has any keyframes.
const N_LASER: usize = 5;
const N_TURRET: usize = 4;
const N_PROJECTOR: usize = 1;

/// Timeline cell size and beat-ruler height (shared by the grid and the panel
/// height calc so the panel is exactly tall enough for every row).
const CELL: f32 = 18.0;
const RULER_H: f32 = 16.0;

/// Build the fixture rows for a project — always all channels, in a fixed order
/// (lasers, then turrets, then the laser projector) so the row layout is stable
/// and matches the category filter. Empty channels render as all-off rows.
fn build_fixture_tracks(
    conn: &DbConnection,
    project_id: u64,
    nf: u32,
    pending: &[PendingFixture],
) -> Vec<FixtureTrack> {
    let mut tracks = Vec::new();

    let mut lasers: Vec<LaserKeyframe> = conn
        .db()
        .laser_kf()
        .iter()
        .filter(|r| r.project_id == project_id)
        .collect();
    for pf in pending {
        if let PendingFixture::Laser(p) = pf {
            if p.project_id == project_id {
                lasers.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                lasers.push(p.clone());
            }
        }
    }
    for (ch, (held, kfs)) in
        expand_fixture_tracks(&lasers, N_LASER, nf, |r| r.channel, |r| r.frame, |r| {
            r.enable && !r.points.is_empty()
        })
        .into_iter()
        .enumerate()
    {
        tracks.push(FixtureTrack {
            label: format!("Laser {}", ch + 1),
            held,
            keyframes: kfs,
            on_color: COL_LASER,
            kind: FixtureKind::Laser,
            channel: ch as u8,
        });
    }

    let mut turrets: Vec<TurretKeyframe> = conn
        .db()
        .turret_kf()
        .iter()
        .filter(|r| r.project_id == project_id)
        .collect();
    for pf in pending {
        if let PendingFixture::Turret(p) = pf {
            if p.project_id == project_id {
                turrets.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                turrets.push(p.clone());
            }
        }
    }
    for (ch, (held, kfs)) in
        expand_fixture_tracks(&turrets, N_TURRET, nf, |r| r.channel, |r| r.frame, |r| r.state > 0)
            .into_iter()
            .enumerate()
    {
        tracks.push(FixtureTrack {
            label: format!("Turret {}", ch + 1),
            held,
            keyframes: kfs,
            on_color: COL_TURRET,
            kind: FixtureKind::Turret,
            channel: ch as u8,
        });
    }

    let mut projectors: Vec<ProjectorKeyframe> = conn
        .db()
        .projector_kf()
        .iter()
        .filter(|r| r.project_id == project_id)
        .collect();
    for pf in pending {
        if let PendingFixture::Projector(p) = pf {
            if p.project_id == project_id {
                projectors.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                projectors.push(p.clone());
            }
        }
    }
    for (held, kfs) in expand_fixture_tracks(&projectors, N_PROJECTOR, nf, |r| r.channel, |r| {
        r.frame
    }, |r| r.state > 0)
    {
        tracks.push(FixtureTrack {
            label: "Laser Projector".to_string(),
            held,
            keyframes: kfs,
            on_color: COL_PROJ,
            kind: FixtureKind::Projector,
            channel: 0,
        });
    }

    tracks
}

/// 3-bit (0..=7) channel → 0..=255 for the colour picker.
fn expand3(c: u8) -> u8 {
    ((c.min(7) as f32 / 7.0) * 255.0).round() as u8
}
/// 0..=255 → 3-bit (0..=7) for storage.
fn quant3(c: u8) -> u8 {
    ((c as f32 / 255.0) * 7.0).round() as u8
}

/// Has the backend echoed a row matching this pending fixture edit? (Compares
/// the key fields, not `points`, since the server fills those itself.)
fn fixture_echoed(conn: &DbConnection, project_id: u64, pf: &PendingFixture) -> bool {
    match pf {
        PendingFixture::Laser(p) => conn.db().laser_kf().iter().any(|r| {
            r.project_id == project_id
                && r.channel == p.channel
                && r.frame == p.frame
                && r.enable == p.enable
                && r.pattern == p.pattern
                && r.cr == p.cr
                && r.cg == p.cg
                && r.cb == p.cb
        }),
        PendingFixture::Turret(p) => conn.db().turret_kf().iter().any(|r| {
            r.project_id == project_id
                && r.channel == p.channel
                && r.frame == p.frame
                && r.state == p.state
                && r.pan == p.pan
                && r.tilt == p.tilt
        }),
        PendingFixture::Projector(p) => conn.db().projector_kf().iter().any(|r| {
            r.project_id == project_id
                && r.channel == p.channel
                && r.frame == p.frame
                && r.state == p.state
                && r.gallery == p.gallery
                && r.pattern == p.pattern
                && r.colour == p.colour
        }),
    }
}

/// Drop spliced fixture edits whose matching row has arrived from the backend.
fn clear_echoed_fixtures(conn: &DbConnection, app: &mut AppState, project_id: u64) {
    if app.pending_fixtures.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut app.pending_fixtures);
    app.pending_fixtures = pending
        .into_iter()
        .filter(|pf| !fixture_echoed(conn, project_id, pf))
        .collect();
}

/// Build a `FixtureEditor` pre-loaded from the keyframe in effect at
/// `(channel, frame)` for the given fixture (held semantics: latest row ≤ frame).
fn load_fixture_editor(
    conn: &DbConnection,
    project_id: u64,
    kind: FixtureKind,
    channel: u8,
    frame: u32,
    pos: Pos2,
) -> FixtureEditor {
    let mut ed = FixtureEditor {
        kind,
        channel,
        frame,
        pos: (pos.x, pos.y),
        laser_enable: true,
        laser_pattern: 0,
        laser_color: [255, 255, 255],
        turret_on: true,
        turret_pan: 128,
        turret_tilt: 128,
        proj_on: true,
        proj_gallery: 0,
        proj_pattern: 0,
        proj_colour: 0,
        proj_pattern_filter: String::new(),
    };
    match kind {
        FixtureKind::Laser => {
            if let Some(r) = conn
                .db()
                .laser_kf()
                .iter()
                .filter(|r| r.project_id == project_id && r.channel == channel && r.frame <= frame)
                .max_by_key(|r| r.frame)
            {
                ed.laser_enable = r.enable;
                ed.laser_pattern = r.pattern;
                ed.laser_color = [expand3(r.cr), expand3(r.cg), expand3(r.cb)];
            }
        }
        FixtureKind::Turret => {
            if let Some(r) = conn
                .db()
                .turret_kf()
                .iter()
                .filter(|r| r.project_id == project_id && r.channel == channel && r.frame <= frame)
                .max_by_key(|r| r.frame)
            {
                ed.turret_on = r.state > 0;
                ed.turret_pan = r.pan;
                ed.turret_tilt = r.tilt;
            }
        }
        FixtureKind::Projector => {
            if let Some(r) = conn
                .db()
                .projector_kf()
                .iter()
                .filter(|r| r.project_id == project_id && r.channel == channel && r.frame <= frame)
                .max_by_key(|r| r.frame)
            {
                ed.proj_on = r.state > 0;
                ed.proj_gallery = r.gallery;
                ed.proj_pattern = r.pattern;
                ed.proj_colour = r.colour;
            }
        }
    }
    ed
}

/// Send the editor's draft to the backend and splice it in for instant feedback.
fn apply_fixture_edit(conn: &DbConnection, app: &mut AppState, project_id: u64, ed: &FixtureEditor) {
    let r = conn.reducers();
    match ed.kind {
        FixtureKind::Laser => {
            let cr = quant3(ed.laser_color[0]);
            let cg = quant3(ed.laser_color[1]);
            let cb = quant3(ed.laser_color[2]);
            if let Err(e) = r.set_laser_keyframe(
                project_id,
                ed.frame,
                ed.channel,
                ed.laser_enable,
                ed.laser_pattern,
                cr,
                cg,
                cb,
            ) {
                app.last_error = Some(format!("{e}"));
                return;
            }
            let points = crate::patterns::get(ed.laser_pattern)
                .map(|p| {
                    p.points
                        .iter()
                        .map(|pt| LaserPoint { x: pt.x, y: pt.y, r: cr, g: cg, b: cb })
                        .collect()
                })
                .unwrap_or_default();
            app.pending_fixtures.push(PendingFixture::Laser(LaserKeyframe {
                id: 0,
                project_id,
                frame: ed.frame,
                channel: ed.channel,
                enable: ed.laser_enable,
                pattern: ed.laser_pattern,
                cr,
                cg,
                cb,
                points,
            }));
        }
        FixtureKind::Turret => {
            let state = if ed.turret_on { 255 } else { 0 };
            if let Err(e) =
                r.set_turret_keyframe(project_id, ed.frame, ed.channel, state, ed.turret_pan, ed.turret_tilt)
            {
                app.last_error = Some(format!("{e}"));
                return;
            }
            app.pending_fixtures.push(PendingFixture::Turret(TurretKeyframe {
                id: 0,
                project_id,
                frame: ed.frame,
                channel: ed.channel,
                state,
                pan: ed.turret_pan,
                tilt: ed.turret_tilt,
            }));
        }
        FixtureKind::Projector => {
            let state = if ed.proj_on { 255 } else { 0 };
            if let Err(e) = r.set_projector_keyframe(
                project_id,
                ed.frame,
                ed.channel,
                state,
                ed.proj_gallery,
                ed.proj_pattern,
                ed.proj_colour,
            ) {
                app.last_error = Some(format!("{e}"));
                return;
            }
            app.pending_fixtures.push(PendingFixture::Projector(ProjectorKeyframe {
                id: 0,
                project_id,
                frame: ed.frame,
                channel: ed.channel,
                state,
                gallery: ed.proj_gallery,
                pattern: ed.proj_pattern,
                colour: ed.proj_colour,
            }));
        }
    }
}

/// The floating right-click fixture editor window (laser / turret / projector).
fn fixture_editor_window(
    ctx: &egui::Context,
    conn: &DbConnection,
    app: &mut AppState,
    project_id: u64,
    locked: bool,
) {
    let Some(mut ed) = app.fixture_editor.take() else {
        return;
    };
    let mut open = true;
    let mut apply = false;
    let title = match ed.kind {
        FixtureKind::Laser => format!("Laser {} · beat {}", ed.channel + 1, beat_label(ed.frame)),
        FixtureKind::Turret => format!("Turret {} · beat {}", ed.channel + 1, beat_label(ed.frame)),
        FixtureKind::Projector => format!("Laser Projector · beat {}", beat_label(ed.frame)),
    };
    egui::Window::new(title)
        .fixed_pos(egui::pos2(ed.pos.0, ed.pos.1))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| match ed.kind {
            FixtureKind::Laser => {
                ui.checkbox(&mut ed.laser_enable, "Enabled");
                ui.horizontal(|ui| {
                    ui.label("Colour");
                    ui.color_edit_button_srgb(&mut ed.laser_color);
                });
                let tint = [
                    quant3(ed.laser_color[0]),
                    quant3(ed.laser_color[1]),
                    quant3(ed.laser_color[2]),
                ];
                // Live preview.
                let (prect, _) = ui.allocate_exact_size(egui::vec2(130.0, 130.0), Sense::hover());
                ui.painter_at(prect).rect_filled(prect, 4.0, COL_OFF);
                if let Some(pat) = crate::patterns::get(ed.laser_pattern) {
                    crate::patterns::paint_pattern(
                        &ui.painter_at(prect),
                        prect.shrink(8.0),
                        &pat.points,
                        Some(tint),
                    );
                }
                ui.label(
                    crate::patterns::get(ed.laser_pattern)
                        .map(|p| p.name)
                        .unwrap_or(""),
                );
                // Pattern picker grid.
                egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for pat in crate::patterns::library() {
                            let (r, resp) =
                                ui.allocate_exact_size(egui::vec2(46.0, 46.0), Sense::click());
                            let selected = ed.laser_pattern == pat.id;
                            let p = ui.painter_at(r);
                            p.rect_filled(r, 3.0, if selected { Color32::from_gray(64) } else { COL_OFF });
                            crate::patterns::paint_pattern(&p, r.shrink(5.0), &pat.points, Some([7, 7, 7]));
                            if selected {
                                p.rect_stroke(r, 3.0, Stroke::new(1.5, COL_PLAYHEAD), egui::StrokeKind::Inside);
                            }
                            if resp.on_hover_text(pat.name).clicked() {
                                ed.laser_pattern = pat.id;
                            }
                        }
                    });
                });
                if ui.button("Apply").clicked() {
                    apply = true;
                }
            }
            FixtureKind::Turret => {
                ui.checkbox(&mut ed.turret_on, "On");
                ui.label("Aim (drag):");
                let (r, resp) =
                    ui.allocate_exact_size(egui::vec2(130.0, 130.0), Sense::click_and_drag());
                let p = ui.painter_at(r);
                p.rect_filled(r, 4.0, COL_OFF);
                if let Some(pos) = resp.interact_pointer_pos() {
                    if resp.clicked() || resp.dragged() {
                        ed.turret_pan = (((pos.x - r.min.x) / r.width()).clamp(0.0, 1.0) * 255.0) as u8;
                        ed.turret_tilt = (((pos.y - r.min.y) / r.height()).clamp(0.0, 1.0) * 255.0) as u8;
                    }
                }
                let tip = egui::pos2(
                    r.min.x + (ed.turret_pan as f32 / 255.0) * r.width(),
                    r.min.y + (ed.turret_tilt as f32 / 255.0) * r.height(),
                );
                p.line_segment([r.center_top(), tip], Stroke::new(2.0, COL_TURRET));
                p.circle_filled(tip, 4.0, COL_TURRET);
                ui.add(egui::Slider::new(&mut ed.turret_pan, 0..=255).text("pan"));
                ui.add(egui::Slider::new(&mut ed.turret_tilt, 0..=255).text("tilt"));
                if ui.button("Apply").clicked() {
                    apply = true;
                }
            }
            FixtureKind::Projector => {
                ui.checkbox(&mut ed.proj_on, "On");
                // Pattern name picker: resolves the current (gallery, pattern)
                // bytes to a name, and writes them back on selection.
                let current = crate::projector_patterns::name_for(ed.proj_gallery, ed.proj_pattern)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{:#04x},{:#04x}", ed.proj_gallery, ed.proj_pattern));
                egui::ComboBox::from_label("pattern")
                    .selected_text(current)
                    // Keep the popup open while typing in the filter; the default
                    // CloseOnClick would dismiss it on the first click in the box.
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show_ui(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.proj_pattern_filter)
                                .hint_text("filter…"),
                        );
                        let needle = ed.proj_pattern_filter.to_ascii_lowercase();
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            let mut last_gallery: Option<u8> = None;
                            for pat in crate::projector_patterns::library() {
                                if !needle.is_empty()
                                    && !pat.name.to_ascii_lowercase().contains(&needle)
                                {
                                    continue;
                                }
                                if last_gallery != Some(pat.gallery) {
                                    ui.label(if pat.gallery == 0xff {
                                        egui::RichText::new("Scenes").weak()
                                    } else {
                                        egui::RichText::new("Gobos").weak()
                                    });
                                    last_gallery = Some(pat.gallery);
                                }
                                let selected = ed.proj_gallery == pat.gallery
                                    && ed.proj_pattern == pat.pattern;
                                if ui.selectable_label(selected, &pat.name).clicked() {
                                    ed.proj_gallery = pat.gallery;
                                    ed.proj_pattern = pat.pattern;
                                    ui.close(); // selection made → dismiss the popup
                                }
                            }
                        });
                    });
                ui.add(egui::Slider::new(&mut ed.proj_colour, 0..=255).text("colour"));
                if ui.button("Apply").clicked() {
                    apply = true;
                }
            }
        });

    if apply && !locked {
        apply_fixture_edit(conn, app, project_id, &ed);
        // leave closed
    } else if open {
        app.fixture_editor = Some(ed);
    }
}

/// Pick the keyframes of one fixture channel that reproduce a `[f0, f1]` region:
/// the carried-in keyframe (latest at or before `f0`) placed at offset 0, plus
/// every keyframe strictly inside `(f0, f1]` at its relative offset. Returns
/// `(frame_offset, row)` pairs.
fn pick_region<T: Clone>(
    rows: &[T],
    frame_of: impl Fn(&T) -> u32,
    f0: u32,
    f1: u32,
) -> Vec<(u32, T)> {
    let mut out = Vec::new();
    if let Some(carry) = rows
        .iter()
        .filter(|r| frame_of(r) <= f0)
        .max_by_key(|r| frame_of(r))
    {
        out.push((0, carry.clone()));
    }
    for r in rows.iter().filter(|r| {
        let f = frame_of(r);
        f > f0 && f <= f1
    }) {
        out.push((frame_of(r) - f0, r.clone()));
    }
    out
}

/// Extract a marquee region into a `Clipboard`. Lights come from the folded
/// keyframes + held grid (with a boundary keyframe baked in so carried-in state
/// survives the copy); fixtures are read raw from the DB for the channels whose
/// rows the selection covers. See `state::Clipboard`.
fn extract_region(
    conn: &DbConnection,
    project_id: u64,
    nl: u32,
    sel: GridSelection,
    keyframes: &HashMap<(u32, u32), bool>,
    held: &[Vec<bool>],
    fixtures: &[FixtureTrack],
) -> Clipboard {
    let (f0, f1) = (sel.frame_min, sel.frame_max);

    let mut lights = Vec::new();
    let mut light_span = 0;
    let mut light_min = 0;
    if sel.row_min < nl {
        light_min = sel.row_min;
        let light_max = sel.row_max.min(nl - 1);
        light_span = light_max - light_min + 1;
        for l in light_min..=light_max {
            let loff = l - light_min;
            // Boundary: carried-in "on" state at f0 when no explicit keyframe
            // sits there (an off default needs no keyframe).
            if !keyframes.contains_key(&(l, f0)) && held[l as usize][f0 as usize] {
                lights.push(LightEditInput { light: loff, frame: 0, state: 1 });
            }
            for f in f0..=f1 {
                if let Some(&on) = keyframes.get(&(l, f)) {
                    lights.push(LightEditInput {
                        light: loff,
                        frame: f - f0,
                        state: if on { 1 } else { 0 },
                    });
                }
            }
        }
    }

    let mut lasers = Vec::new();
    let mut projectors = Vec::new();
    let mut turrets = Vec::new();
    if sel.row_max >= nl {
        for r in sel.row_min.max(nl)..=sel.row_max {
            let Some(tr) = fixtures.get((r - nl) as usize) else {
                continue;
            };
            let ch = tr.channel;
            match tr.kind {
                FixtureKind::Laser => {
                    let raw: Vec<LaserKeyframe> = conn
                        .db()
                        .laser_kf()
                        .iter()
                        .filter(|r| r.project_id == project_id && r.channel == ch)
                        .collect();
                    for (off, r) in pick_region(&raw, |r| r.frame, f0, f1) {
                        lasers.push(LaserKeyframeInput {
                            frame: off,
                            channel: r.channel,
                            enable: r.enable,
                            pattern: r.pattern,
                            cr: r.cr,
                            cg: r.cg,
                            cb: r.cb,
                            points: r.points,
                        });
                    }
                }
                FixtureKind::Projector => {
                    let raw: Vec<ProjectorKeyframe> = conn
                        .db()
                        .projector_kf()
                        .iter()
                        .filter(|r| r.project_id == project_id && r.channel == ch)
                        .collect();
                    for (off, r) in pick_region(&raw, |r| r.frame, f0, f1) {
                        projectors.push(ProjectorKeyframeInput {
                            frame: off,
                            channel: r.channel,
                            state: r.state,
                            gallery: r.gallery,
                            pattern: r.pattern,
                            colour: r.colour,
                        });
                    }
                }
                FixtureKind::Turret => {
                    let raw: Vec<TurretKeyframe> = conn
                        .db()
                        .turret_kf()
                        .iter()
                        .filter(|r| r.project_id == project_id && r.channel == ch)
                        .collect();
                    for (off, r) in pick_region(&raw, |r| r.frame, f0, f1) {
                        turrets.push(TurretKeyframeInput {
                            frame: off,
                            channel: r.channel,
                            state: r.state,
                            pan: r.pan,
                            tilt: r.tilt,
                        });
                    }
                }
            }
        }
    }

    Clipboard {
        light_span,
        frame_span: f1 - f0 + 1,
        src_light_min: light_min,
        lights,
        lasers,
        projectors,
        turrets,
    }
}

/// Paste a clipboard into `project_id` at `(base_light, base_frame)` via the
/// bulk reducers. Records any reducer error on `app.last_error`.
fn apply_clipboard(
    conn: &DbConnection,
    app: &mut AppState,
    project_id: u64,
    cb: &Clipboard,
    base_light: u32,
    base_frame: u32,
) {
    let r = conn.reducers();
    let lights = cb.light_rows(base_light, base_frame);
    if !lights.is_empty() {
        if let Err(e) = r.append_edits(project_id, lights) {
            app.last_error = Some(format!("{e}"));
            return;
        }
    }
    let lasers = cb.laser_rows(base_frame);
    if !lasers.is_empty() {
        if let Err(e) = r.paste_laser_keyframes(project_id, lasers) {
            app.last_error = Some(format!("{e}"));
            return;
        }
    }
    let projectors = cb.projector_rows(base_frame);
    if !projectors.is_empty() {
        if let Err(e) = r.paste_projector_keyframes(project_id, projectors) {
            app.last_error = Some(format!("{e}"));
            return;
        }
    }
    let turrets = cb.turret_rows(base_frame);
    if !turrets.is_empty() {
        if let Err(e) = r.paste_turret_keyframes(project_id, turrets) {
            app.last_error = Some(format!("{e}"));
        }
    }
}

/// Copy the current marquee selection into the in-memory clipboard.
#[allow(clippy::too_many_arguments)]
fn do_copy(
    app: &mut AppState,
    conn: &DbConnection,
    project_id: u64,
    nl: u32,
    keyframes: &HashMap<(u32, u32), bool>,
    held: &[Vec<bool>],
    fixtures: &[FixtureTrack],
) {
    let Some(sel) = app.selection else { return };
    let cb = extract_region(conn, project_id, nl, sel, keyframes, held, fixtures);
    if cb.is_empty() {
        app.last_error = Some("Nothing in the selection to copy".to_string());
    } else {
        app.clipboard = Some(cb);
        app.last_error = None;
    }
}

/// Paste the clipboard at the playhead, keeping the source light rows.
fn do_paste(conn: &DbConnection, app: &mut AppState, project_id: u64) {
    let Some(cb) = app.clipboard.clone() else { return };
    let base_light = cb.src_light_min;
    let base_frame = app.current_frame;
    apply_clipboard(conn, app, project_id, &cb, base_light, base_frame);
}

/// Duplicate the selection immediately after itself (same rows, next frames).
#[allow(clippy::too_many_arguments)]
fn do_duplicate(
    app: &mut AppState,
    conn: &DbConnection,
    project_id: u64,
    nl: u32,
    keyframes: &HashMap<(u32, u32), bool>,
    held: &[Vec<bool>],
    fixtures: &[FixtureTrack],
) {
    let Some(sel) = app.selection else { return };
    let cb = extract_region(conn, project_id, nl, sel, keyframes, held, fixtures);
    if cb.is_empty() {
        return;
    }
    let base_light = cb.src_light_min;
    let base_frame = sel.frame_max + 1;
    apply_clipboard(conn, app, project_id, &cb, base_light, base_frame);
    app.clipboard = Some(cb);
}

/// Save the current selection as a named, persistent blueprint.
#[allow(clippy::too_many_arguments)]
fn do_save_blueprint(
    app: &mut AppState,
    conn: &DbConnection,
    project_id: u64,
    nl: u32,
    keyframes: &HashMap<(u32, u32), bool>,
    held: &[Vec<bool>],
    fixtures: &[FixtureTrack],
) {
    let Some(sel) = app.selection else { return };
    let cb = extract_region(conn, project_id, nl, sel, keyframes, held, fixtures);
    if cb.is_empty() {
        app.last_error = Some("Nothing in the selection to save".to_string());
        return;
    }
    let name = app.blueprint_name.trim().to_string();
    if name.is_empty() {
        app.last_error = Some("Blueprint name cannot be empty".to_string());
        return;
    }
    let num_lights = cb.light_span.max(1);
    let num_frames = cb.frame_span;
    if let Err(e) = conn.reducers().save_blueprint(
        name,
        num_lights,
        num_frames,
        cb.lights,
        cb.lasers,
        cb.projectors,
        cb.turrets,
    ) {
        app.last_error = Some(format!("{e}"));
    } else {
        app.last_error = None;
    }
}

/// The single egui system (runs in `EguiPrimaryContextPass`).
pub fn ui_system(
    mut contexts: EguiContexts,
    conn: NonSend<ConnResource>,
    mut app: ResMut<AppState>,
    mut playback: ResMut<Playback>,
    upload: NonSend<UploadState>,
    audio: NonSend<AudioPlayback>,
    mut vp: ResMut<Viewport3dRect>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    // Bump the default text sizes up a touch for readability (idempotent: we set
    // absolute sizes each frame rather than scaling the current ones).
    ctx.style_mut(|s| {
        use egui::FontFamily::{Monospace, Proportional};
        use egui::{FontId, TextStyle};
        s.text_styles.insert(TextStyle::Small, FontId::new(11.0, Proportional));
        s.text_styles.insert(TextStyle::Body, FontId::new(15.0, Proportional));
        s.text_styles.insert(TextStyle::Button, FontId::new(15.0, Proportional));
        s.text_styles.insert(TextStyle::Heading, FontId::new(22.0, Proportional));
        s.text_styles.insert(TextStyle::Monospace, FontId::new(13.0, Monospace));
    });
    let guard = conn.state.borrow();
    match &*guard {
        ConnState::NeedsLogin => login_screen(ctx, None),
        ConnState::Authenticating => {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label("Signing you in…");
                    });
                });
            });
        }
        ConnState::Connecting => {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label("Connecting to SpacetimeDB…");
                });
            });
        }
        ConnState::Failed(e) => login_screen(ctx, Some(e)),
        ConnState::Connected(c) => {
            ui_connected(ctx, c, &mut app, &mut playback, &upload, &audio);
        }
    }
    // All panels are laid out now, so `available_rect` is the central region left
    // for the 3D scene (editor mode) — or empty when a full CentralPanel covers the
    // window (login / picker), giving `None`. Convert egui points to physical pixels
    // for `scene::apply_3d_viewport`. (egui's `Vec2` is in scope here, so the resource
    // tuple is built with bevy's `Vec2` explicitly.)
    let avail = ctx.available_rect();
    let ppp = ctx.pixels_per_point();
    vp.rect = (avail.width() > 1.0 && avail.height() > 1.0).then(|| {
        (
            bevy::math::Vec2::new(avail.min.x * ppp, avail.min.y * ppp),
            bevy::math::Vec2::new(avail.width() * ppp, avail.height() * ppp),
        )
    });
    Ok(())
}

/// Full-screen login gate (shown until we have a working connection). `error`
/// is set when a previous attempt failed (expired token, network, etc.).
fn login_screen(ctx: &egui::Context, error: Option<&str>) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.30);
                ui.heading("🎚 Light Show Editor");
                ui.add_space(8.0);
                ui.label("Sign in to create and edit light shows.");
                ui.add_space(16.0);
                let btn = egui::Button::new(
                    egui::RichText::new("🎮  Log in with Discord").size(18.0),
                )
                .min_size(Vec2::new(240.0, 44.0))
                .fill(Color32::from_rgb(88, 101, 242)); // Discord blurple
                if ui.add(btn).clicked() {
                    #[cfg(target_arch = "wasm32")]
                    crate::auth::login();
                }
                if let Some(e) = error {
                    ui.add_space(14.0);
                    ui.colored_label(Color32::LIGHT_RED, e);
                    ui.weak("Try logging in again.");
                }
            });
        });
    });
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

    // Display name for the top bar: Discord username when we have it, else the
    // short identity hash.
    let display = {
        #[cfg(target_arch = "wasm32")]
        {
            crate::auth::stored_username()
                .unwrap_or_else(|| format!("id {}", short_id(me.as_ref())))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            format!("id {}", short_id(me.as_ref()))
        }
    };

    // Show the user's own projects *and* every seeded sample show (templates are
    // owned by the seeder but visible to everyone, read-only until forked).
    let mut projects: Vec<Project> = conn
        .db()
        .project()
        .iter()
        .filter(|p| me.as_ref() == Some(&p.owner) || p.is_template)
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
                if ui.button("Log out").on_hover_text("Sign out of this device").clicked() {
                    #[cfg(target_arch = "wasm32")]
                    crate::auth::logout();
                }
                ui.label(egui::RichText::new(&display).strong())
                    .on_hover_text(format!("id {}", short_id(me.as_ref())));
                ui.colored_label(Color32::from_rgb(120, 220, 120), "● live");
                ui.separator();
                ui.add(
                    egui::Slider::new(&mut app.camera_angle, 0.0..=360.0)
                        .text("⟳ orbit")
                        .show_value(false),
                );
            });
        });
    });

    let open = app
        .open_project
        .and_then(|pid| projects.iter().find(|p| p.id == pid).cloned());

    match open {
        Some(project) => {
            // Templates the user doesn't own are read-only until forked.
            let read_only = project.is_template && me.as_ref() != Some(&project.owner);
            editor_ui(ctx, conn, app, playback, &project, upload, audio, read_only);
        }
        None => {
            app.open_project = None;
            project_list_ui(ctx, app, &projects, me.as_ref(), upload);
        }
    }
}

fn project_list_ui(
    ctx: &egui::Context,
    app: &mut AppState,
    projects: &[Project],
    me: Option<&spacetimedb_sdk::Identity>,
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
                ui.weak("· 7 lights (standard rig)");
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

        let mine: Vec<&Project> = projects
            .iter()
            .filter(|p| !p.is_template && !p.is_blueprint && me == Some(&p.owner))
            .collect();
        let samples: Vec<&Project> = projects.iter().filter(|p| p.is_template).collect();

        ui.add_space(12.0);
        ui.label(egui::RichText::new("Your light shows").strong());
        ui.separator();
        if mine.is_empty() {
            ui.weak("None yet — create one above.");
        }
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .id_salt("my_shows")
            .show(ui, |ui| {
                for p in &mine {
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

        if !samples.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Sample shows").strong());
            ui.weak("Classic shows imported from the previous software — open to watch, or duplicate to edit your own copy.");
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .id_salt("sample_shows")
                .show(ui, |ui| {
                    for p in &samples {
                        ui.horizontal(|ui| {
                            if ui.button("Open").clicked() {
                                app.open_project = Some(p.id);
                                app.current_frame = 0;
                                app.history_pos = None;
                            }
                            ui.label(egui::RichText::new(&p.name).strong());
                            ui.weak(format!(
                                "{} lights · {} beats",
                                p.num_lights,
                                p.num_frames / 2
                            ));
                        });
                    }
                });
        }

        if let Some(e) = &app.last_error {
            ui.add_space(8.0);
            ui.colored_label(Color32::LIGHT_RED, e);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn editor_ui(
    ctx: &egui::Context,
    conn: &DbConnection,
    app: &mut AppState,
    playback: &mut Playback,
    project: &Project,
    upload: &UploadState,
    audio: &AudioPlayback,
    read_only: bool,
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
    // No edits allowed when time-travelling *or* viewing a read-only sample.
    let locked = viewing_history || read_only;

    // ---- Optimistic prediction: clear the pending overlay once our own edits
    // have echoed back (or on project switch / history view), then apply what
    // remains on top of the authoritative fold so edits show instantly. ----
    let me = conn.try_identity();
    app.cur_head = head;
    if app.pending_project != project.id {
        app.pending.clear();
        app.pending_fixtures.clear();
        app.fixture_editor = None;
        app.pending_project = project.id;
    }
    if !app.pending.is_empty() {
        let landed = edits
            .iter()
            .filter(|e| Some(&e.author) == me.as_ref() && e.seq > app.pending_base)
            .count();
        if viewing_history || landed >= app.pending.len() {
            app.pending.clear();
        }
    }

    let mut keyframes = fold_keyframes(&edits, cutoff);
    if !viewing_history {
        apply_pending(&mut keyframes, &app.pending);
    }
    let held = expand_held(&keyframes, nl, nf);
    // Drop spliced fixture edits once the backend has echoed a matching row.
    clear_echoed_fixtures(conn, app, project.id);
    // Fixture rows (lasers / turrets / gobo projector), with any just-edited
    // keyframe spliced in for instant feedback.
    let fixture_tracks = build_fixture_tracks(conn, project.id, nf, &app.pending_fixtures);

    // Drop a selection left over from a different (or since-shrunk) project so a
    // stale highlight never paints over this grid. The clipboard intentionally
    // survives, so a region can be pasted into another project.
    if let Some(sel) = app.selection {
        if sel.row_max >= nl + fixture_tracks.len() as u32 || sel.frame_max >= nf {
            app.selection = None;
        }
    }

    // Blueprint paste-preview ghost at the playhead (lights only — a non-open
    // blueprint's fixture rows aren't replicated to this client).
    let preview = app.active_blueprint.and_then(|bid| {
        let bp = conn.db().project().iter().find(|p| p.id == bid && p.is_blueprint)?;
        let mut bedits: Vec<Edit> = conn
            .db()
            .edit_log()
            .iter()
            .filter(|e| e.project_id == bid)
            .collect();
        bedits.sort_by_key(|e| e.seq);
        let bkf = fold_keyframes(&bedits, bp.head_seq);
        let bheld = expand_held(&bkf, bp.num_lights, bp.num_frames);
        Some(BlueprintPreview {
            base_frame: app.current_frame,
            num_frames: bp.num_frames,
            held: bheld,
        })
    });

    // The project's song (may still be uploading). Beats/markers come from the
    // stored on-beat frame list; playback needs a decoded buffer (`audio_ready`).
    let song: Option<Song> = conn.db().song().iter().find(|s| s.project_id == project.id);
    let beats: Vec<u32> = song.as_ref().map(|s| s.beats_frames.clone()).unwrap_or_default();
    let song_complete = song.as_ref().map(|s| s.complete).unwrap_or(false);
    let song_id = song.as_ref().map(|s| s.id);
    let audio_ready = has_playable_audio(audio, if song_complete { song_id } else { None });
    let nbeats = nf / 2;

    // ---- Keyboard shortcuts: Ctrl+C copy, Ctrl+V paste, Ctrl+D duplicate ----
    // (ignored while a text field is focused). Copy is allowed read-only; paste
    // and duplicate mutate, so they are gated like every other edit.
    if !ctx.wants_keyboard_input() {
        let (copy, paste, dup) = ctx.input(|i| {
            let cmd = i.modifiers.command;
            (
                cmd && i.key_pressed(egui::Key::C),
                cmd && i.key_pressed(egui::Key::V),
                cmd && i.key_pressed(egui::Key::D),
            )
        });
        if copy {
            do_copy(app, conn, project.id, nl, &keyframes, &held, &fixture_tracks);
        }
        if paste && !locked {
            do_paste(conn, app, project.id);
        }
        if dup && !locked {
            do_duplicate(app, conn, project.id, nl, &keyframes, &held, &fixture_tracks);
        }
        // Delete clears a selection's contents; Esc drops the in-hand ghost /
        // clears the selection.
        let (del, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if del && !locked {
            if let Some(sel) = app.selection {
                do_delete_region(conn, app, project.id, sel, nl, &held, &keyframes, &fixture_tracks);
                app.selection = None;
            }
        }
        if esc {
            app.ghost = None;
            app.selection = None;
            app.fixture_editor = None;
        }
    }

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
            if read_only {
                ui.separator();
                ui.colored_label(Color32::from_rgb(240, 200, 80), "sample · read-only");
                if ui
                    .button("⧉ Duplicate to my projects")
                    .on_hover_text("Make an editable copy of this show that you own")
                    .clicked()
                {
                    if let Err(e) = conn.reducers().fork_project(project.id) {
                        app.last_error = Some(format!("{e}"));
                    } else {
                        // Return to the picker; the new copy appears under "Your
                        // light shows" once the insert replicates.
                        app.open_project = None;
                        app.history_pos = None;
                        app.last_error = None;
                    }
                }
            }

            // Export this show as a rusty-halloween .zip (instructions + audio +
            // metadata), assembled in the browser from the replicated cache.
            ui.separator();
            let export = ui
                .add_enabled(song_complete, egui::Button::new("⬇ Export show"))
                .on_hover_text(if song_complete {
                    "Download this show as a rusty-halloween .zip"
                } else {
                    "Upload a song and let it finish before exporting"
                });
            if export.clicked() {
                app.last_error = Some(match crate::export::export_open_project(conn, project) {
                    Ok(file) => format!("Exported {file}"),
                    Err(e) => format!("Export failed: {e}"),
                });
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
                if ui.add(btn).clicked() && !locked {
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
                let gen = ui.add_enabled(!locked, egui::Button::new("✨ Generate on beats"));
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

            // ---- Blueprints: save the selection, insert saved snippets ----
            ui.separator();
            ui.label(egui::RichText::new("Blueprints").strong());
            ui.weak("Reusable timeline snippets. Save a Shift+drag selection, then insert it here or in another project.");
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut app.blueprint_name);
                if ui
                    .add_enabled(app.selection.is_some(), egui::Button::new("💾 Save selection"))
                    .clicked()
                {
                    do_save_blueprint(app, conn, project.id, nl, &keyframes, &held, &fixture_tracks);
                }
            });
            let mut blueprints: Vec<Project> = conn
                .db()
                .project()
                .iter()
                .filter(|p| p.is_blueprint && me.as_ref() == Some(&p.owner))
                .collect();
            blueprints.sort_by_key(|p| p.id);
            if blueprints.is_empty() {
                ui.weak("No blueprints yet.");
            }
            egui::ScrollArea::vertical()
                .id_salt("blueprints")
                .max_height(150.0)
                .show(ui, |ui| {
                    for bp in &blueprints {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!locked, egui::Button::new("Insert"))
                                .on_hover_text("Insert at the playhead (lights from row 0)")
                                .clicked()
                            {
                                if let Err(e) = conn.reducers().insert_blueprint(
                                    bp.id,
                                    project.id,
                                    0,
                                    app.current_frame,
                                ) {
                                    app.last_error = Some(format!("{e}"));
                                }
                            }
                            if ui
                                .selectable_label(
                                    app.active_blueprint == Some(bp.id),
                                    egui::RichText::new(&bp.name).strong(),
                                )
                                .on_hover_text("Toggle a paste preview at the playhead")
                                .clicked()
                            {
                                app.active_blueprint = if app.active_blueprint == Some(bp.id) {
                                    None
                                } else {
                                    Some(bp.id)
                                };
                            }
                            ui.weak(format!("{}×{}", bp.num_lights, bp.num_frames));
                            if ui.small_button("🗑").on_hover_text("Delete blueprint").clicked() {
                                if app.active_blueprint == Some(bp.id) {
                                    app.active_blueprint = None;
                                }
                                if let Err(e) = conn.reducers().delete_blueprint(bp.id) {
                                    app.last_error = Some(format!("{e}"));
                                }
                            }
                        });
                    }
                });

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
    // Non-resizable and unsized: the panel shrink-wraps its content, so it is
    // exactly as tall as the ruler + the currently-visible rows + the controls,
    // and shortens automatically when the category filter hides rows.
    egui::TopBottomPanel::bottom("timeline")
        .resizable(false)
        .show(ctx, |ui| {
            ui.weak("Ruler = scrub · click/drag light rows to edit bars (drag an edge to resize) · Shift+drag = select (Del clears) · Ctrl+drag = white ghost (click stamps) · drag a selection to move");

            // ---- Category filter ----
            ui.horizontal(|ui| {
                ui.label("Show:");
                for (f, lbl) in [
                    (TimelineFilter::All, "All"),
                    (TimelineFilter::Lights, "Lights"),
                    (TimelineFilter::Lasers, "Lasers"),
                    (TimelineFilter::Turrets, "Turrets"),
                    (TimelineFilter::Projector, "Projector"),
                ] {
                    if ui
                        .selectable_label(app.timeline_filter == f, lbl)
                        .clicked()
                    {
                        app.timeline_filter = f;
                    }
                }
            });

            // ---- Region clipboard actions ----
            ui.horizontal(|ui| {
                let has_sel = app.selection.is_some();
                let has_clip = app.clipboard.is_some();
                if ui
                    .add_enabled(has_sel, egui::Button::new("⧉ Copy"))
                    .on_hover_text("Copy the selected region (Ctrl+C)")
                    .clicked()
                {
                    do_copy(app, conn, project.id, nl, &keyframes, &held, &fixture_tracks);
                }
                if ui
                    .add_enabled(has_clip && !locked, egui::Button::new("📋 Paste @ playhead"))
                    .on_hover_text("Paste the clipboard at the playhead (Ctrl+V)")
                    .clicked()
                {
                    do_paste(conn, app, project.id);
                }
                if ui
                    .add_enabled(has_sel && !locked, egui::Button::new("⇥ Duplicate"))
                    .on_hover_text("Duplicate the selection right after itself (Ctrl+D)")
                    .clicked()
                {
                    do_duplicate(app, conn, project.id, nl, &keyframes, &held, &fixture_tracks);
                }
                if let Some(sel) = app.selection {
                    ui.separator();
                    ui.weak(format!(
                        "selection: {} rows × {} frames",
                        sel.row_max - sel.row_min + 1,
                        sel.frame_max - sel.frame_min + 1
                    ));
                }
                if let Some(cb) = &app.clipboard {
                    ui.separator();
                    ui.weak(format!(
                        "clipboard: {} lights × {} frames",
                        cb.light_span, cb.frame_span
                    ));
                }
            });

            let filter = app.timeline_filter;
            draw_frame_grid(
                ui,
                conn,
                app,
                playback,
                project,
                &held,
                &keyframes,
                &fixture_tracks,
                &beats,
                locked,
                filter,
                preview.as_ref(),
            );
            if let Some(e) = &app.last_error {
                ui.add_space(6.0);
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        });

    // Right-click fixture editor (floating window), drawn last so it overlays.
    fixture_editor_window(ctx, conn, app, project.id, locked);
}

/// Where a pointer landed on the grid: the beat ruler, a device row, or off-grid.
enum GridHit {
    Ruler(u32),
    Row(u32, u32),
    Outside,
}

impl GridHit {
    fn frame(&self, fallback: u32) -> u32 {
        match self {
            GridHit::Ruler(f) | GridHit::Row(_, f) => *f,
            GridHit::Outside => fallback,
        }
    }
}

/// Inclusive on-run containing frame `f`, if `row[f]` is on.
fn run_containing(row: &[bool], f: u32) -> Option<(u32, u32)> {
    let fi = f as usize;
    if fi >= row.len() || !row[fi] {
        return None;
    }
    let mut s = fi;
    while s > 0 && row[s - 1] {
        s -= 1;
    }
    let mut e = fi;
    while e + 1 < row.len() && row[e + 1] {
        e += 1;
    }
    Some((s as u32, e as u32))
}

/// Explicit keyframes for one light, pulled from the folded keyframe map.
fn light_kfs(keyframes: &HashMap<(u32, u32), bool>, light: u32) -> HashMap<u32, bool> {
    keyframes
        .iter()
        .filter(|((l, _), _)| *l == light)
        .map(|((_, f), v)| (*f, *v))
        .collect()
}

/// Append the minimal edits so light `light`'s held row becomes `desired`,
/// rewriting its keyframes to exactly the run boundaries of `desired`.
fn reconcile_light_edits(
    light: u32,
    cur_kfs: &HashMap<u32, bool>,
    desired: &[bool],
    out: &mut Vec<LightEditInput>,
) {
    let mut want: HashMap<u32, bool> = HashMap::new();
    let mut prev = false;
    for (f, &d) in desired.iter().enumerate() {
        if d != prev {
            want.insert(f as u32, d);
            prev = d;
        }
    }
    for (&f, &v) in cur_kfs {
        if want.get(&f) != Some(&v) {
            out.push(LightEditInput { light, frame: f, state: 2 });
        }
    }
    for (f, v) in want {
        if cur_kfs.get(&f) != Some(&v) {
            out.push(LightEditInput { light, frame: f, state: if v { 1 } else { 0 } });
        }
    }
}

/// Send a batch of light edits (no-op if empty).
/// Record edits as optimistic/pending so they render before the backend echoes.
fn predict(app: &mut AppState, edits: &[LightEditInput]) {
    if edits.is_empty() {
        return;
    }
    if app.pending.is_empty() {
        app.pending_base = app.cur_head;
    }
    app.pending.extend(edits.iter().cloned());
}

fn send_light_edits(conn: &DbConnection, app: &mut AppState, project_id: u64, edits: Vec<LightEditInput>) {
    if edits.is_empty() {
        return;
    }
    match conn.reducers().append_edits(project_id, edits.clone()) {
        Ok(()) => predict(app, &edits),
        Err(e) => {
            app.last_error = Some(format!("{e}"));
            app.pending.clear();
        }
    }
}

/// The fixture channels a selection's rows cover (canonical rows `>= nl`).
fn selection_fixture_channels(
    sel: GridSelection,
    nl: u32,
    fixtures: &[FixtureTrack],
) -> (Vec<u8>, Vec<u8>, bool) {
    let (mut lasers, mut turrets, mut projector) = (Vec::new(), Vec::new(), false);
    if sel.row_max >= nl {
        for r in sel.row_min.max(nl)..=sel.row_max {
            if let Some(tr) = fixtures.get((r - nl) as usize) {
                match tr.kind {
                    FixtureKind::Laser => lasers.push(tr.channel),
                    FixtureKind::Turret => turrets.push(tr.channel),
                    FixtureKind::Projector => projector = true,
                }
            }
        }
    }
    (lasers, turrets, projector)
}

/// Delete everything inside a selection: lights set off (reconciled), fixture
/// keyframes removed in range.
#[allow(clippy::too_many_arguments)]
fn do_delete_region(
    conn: &DbConnection,
    app: &mut AppState,
    project_id: u64,
    sel: GridSelection,
    nl: u32,
    held: &[Vec<bool>],
    keyframes: &HashMap<(u32, u32), bool>,
    fixtures: &[FixtureTrack],
) {
    let mut edits = Vec::new();
    if sel.row_min < nl {
        for l in sel.row_min..=sel.row_max.min(nl - 1) {
            let mut desired = held[l as usize].clone();
            for f in sel.frame_min..=sel.frame_max {
                desired[f as usize] = false;
            }
            reconcile_light_edits(l, &light_kfs(keyframes, l), &desired, &mut edits);
        }
    }
    send_light_edits(conn, app, project_id, edits);
    let (lasers, turrets, projector) = selection_fixture_channels(sel, nl, fixtures);
    if !lasers.is_empty() || !turrets.is_empty() || projector {
        if let Err(e) = conn.reducers().delete_fixture_region(
            project_id,
            sel.frame_min,
            sel.frame_max,
            lasers,
            turrets,
            projector,
        ) {
            app.last_error = Some(format!("{e}"));
        }
    }
}

/// Overwrite-stamp a clipboard region into the project at `(base_light,
/// base_frame)`: light cells are replaced, fixture rows in range are cleared
/// then re-inserted (frames offset, channels preserved).
#[allow(clippy::too_many_arguments)]
fn do_stamp_region(
    conn: &DbConnection,
    app: &mut AppState,
    project_id: u64,
    cb: &Clipboard,
    base_light: u32,
    base_frame: u32,
    nl: u32,
    nf: u32,
    held: &[Vec<bool>],
    keyframes: &HashMap<(u32, u32), bool>,
) {
    let gh = cb.light_held();
    let mut edits = Vec::new();
    for (loff, grow) in gh.iter().enumerate() {
        let dl = base_light + loff as u32;
        if dl >= nl {
            break;
        }
        let mut desired = held[dl as usize].clone();
        for (foff, &on) in grow.iter().enumerate() {
            let df = base_frame + foff as u32;
            if df < nf {
                desired[df as usize] = on;
            }
        }
        reconcile_light_edits(dl, &light_kfs(keyframes, dl), &desired, &mut edits);
    }
    send_light_edits(conn, app, project_id, edits);

    let (lc, tc, pj) = cb.fixture_channels();
    if !lc.is_empty() || !tc.is_empty() || pj {
        let dest_f1 = (base_frame + cb.frame_span).saturating_sub(1).min(nf.saturating_sub(1));
        if let Err(e) =
            conn.reducers().delete_fixture_region(project_id, base_frame, dest_f1, lc, tc, pj)
        {
            app.last_error = Some(format!("{e}"));
        }
    }
    let lasers = cb.laser_rows(base_frame);
    if !lasers.is_empty() {
        let _ = conn.reducers().paste_laser_keyframes(project_id, lasers);
    }
    let turrets = cb.turret_rows(base_frame);
    if !turrets.is_empty() {
        let _ = conn.reducers().paste_turret_keyframes(project_id, turrets);
    }
    let projectors = cb.projector_rows(base_frame);
    if !projectors.is_empty() {
        let _ = conn.reducers().paste_projector_keyframes(project_id, projectors);
    }
}

/// Move the selected region by `(drow, dframe)`, overwriting the destination.
/// Lights shift by both deltas; fixtures shift in time only (channels are
/// physical), as a cut-and-overwrite of their keyframe rows.
#[allow(clippy::too_many_arguments)]
fn do_move_region(
    conn: &DbConnection,
    app: &mut AppState,
    project_id: u64,
    orig: GridSelection,
    drow: i64,
    dframe: i64,
    nl: u32,
    nf: u32,
    held: &[Vec<bool>],
    keyframes: &HashMap<(u32, u32), bool>,
    fixtures: &[FixtureTrack],
) {
    if drow == 0 && dframe == 0 {
        return;
    }
    let (f0, f1) = (orig.frame_min, orig.frame_max);

    // ---- Lights: clear source, then write source pattern at the destination ----
    if orig.row_min < nl {
        let src_hi = orig.row_max.min(nl - 1);
        let mut desired: std::collections::BTreeMap<u32, Vec<bool>> = std::collections::BTreeMap::new();
        for l in orig.row_min..=src_hi {
            desired.entry(l).or_insert_with(|| held[l as usize].clone());
            let row = desired.get_mut(&l).unwrap();
            for f in f0..=f1 {
                row[f as usize] = false;
            }
        }
        for l in orig.row_min..=src_hi {
            let dl = l as i64 + drow;
            if dl < 0 || dl >= nl as i64 {
                continue;
            }
            let dl = dl as u32;
            desired.entry(dl).or_insert_with(|| held[dl as usize].clone());
            for f in f0..=f1 {
                let df = f as i64 + dframe;
                if df < 0 || df >= nf as i64 {
                    continue;
                }
                let v = held[l as usize][f as usize];
                desired.get_mut(&dl).unwrap()[df as usize] = v;
            }
        }
        let mut edits = Vec::new();
        for (l, d) in &desired {
            reconcile_light_edits(*l, &light_kfs(keyframes, *l), d, &mut edits);
        }
        send_light_edits(conn, app, project_id, edits);
    }

    // ---- Fixtures: cut source rows, clear destination, paste shifted ----
    let (lc, tc, pj) = selection_fixture_channels(orig, nl, fixtures);
    if lc.is_empty() && tc.is_empty() && !pj {
        return;
    }
    let mut lasers = Vec::new();
    for &ch in &lc {
        for r in conn.db().laser_kf().iter().filter(|r| {
            r.project_id == project_id && r.channel == ch && r.frame >= f0 && r.frame <= f1
        }) {
            let nf2 = r.frame as i64 + dframe;
            if nf2 >= 0 && (nf2 as u32) < nf {
                lasers.push(LaserKeyframeInput {
                    frame: nf2 as u32,
                    channel: r.channel,
                    enable: r.enable,
                    pattern: r.pattern,
                    cr: r.cr,
                    cg: r.cg,
                    cb: r.cb,
                    points: r.points,
                });
            }
        }
    }
    let mut turrets = Vec::new();
    for &ch in &tc {
        for r in conn.db().turret_kf().iter().filter(|r| {
            r.project_id == project_id && r.channel == ch && r.frame >= f0 && r.frame <= f1
        }) {
            let nf2 = r.frame as i64 + dframe;
            if nf2 >= 0 && (nf2 as u32) < nf {
                turrets.push(TurretKeyframeInput {
                    frame: nf2 as u32,
                    channel: r.channel,
                    state: r.state,
                    pan: r.pan,
                    tilt: r.tilt,
                });
            }
        }
    }
    let mut projectors = Vec::new();
    if pj {
        for r in conn.db().projector_kf().iter().filter(|r| {
            r.project_id == project_id && r.frame >= f0 && r.frame <= f1
        }) {
            let nf2 = r.frame as i64 + dframe;
            if nf2 >= 0 && (nf2 as u32) < nf {
                projectors.push(ProjectorKeyframeInput {
                    frame: nf2 as u32,
                    channel: r.channel,
                    state: r.state,
                    gallery: r.gallery,
                    pattern: r.pattern,
                    colour: r.colour,
                });
            }
        }
    }
    let df0 = (f0 as i64 + dframe).max(0) as u32;
    let df1 = ((f1 as i64 + dframe).max(0) as u32).min(nf.saturating_sub(1));
    let _ = conn.reducers().delete_fixture_region(project_id, f0, f1, lc.clone(), tc.clone(), pj);
    let _ = conn.reducers().delete_fixture_region(project_id, df0, df1, lc, tc, pj);
    if !lasers.is_empty() {
        let _ = conn.reducers().paste_laser_keyframes(project_id, lasers);
    }
    if !turrets.is_empty() {
        let _ = conn.reducers().paste_turret_keyframes(project_id, turrets);
    }
    if !projectors.is_empty() {
        let _ = conn.reducers().paste_projector_keyframes(project_id, projectors);
    }
}

/// Paint a white ghost (light pattern + bounding box) at `(bl, bf)` in canonical
/// row space, clipped to the `[first, first+count)` visible rows.
#[allow(clippy::too_many_arguments)]
fn draw_ghost_overlay(
    painter: &egui::Painter,
    origin: Pos2,
    row_top: f32,
    cell: Vec2,
    first: u32,
    count: u32,
    nf: u32,
    bl: u32,
    bf: u32,
    ghd: &[Vec<bool>],
    fspan: u32,
) {
    for (loff, grow) in ghd.iter().enumerate() {
        let c = bl + loff as u32;
        if c < first || c >= first + count {
            continue;
        }
        let ry = row_top + (c - first) as f32 * cell.y;
        for (foff, &on) in grow.iter().enumerate() {
            let tf = bf + foff as u32;
            if tf >= nf || !on {
                continue;
            }
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(origin.x + tf as f32 * cell.x, ry), cell - Vec2::splat(1.0)),
                2.0,
                COL_GHOST_FILL,
            );
        }
    }
    let rspan = ghd.len() as u32;
    let lo = bl.max(first);
    let hi = (bl + rspan).min(first + count);
    if rspan > 0 && hi > lo {
        let brect = Rect::from_min_max(
            Pos2::new(origin.x + bf as f32 * cell.x, row_top + (lo - first) as f32 * cell.y),
            Pos2::new(
                origin.x + (bf + fspan).min(nf) as f32 * cell.x,
                row_top + (hi - first) as f32 * cell.y,
            ),
        );
        painter.rect_stroke(brect, 2.0, Stroke::new(1.5, COL_GHOST_EDGE), egui::StrokeKind::Inside);
    }
}

/// Paint a selection rectangle, clipped to the visible rows.
#[allow(clippy::too_many_arguments)]
fn draw_selection_overlay(
    painter: &egui::Painter,
    origin: Pos2,
    row_top: f32,
    cell: Vec2,
    first: u32,
    count: u32,
    sel: GridSelection,
    fill: Color32,
    edge: Color32,
) {
    let lo = sel.row_min.max(first);
    let hi = sel.row_max.min(first + count - 1);
    if lo <= hi {
        let srect = Rect::from_min_max(
            Pos2::new(origin.x + sel.frame_min as f32 * cell.x, row_top + (lo - first) as f32 * cell.y),
            Pos2::new(
                origin.x + (sel.frame_max + 1) as f32 * cell.x,
                row_top + (hi - first + 1) as f32 * cell.y,
            ),
        );
        painter.rect_filled(srect, 2.0, fill);
        painter.rect_stroke(srect, 2.0, Stroke::new(1.5, edge), egui::StrokeKind::Inside);
    }
}

/// Long label + accent colour for a canonical row (`< nl` = a light, otherwise a
/// fixture). Numbering is 1-based for humans.
fn row_label(c: u32, nl: u32, fixtures: &[FixtureTrack]) -> (String, Color32) {
    if c < nl {
        (format!("Light {}", c + 1), Color32::from_gray(205))
    } else {
        let tr = &fixtures[(c - nl) as usize];
        (tr.label.clone(), tr.on_color)
    }
}

/// Frame-resolution grid: a sticky left label column plus a horizontally
/// scrolling cell area showing every device row (filtered by category), with
/// beat markers, playhead, marquee and blueprint-preview overlays.
#[allow(clippy::too_many_arguments)]
fn draw_frame_grid(
    ui: &mut egui::Ui,
    conn: &DbConnection,
    app: &mut AppState,
    playback: &mut Playback,
    project: &Project,
    held: &[Vec<bool>],
    keyframes: &HashMap<(u32, u32), bool>,
    fixtures: &[FixtureTrack],
    beats: &[u32],
    locked: bool,
    filter: TimelineFilter,
    preview: Option<&BlueprintPreview>,
) {
    let nl = project.num_lights;
    let nf = project.num_frames;
    let nfx = fixtures.len() as u32;
    let n_laser = fixtures.iter().filter(|t| t.kind == FixtureKind::Laser).count() as u32;
    let n_turret = fixtures.iter().filter(|t| t.kind == FixtureKind::Turret).count() as u32;
    let laser_start = nl;
    let turret_start = nl + n_laser;
    let proj_start = nl + n_laser + n_turret;

    // The contiguous canonical row range the active filter shows.
    let (first, count) = match filter {
        TimelineFilter::All => (0, nl + nfx),
        TimelineFilter::Lights => (0, nl),
        TimelineFilter::Lasers => (laser_start, n_laser),
        TimelineFilter::Turrets => (turret_start, n_turret),
        TimelineFilter::Projector => (proj_start, nfx - proj_start.saturating_sub(nl)),
    };
    if count == 0 {
        return;
    }

    let cell = Vec2::splat(CELL);
    let ruler_h = RULER_H;
    let label_w = 122.0_f32;
    let content_h = ruler_h + count as f32 * cell.y;
    // Reserve one extra row of height for the allocated regions so the
    // horizontal scrollbar sits below the last row instead of overlapping the
    // projector row. Painting / hit-testing below still use `content_h`.
    let alloc_h = content_h + cell.y;
    let boundaries = [laser_start, turret_start, proj_start];
    let total_rows = nl + nfx;
    let cur_frame = app.current_frame;
    let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));

    let scroll_no_drag =
        egui::scroll_area::ScrollSource::SCROLL_BAR | egui::scroll_area::ScrollSource::MOUSE_WHEEL;
    ui.horizontal_top(|ui| {
        // ---- Sticky label column ----
        let (lrect, _) = ui.allocate_exact_size(Vec2::new(label_w, alloc_h), Sense::hover());
        let lp = ui.painter_at(lrect);
        lp.text(
            Pos2::new(lrect.min.x + 6.0, lrect.min.y + ruler_h * 0.5),
            egui::Align2::LEFT_CENTER,
            "beat ▸",
            egui::FontId::monospace(10.0),
            Color32::from_gray(120),
        );
        for vi in 0..count {
            let (label, col) = row_label(first + vi, nl, fixtures);
            lp.text(
                Pos2::new(
                    lrect.min.x + 6.0,
                    lrect.min.y + ruler_h + vi as f32 * cell.y + cell.y * 0.5,
                ),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::monospace(12.0),
                col,
            );
        }

        // ---- Horizontally scrolling cell area ----
        egui::ScrollArea::horizontal()
            .id_salt("frame_cells")
            .scroll_source(scroll_no_drag)
            .show(ui, |ui| {
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(nf as f32 * cell.x, alloc_h), Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            let origin = rect.min;
            let row_top = origin.y + ruler_h;
            let rows_range = egui::Rangef::new(row_top, row_top + count as f32 * cell.y);
            let full_range = egui::Rangef::new(origin.y, origin.y + content_h);

            // Pointer → ruler / row / off-grid.
            let at = |p: Pos2| -> GridHit {
                let lx = p.x - origin.x;
                if lx < 0.0 {
                    return GridHit::Outside;
                }
                let f = ((lx / cell.x) as i64).clamp(0, nf as i64 - 1) as u32;
                let ly = p.y - origin.y;
                if ly < ruler_h {
                    return GridHit::Ruler(f);
                }
                let vi = ((ly - ruler_h) / cell.y) as i64;
                if vi < 0 || vi >= count as i64 {
                    return GridHit::Outside;
                }
                GridHit::Row(first + vi as u32, f)
            };
            let clamp_rf = |h: &GridHit| -> (u32, u32) {
                match h {
                    GridHit::Row(r, f) => (*r, *f),
                    GridHit::Ruler(f) => (first, *f),
                    GridHit::Outside => (first, cur_frame),
                }
            };

            // ===== Interaction =====
            if resp.drag_started() {
                app.drag = None;
                if let Some(p) = resp.interact_pointer_pos() {
                    let hit = at(p);
                    if shift || ctrl {
                        let (r, f) = clamp_rf(&hit);
                        app.sel_anchor = Some((r, f));
                        app.selection = Some(GridSelection { row_min: r, row_max: r, frame_min: f, frame_max: f });
                        app.drag = Some(DragKind::Marquee { ghost: ctrl && !shift });
                    } else {
                        match hit {
                            GridHit::Ruler(f) => {
                                app.current_frame = f;
                                playback.playing = false;
                                app.drag = Some(DragKind::Scrub);
                            }
                            GridHit::Row(c, f) => {
                                let in_sel = app.selection.is_some_and(|s| {
                                    c >= s.row_min && c <= s.row_max && f >= s.frame_min && f <= s.frame_max
                                });
                                if app.ghost.is_some() {
                                    // a click stamps the ghost; ignore drags
                                } else if let (true, Some(sel)) = (in_sel, app.selection) {
                                    app.drag = Some(DragKind::Move { orig: sel, grab_row: c, grab_frame: f, cur_row: c, cur_frame: f });
                                } else if !locked && c < nl {
                                    let row = &held[c as usize];
                                    if let Some((bs, be)) = run_containing(row, f) {
                                        let xl = origin.x + bs as f32 * cell.x;
                                        let xr = origin.x + (be + 1) as f32 * cell.x;
                                        if (p.x - xl).abs() <= 4.0 {
                                            app.drag = Some(DragKind::Resize { light: c, bar_start: bs, bar_end: be, drag_left: true, cur: bs });
                                        } else if (xr - p.x).abs() <= 4.0 {
                                            app.drag = Some(DragKind::Resize { light: c, bar_start: bs, bar_end: be, drag_left: false, cur: be });
                                        } else {
                                            app.drag = Some(DragKind::Paint { light: c, start: f, cur: f });
                                        }
                                    } else {
                                        app.drag = Some(DragKind::Paint { light: c, start: f, cur: f });
                                    }
                                }
                            }
                            GridHit::Outside => {}
                        }
                    }
                }
            }
            if resp.dragged() {
                if let (Some(p), Some(mut dk)) = (resp.interact_pointer_pos(), app.drag.clone()) {
                    let hit = at(p);
                    match &mut dk {
                        DragKind::Scrub => {
                            app.current_frame = hit.frame(cur_frame);
                            playback.playing = false;
                        }
                        DragKind::Marquee { .. } => {
                            if let Some((ar, af)) = app.sel_anchor {
                                let (r, f) = clamp_rf(&hit);
                                app.selection = Some(GridSelection {
                                    row_min: ar.min(r), row_max: ar.max(r),
                                    frame_min: af.min(f), frame_max: af.max(f),
                                });
                            }
                        }
                        DragKind::Paint { cur, .. } => *cur = hit.frame(cur_frame),
                        DragKind::Resize { cur, .. } => *cur = hit.frame(cur_frame),
                        DragKind::Move { cur_row, cur_frame, .. } => {
                            let (r, f) = clamp_rf(&hit);
                            *cur_row = r;
                            *cur_frame = f;
                        }
                    }
                    app.drag = Some(dk);
                }
            }
            if resp.drag_stopped() {
                app.sel_anchor = None;
                match app.drag.take() {
                    Some(DragKind::Marquee { ghost }) => {
                        if ghost {
                            if let Some(sel) = app.selection {
                                let cb = extract_region(conn, project.id, nl, sel, keyframes, held, fixtures);
                                if !cb.is_empty() {
                                    app.ghost = Some(cb);
                                }
                                app.selection = None;
                            }
                        }
                    }
                    Some(DragKind::Paint { light, start, cur }) if !locked => {
                        let (a, b) = (start.min(cur), start.max(cur));
                        let mut desired = held[light as usize].clone();
                        for f in a..=b { desired[f as usize] = true; }
                        let mut edits = Vec::new();
                        reconcile_light_edits(light, &light_kfs(keyframes, light), &desired, &mut edits);
                        send_light_edits(conn, app, project.id, edits);
                    }
                    Some(DragKind::Resize { light, bar_start, bar_end, drag_left, cur }) if !locked => {
                        let (ns, ne) = if drag_left { (cur.min(bar_end), bar_end) } else { (bar_start, cur.max(bar_start)) };
                        let mut desired = held[light as usize].clone();
                        for f in bar_start..=bar_end { desired[f as usize] = false; }
                        for f in ns..=ne { desired[f as usize] = true; }
                        let mut edits = Vec::new();
                        reconcile_light_edits(light, &light_kfs(keyframes, light), &desired, &mut edits);
                        send_light_edits(conn, app, project.id, edits);
                    }
                    Some(DragKind::Move { orig, grab_row, grab_frame, cur_row, cur_frame }) if !locked => {
                        let drow = cur_row as i64 - grab_row as i64;
                        let dframe = cur_frame as i64 - grab_frame as i64;
                        do_move_region(conn, app, project.id, orig, drow, dframe, nl, nf, held, keyframes, fixtures);
                        app.selection = Some(GridSelection {
                            row_min: (orig.row_min as i64 + drow).clamp(0, total_rows as i64 - 1) as u32,
                            row_max: (orig.row_max as i64 + drow).clamp(0, total_rows as i64 - 1) as u32,
                            frame_min: (orig.frame_min as i64 + dframe).clamp(0, nf as i64 - 1) as u32,
                            frame_max: (orig.frame_max as i64 + dframe).clamp(0, nf as i64 - 1) as u32,
                        });
                    }
                    _ => {}
                }
            }
            if resp.secondary_clicked() {
                // Right-click drops an in-hand ghost; otherwise it opens the
                // editor for a fixture row (light rows keep their left-click).
                if app.ghost.is_some() {
                    app.ghost = None;
                } else if !locked {
                    if let Some(GridHit::Row(c, f)) = resp.interact_pointer_pos().map(at) {
                        if c >= nl {
                            let tr = &fixtures[(c - nl) as usize];
                            let pos = resp.interact_pointer_pos().unwrap_or(origin);
                            app.fixture_editor =
                                Some(load_fixture_editor(conn, project.id, tr.kind, tr.channel, f, pos));
                        }
                    }
                }
            }
            if resp.clicked() && !shift && !ctrl {
                if let Some(p) = resp.interact_pointer_pos() {
                    let hit = at(p);
                    if let Some(cb) = app.ghost.clone() {
                        match hit {
                            GridHit::Row(c, f) if !locked => {
                                let lspan = cb.light_span.max(1);
                                let base_light = if c < nl { c.min(nl.saturating_sub(lspan)) } else { 0 };
                                let base_frame = f.min(nf.saturating_sub(cb.frame_span.max(1)));
                                do_stamp_region(conn, app, project.id, &cb, base_light, base_frame, nl, nf, held, keyframes);
                            }
                            GridHit::Ruler(f) => app.current_frame = f,
                            _ => {}
                        }
                    } else {
                        match hit {
                            GridHit::Ruler(f) => {
                                app.current_frame = f;
                                playback.playing = false;
                            }
                            GridHit::Row(c, f) => {
                                app.selection = None;
                                if !locked && c < nl {
                                    let mut desired = held[c as usize].clone();
                                    desired[f as usize] = !desired[f as usize];
                                    let mut edits = Vec::new();
                                    reconcile_light_edits(c, &light_kfs(keyframes, c), &desired, &mut edits);
                                    send_light_edits(conn, app, project.id, edits);
                                }
                            }
                            GridHit::Outside => {}
                        }
                    }
                }
            }

            // ===== Render =====
            // Beat ruler strip.
            painter.rect_filled(
                Rect::from_min_size(origin, Vec2::new(nf as f32 * cell.x, ruler_h)),
                0.0,
                Color32::from_gray(28),
            );
            for (i, &bf) in beats.iter().enumerate() {
                if bf < nf && i % 4 == 0 {
                    let bx = origin.x + bf as f32 * cell.x;
                    painter.text(
                        Pos2::new(bx + 2.0, origin.y + ruler_h * 0.5),
                        egui::Align2::LEFT_CENTER,
                        format!("{}", i / 4 + 1),
                        egui::FontId::monospace(9.0),
                        Color32::from_gray(150),
                    );
                }
            }

            // Live drag preview: apply the in-progress drag to the affected
            // light rows so the bar grows/shrinks in its real colour (the white
            // ghost is reserved for Ctrl+drag copies and blueprint previews).
            let mut override_rows: HashMap<u32, Vec<bool>> = HashMap::new();
            match &app.drag {
                Some(DragKind::Paint { light, start, cur }) => {
                    let mut row = held[*light as usize].clone();
                    for f in (*start).min(*cur)..=(*start).max(*cur) {
                        row[f as usize] = true;
                    }
                    override_rows.insert(*light, row);
                }
                Some(DragKind::Resize { light, bar_start, bar_end, drag_left, cur }) => {
                    let mut row = held[*light as usize].clone();
                    for f in *bar_start..=*bar_end {
                        row[f as usize] = false;
                    }
                    let (ns, ne) = if *drag_left {
                        ((*cur).min(*bar_end), *bar_end)
                    } else {
                        (*bar_start, (*cur).max(*bar_start))
                    };
                    for f in ns..=ne {
                        row[f as usize] = true;
                    }
                    override_rows.insert(*light, row);
                }
                Some(DragKind::Move { orig, grab_row, grab_frame, cur_row, cur_frame }) => {
                    let drow = *cur_row as i64 - *grab_row as i64;
                    let dframe = *cur_frame as i64 - *grab_frame as i64;
                    let (f0, f1) = (orig.frame_min, orig.frame_max);
                    if orig.row_min < nl {
                        let src_hi = orig.row_max.min(nl - 1);
                        for l in orig.row_min..=src_hi {
                            override_rows.entry(l).or_insert_with(|| held[l as usize].clone());
                            for f in f0..=f1 {
                                override_rows.get_mut(&l).unwrap()[f as usize] = false;
                            }
                        }
                        for l in orig.row_min..=src_hi {
                            let dl = l as i64 + drow;
                            if dl < 0 || dl >= nl as i64 {
                                continue;
                            }
                            let dl = dl as u32;
                            override_rows.entry(dl).or_insert_with(|| held[dl as usize].clone());
                            for f in f0..=f1 {
                                let df = f as i64 + dframe;
                                if df < 0 || df >= nf as i64 {
                                    continue;
                                }
                                let v = held[l as usize][f as usize];
                                override_rows.get_mut(&dl).unwrap()[df as usize] = v;
                            }
                        }
                    }
                }
                _ => {}
            }

            // Governing fixture keyframe at a (channel, frame) — latest row ≤ frame.
            let laser_at = |channel: u8, frame: u32| -> Option<LaserKeyframe> {
                conn.db()
                    .laser_kf()
                    .iter()
                    .filter(|r| r.project_id == project.id && r.channel == channel && r.frame <= frame)
                    .max_by_key(|r| r.frame)
            };
            let turret_at = |channel: u8, frame: u32| -> Option<TurretKeyframe> {
                conn.db()
                    .turret_kf()
                    .iter()
                    .filter(|r| r.project_id == project.id && r.channel == channel && r.frame <= frame)
                    .max_by_key(|r| r.frame)
            };
            let proj_at = |frame: u32| -> Option<ProjectorKeyframe> {
                conn.db()
                    .projector_kf()
                    .iter()
                    .filter(|r| r.project_id == project.id && r.channel == 0 && r.frame <= frame)
                    .max_by_key(|r| r.frame)
            };

            // Rows: off background + on-runs as bars (lasers also show the shape).
            for vi in 0..count {
                let c = first + vi;
                let ry = row_top + vi as f32 * cell.y;
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(origin.x, ry), Vec2::new(nf as f32 * cell.x, cell.y - 1.0)),
                    0.0,
                    COL_OFF,
                );
                let kind = (c >= nl).then(|| fixtures[(c - nl) as usize].kind);
                let (row, color): (&Vec<bool>, Color32) = if c < nl {
                    (override_rows.get(&c).unwrap_or(&held[c as usize]), COL_ON)
                } else {
                    let tr = &fixtures[(c - nl) as usize];
                    (&tr.held, tr.on_color)
                };
                let channel = (c >= nl).then(|| fixtures[(c - nl) as usize].channel).unwrap_or(0);
                let mut f = 0usize;
                while f < row.len() {
                    if row[f] {
                        let s = f;
                        let mut e = f;
                        while e + 1 < row.len() && row[e + 1] { e += 1; }
                        let brect = Rect::from_min_max(
                            Pos2::new(origin.x + s as f32 * cell.x + 1.0, ry + 1.0),
                            Pos2::new(origin.x + (e + 1) as f32 * cell.x - 1.0, ry + cell.y - 1.0),
                        );
                        painter.rect_filled(brect, 3.0, color);
                        // Inline laser shape thumbnail (runs wide enough to read).
                        // Prefer stored points (legacy per-point colours); else
                        // the library shape tinted by the keyframe colour.
                        if kind == Some(FixtureKind::Laser) && e - s >= 2 {
                            if let Some(kf) = laser_at(channel, s as u32) {
                                if kf.enable {
                                    if !kf.points.is_empty() {
                                        let pts: Vec<crate::patterns::PatternPoint> = kf
                                            .points
                                            .iter()
                                            .map(|p| crate::patterns::PatternPoint {
                                                x: p.x,
                                                y: p.y,
                                                r: p.r,
                                                g: p.g,
                                                b: p.b,
                                            })
                                            .collect();
                                        crate::patterns::paint_pattern(
                                            &painter,
                                            brect.shrink(2.0),
                                            &pts,
                                            None,
                                        );
                                    } else if let Some(pat) = crate::patterns::get(kf.pattern) {
                                        crate::patterns::paint_pattern(
                                            &painter,
                                            brect.shrink(2.0),
                                            &pat.points,
                                            Some([kf.cr, kf.cg, kf.cb]),
                                        );
                                    }
                                }
                            }
                        }
                        f = e + 1;
                    } else {
                        f += 1;
                    }
                }
                // Fixture keyframe markers: turret rows show an aim arrow, others a dot.
                if c >= nl {
                    for &kf in &fixtures[(c - nl) as usize].keyframes {
                        if kf >= nf {
                            continue;
                        }
                        let cx = origin.x + kf as f32 * cell.x + cell.x * 0.5;
                        let cy = ry + cell.y * 0.5;
                        if kind == Some(FixtureKind::Turret) {
                            if let Some(t) = turret_at(channel, kf) {
                                let yaw = (t.pan as f32 / 255.0 - 0.5) * std::f32::consts::PI;
                                let v = egui::vec2(yaw.sin(), -(1.0 - t.tilt as f32 / 255.0)).normalized()
                                    * 6.0;
                                let end = Pos2::new(cx, cy) + v;
                                painter.line_segment([Pos2::new(cx, cy), end], Stroke::new(1.5, COL_KEYFRAME));
                                painter.circle_filled(end, 1.5, COL_TURRET);
                                continue;
                            }
                        }
                        painter.circle_filled(Pos2::new(cx, cy), 1.5, COL_KEYFRAME);
                    }
                }
            }

            // Hover tooltip describing the fixture keyframe under the cursor.
            if let Some(GridHit::Row(c, f)) = resp.hover_pos().map(at) {
                if c >= nl {
                    let tr = &fixtures[(c - nl) as usize];
                    let n = tr.channel + 1;
                    let text = match tr.kind {
                        FixtureKind::Laser => match laser_at(tr.channel, f) {
                            Some(k) if k.enable => format!(
                                "Laser {n} · {} · rgb {} {} {}",
                                crate::patterns::get(k.pattern).map(|p| p.name).unwrap_or("?"),
                                k.cr, k.cg, k.cb
                            ),
                            Some(_) => format!("Laser {n} · off"),
                            None => format!("Laser {n} · —"),
                        },
                        FixtureKind::Turret => match turret_at(tr.channel, f) {
                            Some(t) => format!(
                                "Turret {n} · pan {}° tilt {}° · {}",
                                ((t.pan as f32 / 255.0 - 0.5) * 180.0).round() as i32,
                                (t.tilt as f32 / 255.0 * 90.0).round() as i32,
                                if t.state > 0 { "on" } else { "off" }
                            ),
                            None => format!("Turret {n} · —"),
                        },
                        FixtureKind::Projector => match proj_at(f) {
                            Some(p) => {
                                let gobo = crate::projector_patterns::name_for(p.gallery, p.pattern)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| p.pattern.to_string());
                                format!(
                                    "Laser Projector · {} · colour {} · {}",
                                    gobo,
                                    p.colour,
                                    if p.state > 0 { "on" } else { "off" }
                                )
                            }
                            None => "Laser Projector · —".to_string(),
                        },
                    };
                    resp.clone()
                        .on_hover_ui_at_pointer(|ui| {
                            ui.label(text);
                        });
                }
            }

            // Category dividers.
            for &b in &boundaries {
                if b > first && b < first + count {
                    let dy = row_top + (b - first) as f32 * cell.y;
                    painter.hline(rect.x_range(), dy, Stroke::new(1.0_f32, Color32::from_gray(70)));
                }
            }
            // Beat lines over the rows.
            for (i, &bf) in beats.iter().enumerate() {
                if bf < nf {
                    let bx = origin.x + bf as f32 * cell.x;
                    let col = if i % 4 == 0 { COL_DOWNBEAT } else { COL_BEAT };
                    painter.vline(bx, rows_range, Stroke::new(1.0_f32, col));
                }
            }

            // Ghost: in-hand region (follows cursor) takes precedence over the
            // saved-blueprint preview (at the playhead).
            if let Some(cb) = &app.ghost {
                let ghd = cb.light_held();
                let (bl, bf) = match resp.hover_pos().map(at) {
                    Some(GridHit::Row(c, f)) => (
                        if c < nl { c.min(nl.saturating_sub(cb.light_span.max(1))) } else { 0 },
                        f.min(nf.saturating_sub(cb.frame_span.max(1))),
                    ),
                    _ => (0, cur_frame.min(nf.saturating_sub(cb.frame_span.max(1)))),
                };
                draw_ghost_overlay(&painter, origin, row_top, cell, first, count, nf, bl, bf, &ghd, cb.frame_span);
            } else if let Some(pv) = preview {
                draw_ghost_overlay(&painter, origin, row_top, cell, first, count, nf, 0, pv.base_frame, &pv.held, pv.num_frames);
            }

            // Playhead over ruler + rows.
            let px = origin.x + app.current_frame as f32 * cell.x + cell.x * 0.5;
            painter.vline(px, full_range, Stroke::new(2.0_f32, COL_PLAYHEAD));

            // Selection (and live move-destination preview).
            if let Some(sel) = app.selection {
                draw_selection_overlay(&painter, origin, row_top, cell, first, count, sel, Color32::from_rgba_unmultiplied(90, 200, 250, 40), COL_PLAYHEAD);
            }
            // Paint/resize already preview in real colour via `override_rows`;
            // for a move, also outline where the region will land.
            if let Some(DragKind::Move { orig, grab_row, grab_frame, cur_row, cur_frame }) = &app.drag {
                let drow = *cur_row as i64 - *grab_row as i64;
                let dframe = *cur_frame as i64 - *grab_frame as i64;
                let dst = GridSelection {
                    row_min: (orig.row_min as i64 + drow).clamp(0, total_rows as i64 - 1) as u32,
                    row_max: (orig.row_max as i64 + drow).clamp(0, total_rows as i64 - 1) as u32,
                    frame_min: (orig.frame_min as i64 + dframe).clamp(0, nf as i64 - 1) as u32,
                    frame_max: (orig.frame_max as i64 + dframe).clamp(0, nf as i64 - 1) as u32,
                };
                draw_selection_overlay(&painter, origin, row_top, cell, first, count, dst, Color32::from_rgba_unmultiplied(245, 245, 245, 25), COL_GHOST_EDGE);
            }
        });
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
    match conn.reducers().append_edit(project_id, light, frame, state) {
        Ok(()) => predict(app, &[LightEditInput { light, frame, state }]),
        Err(e) => {
            app.last_error = Some(format!("{e}"));
            app.pending.clear();
        }
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
