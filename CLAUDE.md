# SpacetimeDB Core Concepts

SpacetimeDB is a relational database that is also a server. It lets you upload application logic directly into the database via WebAssembly modules, eliminating the traditional web/game server layer entirely.

---

## Critical Rules

1. **Reducers are transactional.** They do not return data to callers. Use subscriptions to read data.
2. **Reducers must be deterministic.** No filesystem, network, timers, or random. All state must come from tables.
3. **Read data via tables/subscriptions**, not reducer return values. Clients get data through subscribed queries.
4. **Auto-increment IDs are not sequential.** Gaps are normal, do not use for ordering. Use timestamps or explicit sequence columns.
5. **`ctx.sender` is the authenticated principal.** Never trust identity passed as arguments.

---

## Feature Implementation Checklist

1. **Backend:** Define table(s) to store the data
2. **Backend:** Define reducer(s) to mutate the data
3. **Client:** Subscribe to the table(s)
4. **Client:** Call the reducer(s) from UI
5. **Client:** Render the data from the table(s)

---

## Debugging Checklist

1. Is SpacetimeDB server running? (`spacetime start`)
2. Is the module published? (`spacetime publish`)
3. Are client bindings generated? (`spacetime generate`)
4. Check server logs for errors (`spacetime logs <db-name>`)
5. Is the reducer actually being called from the client?

---

## Tables

- **Private tables** (default): Only accessible by reducers and the database owner.
- **Public tables**: Exposed for client read access through subscriptions. Writes still require reducers.

Organize data by access pattern, not by entity:

```
Player          PlayerState         PlayerStats
id         <--  player_id           player_id
name            position_x          total_kills
                position_y          total_deaths
                velocity_x          play_time
```

## Reducers

Reducers are transactional functions that modify database state. They run atomically, cannot interact with the outside world, and do not return data to callers. See the language-specific server skills for syntax.

## Event Tables

Event tables broadcast reducer-specific data to clients. Rows are never stored in the client cache (`count()` returns 0, `iter()` yields nothing); only `onInsert` callbacks fire.

## Subscriptions

Subscriptions replicate database rows to clients in real-time.

1. **Subscribe**: Register SQL queries describing needed data
2. **Receive initial data**: All matching rows are sent immediately
3. **Receive updates**: Real-time updates when subscribed rows change
4. **React to changes**: Use callbacks (`onInsert`, `onDelete`, `onUpdate`)

Best practices:
- Group subscriptions by lifetime
- Subscribe before unsubscribing when updating subscriptions
- Avoid overlapping queries
- Use indexes for efficient queries

## Modules

Modules are WebAssembly bundles containing application logic that runs inside the database.

- **Tables**: Define the data schema
- **Reducers**: Define callable functions that modify state
- **Event Tables**: Broadcast reducer-specific data to clients
- **Views**: Read-only functions that expose computed subsets of data to clients
- **Procedures**: (Unstable) Functions that can have side effects (HTTP requests, `ctx.withTx`)

Server-side modules can be written in: Rust, C#, TypeScript, C++

Lifecycle: Write → Compile → Publish (`spacetime publish`) → Hot-swap (republish without disconnecting clients)

## Identity

- **Identity**: A long-lived, globally unique identifier for a user.
- **ConnectionId**: Identifies a specific client connection.
- Always use `ctx.sender` / `ctx.Sender` / `ctx.sender()` for authorization.

SpacetimeDB works with many OIDC providers, including SpacetimeAuth (built-in), Auth0, Clerk, Keycloak, Google, and GitHub.


# SpacetimeDB CLI

Use this skill when the user needs help with the `spacetime` CLI tool - initializing projects, building modules, publishing databases, querying data, managing servers, or troubleshooting CLI issues.

## Quick Reference

### Project Initialization & Development

```bash
# Initialize new project
spacetime init my-project --lang rust|csharp|typescript|cpp
spacetime init my-project --template <template-id>

# Build module
spacetime build                    # release build
spacetime build --debug            # faster iteration, slower runtime

# Dev mode (auto-rebuild, auto-publish, generates bindings)
spacetime dev
spacetime dev --client-lang typescript --module-bindings-path ./client/src/module_bindings

# Generate client bindings
spacetime generate --lang typescript|csharp|rust|unrealcpp --out-dir ./bindings --module-path ./server
```

### Publishing & Deployment

```bash
# Publish to Maincloud (default)
spacetime publish my-database --yes

# Publish to local server
spacetime publish my-database --server local --yes

# Clear database and republish
spacetime publish my-database --delete-data always --yes
```

### Database Interaction

