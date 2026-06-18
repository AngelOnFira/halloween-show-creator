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

pub enum ConnState {
    /// No session token yet — the user must log in with Discord first.
    NeedsLogin,
    /// Exchanging an OIDC authorization code for a token (post-redirect).
    Authenticating,
    /// Have a token; building the SpacetimeDB connection.
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

/// Startup (exclusive) system: decide the initial auth state and (if we already
/// have a token) kick off the async connection, then store the resource.
pub fn setup_connection(world: &mut World) {
    // Default to NeedsLogin; `bootstrap` upgrades it to Connecting/Authenticating
    // when a token or an OIDC callback is present. On native (no wasm) the app
    // simply sits on the login screen — the connection is wasm-only.
    let shared = Rc::new(RefCell::new(ConnState::NeedsLogin));
    #[cfg(target_arch = "wasm32")]
    bootstrap(shared.clone());
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

/// Decide how to get online: complete an OIDC redirect if we're returning from
/// one, else silently reconnect with a stored token, else require login.
#[cfg(target_arch = "wasm32")]
fn bootstrap(shared: Rc<RefCell<ConnState>>) {
    use crate::auth;

    // 1. Returning from a Discord/SpacetimeAuth redirect (?code=…)?
    if let Some(code) = auth::pending_code() {
        *shared.borrow_mut() = ConnState::Authenticating;
        let sh = shared.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match auth::exchange(code).await {
                Ok(id_token) => {
                    auth::clear_session();
                    auth::clear_callback_url();
                    connect_with(sh, id_token);
                }
                Err(e) => {
                    auth::clear_session();
                    auth::clear_callback_url();
                    log::error!("OIDC exchange failed: {e}");
                    *sh.borrow_mut() = ConnState::Failed(format!("Login failed: {e}"));
                }
            }
        });
        return;
    }

    // 2. Already have a session token → silent reconnect.
    if let Some(token) = auth::stored_token() {
        connect_with(shared, token);
        return;
    }

    // 3. Otherwise, the UI shows the Discord login screen.
    *shared.borrow_mut() = ConnState::NeedsLogin;
}

/// Build the SpacetimeDB connection with the given token (an OIDC id_token on
/// first login, or the persisted SpacetimeDB session token on reconnect).
#[cfg(target_arch = "wasm32")]
fn connect_with(shared: Rc<RefCell<ConnState>>, token: String) {
    *shared.borrow_mut() = ConnState::Connecting;
    let sh = shared.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let builder = DbConnection::builder()
            .with_uri(HOST)
            .with_database_name(DB_NAME)
            .with_token(Some(token))
            .on_connect(|conn, _identity, token| {
                // Persist the SpacetimeDB session token so a reload reconnects
                // without bouncing through Discord (it preserves the OIDC
                // identity via the iss/sub claims).
                crate::auth::store_token(token);
                // Always-on, cheap metadata subscriptions. The heavy
                // `song_chunk` table is subscribed per-project in
                // `sync_subscriptions`.
                conn.subscription_builder()
                    .on_error(|_, err| log::error!("subscription error: {err}"))
                    .subscribe(vec![
                        "SELECT * FROM project".to_string(),
                        "SELECT * FROM edit_log".to_string(),
                        "SELECT * FROM song".to_string(),
                        // The laser pattern library: global reference data.
                        "SELECT * FROM pattern".to_string(),
                    ]);
            })
            .on_connect_error(|_ctx, err| log::error!("connect error: {err}"));
        match builder.build().await {
            Ok(conn) => *sh.borrow_mut() = ConnState::Connected(conn),
            Err(e) => {
                // The token may be expired/invalid — drop it so the login screen
                // offers a clean retry instead of looping on a dead token.
                crate::auth::clear_token();
                *sh.borrow_mut() = ConnState::Failed(format!("{e}"));
            }
        }
    });
}
