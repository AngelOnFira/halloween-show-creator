//! SpacetimeDB connection, wired into Bevy.
//!
//! `DbConnection` is `Rc`-backed and not `Send`, so it lives in a Bevy
//! `NonSend` resource. On wasm we build it asynchronously via `spawn_local`
//! and drive it each frame with `frame_tick()` (the browser-safe path — the
//! native `run_threaded()` is compiled out by the SDK's `browser` feature).
//!
//! Subscriptions are **scoped**, not `subscribe_to_all_tables()`: the small
//! metadata tables (`project`, `edit_log`, `song`) are subscribed once on
//! connect, but the potentially multi-MB `song_chunk` blob table is only
//! subscribed for the *currently open* project (and dropped when it changes).
//! This keeps the project-picker screen — and other clients — from ever
//! downloading audio they aren't looking at.

use std::cell::RefCell;
use std::rc::Rc;

use bevy::prelude::*;
// `SubscriptionHandle` trait imported anonymously: it provides `.unsubscribe()`
// but would otherwise clash with the generated `SubscriptionHandle` struct.
use spacetimedb_sdk::{DbContext, SubscriptionHandle as _};

use crate::module_bindings::*;
use crate::state::AppState;

pub const HOST: &str = "https://maincloud.spacetimedb.com";
pub const DB_NAME: &str = "stdb-lightshow-spike";
#[cfg(target_arch = "wasm32")]
const TOKEN_KEY: &str = "stdb-lightshow-token";

pub enum ConnState {
    Connecting,
    Connected(DbConnection),
    Failed(String),
}

/// Tracks the dynamic, project-scoped `song_chunk` subscription so we can swap
/// it when the open project changes.
#[derive(Default)]
pub struct SubTracker {
    /// The project id the current `song_chunk` subscription is scoped to.
    pub chunk_project: Option<u64>,
    pub chunk_handle: Option<SubscriptionHandle>,
}

/// Holds the connection. `NonSend` because `DbConnection` is not `Send`/`Sync`.
pub struct ConnResource {
    pub state: Rc<RefCell<ConnState>>,
    pub subs: RefCell<SubTracker>,
}

/// Startup (exclusive) system: kick off the async connection and store it.
pub fn setup_connection(world: &mut World) {
    let shared = Rc::new(RefCell::new(ConnState::Connecting));
    #[cfg(target_arch = "wasm32")]
    connect(shared.clone());
    world.insert_non_send_resource(ConnResource {
        state: shared,
        subs: RefCell::new(SubTracker::default()),
    });
}

/// Pump the WebSocket each frame so cache updates and reducer acks arrive.
pub fn pump_connection(conn: NonSend<ConnResource>) {
    if let ConnState::Connected(c) = &*conn.state.borrow() {
        let _ = c.frame_tick();
    }
}

/// Keep the project-scoped `song_chunk` subscription in sync with the open
/// project: subscribe to the open project's chunks, drop the subscription when
/// no project is open or a different one is opened.
pub fn sync_subscriptions(conn: NonSend<ConnResource>, app: Res<AppState>) {
    let guard = conn.state.borrow();
    let ConnState::Connected(c) = &*guard else {
        return;
    };
    let mut subs = conn.subs.borrow_mut();
    if subs.chunk_project == app.open_project {
        return;
    }

    // Subscribe to the new project's heavy/per-project rows *before* dropping
    // the old handle (the SDK prefers subscribe-new-then-unsubscribe-old to
    // avoid row churn): the audio chunks plus the rich fixture keyframes
    // (lasers / gobo projector / turrets), which only matter while the project
    // is open.
    let new_handle = app.open_project.map(|pid| {
        c.subscription_builder()
            .on_error(|_, err| log::error!("project-scoped subscription error: {err}"))
            .subscribe(vec![
                format!("SELECT * FROM song_chunk WHERE project_id = {pid}"),
                format!("SELECT * FROM laser_kf WHERE project_id = {pid}"),
                format!("SELECT * FROM projector_kf WHERE project_id = {pid}"),
                format!("SELECT * FROM turret_kf WHERE project_id = {pid}"),
            ])
    });
    if let Some(old) = subs.chunk_handle.take() {
        let _ = old.unsubscribe();
    }
    subs.chunk_handle = new_handle;
    subs.chunk_project = app.open_project;
}

#[cfg(target_arch = "wasm32")]
fn connect(shared: Rc<RefCell<ConnState>>) {
    use spacetimedb_sdk::credentials::{LocalStorage, Storage};
    wasm_bindgen_futures::spawn_local(async move {
        let token: Option<String> = LocalStorage::get(TOKEN_KEY).ok();
        let builder = DbConnection::builder()
            .with_uri(HOST)
            .with_database_name(DB_NAME)
            .with_token(token)
            .on_connect(|conn, _identity, token| {
                let _ = LocalStorage::set(TOKEN_KEY, token);
                // Always-on, cheap metadata subscriptions. The heavy
                // `song_chunk` table is subscribed per-project in
                // `sync_subscriptions`.
                conn.subscription_builder()
                    .on_error(|_, err| log::error!("subscription error: {err}"))
                    .subscribe(vec![
                        "SELECT * FROM project".to_string(),
                        "SELECT * FROM edit_log".to_string(),
                        "SELECT * FROM song".to_string(),
                    ]);
            })
            .on_connect_error(|_ctx, err| log::error!("connect error: {err}"));
        match builder.build().await {
            Ok(conn) => *shared.borrow_mut() = ConnState::Connected(conn),
            Err(e) => *shared.borrow_mut() = ConnState::Failed(format!("{e}")),
        }
    });
}
