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
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll};
use bevy::light::SpotLightTexture;
use bevy::prelude::*;
use bevy::render::view::Hdr;
use bevy_egui::{egui, EguiContexts};

use crate::cookies::PatternCookies;

use crate::conn::{ConnResource, ConnState};
use crate::logic::{apply_pending, expand_held, fold_fixtures, fold_keyframes, turret_pose_at};
use crate::module_bindings::*;
use crate::state::{
    AppState, EmitterPlacement, EmitterPlacements, FixtureGrid, HeldGrid, PendingFixture, Playback,
    PlayheadTime,
};
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

/// Marks every entity spawned for the loaded scene (glTF meshes, fixtures, emitter
/// lights + bodies) so they can be despawned when a new `.glb` is uploaded. The
/// app-provided camera, walls, and floor are NOT tagged (they persist).
#[derive(Component)]
pub struct SceneSpawned;

/// Which rich-fixture family a spawned `SpotLight` emitter belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmitterFamily {
    Laser,
    Turret,
    Projector,
}

/// Marks a real `SpotLight` entity standing in for laser/turret/projector
/// `channel`. `update_emitters` drives its intensity/colour/aim from the folded
/// fixture state each frame.
#[derive(Component)]
pub struct Emitter {
    pub family: EmitterFamily,
    pub channel: u8,
}

/// Spotlight intensities (lumens) for the emitter families. Tuned for the small
/// stage; refined alongside the volumetric beams.
const TURRET_INTENSITY: f32 = 400_000.0;
// Wider laser/projector cones spread their lumens over more wall, so they need
// more intensity than the tight turret beams to read clearly.
const LASER_INTENSITY: f32 = 700_000.0;
const PROJECTOR_INTENSITY: f32 = 800_000.0;

