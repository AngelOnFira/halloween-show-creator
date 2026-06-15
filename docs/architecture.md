# Architecture & lessons (read this first)

A browser light-show editor. **Bevy hosts the app; egui (via `bevy_egui`) is an
overlay; SpacetimeDB is the backend.** Everything runs in the browser (wasm).
This doc is the locked-in stack + the dead ends we already hit — don't re-litigate
them.

## The stack (pinned)

| Layer | Choice | Notes |
|---|---|---|
| Host / 3D | **Bevy 0.18** (`default-features = false`, see below) → WebGL2 | owns the window/canvas + render loop |
| UI | **bevy_egui 0.39** (pins **egui 0.33** — use `bevy_egui::egui`) | drawn in the `EguiPrimaryContextPass` schedule |
| DB client | **spacetimedb-sdk 2.5**, `browser` feature | as a `NonSend` resource; `build().await` in `spawn_local`, pumped by `frame_tick()` |
| Backend | SpacetimeDB module (Rust), **Maincloud** db `stdb-lightshow-spike` | event-sourced; reducers are the only writes |
| Build | **trunk** (`trunk serve`, port 8123), wasm-bindgen 0.2.125 | `.cargo/config.toml` sets `getrandom_backend="wasm_js"` |

## Module map

```
spacetimedb/src/lib.rs  project + append-only edit tables; create_project/append_edit reducers
src/conn.rs   ConnResource (NonSend) + async connect + pump_connection (frame_tick each frame)
src/state.rs  ECS resources: AppState (UI), Playback, HeldGrid
src/logic.rs  PURE timeline fold: fold_keyframes / expand_held (held[light][frame]) — reuse, don't fork
src/ui.rs     bevy_egui editor: picker, grid, history, playback/scrub controls
src/scene.rs  3D: load fixtures.glb, spawn fixtures, recompute_held, playback clock, emissive apply
src/main.rs   Bevy App: plugins, resources, system schedule
```

**Data model:** every change is an immutable `edit` row `(project_id, seq, light, frame,
state[0=off/1=on/2=clear])`. Project state = fold of its edits (keyframe + hold). State
"as of edit N" = fold edits with `seq ≤ N` → that's both undo-safety and time travel.

**System order:** `PreUpdate: pump_connection` → `Update: spawn_gltf_fixtures`,
`(recompute_held → playback_advance → apply_lights)` → `EguiPrimaryContextPass: ui_system`.

## Invariants (respect these)

- **Writes go through reducers only**; the client reads the replicated cache
  (`conn.db().*.iter()`). Never mutate state any other way.
- **The connection is `NonSend`** (`DbConnection` is `Rc`-based, not `Send`). Touch it
  only from `NonSend`/`NonSendMut` systems; never make it a normal `Resource`.
- **Identity** = anonymous token cached in `localStorage` (`stdb-lightshow-token`); the
  UI filters projects to `owner == try_identity()`.
- **The egui center is intentionally empty** so the Bevy 3D shows through — the editor
  uses top/right/bottom panels, **no `CentralPanel`** (its opaque fill would hide the 3D).
- **3D fixtures** are entities with `LightFixture { index }` + their own material;
  `apply_lights` sets emissive from `held[index][current_frame]`. Objects named
  `Light.<n>` (procedural or glTF) map to timeline light `n`.

## What did NOT work (don't retry)

1. **Rust SDK with default features won't compile for wasm** — it uses native
   `tokio`/`tokio-tungstenite`/`run_threaded`. You MUST enable the `browser` feature
   (web-sys WebSocket + `spawn_local`) and drive it with `frame_tick()`.
2. **You cannot embed a Bevy view *inside* an egui/eframe app on wasm** — eframe=glow,
   Bevy=wgpu, no texture bridge. That's why Bevy hosts and egui overlays it.
3. **`bevy = { features=["webgl2"] }` (default features) does NOT resolve on 0.18.1** —
   `bevy_animation` has no stable 0.18 crate (only a prerelease). Keep the trimmed
   feature list in `Cargo.toml`.
4. **glTF `SceneRoot` spawning panics** ("unregistered type `Transform`") on the trimmed
   features (reflection-based scene clone needs many `register_type`s). Fix used:
   **load the `Gltf` asset and `commands.spawn` each node directly** (`scene.rs`
   `spawn_gltf_fixtures`) — no reflection. Don't switch back to `SceneRoot`.
5. **The `bevy_spacetimedb` crate is unusable here** — it's native-only (`run_threaded` +
   threads) and on SpacetimeDB SDK 1.x, not our 2.5. We integrate the SDK manually.
6. **glTF assets fail to load on wasm without `AssetMetaCheck::Never`** — the trunk dev
   server answers `.meta` requests with a 200 SPA fallback, which Bevy tries to parse as
   asset meta. It's set in `main.rs`; keep it.
7. **Don't depend on `egui` directly** — match `bevy_egui`'s version via `bevy_egui::egui`,
   or you get two egui versions.

## Build / run / verify

```bash
trunk serve                                   # http://127.0.0.1:8123 (first build is slow)
cargo check --target wasm32-unknown-unknown   # fast inner loop
# backend changes: spacetime publish stdb-lightshow-spike --server maincloud -c -y
#                  spacetime generate --lang rust --out-dir src/module_bindings --module-path ./spacetimedb
```

In-browser verification used headless Chromium via Playwright (load page → read
`localStorage` token to confirm connect → drive HTTP reducers/SQL with the browser
token → screenshot). Reducer/SQL HTTP endpoints (handy for tests):
`POST /v1/database/<db>/call/<reducer>` (JSON arg array) and `POST /v1/database/<db>/sql`.

## Known follow-ups (not done yet)

- Runtime "Scene URL" to swap `.glb` without rebuilding; nested glTF hierarchy (transforms
  are currently used flat).
- Lights are on/off only (no dim/color) — the data model extends cleanly (add fields to
  `edit` + the reducer).
- Procedural fallback shows a fixed 12 fixtures regardless of project light count.
- `data-wasm-opt="z"` for a smaller deploy build (raw release wasm ~75 MB).
- fps is client-side; promote to a `project` column for multi-user consistency.

See also: `README.md` (overview/run) and `docs/blender-scenes.md` (authoring scenes).
