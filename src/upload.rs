//! Runtime glТF upload: pick a `.glb` in the browser and hot-swap the 3D scene
//! without rebuilding. Picked bytes are written into an in-memory asset source
//! (registered as `upload://` in `main`), then the scene loader is pointed at the
//! new asset and the old scene entities are cleared so they respawn.

use bevy::asset::io::memory::Dir;
use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts};

use crate::scene::{GltfScene, SceneSpawned};

/// The in-memory asset-source directory uploaded glbs are written into, shared
/// with the `upload://` `MemoryAssetReader` registered in `main`.
#[derive(Resource)]
pub struct SceneUpload {
    pub dir: Dir,
    /// Bumped per upload so each load uses a fresh path (otherwise the asset
    /// cache would hand back the previous glb under the same path).
    pub version: u32,
}

/// A small top-right button to pick a `.glb` and hot-swap the scene.
pub fn upload_ui(mut contexts: EguiContexts) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    egui::Area::new(egui::Id::new("scene_upload"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
        .show(ctx, |ui| {
            if ui.button("⤓ Load scene .glb").clicked() {
                pick_glb();
            }
        });
}

/// Drain a freshly-picked glb (if any): write it into the in-memory source, clear
/// the current scene entities, and point the loader at the new asset.
pub fn drive_glb_upload(
    mut upload: ResMut<SceneUpload>,
    mut scene: ResMut<GltfScene>,
    asset_server: Res<AssetServer>,
    mut commands: Commands,
    spawned: Query<Entity, With<SceneSpawned>>,
) {
    let Some(bytes) = take_uploaded() else {
        return;
    };
    upload.version += 1;
    let path = format!("scene_{}.glb", upload.version);
    upload.dir.insert_asset(std::path::Path::new(&path), bytes);
    for e in &spawned {
        commands.entity(e).despawn();
    }
    scene.handle = asset_server.load(format!("upload://{path}"));
    scene.spawned = false;
    info!("loading uploaded scene: {path}");
}

// --- platform-specific file pick + byte inbox ------------------------------

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::RefCell;

    thread_local! {
        /// Bytes from the most recent successful pick, drained by `drive_glb_upload`.
        static INBOX: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    pub fn pick_glb() {
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(file) = rfd::AsyncFileDialog::new()
                .add_filter("glTF", &["glb", "gltf"])
                .pick_file()
                .await
            {
                let bytes = file.read().await;
                INBOX.with(|c| *c.borrow_mut() = Some(bytes));
            }
        });
    }

    pub fn take_uploaded() -> Option<Vec<u8>> {
        INBOX.with(|c| c.borrow_mut().take())
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    pub fn pick_glb() {}
    pub fn take_uploaded() -> Option<Vec<u8>> {
        None
    }
}

use imp::{pick_glb, take_uploaded};
