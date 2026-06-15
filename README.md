# 🎚 Light Show Editor (Bevy + egui + SpacetimeDB, in the browser)

A browser-based timeline editor for designing light shows. **Every change is stored
in SpacetimeDB as an immutable, append-only edit** — giving perfect version control
and time travel for free. A **Bevy** 3D viewport shows the lights and lights them up
to mirror the timeline as you build the show. Runs entirely in the browser (WASM).

## What it does

- **Project picker** — create shows and reopen previous ones. Your identity is a
  SpacetimeDB token cached in `localStorage`, so closing and reopening the tab brings
  back your projects.
- **Timeline editor** — scrub to any frame and toggle which lights are on, or click
  cells in the light × frame grid. Lights use **keyframe + hold** semantics: a keyframe
  sets a light on/off at a frame and that state holds until the next keyframe.
- **3D viewport** — a Bevy scene of light fixtures, drawn behind the egui panels. Each
  fixture glows on/off to mirror the timeline at the current frame, in real time.
- **Playback** — press ▶ and the playhead advances at a chosen fps; the grid and the 3D
  viewport animate. Loop on/off.
- **Scrubbing** — drag the playhead across the timeline grid (or use the Frame slider).
- **Time travel** — a history slider scrubs through every edit ever made (read-only
  preview); "Restore this version" brings the live show back to any past point by
  appending compensating edits. Nothing is ever deleted.

## Architecture (and why)

| Concern | Choice | Why |
|---|---|---|
| Host / 3D | **Bevy** (wgpu→WebGL2) hosts the app | A Bevy view can't embed *inside* an egui app on wasm (glow vs wgpu, no texture bridge), so Bevy hosts and egui is an overlay. |
| UI | **`bevy_egui`** overlay (egui 0.33) | The egui editor is drawn over the 3D scene; the center is left transparent so the 3D shows through. |
| Client ↔ DB | **`spacetimedb-sdk` `browser` feature** as a Bevy `NonSend` resource | The SDK's native `run_threaded()` is compiled out in the browser; we build the connection with `build().await` in `spawn_local` and drive it with `frame_tick()` in a Bevy system each frame. (The `bevy_spacetimedb` crate is native-only and on SDK 1.x, so it can't be used.) |
| Hosting | SpacetimeDB **Maincloud** (`stdb-lightshow-spike`) | Browser-reachable over WSS, permissive CORS, no local server to babysit. |
| Data model | Event sourcing | "Store all changes" → an append-only `edit` log is the single source of truth; project state is the fold of its edits. |

### Code layout

```
src/
├── main.rs        # Bevy App: plugins, resources, systems
├── conn.rs        # SpacetimeDB connection (NonSend) + frame_tick pump
├── state.rs       # ECS resources (AppState, Playback, HeldGrid)
├── logic.rs       # pure timeline fold (fold_keyframes / expand_held)
├── ui.rs          # the bevy_egui editor UI (picker, grid, history, controls)
├── scene.rs       # 3D viewport: fixtures, held recompute, playback, emissive apply
└── module_bindings/   # generated SpacetimeDB client bindings
spacetimedb/src/lib.rs  # the module: tables + reducers
assets/scenes/fixtures.glb  # sample named-fixture glTF (for the glTF path)
```

### Data model

- `project` — `id`, `owner`, `name`, `num_lights`, `num_frames`, `created_at`, `head_seq`.
- `edit` — append-only log: `id`, `project_id`, `seq`, `author`, `created_at`, `light`,
  `frame`, `state` (`0`=off, `1`=on, `2`=clear keyframe). **The complete history.**
- Reducers: `create_project`, `append_edit` (validates ownership, bumps `head_seq`).

The current show = fold all edits. The show "as of edit N" = fold edits with `seq ≤ N`.
That single idea powers both live editing and time travel. Playback fps is client-side.

## Run it

Prereqs (already set up here): Rust + `wasm32-unknown-unknown`, `trunk`, the `spacetime` CLI.

```bash
# (Only if you change the backend) publish the module + regenerate bindings:
spacetime publish stdb-lightshow-spike --server maincloud -c -y
spacetime generate --lang rust --out-dir src/module_bindings --module-path ./spacetimedb

# Run the web app:
trunk serve            # serves http://127.0.0.1:8123
```

Open **http://127.0.0.1:8123**, create a project, open it, and toggle lights — the 3D
fixtures glow to match. Press ▶ to play, drag the grid to scrub, use the History panel
to time-travel. Reload the page and everything is still there.

> First build is slow (Bevy compiles ~500 crates). `cargo check --target
> wasm32-unknown-unknown` is the fast inner loop.

## The 3D scene (Blender workflow)

The viewport loads **`assets/scenes/fixtures.glb`**. Objects named `Light.000`,
`Light.001`, … become controllable fixtures (mapped to timeline light `0`, `1`, …);
all other meshes render as static set geometry. Each frame, `scene.rs` sets each
fixture's emissive from `held[index][current_frame]`.

**👉 See [docs/blender-scenes.md](docs/blender-scenes.md)** for the full authoring +
export guide. In short: model your scene, name the light objects `Light.<n>`, export
glTF Binary as `assets/scenes/fixtures.glb`, and refresh.

`scene.rs` loads the file as a `Gltf` asset and spawns each node directly (it does
**not** use Bevy's reflection-based `Scene` spawning, which panics on this trimmed
feature set — core types aren't registered and Bevy *default* features don't resolve
on 0.18.1 because `bevy_animation` has no stable 0.18 release). If the `.glb` is
missing or has no `Light.<n>` objects, a procedural row of fixtures is used.

## Known limitations (it's a spike)

- Lights are on/off only (no dimming/color yet) — the data model extends cleanly.
- The procedural scene shows a fixed 12 fixtures regardless of project light count.
- The bundled `.glb` loads at startup; swapping scenes at runtime (a "Scene URL"
  field) isn't wired yet. Scene hierarchy is assumed flat (see the Blender guide).
- Targets Maincloud; change `HOST`/`DB_NAME` in `src/conn.rs` to point elsewhere.
- Continuous redraw (Bevy renders every frame), so this is not an idle-0%-CPU app.
