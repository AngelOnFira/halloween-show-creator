//! The 3D viewport: a Blender-authored glTF scene whose fixtures light up to
//! mirror the timeline.
//!
//! Workflow: author a scene in Blender, name the light objects `Light.000`,
//! `Light.001`, … (matching timeline light indices), and export it as
//! `assets/scenes/fixtures.glb`. We load it as a `Gltf` asset and spawn each
//! node named `Light.<n>` directly as a `LightFixture { index: n }` with its
//! own material clone — bypassing Bevy's reflection-based `Scene` spawning
//! (which needs many type registrations on this trimmed feature set). If the
//! `.glb` is missing or has no `Light.<n>` nodes, a procedural row of fixtures
//! is used instead. Each frame we read the held on/off grid (`HeldGrid`) and
//! set each fixture's emissive accordingly.

use bevy::asset::LoadState;
use bevy::gltf::{Gltf, GltfMesh, GltfNode};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy_egui::EguiContexts;

use crate::conn::{ConnResource, ConnState};
use crate::logic::{apply_pending, expand_held, fold_fixtures, fold_keyframes};
use crate::module_bindings::*;
use crate::state::{AppState, FixtureGrid, HeldGrid, Playback};
use spacetimedb_sdk::{DbContext, Table};

/// Channel counts for the rich fixtures (mirrors the legacy hardware layout).
const NUM_LASERS: usize = 5;
const NUM_PROJECTORS: usize = 1;
const NUM_TURRETS: usize = 4;

/// The bundled scene. Replace this `.glb` with your Blender export (keep the
/// `Light.<n>` object naming) — trunk copies `assets/` into the served site.
const SCENE_GLB: &str = "scenes/fixtures.glb";

/// Number of fixtures in the procedural fallback scene.
const DEFAULT_FIXTURES: u32 = 12;

/// Marks an entity as the visual for timeline light `index`.
#[derive(Component)]
pub struct LightFixture {
    pub index: u32,
}

/// Tracks the glTF scene load so we spawn fixtures exactly once.
#[derive(Resource)]
pub struct GltfScene {
    pub handle: Handle<Gltf>,
    pub spawned: bool,
}

/// Marks the orbiting viewport camera so the orbit/drag systems can target it.
#[derive(Component)]
pub struct OrbitCamera;

/// Defines an orbit of the camera around `center`. The downward tilt (`pitch`)
/// is fixed at 45° for the RTS look; `radius` (zoom) and `base_angle` (starting
/// azimuth) are seeded from the GLB camera node when the scene loads. The
/// user-controlled azimuth lives in `AppState::camera_angle` (degrees) and is
/// added to `base_angle` each frame by `orbit_camera`.
#[derive(Resource)]
pub struct CameraOrbit {
    /// Look-at target / orbit center.
    pub center: Vec3,
    /// 3D distance from `center` to the camera.
    pub radius: f32,
    /// Elevation angle above the horizontal plane (radians).
    pub pitch: f32,
    /// Azimuth offset (radians) applied before the user's `camera_angle`.
    pub base_angle: f32,
}

impl Default for CameraOrbit {
    fn default() -> Self {
        Self {
            center: Vec3::new(0.0, 0.6, 0.0),
            radius: 14.0,
            // 40° elevation for the RTS-style top-down look.
            pitch: 40.0_f32.to_radians(),
            base_angle: 0.0,
        }
    }
}

/// Colors / intensity for the on/off look.
#[derive(Resource)]
pub struct SceneConfig {
    pub on_color: Color,
    pub off_color: Color,
    pub emissive_strength: f32,
}

impl Default for SceneConfig {
    fn default() -> Self {
        Self {
            on_color: Color::srgb(1.0, 0.82, 0.35),
            off_color: Color::srgb(0.07, 0.07, 0.09),
            emissive_strength: 6.0,
        }
    }
}

/// Startup: camera, key light, and kick off the glTF load. The set geometry
/// (fixtures + house) comes entirely from the Blender scene; the glTF carries no
/// lights, so we add one directional key light here.
pub fn setup_scene_3d(mut commands: Commands, asset_server: Res<AssetServer>) {
    // `orbit_camera` overwrites this transform every frame; the starting values
    // just give a sensible first frame before the orbit resource is read.
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 3.5, 13.0).looking_at(Vec3::new(0.0, 0.6, 0.0), Vec3::Y),
        OrbitCamera,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 2500.0,
            ..default()
        },
        Transform::from_xyz(3.0, 8.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    commands.insert_resource(GltfScene {
        handle: asset_server.load(SCENE_GLB),
        spawned: false,
    });
    commands.insert_resource(SceneConfig::default());
}

