# SpacetimeDB Rust SDK

## ⛔ COMMON MISTAKES — LLM HALLUCINATIONS

These are **actual errors** observed when LLMs generate SpacetimeDB Rust code:

### 1. Wrong Crate for Server vs Client

```rust
// ❌ WRONG — using client crate for server module
use spacetimedb_sdk::*;  // This is for CLIENTS only!

// ✅ CORRECT — use spacetimedb for server modules
use spacetimedb::{table, reducer, Table, ReducerContext, Identity, Timestamp};
```

### 2. Wrong Table Macro Syntax

```rust
// ❌ WRONG — using attribute-style like C#
#[spacetimedb::table]
#[primary_key]
pub struct User { ... }

// ❌ WRONG — SpacetimeType on tables (causes conflicts!)
#[derive(SpacetimeType)]
#[table(accessor = my_table)]
pub struct MyTable { ... }

// ✅ CORRECT — use #[table(...)] macro with options, NO SpacetimeType
#[table(accessor = user, public)]
pub struct User {
    #[primary_key]
    identity: Identity,
    name: Option<String>,
}
```

### 3. Wrong Table Access Pattern

```rust
// ❌ WRONG — using ctx.Db or ctx.db() method or field access
ctx.Db.user.Insert(...);
ctx.db().user().insert(...);
ctx.db.player;  // Field access

// ✅ CORRECT — ctx.db is a field, table names are methods with parentheses
ctx.db.user().insert(User { ... });
ctx.db.user().identity().find(ctx.sender);
ctx.db.player().id().find(&player_id);
```

### 4. Wrong Update Pattern

```rust
// ❌ WRONG — partial update or using .update() directly on table
ctx.db.user().update(User { name: Some("new".into()), ..Default::default() });

// ✅ CORRECT — find existing, spread it, update via primary key accessor
if let Some(user) = ctx.db.user().identity().find(ctx.sender) {
    ctx.db.user().identity().update(User { name: Some("new".into()), ..user });
}
```

### 5. Wrong Reducer Return Type

```rust
// ❌ WRONG — returning data from reducer
#[reducer]
pub fn get_user(ctx: &ReducerContext, id: Identity) -> Option<User> { ... }

// ❌ WRONG — mutable context
pub fn my_reducer(ctx: &mut ReducerContext, ...) { }

// ✅ CORRECT — reducers return Result<(), String> or nothing, immutable context
#[reducer]
pub fn do_something(ctx: &ReducerContext, value: String) -> Result<(), String> {
    if value.is_empty() {
        return Err("Value cannot be empty".to_string());
    }
    Ok(())
}
```

### 6. Wrong Client Connection Pattern

```rust
// ❌ WRONG — subscribing before connected
let conn = DbConnection::builder().build()?;
conn.subscription_builder().subscribe_to_all_tables();  // NOT CONNECTED YET!

// ✅ CORRECT — subscribe in on_connect callback
DbConnection::builder()
    .on_connect(|conn, identity, token| {
        conn.subscription_builder()
            .on_applied(|ctx| println!("Ready!"))
            .subscribe_to_all_tables();
    })
    .build()?;
```

### 7. Forgetting to Advance the Connection

```rust
// ❌ WRONG — connection never processes messages
let conn = DbConnection::builder().build()?;
// ... callbacks never fire ...

// ✅ CORRECT — must call one of these to process messages
conn.run_threaded();           // Spawn background thread
// OR
conn.run_async().await;        // Async task
// OR (in game loop)
conn.frame_tick()?;            // Manual polling
```

### 8. Missing Table Trait Import

```rust
// ❌ WRONG — "no method named `insert` found"
use spacetimedb::{table, reducer, ReducerContext};
ctx.db.user().insert(...);  // ERROR!

// ✅ CORRECT — import Table trait for table methods
use spacetimedb::{table, reducer, Table, ReducerContext};
ctx.db.user().insert(...);  // Works!
```

### 9. Wrong ScheduleAt Variant

```rust
// ❌ WRONG — At variant doesn't exist
scheduled_at: ScheduleAt::At(future_time),

// ✅ CORRECT — use Time variant
scheduled_at: ScheduleAt::Time(future_time),
```

