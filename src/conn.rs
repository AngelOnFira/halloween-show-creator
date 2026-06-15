//! SpacetimeDB connection, wired into Bevy.
//!
//! `DbConnection` is `Rc`-backed and not `Send`, so it lives in a Bevy
//! `NonSend` resource. On wasm we build it asynchronously via `spawn_local`
//! and drive it each frame with `frame_tick()` (the browser-safe path — the
//! native `run_threaded()` is compiled out by the SDK's `browser` feature).

use std::cell::RefCell;
use std::rc::Rc;

use bevy::prelude::*;

use crate::module_bindings::*;

pub const HOST: &str = "https://maincloud.spacetimedb.com";
pub const DB_NAME: &str = "stdb-lightshow-spike";
#[cfg(target_arch = "wasm32")]
const TOKEN_KEY: &str = "stdb-lightshow-token";

pub enum ConnState {
    Connecting,
    Connected(DbConnection),
    Failed(String),
}

/// Holds the connection. `NonSend` because `DbConnection` is not `Send`/`Sync`.
pub struct ConnResource {
    pub state: Rc<RefCell<ConnState>>,
}

/// Startup (exclusive) system: kick off the async connection and store it.
pub fn setup_connection(world: &mut World) {
    let shared = Rc::new(RefCell::new(ConnState::Connecting));
    #[cfg(target_arch = "wasm32")]
    connect(shared.clone());
    world.insert_non_send_resource(ConnResource { state: shared });
}

/// Pump the WebSocket each frame so cache updates and reducer acks arrive.
pub fn pump_connection(conn: NonSend<ConnResource>) {
    if let ConnState::Connected(c) = &*conn.state.borrow() {
        let _ = c.frame_tick();
    }
}

#[cfg(target_arch = "wasm32")]
fn connect(shared: Rc<RefCell<ConnState>>) {
    use spacetimedb_sdk::credentials::{LocalStorage, Storage};
    use spacetimedb_sdk::DbContext;
    wasm_bindgen_futures::spawn_local(async move {
        let token: Option<String> = LocalStorage::get(TOKEN_KEY).ok();
        let builder = DbConnection::builder()
            .with_uri(HOST)
            .with_database_name(DB_NAME)
            .with_token(token)
            .on_connect(|conn, _identity, token| {
                let _ = LocalStorage::set(TOKEN_KEY, token);
                conn.subscription_builder()
                    .on_error(|_, err| log::error!("subscription error: {err}"))
                    .subscribe_to_all_tables();
            })
            .on_connect_error(|_ctx, err| log::error!("connect error: {err}"));
        match builder.build().await {
            Ok(conn) => *shared.borrow_mut() = ConnState::Connected(conn),
            Err(e) => *shared.borrow_mut() = ConnState::Failed(format!("{e}")),
        }
    });
}