/// Demo mode: when active, every emitter is driven by a synthetic animation
/// (turrets sweep, lasers cycle patterns, projector cycles colour) instead of the
/// project's timeline, so each fixture type can be verified in isolation. Toggle
/// with the `D` key.
#[derive(Resource, Default)]
pub struct DemoMode {
    pub active: bool,
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
/// (stage, walls, fixtures) comes entirely from the Blender scene; the glTF
/// carries no lights, so we add one directional key light here.
pub fn setup_scene_3d(mut commands: Commands, asset_server: Res<AssetServer>) {
    // `orbit_camera` overwrites this transform every frame; the starting values
    // just give a sensible first frame before the orbit resource is read.
    commands.spawn((
        Camera3d::default(),
        Hdr,
        AmbientLight {
            brightness: 40.0,
            ..default()
        },
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

fn fam_name(f: EmitterFamily) -> &'static str {
    match f {
        EmitterFamily::Laser => "Laser",
        EmitterFamily::Turret => "Turret",
        EmitterFamily::Projector => "Projector",
    }
}

/// Parse an emitter node name like `Turret.002` into its family + channel.
fn parse_emitter(name: &str) -> Option<(EmitterFamily, u32)> {
    let name = name.trim();
    for (prefix, fam) in [
        ("Laser.", EmitterFamily::Laser),
        ("Turret.", EmitterFamily::Turret),
        ("Projector.", EmitterFamily::Projector),
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return Some((fam, n));
            }
        }
    }
    None
}

/// Read emitter placements from any `Laser.<n>`/`Turret.<n>`/`Projector.<n>` glTF
/// nodes (mesh-less Empties or fixture models), starting from the built-in
/// defaults and overwriting only the channels that have a node. A fixture casts
/// along its node's local −Z; local +Y is "up".
fn emitter_placements_from_gltf(
    gltf: &Gltf,
    gltf_nodes: &Assets<GltfNode>,
) -> EmitterPlacements {
    let mut placements = EmitterPlacements::default();
    for node_handle in &gltf.nodes {
        let Some(node) = gltf_nodes.get(node_handle) else {
            continue;
        };
        let Some((fam, n)) = parse_emitter(&node.name) else {
            continue;
        };
        let rot = node.transform.rotation;
        // Cast along the node's local +Y (in glТF space). A Blender "Single Arrow"
        // empty points along its local +Z, which the Z-up→Y-up export turns into
        // glТF +Y — so the visible arrow IS the cast direction.
        let p = EmitterPlacement {
            origin: node.transform.translation,
            forward: rot * Vec3::Y,
            up: rot * Vec3::Z,
            scale: 2.0,
        };
        let target = match fam {
            EmitterFamily::Laser => &mut placements.lasers,
            EmitterFamily::Turret => &mut placements.turrets,
            EmitterFamily::Projector => &mut placements.projectors,
        };
        if let Some(slot) = target.get_mut(n as usize) {
            *slot = p;
        }
    }
    placements
}

/// Spawn, per laser/turret/projector channel: an always-visible marker body, a
/// real `SpotLight` that lights the scene, and an additive translucent cone that
/// makes the beam visible in mid-air. The light + beam start hidden; the beam is a
/// child of the light so it sweeps with the (turret) aim. `update_emitters`
/// shows/hides + colours them per the timeline.
fn spawn_emitters(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    cookies: &PatternCookies,
    placements: &EmitterPlacements,
) {
    use std::f32::consts::FRAC_PI_2;
    let body = meshes.add(Sphere::new(0.6));
    let mut spawn_one = |fam: EmitterFamily,
                         ch: usize,
                         p: &EmitterPlacement,
                         inner: f32,
                         outer: f32,
                         shadows: bool,
                         tint: Color,
                         beam_len: f32| {
        let lm = tint.to_linear();
        // Always-visible marker so the placement is findable even when dark.
        let body_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.08, 0.08, 0.1),
            emissive: LinearRgba::rgb(lm.red * 0.5, lm.green * 0.5, lm.blue * 0.5),
            ..default()
        });
        commands.spawn((
            Mesh3d(body.clone()),
            MeshMaterial3d(body_mat),
            Transform::from_translation(p.origin),
            Name::new(format!("{}.{ch:03}.body", fam_name(fam))),
            SceneSpawned,
        ));
        // Additive cone showing the beam in the air. The `Cone` primitive points
        // +Y with its apex at +height/2; rotate +Y -> +Z and shift back by
        // height/2 so the apex sits at the fixture and the cone opens along -Z
        // (the spotlight's forward).
        let beam_radius = (beam_len * outer.tan()).max(0.15);
        let beam_mesh = meshes.add(Cone {
            radius: beam_radius,
            height: beam_len,
        });
        // A faint cool-white shaft so the beam reads as light, not coloured
        // geometry; the spotlight itself carries the colour onto surfaces. Keep a
        // faint hint of the family tint near white.
        let beam_mat = materials.add(StandardMaterial {
            base_color: Color::srgba(
                0.75 + lm.red * 0.25,
                0.78 + lm.green * 0.22,
                0.85 + lm.blue * 0.15,
                0.07,
            ),
            emissive: LinearRgba::rgb(0.12, 0.13, 0.16),
            alpha_mode: AlphaMode::Add,
            cull_mode: None,
            unlit: true,
            ..default()
        });
        let beam_tf = Transform::from_translation(Vec3::new(0.0, 0.0, -beam_len / 2.0))
            .with_rotation(Quat::from_rotation_x(FRAC_PI_2));
        let mut entity = commands.spawn((
            SpotLight {
                color: tint,
                intensity: 0.0, // lit by `update_emitters`
                range: 40.0,
                radius: 0.0,
                inner_angle: inner,
                outer_angle: outer,
                shadows_enabled: shadows,
                ..default()
            },
            Transform::from_translation(p.origin).looking_to(p.forward, p.up),
            Visibility::Hidden, // shown by `update_emitters` when lit
            Emitter {
                family: fam,
                channel: ch as u8,
            },
            Name::new(format!("{}.{ch:03}.light", fam_name(fam))),
            SceneSpawned,
        ));
        entity.with_children(|parent| {
            parent.spawn((
                Mesh3d(beam_mesh),
                MeshMaterial3d(beam_mat),
                beam_tf,
                Name::new(format!("{}.{ch:03}.beam", fam_name(fam))),
            ));
        });
        // Lasers project their pattern as a gobo cookie; `update_emitters` swaps
        // the image to the laser's current pattern. Seed with pattern 0.
        if fam == EmitterFamily::Laser {
            if let Some(h) = cookies.images.first() {
                entity.insert(SpotLightTexture { image: h.clone() });
            }
        }
    };
    // Shadows off everywhere: WebGL2 only comfortably handles a few shadow casters,
    // and the cones/cookies carry the look without them.
    for (i, p) in placements.turrets.iter().enumerate() {
        spawn_one(EmitterFamily::Turret, i, p, 0.12, 0.26, false, Color::srgb(0.6, 0.85, 1.0), 9.0);
    }
    for (i, p) in placements.lasers.iter().enumerate() {
        // A wide-ish cone so the projected gobo shape is large enough to read.
        spawn_one(EmitterFamily::Laser, i, p, 0.18, 0.36, false, Color::srgb(0.4, 1.0, 0.5), 11.0);
    }
    for (i, p) in placements.projectors.iter().enumerate() {
        spawn_one(EmitterFamily::Projector, i, p, 0.24, 0.5, false, Color::srgb(0.85, 0.5, 0.9), 10.0);
    }
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
    cookies: Res<PatternCookies>,
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
            // Center the orbit on the loaded content: take the AABB of every mesh
            // + emitter node, orbit around its center, and frame it by distance.
            // The Blender camera node (if any) only sets the starting azimuth.
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            let mut cam_pos: Option<Vec3> = None;
            for node_handle in &gltf.nodes {
                let Some(node) = gltf_nodes.get(node_handle) else {
                    continue;
                };
                let t = node.transform.translation;
                if node.mesh.is_some() || parse_emitter(&node.name).is_some() {
                    min = min.min(t);
                    max = max.max(t);
                } else if node.name.starts_with("Camera") {
                    cam_pos = Some(t);
                }
            }
            if min.x.is_finite() {
                let center = (min + max) * 0.5;
                orbit.center = center;
                // Frame the whole scene with margin (scroll wheel adjusts live).
                orbit.radius = ((max - min).length() * 1.2).max(5.0);
                if let Some(cam) = cam_pos {
                    let d = cam - center;
                    orbit.base_angle = d.x.atan2(d.z);
                }
                info!(
                    "framed orbit on scene: center ({:.1}, {:.1}, {:.1}), radius {:.1}",
                    center.x, center.y, center.z, orbit.radius
                );
            }
            // Spawn every mesh node (so the whole Blender scene is visible);
            // nodes named `Light.<n>` become toggleable fixtures, everything
            // else is static set geometry. (Assumes a flat scene hierarchy —
            // node transforms are used directly.)
            let mut light_count = 0u32;
            let mut mesh_count = 0u32;
            // Fallback material for static meshes that ship without one (Bevy's PBR
            // pipeline skips meshes that have no material, making them invisible).
            let default_static = materials.add(StandardMaterial {
                base_color: Color::srgb(0.5, 0.5, 0.55),
                perceptual_roughness: 0.9,
                ..default()
            });
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
                        SceneSpawned,
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
                            // Static set geometry: the glTF material, or the
                            // fallback so material-less meshes still render.
                            let mat = prim.material.clone().unwrap_or_else(|| default_static.clone());
                            entity.insert(MeshMaterial3d(mat));
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
            // Real spotlight emitters (lasers / turrets / gobo projector), placed
            // from any emitter nodes in the scene or the built-in defaults.
            let placements = emitter_placements_from_gltf(gltf, &gltf_nodes);
            spawn_emitters(&mut commands, &mut meshes, &mut materials, &cookies, &placements);
            commands.insert_resource(placements);
            scene.spawned = true;
        }
        LoadState::Failed(_) => {
            warn!("failed to load {SCENE_GLB}; using procedural fixtures");
            spawn_procedural(&mut commands, &mut meshes, &mut materials);
            let placements = EmitterPlacements::default();
            spawn_emitters(&mut commands, &mut meshes, &mut materials, &cookies, &placements);
            commands.insert_resource(placements);
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
            SceneSpawned,
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

/// Scroll wheel zooms the orbit (dollies the camera in/out), unless the pointer is
/// over an egui panel.
pub fn camera_zoom(
    mut contexts: EguiContexts,
    scroll: Res<AccumulatedMouseScroll>,
    mut orbit: ResMut<CameraOrbit>,
) {
    let over_ui = contexts
        .ctx_mut()
        .map(|c| c.wants_pointer_input())
        .unwrap_or(false);
    if over_ui || scroll.delta.y == 0.0 {
        return;
    }
    orbit.radius = (orbit.radius * (1.0 - scroll.delta.y * 0.1)).clamp(2.0, 2000.0);
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

    // Splice in just-edited (not-yet-echoed) fixture keyframes so the 3D view
    // reacts instantly, mirroring the timeline's optimistic feedback.
    let mut lasers: Vec<LaserKeyframe> = c
        .db()
        .laser_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    let mut projectors: Vec<ProjectorKeyframe> = c
        .db()
        .projector_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    let mut turrets: Vec<TurretKeyframe> = c
        .db()
        .turret_kf()
        .iter()
        .filter(|r| r.project_id == pid)
        .collect();
    for pf in &app.pending_fixtures {
        match pf {
            PendingFixture::Laser(p) if p.project_id == pid => {
                lasers.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                lasers.push(p.clone());
            }
            PendingFixture::Turret(p) if p.project_id == pid => {
                turrets.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                turrets.push(p.clone());
            }
            PendingFixture::Projector(p) if p.project_id == pid => {
                projectors.retain(|r| !(r.channel == p.channel && r.frame == p.frame));
                projectors.push(p.clone());
            }
            _ => {}
        }
    }
    fx.lasers = fold_fixtures(&lasers, frame, NUM_LASERS, |r| r.channel, |r| r.frame);
    fx.projectors = fold_fixtures(&projectors, frame, NUM_PROJECTORS, |r| r.channel, |r| r.frame);
    fx.turrets = fold_fixtures(&turrets, frame, NUM_TURRETS, |r| r.channel, |r| r.frame);
    // Keep the full turret keyframe set so the render/animation systems can tween
    // between keyframes (held semantics above are still used for the timeline).
    fx.turret_rows = turrets;
}

/// Publish the continuous playhead (`current_frame` + sub-frame fraction) so
/// fixtures can interpolate smoothly between integer keyframes. Runs after the
/// frame-advance systems and before `recompute_fixtures`. Paused => fraction 0.
pub fn publish_playhead(app: Res<AppState>, pb: Res<Playback>, mut ph: ResMut<PlayheadTime>) {
    let frac = if !pb.playing {
        0.0
    } else if pb.audio_driven {
        pb.audio_fraction
    } else {
        pb.accumulator
    };
    ph.t = app.current_frame as f32 + frac.clamp(0.0, 1.0);
}

/// 3-bit (0..=7) per-channel laser colour → linear-ish display colour.
fn laser_color(r: u8, g: u8, b: u8) -> Color {
    Color::srgb(r as f32 / 7.0, g as f32 / 7.0, b as f32 / 7.0)
}

/// Beam direction for a moving head from its (possibly interpolated) DMX pan/tilt
/// values (0..=255).
fn turret_dir_f(pan: f32, tilt: f32) -> Vec3 {
    let yaw = (pan / 255.0 - 0.5) * std::f32::consts::PI; // ±90°
    let pitch = (tilt / 255.0) * std::f32::consts::FRAC_PI_2; // 0..90°
    let rot = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
    // Aim from +Y (180° flip of the original -Y base) so the heads point the
    // right way up in the scene.
    (rot * Vec3::Y).normalize()
}

/// Gobo projector DMX colour byte → a display colour.
fn gobo_color(colour: u8) -> Color {
    Color::hsl((colour as f32 / 255.0) * 360.0, 0.85, 0.6)
}

/// Drive every emitter `SpotLight` from the folded fixture state: intensity
/// (0 = off), colour, and — for turrets — the eased aim at the continuous
/// playhead so the cone sweeps smoothly. Runs after `recompute_fixtures`.
pub fn update_emitters(
    time: Res<Time>,
    demo: Res<DemoMode>,
    app: Res<AppState>,
    fx: Res<FixtureGrid>,
    placements: Res<EmitterPlacements>,
    playhead: Res<PlayheadTime>,
    cookies: Res<PatternCookies>,
    mut q: Query<(
        &Emitter,
        &mut SpotLight,
        &mut Transform,
        &mut Visibility,
        Option<&mut SpotLightTexture>,
    )>,
) {
    let has_project = app.open_project.is_some();
    let demo_t = time.elapsed_secs();
    for (em, mut light, mut tf, mut vis, tex) in &mut q {
        let mut lit = false;
        if demo.active {
            // Synthetic animation so every fixture type is visibly exercised.
            match em.family {
                EmitterFamily::Turret => {
                    let ph = demo_t * 0.9 + em.channel as f32 * 1.2;
                    let pan = (127.0 + 115.0 * ph.sin()).clamp(0.0, 255.0);
                    let tilt = (140.0 + 90.0 * (ph * 0.6).cos()).clamp(0.0, 255.0);
                    if let Some(p) = placements.turrets.get(em.channel as usize) {
                        let dir = turret_dir_f(pan, tilt);
                        let up = if dir.dot(Vec3::Y).abs() > 0.95 {
                            Vec3::Z
                        } else {
                            Vec3::Y
                        };
                        *tf = Transform::from_translation(p.origin).looking_to(dir, up);
                    }
                    light.color = Color::srgb(0.7, 0.85, 1.0);
                    light.intensity = TURRET_INTENSITY;
                    lit = true;
                }
                EmitterFamily::Laser => {
                    let count = cookies.images.len().max(1);
                    let pat = ((demo_t * 0.8) as usize + em.channel as usize) % count;
                    light.color =
                        Color::hsl((demo_t * 40.0 + em.channel as f32 * 70.0) % 360.0, 0.9, 0.6);
                    light.intensity = LASER_INTENSITY;
                    if let (Some(mut tex), Some(h)) = (tex, cookies.images.get(pat)) {
                        tex.image = h.clone();
                    }
                    lit = true;
                }
                EmitterFamily::Projector => {
                    light.color = Color::hsl((demo_t * 30.0) % 360.0, 0.8, 0.6);
                    light.intensity = PROJECTOR_INTENSITY;
                    lit = true;
                }
            }
        } else if has_project {
            match em.family {
                EmitterFamily::Turret => {
                    if let Some(pose) = turret_pose_at(&fx.turret_rows, em.channel, playhead.t) {
                        if pose.on {
                            if let Some(p) = placements.turrets.get(em.channel as usize) {
                                let dir = turret_dir_f(pose.pan, pose.tilt);
                                // Pick a non-parallel up so `looking_to` stays stable
                                // whether the head points down or out.
                                let up = if dir.dot(Vec3::Y).abs() > 0.95 {
                                    Vec3::Z
                                } else {
                                    Vec3::Y
                                };
                                *tf = Transform::from_translation(p.origin).looking_to(dir, up);
                            }
                            light.color = Color::srgb(0.6, 0.85, 1.0);
                            light.intensity = TURRET_INTENSITY;
                            lit = true;
                        }
                    }
                }
                EmitterFamily::Laser => {
                    if let Some(Some(l)) = fx.lasers.get(em.channel as usize) {
                        if l.enable {
                            light.color = laser_color(l.cr, l.cg, l.cb);
                            light.intensity = LASER_INTENSITY;
                            // Project the laser's current pattern as a gobo cookie.
                            if let (Some(mut tex), Some(h)) =
                                (tex, cookies.images.get(l.pattern as usize))
                            {
                                tex.image = h.clone();
                            }
                            lit = true;
                        }
                    }
                }
                EmitterFamily::Projector => {
                    if let Some(Some(pr)) = fx.projectors.get(em.channel as usize) {
                        if pr.state > 0 {
                            light.color = gobo_color(pr.colour);
                            light.intensity = PROJECTOR_INTENSITY;
                            lit = true;
                        }
                    }
                }
            }
        }
        if !lit {
            light.intensity = 0.0;
        }
        // Show the light + its beam cone only when lit.
        *vis = if lit {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
}

/// Draw the gobo projector's "Pattern N" title as a screen-space label anchored
/// where its beam meets the back wall. Uses an egui overlay since the trimmed
/// feature set has no in-world text.
pub fn draw_projector_label(
    mut contexts: EguiContexts,
    app: Res<AppState>,
    fx: Res<FixtureGrid>,
    placements: Res<EmitterPlacements>,
    cam: Query<(&Camera, &GlobalTransform), With<OrbitCamera>>,
) {
    if app.open_project.is_none() {
        return;
    }
    let Some(Some(pr)) = fx.projectors.first() else {
        return;
    };
    if pr.state == 0 {
        return;
    }
    let Some(p) = placements.projectors.first() else {
        return;
    };
    // Anchor the label out along the projector's beam (roughly where it lands on
    // the wall it faces). A fixed throw avoids needing a raycast against geometry.
    const THROW: f32 = 18.0;
    let hit = p.origin + p.forward.normalize_or_zero() * THROW;

    let Ok((camera, cam_tf)) = cam.single() else {
        return;
    };
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let Ok(px) = camera.world_to_viewport(cam_tf, hit) else {
        return; // behind camera / off screen
    };
    let ppp = ctx.pixels_per_point();
    let pos = egui::pos2(px.x / ppp, px.y / ppp);
    let c = gobo_color(pr.colour).to_srgba();
    let col = egui::Color32::from_rgb(
        (c.red * 255.0) as u8,
        (c.green * 255.0) as u8,
        (c.blue * 255.0) as u8,
    );
    let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("projector_label"));
    let painter = ctx.layer_painter(layer);
    painter.text(
        pos,
        egui::Align2::CENTER_CENTER,
        crate::projector_patterns::name_for(pr.gallery, pr.pattern)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Pattern {}", pr.pattern)),
        egui::FontId::proportional(18.0),
        col,
    );
}

/// Toggle the fixture demo with the `D` key (ignored while typing in a text box).
pub fn toggle_demo(
    keys: Res<ButtonInput<KeyCode>>,
    mut contexts: EguiContexts,
    mut demo: ResMut<DemoMode>,
) {
    let typing = contexts
        .ctx_mut()
        .map(|c| c.wants_keyboard_input())
        .unwrap_or(false);
    if !typing && keys.just_pressed(KeyCode::KeyD) {
        demo.active = !demo.active;
    }
}

/// Show a banner while the fixture demo is running.
pub fn draw_demo_overlay(mut contexts: EguiContexts, demo: Res<DemoMode>) {
    if !demo.active {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui::Area::new(egui::Id::new("demo_overlay"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 8.0))
        .show(ctx, |ui| {
            ui.colored_label(
                egui::Color32::from_rgb(120, 230, 255),
                "DEMO MODE — every fixture is animating · press D to exit",
            );
        });
}