```bash
# SQL queries
spacetime sql my-database "SELECT * FROM users"
spacetime sql my-database --interactive   # REPL mode

# Call reducers (each argument is a separate positional arg)
spacetime call my-database my_reducer '"value"' '123'

# Subscribe to changes
spacetime subscribe my-database "SELECT * FROM users" --num-updates 10

# View logs
spacetime logs my-database -f              # follow logs
spacetime logs my-database -n 100          # up to 100 log lines

# Describe schema
spacetime describe my-database --json
spacetime describe my-database table users --json
spacetime describe my-database reducer my_reducer --json
```

### Database Management

```bash
# List databases
spacetime list

# Delete database
spacetime delete my-database

# Rename database
spacetime rename <database-identity> --to new-name
```

### Server Management

```bash
# List configured servers
spacetime server list

# Add server
spacetime server add local --url http://localhost:3000 --default
spacetime server add myserver --url https://my-spacetime.example.com

# Set default server
spacetime server set-default local

# Test connectivity
spacetime server ping local

# Start local instance
spacetime start

# Clear local data
spacetime server clear
```

### Authentication

```bash
# Login (opens browser)
spacetime login

# Login with token
spacetime login --token <token>

# Show login status
spacetime login show

# Logout
spacetime logout
```

## Default Servers

| Name | URL | Description |
|------|-----|-------------|
| `maincloud` | `https://maincloud.spacetimedb.com` | Production cloud (default) |
| `local` | `http://127.0.0.1:3000` | Local development server |

## Common Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--server` | `-s` | Target server (nickname, hostname, or URL) |
| `--yes` | `-y` | Non-interactive mode (skip confirmations) |
| `--anonymous` | | Use anonymous identity |
| `--module-path` | `-p` | Path to module project |

## Troubleshooting

### "Not logged in"
```bash
spacetime login
# Or use --anonymous for public operations
```

### "Server not responding"
```bash
spacetime server ping <server>
# For local: ensure spacetime start is running
```

### "Schema conflict"
```bash
# Clear data and republish
spacetime publish my-db --delete-data always --yes
```

### "Build failed"
```bash
# Check Rust/C# toolchain
rustup show
# For Rust modules, ensure wasm32-unknown-unknown target
rustup target add wasm32-unknown-unknown
```

## Module Languages

**Server-side (modules):** Rust, C#, TypeScript, C++
**Client SDKs:** TypeScript, C#, Rust, Unreal Engine
**CLI `generate` targets:** TypeScript, C#, Rust, Unreal C++



# SpacetimeDB Rust SDK Reference

## Imports

```rust
use spacetimedb::{
    reducer, table, Identity, ReducerContext, SpacetimeType, Table,
    ConnectionId, ScheduleAt, TimeDuration, Timestamp, Uuid,
};
```

**`Table` is required.** Without it, `ctx.db.*.insert()`, `.iter()`, `.find()` etc. won't compile (`no method named 'insert' found`).

## Tables

`#[spacetimedb::table(...)]` on a `pub struct`. `accessor` must be snake_case:

```rust
#[spacetimedb::table(accessor = entity, public)]
pub struct Entity {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub owner: Identity,
    pub name: String,
    pub active: bool,
}
```

Options: `accessor = snake_case` (required), `public`, `scheduled(reducer_fn)`, `index(...)`

`ctx.db` accessors use the `accessor` name (snake_case).

## Column Types

| Rust type | Notes |
|-----------|-------|
| `u8` / `u16` / `u32` / `u64` / `u128` | unsigned integers |
| `i8` / `i16` / `i32` / `i64` / `i128` | signed integers |
| `f32` / `f64` | floats |
| `bool` | boolean |
| `String` | text |
| `Vec<T>` | list/array |
| `Identity` | user identity |
| `ConnectionId` | connection handle |
| `Timestamp` | server timestamp (microseconds since epoch) |
| `TimeDuration` | duration in microseconds |
| `Uuid` | UUID |
| `Option<T>` | nullable column |

## Column Attributes

```rust
#[primary_key]          // primary key
#[auto_inc]             // auto-increment (use 0 as placeholder on insert)
#[unique]               // unique constraint
#[index(btree)]         // btree index (enables .filter() on this column)
```

## Indexes

Prefer `#[index(btree)]` inline for single-column. Multi-column uses table-level:

```rust
// Inline (preferred for single-column):
#[index(btree)]
pub author_id: u64,
// Access: ctx.db.post().author_id().filter(author_id)

// Multi-column (table-level):
#[spacetimedb::table(accessor = membership, public,
    index(accessor = by_group_user, btree(columns = [group_id, user_id]))
)]
pub struct Membership { pub group_id: u64, pub user_id: Identity, ... }
// Access: ctx.db.membership().by_group_user().filter((group_id, &user_id))
```

When you frequently look up rows by multiple columns, prefer a multi-column index over filtering by one column and looping over the results.