### 10. Identity to String Conversion

```rust
// ❌ WRONG — to_hex() returns HexString<32>, not String
let id: String = identity.to_hex();  // Type mismatch!

// ✅ CORRECT — chain .to_string()
let id: String = identity.to_hex().to_string();
```

### 11. Client SDK Uses Blocking I/O

The SpacetimeDB Rust client SDK uses blocking I/O. If mixing with async runtimes (Tokio, async-std), use `spawn_blocking` or run the SDK on a dedicated thread to avoid blocking the async executor.

### 12. Wrong Schedule Syntax
```rust
// ❌ WRONG — `schedule` is not a valid table type
#[table(name = tick_timer, schedule(reducer = tick, column = scheduled_at))]

// ✅ CORRECT — `scheduled` is a valid table type
#[table(name = tick_timer, scheduled(reducer = tick, column = scheduled_at))]
```

### 13. Using `.iter()` Inside a View

```rust
// ❌ WRONG — views must access tables via indexed lookups, not full scans
#[view(accessor = high_scorers, public)]
fn high_scorers(ctx: &AnonymousViewContext) -> Vec<Player> {
    ctx.db.player().iter().filter(|p| p.score >= 1000).collect()
}

// ✅ CORRECT — index lookup (btree, primary key, or unique)
#[view(accessor = high_scorers, public)]
fn high_scorers(ctx: &AnonymousViewContext) -> Vec<Player> {
    ctx.db.player().score().filter(1000u64..).collect()
}
```

### 14. Expecting Event Table Rows on the Client

```rust
// ❌ WRONG — event tables are always empty on the client
for event in conn.db.damage_event().iter() { /* never runs */ }

// ✅ CORRECT — observe via on_insert; no on_update/on_delete exist
conn.db.damage_event().on_insert(|ctx, event| {
    println!("damage: {}", event.damage);
});
```

### 15. Non-Const Default Values

```rust
// ❌ WRONG — .to_string() is not const-evaluable
#[default("guest".to_string())]
name: String,

// ✅ CORRECT — primitives, bools, and other const-constructible types only
#[default(0)]
score: u32,
#[default(true)]
is_active: bool,
```

### 16. Procedure `with_tx` Closures Must Be Idempotent

```rust
// ❌ DANGEROUS — captures mutable state; procedure may retry
let mut counter = 0;
ctx.with_tx(|tx| {
    counter += 1;  // ends up wrong if the closure runs again
    tx.db.log().insert(LogRow { count: counter });
});

// ✅ CORRECT — derive everything from inside the transaction
ctx.with_tx(|tx| {
    let count = tx.db.log().iter().count() + 1;
    tx.db.log().insert(LogRow { count: count as u32 });
});
```

Procedures may execute their `with_tx` closure more than once against different database states. Treat the closure as a pure function of its inputs and the transaction.

---

## 1) Common Mistakes Table

### Server-side errors

(Items already shown with code examples in the Common Mistakes section above are not repeated here.)

| Wrong | Right | Error |
|-------|-------|-------|
| `ctx.db.player().find(id)` | `ctx.db.player().id().find(&id)` | Must access via index |
| `#[table(accessor = "my_table")]` | `#[table(accessor = my_table)]` | String literals not allowed |
| Missing `public` on table | Add `public` flag | Clients can't subscribe |
| `#[spacetimedb::reducer]` | `#[reducer]` after import | Wrong attribute path |
| Network/filesystem in reducer | Use procedures instead | Sandbox violation |
| Panic for expected errors | Return `Result<(), String>` | WASM instance destroyed |
| `ctx.timestamp.to_duration_since_unix_epoch().as_micros()` | `.unwrap_or_default().as_micros()` | Returns `Result`, not `Duration` |
| Borrow after moving a value into a struct | Capture into local before insert, or `.clone()` | "value used after move" |
| Capturing mutable state in `ctx.with_tx` closure | Derive values inside the closure | Wrong results on retry |
| Using `where` directly in client query builder | Use `r#where` (raw identifier) | Reserved keyword error |
| `ctx.db.x.y` (procedure) | `ctx.with_tx(\|tx\| tx.db.y...)` | No `db` field on procedure ctx |