fn parse_light_index(name: &str) -> Option<u32> {
    name.strip_prefix("Light.")
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Spawn a fixture per `Light.<n>` glTF node once the asset has loaded (or a
/// procedural row if the load fails / has no such nodes).
pub fn spawn_gltf_fixtures(
    mut commands: Commands,
    mut scene: ResMut<GltfScene>,
    mut orbit: ResMut<CameraOrbit>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    gltf_meshes: Res<Assets<GltfMesh>>,
    gltf_nodes: Res<Assets<GltfNode>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if scene.spawned {
        return;
    }
    match asset_server.load_state(&scene.handle) {
        LoadState::Loaded => {
            let Some(gltf) = gltfs.get(&scene.handle) else {
                return;
            };
            // Seed the orbit (zoom + starting azimuth) from the Blender camera
            // node if present — it has no mesh but still appears in `gltf.nodes`
            // by name with its world transform. Pitch stays fixed at 45°.
            for node_handle in &gltf.nodes {
                let Some(node) = gltf_nodes.get(node_handle) else {
                    continue;
                };
                if node.mesh.is_none() && node.name.starts_with("Camera") {
                    let d = node.transform.translation - orbit.center;
                    orbit.radius = d.length();
                    orbit.base_angle = d.x.atan2(d.z);
                    info!(
                        "seeded camera orbit from glTF node '{}': radius {:.2}, base_angle {:.1}°",
                        node.name,
                        orbit.radius,
                        orbit.base_angle.to_degrees()
                    );
                    break;
                }
            }
            // Spawn every mesh node (so the whole Blender scene is visible);
            // nodes named `Light.<n>` become toggleable fixtures, everything
            // else is static set geometry. (Assumes a flat scene hierarchy —
            // node transforms are used directly.)
            let mut light_count = 0u32;
            let mut mesh_count = 0u32;
            for node_handle in &gltf.nodes {
                let Some(node) = gltf_nodes.get(node_handle) else {
                    continue;
                };
                let Some(mesh_handle) = &node.mesh else { continue };
                let Some(gltf_mesh) = gltf_meshes.get(mesh_handle) else {
                    continue;
                };
                let light_index = parse_light_index(&node.name);
                for prim in &gltf_mesh.primitives {
                    mesh_count += 1;
                    let mut entity = commands.spawn((
                        Mesh3d(prim.mesh.clone()),
                        node.transform,
                        Name::new(node.name.clone()),
                    ));
                    match light_index {
                        Some(index) => {
                            // Fixture: own material clone so we can toggle emissive.
                            let base = prim
                                .material
                                .as_ref()
                                .and_then(|m| materials.get(m))
                                .cloned()
                                .unwrap_or_default();
                            let mat = materials.add(base);
                            entity.insert((MeshMaterial3d(mat), LightFixture { index }));
                            light_count += 1;
                        }
                        None => {
                            // Static set geometry: use the glTF material as-is.
                            if let Some(m) = &prim.material {
                                entity.insert(MeshMaterial3d(m.clone()));
                            }
                        }
                    }
                }
            }
            if mesh_count == 0 {
                warn!("{SCENE_GLB} has no meshes; using procedural fixtures");
                spawn_procedural(&mut commands, &mut meshes, &mut materials);
            } else {
                info!(
                    "spawned {light_count} glTF light fixtures (+{} static meshes) from {SCENE_GLB}",
                    mesh_count - light_count
                );
            }
            scene.spawned = true;
        }
        LoadState::Failed(_) => {
            warn!("failed to load {SCENE_GLB}; using procedural fixtures");
            spawn_procedural(&mut commands, &mut meshes, &mut materials);
            scene.spawned = true;
        }
        _ => {}
    }
}

/// Procedural fallback: a row of named cube fixtures.
fn spawn_procedural(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let n = DEFAULT_FIXTURES;
    let spacing = 1.4;
    let span = (n as f32 - 1.0) * spacing;
    let bulb = meshes.add(Cuboid::new(0.55, 0.55, 0.55));
    for i in 0..n {
        let x = i as f32 * spacing - span / 2.0;
        let mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.07, 0.07, 0.09),
            emissive: LinearRgba::BLACK,
            ..default()
        });
        commands.spawn((
            Mesh3d(bulb.clone()),
            MeshMaterial3d(mat),
            Transform::from_xyz(x, 0.6, 0.0),
            Name::new(format!("Light.{i:03}")),
            LightFixture { index: i },
        ));
    }
}

/// Degrees of orbit azimuth per pixel of horizontal mouse drag.
const DRAG_SENSITIVITY: f32 = 0.3;