## Reducers

```rust
#[spacetimedb::reducer]
pub fn create_entity(ctx: &ReducerContext, name: String) {
    ctx.db.entity().insert(Entity { id: 0, owner: ctx.sender(), name, active: true });
}

// Reducers can return Result<(), String> or Result<(), E> where E: Display
#[spacetimedb::reducer]
pub fn validate_entity(ctx: &ReducerContext, name: String) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    ctx.db.entity().try_insert(Entity { id: 0, owner: ctx.sender(), name, active: true })?;
    Ok(())
}
```

Note: `insert()` panics on constraint violations. Use `try_insert()` with `?` when returning `Result`.

## DB Operations

```rust
ctx.db.entity().insert(Entity { id: 0, name: "Sample".into() });  // Insert (0 for autoInc)
ctx.db.entity().id().find(entity_id);                              // Find by PK → Option<Entity>
ctx.db.entity().identity().find(ctx.sender());                     // Find by unique column → Option<Entity>
ctx.db.item().author_id().filter(author_id);                       // Filter by index → iterator
ctx.db.entity().iter();                                            // All rows → iterator
ctx.db.entity().count();                                           // Count rows
ctx.db.entity().id().update(Entity { ..existing, name: new_name }); // Update (spread + override)
ctx.db.entity().id().delete(entity_id);                            // Delete by PK
ctx.db.entity().name().delete("Alice");                            // Delete by indexed column
```

Note: `iter()` and `filter()` return iterators. Collect to Vec if you need `.sort()`, `.filter()`, `.map()`.

Range queries on btree indexes: `filter(18..=65)`, `filter(18..)`, `filter(..18)`.

## Lifecycle Hooks

```rust
#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) { ... }

#[spacetimedb::reducer(client_connected)]
pub fn on_connect(ctx: &ReducerContext) { ... }

#[spacetimedb::reducer(client_disconnected)]
pub fn on_disconnect(ctx: &ReducerContext) { ... }
```

## Views

Views must access tables via **indexed lookups only** (primary key, `#[unique]`, or `#[index(btree)]`). `.iter()` / full scans are rejected — the view engine can't incrementally maintain them.

```rust
// Anonymous view (computed ONCE, shared across all clients) — prefer this:
use spacetimedb::{view, AnonymousViewContext};

#[view(accessor = active_users, public)]
fn active_users(ctx: &AnonymousViewContext) -> Vec<Entity> {
    ctx.db.entity().active().filter(true).collect()  // btree index on `active`, not .iter()
}

// Per-user view (result varies by sender, materialized PER subscriber — costly at scale):
use spacetimedb::{view, ViewContext};

#[view(accessor = my_profile, public)]
fn my_profile(ctx: &ViewContext) -> Option<Entity> {
    ctx.db.entity().identity().find(ctx.sender())
}
```

Prefer views over RLS (`#[client_visibility_filter]`) for access control — RLS is experimental/unstable.

## Reducer Context API

`ReducerContext` is the single source of sender identity, deterministic time, and deterministic randomness inside a reducer. Always go through `ctx` for these. Standard library clocks and random sources are not available in modules.

```rust
// Auth: ctx.sender() is the caller's Identity
if row.owner != ctx.sender() {
    panic!("unauthorized");
    // or: return Err(anyhow::anyhow!("unauthorized"));
}

// Server timestamp (deterministic per reducer call)
ctx.db.item().insert(Item { id: 0, owner: ctx.sender(), created_at: ctx.timestamp, .. });

// Timestamp arithmetic
let expiry = ctx.timestamp + TimeDuration::from_micros(delay_micros);

// Deterministic RNG: ctx.random() for a single value, ctx.rng() for the rand::Rng trait
use spacetimedb::rand::Rng;
let n: u32 = ctx.random();                    // random u32
let roll: u32 = ctx.rng().gen_range(1..=6);   // any rand::Rng method

// Client: Timestamp → milliseconds since epoch
timestamp.to_micros_since_unix_epoch() / 1000
```

## Scheduled Tables

```rust
#[spacetimedb::table(accessor = tick_timer, scheduled(tick), public)]
pub struct TickTimer {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    scheduled_at: spacetimedb::ScheduleAt,
}

#[spacetimedb::reducer]
pub fn tick(ctx: &ReducerContext, timer: TickTimer) {
    // timer row is auto-deleted after this reducer runs
}

// One-time: fires once at a specific time
let at = ScheduleAt::Time(ctx.timestamp + std::time::Duration::from_secs(10));
// Repeating: fires on an interval
let at = ScheduleAt::Interval(std::time::Duration::from_secs(5).into());

ctx.db.tick_timer().insert(TickTimer { scheduled_id: 0, scheduled_at: at });
```