---

## 2) Table Definition (CRITICAL)

**Tables use `#[table(...)]` macro on `pub struct`.** Always import `Table` — required for `.insert()`, `.iter()`, `.find()`, etc. Do NOT derive `SpacetimeType` on tables (see Common Mistake #2).

```rust
use spacetimedb::{table, reducer, Table, ReducerContext, Identity, Timestamp};

#[table(accessor = message, public)]
pub struct Message {
    #[primary_key]
    #[auto_inc]
    id: u64,
    sender: Identity,
    text: String,
    sent: Timestamp,
}

// With a multi-column btree index
#[table(accessor = task, public, index(name = by_owner, btree(columns = [owner_id])))]
pub struct Task {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub owner_id: Identity,
    pub title: String,
}
```

### Table Options

```rust
#[table(accessor = my_table)]           // Private table (default)
#[table(accessor = my_table, public)]   // Public table - clients can subscribe
```

### Column Attributes

```rust
#[primary_key]           // Primary key (auto-indexed, enables .find())
#[auto_inc]              // Auto-increment (use with #[primary_key])
#[unique]                // Unique constraint (auto-indexed)
#[index(btree)]          // B-Tree index for queries
```

### Unique Constraints

```rust
#[table(accessor = user, public)]
pub struct User {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[unique]
    username: String,
    #[unique]
    email: String,
}

// Look up by unique column with .find() (returns Option)
let user = ctx.db.user().username().find(&"alice".to_string());
```

- `#[unique]` columns are auto-indexed and support `.find()`.
- Unlike primary keys, unique-column updates are in-place — they do not delete and re-insert.
- Multi-column unique constraints are not supported. Use a single auto-inc primary key plus a btree index over the columns you need to look up by.

### Default Values

```rust
#[table(accessor = player, public)]
pub struct Player {
    #[primary_key]
    #[auto_inc]
    id: u64,
    name: String,
    #[default(0)]
    score: u32,
    #[default(true)]
    is_active: bool,
}
```

> ⚠️ **Rust limitation:** `#[default(value)]` requires a **const-evaluable** expression. Strings, `String::new()`, and `.to_string()` are NOT const fns — only primitives, `bool`, and other const-constructible types work.

- Cannot combine `#[default]` with `#[primary_key]`, `#[unique]`, or `#[auto_inc]`.
- New columns added in a migration must include a `#[default]` and be appended at the end of the struct.

### Event Tables

For ephemeral rows that broadcast to clients but don't persist, add the `event` flag — see §8 Event Tables.

### Insert returns ROW, not ID

```rust
let row = ctx.db.task().insert(Task {
    id: 0,  // auto-inc placeholder
    owner_id: ctx.sender,
    title: "New task".to_string(),
    created_at: ctx.timestamp,
});
let new_id = row.id;  // Get the actual ID
```

---

## 3) Reducers

### Definition Syntax

```rust
use spacetimedb::{reducer, ReducerContext, Table};

#[reducer]
pub fn send_message(ctx: &ReducerContext, text: String) {
    let row = ctx.db.message().insert(Message {
        id: 0,
        sender: ctx.sender,
        text,
        sent: ctx.timestamp,
    });
    log::info!("Message {} sent by {:?}", row.id, ctx.sender);
}
```

For validation and error returns, see "Error Handling" below.

### Update Pattern (CRITICAL)

```rust
#[reducer]
pub fn set_name(ctx: &ReducerContext, name: String) -> Result<(), String> {
    // Find existing row
    let user = ctx.db.user().identity().find(ctx.sender)
        .ok_or("User not found")?;
    
    // ✅ CORRECT — spread existing row, override specific fields
    ctx.db.user().identity().update(User {
        name: Some(name),
        ..user  // Preserves identity, online, etc.
    });
    
    Ok(())
}

// ❌ WRONG — partial update nulls out other fields!
// ctx.db.user().identity().update(User { identity: ctx.sender, name: Some(name), ..Default::default() });
```

### Delete Pattern

```rust
#[reducer]
pub fn delete_message(ctx: &ReducerContext, message_id: u64) -> Result<(), String> {
    ctx.db.message().id().delete(&message_id);
    Ok(())
}
```

### Lifecycle Hooks

```rust
#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    // Called when module is first published
}

#[reducer(client_connected)]
pub fn client_connected(ctx: &ReducerContext) {
    // ctx.sender is the connecting identity
    if let Some(user) = ctx.db.user().identity().find(ctx.sender) {
        ctx.db.user().identity().update(User { online: true, ..user });
    } else {
        ctx.db.user().insert(User {
            identity: ctx.sender,
            username: None,
            online: true,
        });
    }
}

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    if let Some(user) = ctx.db.user().identity().find(ctx.sender) {
        ctx.db.user().identity().update(User { online: false, ..user });
    }
}
```

### Error Handling: `Result` vs panic

```rust
// ✅ Expected/user-facing errors → return Err
#[reducer]
pub fn transfer(ctx: &ReducerContext, to: Identity, amount: u32) -> Result<(), String> {
    let from = ctx.db.user().identity().find(ctx.sender)
        .ok_or("Sender not found")?;
    if from.balance < amount {
        return Err("Insufficient balance".to_string());
    }
    // ...
    Ok(())
}

// ✅ Programmer errors / broken invariants → panic
#[reducer]
pub fn process(ctx: &ReducerContext, data: Vec<u8>) -> Result<(), String> {
    let header = parse_header(&data).expect("invariant: header validated upstream");
    // ...
    Ok(())
}
```

- Return `Err(String)` for input validation, missing rows, business-rule violations. The transaction rolls back; the caller sees the error message.
- Panic only for true bugs / impossible states. A panic destroys the WASM instance and rolls back the transaction.

### ReducerContext fields

```rust
ctx.sender          // Identity of the caller
ctx.timestamp       // Current timestamp
ctx.db              // Database access
ctx.rng             // Deterministic RNG (use instead of rand)
```

---

## 4) Index Access

### Primary Key / Unique — `.find()` returns `Option<Row>`

```rust
// Primary key lookup
let user = ctx.db.user().identity().find(ctx.sender);

// Unique column lookup  
let user = ctx.db.user().username().find(&"alice".to_string());

if let Some(user) = user {
    // Found
}
```

### BTree Index — `.filter()` returns iterator

```rust
#[table(accessor = message, public)]
pub struct Message {
    #[primary_key]
    #[auto_inc]
    id: u64,
    
    #[index(btree)]
    room_id: u64,
    
    text: String,
}

// Filter by indexed column
for msg in ctx.db.message().room_id().filter(&room_id) {
    // Process each message in room
}
```

### No Index — `.iter()` + manual filter

```rust
// Full table scan
for user in ctx.db.user().iter() {
    if user.online {
        // Process online users
    }
}
```

---

## 5) Custom Types

**Use `#[derive(SpacetimeType)]` ONLY for custom structs/enums used as fields or parameters.**

```rust
use spacetimedb::SpacetimeType;

// Custom struct for table fields
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

// Custom enum
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub enum PlayerStatus {
    Idle,
    Walking(Position),
    Fighting(Identity),
}

// Use in table (DO NOT derive SpacetimeType on the table!)
#[table(accessor = player, public)]
pub struct Player {
    #[primary_key]
    pub id: Identity,
    pub position: Position,
    pub status: PlayerStatus,
}
```

---

## 6) Views

**Views are the recommended way to control data visibility for clients.** They are server-computed, subscribable, and update incrementally.

```rust
use spacetimedb::{view, ViewContext, AnonymousViewContext, table, SpacetimeType, Identity};

#[table(accessor = player)]
pub struct Player {
    #[primary_key]
    #[auto_inc]
    id: u64,
    #[unique]
    identity: Identity,
    name: String,
    #[index(btree)]
    score: u64,
}

// At-most-one row → return Option<T>
#[view(accessor = my_player, public)]
fn my_player(ctx: &ViewContext) -> Option<Player> {
    ctx.db.player().identity().find(&ctx.sender())
}

// Many rows → return Vec<T>
#[view(accessor = high_scorers, public)]
fn high_scorers(ctx: &AnonymousViewContext) -> Vec<Player> {
    ctx.db.player().score().filter(1000u64..).collect()
}
```

### `ViewContext` vs `AnonymousViewContext`

| Context | Has `ctx.sender()`? | Materialized | Use for |
|---------|---------------------|--------------|---------|
| `ViewContext` | Yes | **Per subscriber** (one computation per user) | Per-user data: my inventory, my messages |
| `AnonymousViewContext` | No | **Once, shared across all subscribers** | Global data: leaderboards, shop inventory |

> ⚠️ **Prefer `AnonymousViewContext` whenever possible.** A `ViewContext` view with 1,000 subscribers is 1,000 separate materializations. Restructure queries (e.g. "entities in region X" rather than "entities near me") to use anonymous views when you can.

### Hard Rule: No `.iter()` in Views

Views must access tables via **indexed lookups only** — primary key, `#[unique]`, or `#[index(btree)]`. Full table scans (`.iter()`) are rejected because the view engine cannot incrementally maintain them.

```rust
// ❌ Rejected
ctx.db.player().iter().filter(|p| p.score > 1000).collect()

// ✅ Use a btree index on the column you filter by
ctx.db.player().score().filter(1000u64..).collect()
```

### Imports

```rust
use spacetimedb::{view, ViewContext, AnonymousViewContext};
```

### RLS Is Deprecated — Use Views

Row-Level Security (`#[client_visibility_filter]`) is an experimental, unstable feature that may change or be removed. **Use views for access control instead** — they are simpler, more flexible, and have better performance characteristics. Only fall back to RLS if you have a specific use case views cannot address.

---

## 7) Scheduled Tables

```rust
use spacetimedb::{table, reducer, ReducerContext, ScheduleAt, Timestamp};

#[table(accessor = cleanup_job, scheduled(cleanup_expired))]
pub struct CleanupJob {
    #[primary_key]
    #[auto_inc]
    scheduled_id: u64,
    
    scheduled_at: ScheduleAt,
    target_id: u64,
}

#[reducer]
pub fn cleanup_expired(ctx: &ReducerContext, job: CleanupJob) {
    // Job row is auto-deleted after reducer completes
    log::info!("Cleaning up: {}", job.target_id);
}

// Schedule a job
#[reducer]
pub fn schedule_cleanup(ctx: &ReducerContext, target_id: u64, delay_ms: u64) {
    let future_time = ctx.timestamp + std::time::Duration::from_millis(delay_ms);
    ctx.db.cleanup_job().insert(CleanupJob {
        scheduled_id: 0,  // auto-inc placeholder
        scheduled_at: ScheduleAt::Time(future_time),
        target_id,
    });
}

// Cancel by deleting the row
#[reducer]
pub fn cancel_cleanup(ctx: &ReducerContext, job_id: u64) {
    ctx.db.cleanup_job().scheduled_id().delete(&job_id);
}
```

---

## 8) Event Tables

**Event tables hold ephemeral rows** — inserted and immediately deleted within the same transaction. They are broadcast to subscribers on commit and never persist.

```rust
#[table(accessor = damage_event, public, event)]
pub struct DamageEvent {
    pub entity_id: Identity,
    pub damage: u32,
    pub source: String,
}

#[reducer]
fn attack(ctx: &ReducerContext, target: Identity, damage: u32) {
    ctx.db.damage_event().insert(DamageEvent {
        entity_id: target,
        damage,
        source: "melee".to_string(),
    });
}
```

### Client side

- Event table rows are **never stored in the client cache**. `.iter()` and `.count()` always yield nothing.
- Observe events through `on_insert` only — `on_update`, `on_delete`, and `on_before_delete` do not exist for event tables.

```rust
conn.db.damage_event().on_insert(|ctx, event| {
    println!("Entity {:?} took {} damage", event.entity_id, event.damage);
});
```

### Constraints

- Primary keys, `#[unique]`, indexes, and `#[auto_inc]` all work — but enforced **per transaction** (the table is empty at the start of each one).
- The `event` flag cannot be toggled in a migration. A regular table cannot become an event table or vice versa.
- Event tables cannot yet be used in views or as the lookup side of subscription joins.

### When to use

Damage numbers, kill notifications, transient chat, particle/audio cues, telemetry — anything where clients need to react to a moment, not query history.

---

## 9) Client SDK

```rust
// Connection pattern
let conn = DbConnection::builder()
    .with_uri("http://localhost:3000")
    .with_module_name("my-module")
    .with_token(load_saved_token())  // None for first connection
    .on_connect(on_connected)
    .build()
    .expect("Failed to connect");

// Subscribe in on_connect callback, NOT before!
fn on_connected(conn: &DbConnection, identity: Identity, token: &str) {
    conn.subscription_builder()
        .on_applied(|ctx| println!("Ready!"))
        .subscribe_to_all_tables();
}
```

### ⚠️ CRITICAL: Advance the Connection

**You MUST call one of these** — without it, no callbacks fire:

```rust
conn.run_threaded();           // Background thread (simplest)
conn.run_async().await;        // Async task
conn.frame_tick()?;            // Manual polling (game loops)
```

### Table Access & Callbacks

```rust
// Iterate
for user in ctx.db.user().iter() { ... }

// Find by primary key
if let Some(user) = ctx.db.user().identity().find(&identity) { ... }

// Row callbacks
ctx.db.user().on_insert(|ctx, user| { ... });
ctx.db.user().on_update(|ctx, old, new| { ... });
ctx.db.user().on_delete(|ctx, user| { ... });

// Call reducers
ctx.reducers.set_name("Alice".to_string()).unwrap();
```

### Subscription Builder & Query Builder

Use the typed query builder to subscribe to specific rows rather than entire tables.

```rust
let subscription = conn
    .subscription_builder()
    .on_applied(|ctx| println!("Initial data loaded"))
    .on_error(|ctx, err| eprintln!("Subscription failed: {err}"))
    .add_query(|q| q.from.shop_items().r#where(|r| r.required_level.lte(5u32)))
    .add_query(|q| q.from.exchange_rates())
    .subscribe();
```

> ⚠️ **`r#where` is required** — `where` is a Rust keyword, so the raw identifier prefix is mandatory in query closures.

### Subscription Handles

```rust
if subscription.is_active() {
    subscription.unsubscribe();  // asynchronous; rows removed when applied
}
subscription.is_ended();  // true after unsubscribe completes or an error
```

### Subscribe-Then-Unsubscribe

When swapping subscriptions (e.g. as a player levels up), **subscribe to the new query first, then unsubscribe from the old one**. Doing it in the other order causes data churn — rows already in cache get deleted, then immediately re-added.

```rust
let new_sub = conn.subscription_builder()
    .add_query(|q| q.from.shop_items().r#where(|r| r.required_level.lte(6u32)))
    .subscribe();

if old_sub.is_active() {
    old_sub.unsubscribe();
}
```

---

## 10) Procedures (Beta)

**Procedures are for side effects (HTTP, filesystem) that reducers can't do.**

⚠️ Procedures are currently **unstable**. The API may change.

### Cargo.toml opt-in (REQUIRED)

```toml
[dependencies]
spacetimedb = { version = "1.*", features = ["unstable"] }
```

Without the `unstable` feature, `#[procedure]` will not compile.

### Definition

```rust
use spacetimedb::{procedure, ProcedureContext};

#[procedure]
fn add_numbers(_ctx: &mut ProcedureContext, a: u32, b: u32) -> u64 {
    a as u64 + b as u64
}
```

Note `&mut ProcedureContext` — procedures take a **mutable** context, unlike reducers.

### Database Access via `with_tx` / `try_with_tx`

Procedures do not have direct `ctx.db` access. Open a transaction explicitly:

```rust
#[procedure]
fn save_external_data(ctx: &mut ProcedureContext, url: String) -> Result<(), String> {
    let data = fetch_from_url(&url)?;  // HTTP allowed (forbidden in reducers)

    ctx.try_with_tx(|tx| {
        tx.db.external_data().insert(ExternalData { id: 0, content: data });
        Ok(())
    })?;

    Ok(())
}
```

- `with_tx(|tx| ...)` — closure returns a value; transaction commits.
- `try_with_tx(|tx| -> Result<...>)` — closure may return `Err`; transaction rolls back on `Err`.

### ⚠️ Idempotency: Closures May Run More Than Once

The `with_tx` / `try_with_tx` closure may be **executed multiple times** against different database states (e.g. if the database state changes between optimistic concurrency attempts). Closures must be **idempotent**:

- Do not capture mutable state from outside the closure.
- Do not depend on side effects from prior runs.
- Derive all values from inside the transaction (`tx.db`, `tx.timestamp`).

```rust
// ❌ WRONG — captures mutable counter
let mut next_id = pre_compute_id();
ctx.with_tx(|tx| { tx.db.foo().insert(Foo { id: next_id, ... }); next_id += 1; });

// ✅ CORRECT — derives id inside the transaction
ctx.with_tx(|tx| {
    let next_id = tx.db.foo().iter().count() as u64;
    tx.db.foo().insert(Foo { id: next_id, ... });
});
```

### Key differences from reducers

| Reducers | Procedures |
|----------|------------|
| `&ReducerContext` (immutable) | `&mut ProcedureContext` (mutable) |
| Direct `ctx.db` access | Must use `ctx.with_tx(\|tx\| ...)` |
| Single auto-transaction | Manual; closure may retry — must be idempotent |
| No HTTP/network | HTTP allowed |
| No return values to caller | Can return data to caller |

---

## 11) Logging

```rust
use spacetimedb::log;

log::trace!("Detailed trace");
log::debug!("Debug info");
log::info!("Information");
log::warn!("Warning");
log::error!("Error occurred");
```

---

## 12) Commands

```bash
# Start local server
spacetime start

# Publish module
spacetime publish <module-name> --module-path <backend-dir>

# Clear database and republish
spacetime publish <module-name> --clear-database -y --module-path <backend-dir>

# Generate bindings
spacetime generate --lang rust --out-dir <client>/src/module_bindings --module-path <backend-dir>

# View logs
spacetime logs <module-name>
```

---

## 13) Hard Requirements

**Rust-specific:**

1. **DO NOT derive `SpacetimeType` on `#[table]` structs** — the macro handles this
2. **Import `Table` trait** — `use spacetimedb::Table;` required for `.insert()`, `.iter()`, etc.
3. **Use `&ReducerContext`** — not `&mut ReducerContext`
4. **Tables are methods** — `ctx.db.table()` not `ctx.db.table`
5. **Server modules use `spacetimedb` crate** — clients use `spacetimedb-sdk`
6. **Reducers must be deterministic** — no filesystem, network, timers, or external RNG
7. **Use `ctx.rng`** — not `rand` crate for random numbers
8. **Use `ctx.timestamp`** — never `std::time::SystemTime::now()` in reducers
9. **Client MUST advance connection** — call `run_threaded()`, `run_async()`, or `frame_tick()`
10. **Subscribe in `on_connect` callback** — not before connection is established
11. **Update requires full row** — spread existing row with `..existing`
12. **DO NOT edit generated bindings** — regenerate with `spacetime generate`
13. **Identity to String needs `.to_string()`** — `identity.to_hex().to_string()`
14. **Client SDK is blocking** — use `spawn_blocking` or dedicated thread if mixing with async runtimes
15. **Views must use indexed lookups** — no `.iter()` inside a view function
16. **`AnonymousViewContext` is preferred over `ViewContext`** — when the view does not depend on the caller
17. **`#[default(...)]` values must be const-evaluable** — no `.to_string()`, no `String::new()`
18. **Use views, not RLS** — RLS is experimental/unstable; views are the recommended access-control mechanism
19. **Procedures require `features = ["unstable"]`** in Cargo.toml
20. **Procedure `with_tx` closures must be idempotent** — they may execute more than once
21. **Use `r#where`** in client query-builder closures (`where` is a Rust keyword)
22. **Event tables are empty on the client** — observe via `on_insert`, never `.iter()`