/// Position the camera each frame on its orbit: fixed 45° elevation, azimuth =
/// `base_angle` (from the GLB) + the user-controlled `camera_angle` (degrees).
pub fn orbit_camera(
    orbit: Res<CameraOrbit>,
    app: Res<AppState>,
    mut q: Query<&mut Transform, With<OrbitCamera>>,
) {
    let theta = orbit.base_angle + app.camera_angle.to_radians();
    let horizontal = orbit.radius * orbit.pitch.cos();
    let height = orbit.radius * orbit.pitch.sin();
    let pos = orbit.center
        + Vec3::new(
            horizontal * theta.sin(),
            height,
            horizontal * theta.cos(),
        );
    for mut t in &mut q {
        *t = Transform::from_translation(pos).looking_at(orbit.center, Vec3::Y);
    }
}

/// Click-drag anywhere on the 3D scene (i.e. not over an egui panel) to orbit
/// the camera, mutating the same `camera_angle` the topbar slider drives.
pub fn camera_drag(
    mut contexts: EguiContexts,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    mut app: ResMut<AppState>,
) {
    // Ignore drags that egui is consuming (over a panel/widget).
    let over_ui = contexts
        .ctx_mut()
        .map(|c| c.wants_pointer_input())
        .unwrap_or(false);
    if over_ui || !mouse.pressed(MouseButton::Left) {
        return;
    }
    let dx = motion.delta.x;
    if dx != 0.0 {
        app.camera_angle = (app.camera_angle - dx * DRAG_SENSITIVITY).rem_euclid(360.0);
    }
}

/// Recompute the held on/off grid for the open project (read by the 3D apply
/// system). Mirrors the inline computation the UI does.
pub fn recompute_held(conn: NonSend<ConnResource>, app: Res<AppState>, mut grid: ResMut<HeldGrid>) {
    let guard = conn.state.borrow();
    let ConnState::Connected(c) = &*guard else {
        return;
    };
    let Some(pid) = app.open_project else {
        grid.held.clear();
        grid.nl = 0;
        grid.nf = 0;
        return;
    };
    let Some(project) = c.db().project().iter().find(|p| p.id == pid) else {
        return;
    };
    let mut edits: Vec<Edit> = c
        .db()
        .edit_log()
        .iter()
        .filter(|e| e.project_id == pid)
        .collect();
    edits.sort_by_key(|e| e.seq);
    let head = project.head_seq;
    let cutoff = app.history_pos.unwrap_or(head).min(head);
    let viewing_history = app.history_pos.is_some_and(|p| p < head);
    let mut kf = fold_keyframes(&edits, cutoff);
    // Mirror the UI's optimistic overlay so the 3D view reacts instantly too.
    if !viewing_history {
        apply_pending(&mut kf, &app.pending);
    }
    grid.held = expand_held(&kf, project.num_lights, project.num_frames);
    grid.keyframes = kf;
    grid.nl = project.num_lights;
    grid.nf = project.num_frames;
    grid.head = head;
    grid.viewing_history = app.history_pos.is_some_and(|p| p < head);
}

/// Advance the playhead over real time while playing (frame-rate independent).
pub fn playback_advance(
    time: Res<Time>,
    mut app: ResMut<AppState>,
    mut pb: ResMut<Playback>,
    grid: Res<HeldGrid>,
) {
    if !pb.playing || pb.fps <= 0.0 || grid.nf == 0 || pb.audio_driven {
        return;
    }
    pb.accumulator += time.delta_secs() * pb.fps;
    if pb.accumulator < 1.0 {
        return;
    }
    let steps = pb.accumulator.floor();
    pb.accumulator -= steps;
    let nf = grid.nf as u64;
    let next = app.current_frame as u64 + steps as u64;
    if pb.looping {
        app.current_frame = (next % nf) as u32;
    } else if next >= nf {
        app.current_frame = (nf - 1) as u32;
        pb.playing = false;
    } else {
        app.current_frame = next as u32;
    }
}