## Logging

```rust
log::info!("Player connected: {:?}", ctx.sender());
log::warn!("Low health: {}", hp);
log::error!("Failed to find entity");
```

## Custom Types

```rust
#[derive(SpacetimeType)]
pub enum Status { Online, Away, Offline }

#[derive(SpacetimeType)]
pub struct Point { x: f32, y: f32 }
```

## Complete Example

```rust
// src/lib.rs
use spacetimedb::{Identity, ReducerContext, SpacetimeType, Table, Timestamp};

#[spacetimedb::table(accessor = entity, public)]
pub struct Entity {
    #[primary_key]
    pub identity: Identity,
    pub name: String,
    pub active: bool,
}

#[spacetimedb::table(accessor = record, public)]
pub struct Record {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub owner: Identity,
    pub value: u32,
    pub created_at: Timestamp,
}

#[spacetimedb::reducer(client_connected)]
pub fn on_connect(ctx: &ReducerContext) {
    if let Some(existing) = ctx.db.entity().identity().find(ctx.sender()) {
        ctx.db.entity().identity().update(Entity { active: true, ..existing });
    }
}

#[spacetimedb::reducer(client_disconnected)]
pub fn on_disconnect(ctx: &ReducerContext) {
    if let Some(existing) = ctx.db.entity().identity().find(ctx.sender()) {
        ctx.db.entity().identity().update(Entity { active: false, ..existing });
    }
}

#[spacetimedb::reducer]
pub fn create_entity(ctx: &ReducerContext, name: String) {
    if ctx.db.entity().identity().find(ctx.sender()).is_some() {
        panic!("already exists");
    }
    ctx.db.entity().insert(Entity { identity: ctx.sender(), name, active: true });
}

#[spacetimedb::reducer]
pub fn add_record(ctx: &ReducerContext, value: u32) {
    if ctx.db.entity().identity().find(ctx.sender()).is_none() {
        panic!("not found");
    }
    ctx.db.record().insert(Record {
        id: 0,
        owner: ctx.sender(),
        value,
        created_at: ctx.timestamp,
    });
}
```

## Event Tables

Add the `event` flag for ephemeral rows that broadcast to clients but never persist (damage numbers, kill/chat notifications, telemetry). Inserted and dropped within the same transaction; constraints/indexes are enforced per-transaction. The `event` flag can't be toggled in a migration, and event tables can't be used in views or as the lookup side of subscription joins.

```rust
#[spacetimedb::table(accessor = damage_event, public, event)]
pub struct DamageEvent { pub target: Identity, pub damage: u32 }
// Client: observe via on_insert ONLY — iter()/count() are always empty; no on_update/on_delete.
```

## Client SDK (Rust)

Client modules use the `spacetimedb-sdk` crate (NOT `spacetimedb`). Key rules that differ from the server side:

```rust
let conn = DbConnection::builder()
    .with_uri("http://localhost:3000")
    .with_module_name("my-module")
    .on_connect(|conn, identity, token| {
        // MUST subscribe inside on_connect — never before the connection is established.
        conn.subscription_builder()
            .on_applied(|ctx| println!("Ready!"))
            .add_query(|q| q.from.shop_items().r#where(|r| r.required_level.lte(5u32)))
            .subscribe();
    })
    .build()?;

// MUST advance the connection or NO callbacks ever fire:
conn.run_threaded();   // background thread (simplest) — or run_async().await, or frame_tick()? in a game loop
```

- `r#where` is mandatory in query closures (`where` is a Rust keyword).
- The client SDK uses **blocking I/O** — use `spawn_blocking` or a dedicated thread if mixing with Tokio/async-std.
- When swapping subscriptions, **subscribe to the new query first, then unsubscribe the old** (avoids row churn).

## High-Value Gotchas

- `insert()` returns the **inserted row, not the id** — get the auto-inc value via `let id = ctx.db.task().insert(...).id;`.
- Multi-column `#[unique]` is **not supported** — use a single auto-inc PK plus a btree index over those columns.
- `ScheduleAt::Time(...)` not `::At(...)`; the scheduled-table column type is `scheduled(...)` not `schedule(...)`.
- `#[default(...)]` requires a **const-evaluable** expression — no `.to_string()` / `String::new()`; new migration columns need a `#[default]` and must be appended last.
- `Identity` → `String` is `identity.to_hex().to_string()` (`.to_hex()` alone returns `HexString<32>`).
- Procedures (HTTP/side effects, beta) require `features = ["unstable"]` in Cargo.toml, take `&mut ProcedureContext`, use `ctx.with_tx(|tx| ...)` for DB access, and their closures **may retry — keep them idempotent** (derive all values from inside the closure).
