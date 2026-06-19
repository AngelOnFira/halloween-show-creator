//! Light Show Editor — a browser (wasm) app hosted by Bevy, with the egui UI
//! drawn as an overlay via bevy_egui, backed by SpacetimeDB (every edit stored
//! as an append-only event → perfect version control + time travel). A 3D
//! viewport's fixtures light up to mirror the timeline.

mod audio;
#[cfg(target_arch = "wasm32")]
mod auth;
mod conn;
mod export;
mod logic;
mod module_bindings;
mod patterns;
mod projector_patterns;
mod scene;
mod state;
mod stick_font;
mod ui;
mod upload;

use bevy::asset::io::memory::{Dir, MemoryAssetReader};
use bevy::asset::io::AssetSourceBuilder;
use bevy::asset::AssetApp;
use bevy::prelude::*;
use bevy_egui::{EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass};

use state::{
    AppState, EmitterPlacements, FixtureGrid, HeldGrid, Playback, PlayheadTime, Viewport3dRect,
};

fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    // In-memory asset source backing runtime `.glb` uploads. Registered before
    // the AssetPlugin (in DefaultPlugins) so `upload://…` paths resolve.
    let upload_dir = Dir::default();
    let reader_dir = upload_dir.clone();

    App::new()
        .register_asset_source(
            "upload",
            AssetSourceBuilder::new(move || Box::new(MemoryAssetReader { root: reader_dir.clone() })),
        )
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        canvas: Some("#the_canvas_id".into()),
                        fit_canvas_to_parent: true,
                        ..default()
                    }),
                    ..default()
                })
                // On wasm the dev server answers `.meta` requests with a 200 SPA
                // fallback; without this Bevy tries to parse that as asset meta
                // and the glTF load fails.
                .set(AssetPlugin {
                    meta_check: bevy::asset::AssetMetaCheck::Never,
                    ..default()
                }),
        )
        .add_plugins(EguiPlugin::default())
        // Host egui on a dedicated full-window camera (spawned in `setup_scene_3d`)
        // rather than auto-attaching the primary context to the first camera (our
        // 3D orbit camera). bevy_egui derives egui's screen rect from its context
        // camera's viewport, so sharing the orbit camera would make `apply_3d_viewport`
        // shrink egui's own surface every frame — a runaway resize feedback loop.
        .insert_resource(EguiGlobalSettings {
            auto_create_primary_context: false,
            ..Default::default()
        })
        .init_resource::<AppState>()
        .init_resource::<HeldGrid>()
        .init_resource::<FixtureGrid>()
        .init_resource::<Playback>()
        .init_resource::<PlayheadTime>()
        .init_resource::<EmitterPlacements>()
        .init_resource::<scene::DemoMode>()
        .init_resource::<scene::CameraOrbit>()
        .init_resource::<Viewport3dRect>()
        .insert_resource(upload::SceneUpload {
            dir: upload_dir,
            version: 0,
        })
        .add_systems(Startup, scene::setup_scene_3d)
        .add_systems(Startup, conn::setup_connection)
        .add_systems(Startup, audio::setup_audio)
        .add_systems(PreUpdate, conn::pump_connection)
        .add_systems(Update, scene::spawn_gltf_fixtures)
        .add_systems(
            Update,
            (
                conn::sync_subscriptions,
                audio::drive_upload,
                audio::ensure_audio_buffer,
                audio::sync_tempo,
            ),
        )
        .add_systems(
            Update,
            (
                scene::recompute_held,
                audio::audio_playback_sync,
                scene::playback_advance,
                scene::publish_playhead,
                scene::recompute_fixtures,
                scene::update_emitters,
                scene::draw_laser_patterns,
                scene::draw_projector_pattern,
                scene::apply_lights,
            )
                .chain(),
        )
        .add_systems(Update, (scene::camera_drag, scene::orbit_camera).chain())
        .add_systems(Update, scene::camera_zoom)
        .add_systems(Update, scene::toggle_demo)
        .add_systems(Update, upload::drive_glb_upload)
        .add_systems(EguiPrimaryContextPass, ui::ui_system)
        // Runs right after `ui_system` (same nested schedule) so it reads the central
        // rect that frame; `camera_system` (PostUpdate) then re-fits the camera to it.
        .add_systems(
            EguiPrimaryContextPass,
            scene::apply_3d_viewport.after(ui::ui_system),
        )
        .add_systems(EguiPrimaryContextPass, scene::draw_demo_overlay)
        .add_systems(EguiPrimaryContextPass, upload::upload_ui)
        .run();
}
