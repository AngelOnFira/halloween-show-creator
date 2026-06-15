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
use bevy::prelude::*;

use crate::conn::{ConnResource, ConnState};
use crate::logic::{expand_held, fold_keyframes};
use crate::module_bindings::*;
use crate::state::{AppState, HeldGrid, Playback};
use spacetimedb_sdk::{DbContext, Table};

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

/// Startup: camera, key light, ground, and kick off the glTF load.
pub fn setup_scene_3d(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    asset_server: Res<AssetServer>,
) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 3.5, 13.0).looking_at(Vec3::new(0.0, 0.6, 0.0), Vec3::Y),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 2500.0,
            ..default()
        },
        Transform::from_xyz(3.0, 8.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Dark ground for context (a thin slab).
    let ground_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.04, 0.04, 0.06),
        perceptual_roughness: 0.95,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(22.0, 0.1, 8.0))),
        MeshMaterial3d(ground_mat),
        Transform::from_xyz(0.0, -0.25, 0.0),
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
    let kf = fold_keyframes(&edits, cutoff);
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
    if !pb.playing || pb.fps <= 0.0 || grid.nf == 0 {
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