/// Set each fixture's emissive from `held[index][current_frame]`.
pub fn apply_lights(
    grid: Res<HeldGrid>,
    app: Res<AppState>,
    cfg: Res<SceneConfig>,
    fixtures: Query<(&LightFixture, &MeshMaterial3d<StandardMaterial>)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let frame = app.current_frame as usize;
    let has_project = app.open_project.is_some();
    let lin = cfg.on_color.to_linear();
    let s = cfg.emissive_strength;
    let on_emissive = LinearRgba::rgb(lin.red * s, lin.green * s, lin.blue * s);

    for (fix, mat_handle) in &fixtures {
        let on = has_project
            && grid
                .held
                .get(fix.index as usize)
                .and_then(|row| row.get(frame))
                .copied()
                .unwrap_or(false);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            if on {
                mat.emissive = on_emissive;
                mat.base_color = cfg.on_color;
            } else {
                mat.emissive = LinearRgba::BLACK;
                mat.base_color = cfg.off_color;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rich fixtures (lasers / gobo projector / turrets), imported from legacy shows
// and drawn each frame as gizmo lines. View-only: there is no editor for them.
// ---------------------------------------------------------------------------

/// Fold the per-project fixture keyframe tables down to the state in effect at
/// the current playhead frame (held semantics), into `FixtureGrid`.
pub fn recompute_fixtures(
    conn: NonSend<ConnResource>,
    app: Res<AppState>,
    mut fx: ResMut<FixtureGrid>,
) {
    let guard = conn.state.borrow();
    let ConnState::Connected(c) = &*guard else {
        return;
    };
    let Some(pid) = app.open_project else {
        fx.lasers.clear();
        fx.projectors.clear();
        fx.turrets.clear();
        return;
    };
    let frame = app.current_frame;

    let lasers: Vec<LaserKeyframe> = c
        .db()
        .laser_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    fx.lasers = fold_fixtures(&lasers, frame, NUM_LASERS, |r| r.channel, |r| r.frame);

    let projectors: Vec<ProjectorKeyframe> = c
        .db()
        .projector_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    fx.projectors = fold_fixtures(&projectors, frame, NUM_PROJECTORS, |r| r.channel, |r| r.frame);

    let turrets: Vec<TurretKeyframe> = c
        .db()
        .turret_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    fx.turrets = fold_fixtures(&turrets, frame, NUM_TURRETS, |r| r.channel, |r| r.frame);
}

/// Map a legacy laser galvo point (x,y in 0..=300) onto the projection plane
/// behind the fixtures.
fn laser_to_world(x: i16, y: i16) -> Vec3 {
    let nx = (x as f32 / 300.0 - 0.5) * 10.0;
    let ny = 0.5 + (y as f32 / 300.0) * 5.0;
    Vec3::new(nx, ny, -4.0)
}

/// 3-bit (0..=7) per-channel laser colour → linear-ish display colour.
fn laser_color(r: u8, g: u8, b: u8) -> Color {
    Color::srgb(r as f32 / 7.0, g as f32 / 7.0, b as f32 / 7.0)
}

/// Base (mount) position of turret `i`, spread across the top of the scene.
fn turret_base(i: usize) -> Vec3 {
    Vec3::new(-3.0 + i as f32 * 2.0, 5.5, 1.0)
}

/// Beam direction for a moving head from its DMX pan/tilt bytes.
fn turret_dir(pan: u8, tilt: u8) -> Vec3 {
    let yaw = (pan as f32 / 255.0 - 0.5) * std::f32::consts::PI; // ±90°
    let pitch = (tilt as f32 / 255.0) * std::f32::consts::FRAC_PI_2; // 0..90° downward
    let rot = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
    (rot * Vec3::NEG_Y).normalize()
}

/// Gobo projector DMX colour byte → a display colour.
fn gobo_color(colour: u8) -> Color {
    Color::hsl((colour as f32 / 255.0) * 360.0, 0.85, 0.6)
}

/// Draw the rich fixtures for the open project as emissive gizmo lines.
pub fn draw_fixtures(app: Res<AppState>, fx: Res<FixtureGrid>, mut gizmos: Gizmos) {
    if app.open_project.is_none() {
        return;
    }

    // Lasers: each active laser draws its path as a colour-graded line strip.
    for laser in fx.lasers.iter().flatten() {
        if !laser.enable || laser.points.len() < 2 {
            continue;
        }
        let pts = laser
            .points
            .iter()
            .map(|p| (laser_to_world(p.x, p.y), laser_color(p.r, p.g, p.b)));
        gizmos.linestrip_gradient(pts);
    }

    // Gobo projector: a coloured ring on the back wall when lit.
    if let Some(Some(proj)) = fx.projectors.first() {
        if proj.state > 0 {
            let center = Vec3::new(0.0, 4.0, -3.95);
            let color = gobo_color(proj.colour);
            let ring = (0..=24).map(|i| {
                let a = i as f32 / 24.0 * std::f32::consts::TAU;
                center + Vec3::new(a.cos() * 1.6, a.sin() * 1.6, 0.0)
            });
            gizmos.linestrip(ring, color);
        }
    }

    // Turrets: a beam from each lit moving head along its pan/tilt direction.
    for (i, turret) in fx.turrets.iter().enumerate() {
        if let Some(t) = turret {
            if t.state > 0 {
                let base = turret_base(i);
                let end = base + turret_dir(t.pan, t.tilt) * 6.0;
                gizmos.line(base, end, Color::srgb(0.6, 0.9, 1.0));
            }
        }
    }
}
