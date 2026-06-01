//! The mutation endpoint — `POST /api/mutations` (P1-T4, issue #15).
//!
//! This is the single write path for every list change. The offline-first client
//! queues mutations locally and drains them here; the server is authoritative for
//! ordering (`shopping_lists.current_revision`) and conflict resolution.
//!
//! Request envelope (see `docs/design.md` §11):
//!
//! ```json
//! {
//!   "client_mutation_id": "uuid-v4",   // idempotency key (UNIQUE in `mutations`)
//!   "device_id": "stable-device-uuid",
//!   "list_id": "...",
//!   "base_revision": 47,                // revision the client was at
//!   "entity_type": "list_entry",
//!   "entity_id": "entry-uuid",
//!   "operation_type": "mark_bought",
//!   "payload": { ... },                 // operation-specific JSON
//!   "created_at_client": "..."
//! }
//! ```
//!
//! Response:
//!
//! ```json
//! { "client_mutation_id": "...", "status": "accepted", "server_revision": 48, "entity": { ... } }
//! ```
//!
//! Processing (all of 1–5 inside ONE transaction):
//!   1. If `client_mutation_id` already exists, return the stored `response_json`
//!      verbatim (byte-identical idempotent replay — never re-applied).
//!   2. Verify the caller is a member of the list's household (404 missing list,
//!      403 non-member — the same distinction `lists.rs` draws).
//!   3. Apply the mutation to `list_entries` (insert / field update), honouring
//!      the conflict rules below.
//!   4. Increment `shopping_lists.current_revision` atomically.
//!   5. Insert the `mutations` row with `server_revision = new current_revision`
//!      and the serialised `response_json`.
//!   6. After COMMIT, broadcast over SSE (P1-T5). Broadcasting is deliberately
//!      outside the transaction; it is a no-op stub until P1-T5 lands.
//!
//! Conflict rules (see `docs/design.md` §11):
//!   * `add_item`: insert; on `uq_list_entries_active_dedup` conflict, increment
//!     `quantity` on the surviving active row and report `status: "merged"`.
//!     Only resolved entries with the same `effective_constraint_hash` merge;
//!     unresolved entries carry the `unresolved:<entry_id>` sentinel and a NULL
//!     `canonical_item_id`, so the partial index never matches them.
//!   * `delete_item` vs a concurrent `mark_bought`: the delete wins (status stays
//!     `deleted`), and the bought action is recorded in `list_entry_events` so it
//!     is not silently lost.
//!   * Every field update is last-write-wins: the server always accepts and
//!     assigns a new revision.
//!
//! Error discipline mirrors `lists.rs`: a DB error is logged and surfaced as 500
//! (never failing open); 401/403/404/422 cover the client conditions.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use buywise_core::constraint_hash::effective_constraint_hash;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the mutation route as a `Router<AppState>`.
///
/// Registered separately (like `lists::router`) and merged by `crate::app`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/mutations", post(post_mutation))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Body for `POST /api/mutations` (the client → server envelope, §11).
#[derive(Debug, Deserialize)]
pub struct MutationRequest {
    /// Client-generated idempotency key; UNIQUE in `mutations`.
    pub client_mutation_id: String,
    /// Stable per-device id (audit / debugging).
    pub device_id: String,
    /// The list this mutation targets.
    pub list_id: String,
    /// Revision the client was at when it created the mutation.
    pub base_revision: i64,
    /// Entity kind (`list_entry` today; carried through for forward-compat).
    pub entity_type: String,
    /// Id of the entity the mutation acts on (the `list_entries.id` for entry ops).
    pub entity_id: String,
    /// Operation discriminator (`add_item`, `mark_bought`, ...).
    pub operation_type: String,
    /// Operation-specific JSON payload. Defaults to `null` when omitted.
    #[serde(default)]
    pub payload: Value,
    /// Client-side creation timestamp (ISO-8601).
    pub created_at_client: String,
}

/// The server response envelope (§11). Serialised once and stored verbatim in
/// `mutations.response_json` so a duplicate `client_mutation_id` replays the
/// exact same bytes.
#[derive(Debug, Serialize, Deserialize)]
pub struct MutationResponse {
    /// Echo of the client's idempotency key.
    pub client_mutation_id: String,
    /// `accepted` for a normal apply; `merged` when an `add_item` deduped into an
    /// existing active row.
    pub status: String,
    /// The list's `current_revision` after this mutation was applied.
    pub server_revision: i64,
    /// The affected `list_entries` row after the mutation (the merged row for a
    /// dedup, the inserted/updated row otherwise).
    pub entity: Option<EntityView>,
}

/// A `list_entries` row as returned in [`MutationResponse::entity`]. Mirrors the
/// `EntryView` shape `lists.rs` returns so a client decodes both identically.
#[derive(Debug, Serialize, Deserialize)]
pub struct EntityView {
    /// `list_entries.id`.
    pub id: String,
    /// `list_entries.list_id`.
    pub list_id: String,
    /// `list_entries.canonical_item_id` (NULL until resolved).
    pub canonical_item_id: Option<String>,
    /// `list_entries.resolution_status`.
    pub resolution_status: String,
    /// `list_entries.resolution_candidates_json` (ambiguous only).
    pub resolution_candidates_json: Option<String>,
    /// `list_entries.raw_text`.
    pub raw_text: String,
    /// `list_entries.display_name`.
    pub display_name: String,
    /// `list_entries.added_by`.
    pub added_by: String,
    /// `list_entries.quantity`.
    pub quantity: i64,
    /// `list_entries.status` (`active`|`bought`|`skipped`|`deleted`).
    pub status: String,
    /// `list_entries.bought_by`.
    pub bought_by: Option<String>,
    /// `list_entries.bought_at`.
    pub bought_at: Option<String>,
    /// `list_entries.effective_constraint_hash`.
    pub effective_constraint_hash: String,
    /// `list_entries.last_updated_revision`.
    pub last_updated_revision: i64,
    /// `list_entries.created_at`.
    pub created_at: String,
    /// `list_entries.updated_at`.
    pub updated_at: String,
}

/// Every `list_entries` column, in a fixed SELECT order, as a flat tuple sqlx can
/// decode. Shared by the load + render helpers.
type EntryRow = (
    String,         // id
    String,         // list_id
    Option<String>, // canonical_item_id
    String,         // resolution_status
    Option<String>, // resolution_candidates_json
    String,         // raw_text
    String,         // display_name
    String,         // added_by
    i64,            // quantity
    String,         // status
    Option<String>, // bought_by
    Option<String>, // bought_at
    String,         // effective_constraint_hash
    i64,            // last_updated_revision
    String,         // created_at
    String,         // updated_at
);

/// The `list_entries` SELECT column list, matching [`EntryRow`] order.
const ENTRY_COLUMNS: &str = "id, list_id, canonical_item_id, resolution_status, \
     resolution_candidates_json, raw_text, display_name, added_by, quantity, status, \
     bought_by, bought_at, effective_constraint_hash, last_updated_revision, created_at, updated_at";

impl From<EntryRow> for EntityView {
    fn from(r: EntryRow) -> Self {
        EntityView {
            id: r.0,
            list_id: r.1,
            canonical_item_id: r.2,
            resolution_status: r.3,
            resolution_candidates_json: r.4,
            raw_text: r.5,
            display_name: r.6,
            added_by: r.7,
            quantity: r.8,
            status: r.9,
            bought_by: r.10,
            bought_at: r.11,
            effective_constraint_hash: r.12,
            last_updated_revision: r.13,
            created_at: r.14,
            updated_at: r.15,
        }
    }
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

/// Map a DB error to a logged 500, mirroring `lists::db_error`.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "mutations: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/mutations` — apply one client mutation; see the module docs for the
/// full processing and conflict contract. CSRF is enforced by the extractor
/// (this is a POST).
async fn post_mutation(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<MutationRequest>,
) -> Result<(StatusCode, Json<MutationResponse>), StatusCode> {
    // 0. Validate the operation discriminator at the boundary.
    if !is_known_operation(&req.operation_type) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // 1. Idempotent replay: a known client_mutation_id returns the stored bytes
    //    without touching any state. Read on the pool (autocommit) before opening
    //    the write transaction.
    if let Some(stored) = stored_response(&state.pool, &req.client_mutation_id)
        .await
        .map_err(db_error("post_mutation.idempotency"))?
    {
        let response: MutationResponse = serde_json::from_str(&stored).map_err(|e| {
            tracing::error!(error = %e, "mutations: stored response_json is not valid JSON");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Ok((StatusCode::OK, Json(response)));
    }

    // 2. Authorisation: the list must exist and belong to a household the caller
    //    is a member of. 404 vs 403 mirrors lists.rs.
    let household_id = list_household(&state.pool, &req.list_id)
        .await
        .map_err(db_error("post_mutation.list_household"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("post_mutation.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // 3–5. Apply, bump the revision, and log the mutation in one transaction.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(db_error("post_mutation.begin"))?;

    let outcome = match apply_mutation(&mut tx, &user, &req).await {
        Ok(outcome) => outcome,
        Err(status) => {
            // Best-effort rollback; the original status is what the client sees.
            let _ = tx.rollback().await;
            return Err(status);
        }
    };

    let server_revision = increment_revision(&mut tx, &req.list_id)
        .await
        .map_err(db_error("post_mutation.increment_revision"))?;

    // Stamp the touched entry with the new revision so reconnect replay and the
    // detail view agree on which revision last changed it.
    if let Some(entry_id) = &outcome.entry_id {
        stamp_entry_revision(&mut tx, entry_id, server_revision)
            .await
            .map_err(db_error("post_mutation.stamp_entry"))?;
    }

    // Re-read the affected entry so the response reflects the committed state
    // (including the revision stamp and any merge increment).
    let entity = match &outcome.entry_id {
        Some(entry_id) => load_entry(&mut tx, entry_id)
            .await
            .map_err(db_error("post_mutation.load_entry"))?
            .map(EntityView::from),
        None => None,
    };

    let response = MutationResponse {
        client_mutation_id: req.client_mutation_id.clone(),
        status: outcome.status.as_str().to_string(),
        server_revision,
        entity,
    };
    let response_json = serde_json::to_string(&response).map_err(|e| {
        tracing::error!(error = %e, "mutations: failed to serialise response");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    insert_mutation_row(
        &mut tx,
        &req,
        &user,
        server_revision,
        &outcome,
        &response_json,
    )
    .await
    .map_err(db_error("post_mutation.insert_mutation"))?;

    tx.commit()
        .await
        .map_err(db_error("post_mutation.commit"))?;

    // 6. Broadcast over SSE (P1-T5). Stub until that lands; intentionally after
    //    COMMIT so subscribers never observe an uncommitted mutation.
    broadcast_mutation(&state, &req.list_id, server_revision, &response);

    Ok((StatusCode::CREATED, Json(response)))
}

// ---------------------------------------------------------------------------
// Mutation application
// ---------------------------------------------------------------------------

/// How a mutation row was recorded — drives `mutations.status` and the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationStatus {
    /// Applied normally.
    Accepted,
    /// `add_item` deduped into an existing active row (quantity incremented).
    Merged,
}

impl MutationStatus {
    fn as_str(self) -> &'static str {
        match self {
            MutationStatus::Accepted => "accepted",
            MutationStatus::Merged => "merged",
        }
    }
}

/// The result of applying a mutation inside the transaction.
struct ApplyOutcome {
    /// The `list_entries.id` that ended up affected (the surviving row for a
    /// merge), or `None` if the op touched no entry.
    entry_id: Option<String>,
    /// How to record the mutation.
    status: MutationStatus,
}

/// Apply one mutation to `list_entries`, returning which entry was affected and
/// the resulting status. Pure DB errors are propagated as 500 via the caller;
/// client-fault conditions short-circuit with the mapped status here.
async fn apply_mutation(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    match req.operation_type.as_str() {
        "add_item" => apply_add_item(tx, user, req).await,
        "delete_item" => apply_delete_item(tx, user, req).await,
        "mark_bought" => apply_mark_bought(tx, user, req).await,
        "mark_skipped" => apply_mark_skipped(tx, req).await,
        "rename_item" => apply_rename_item(tx, req).await,
        // resolve_alias / reject_match / set_constraint are accepted (last-write-
        // wins) and recorded; the richer field semantics land with the resolution
        // cascade (Phase 3). Accept-and-log keeps the revision counter correct
        // and the mutation auditable without inventing behaviour ahead of spec.
        "resolve_alias" | "reject_match" | "set_constraint" => Ok(ApplyOutcome {
            entry_id: entry_target(req),
            status: MutationStatus::Accepted,
        }),
        // Unreachable: the discriminator was validated by is_known_operation.
        _ => Err(StatusCode::UNPROCESSABLE_ENTITY),
    }
}

/// `add_item`: insert a new `list_entries` row, deduping resolved entries via the
/// partial unique index. On conflict, increment the surviving row's quantity and
/// report `merged`.
async fn apply_add_item(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = req.entity_id.clone();
    if entry_id.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let p = &req.payload;
    let canonical_item_id = string_field(p, "canonical_item_id");
    let raw_text = string_field(p, "raw_text").unwrap_or_default();
    let display_name = string_field(p, "display_name").unwrap_or_else(|| raw_text.clone());
    let quantity = p
        .get("quantity")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .max(1);
    let resolution_status = match canonical_item_id {
        Some(_) => "resolved",
        None => "unresolved",
    };

    // raw_text / display_name are NOT NULL in the schema; a blank add is a 422.
    if raw_text.trim().is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let hash = effective_constraint_hash(
        &entry_id,
        canonical_item_id.as_deref(),
        p.get("constraints"),
    );
    let now = buywise_core::db::now_iso();

    // Try the insert. The partial unique index fires only for resolved entries
    // (canonical_item_id IS NOT NULL) that collide on (list_id, canonical_item_id,
    // effective_constraint_hash) while active — exactly the dedup boundary.
    let insert = sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, 0, ?10, ?10)",
    )
    .bind(&entry_id)
    .bind(&req.list_id)
    .bind(&canonical_item_id)
    .bind(resolution_status)
    .bind(&raw_text)
    .bind(&display_name)
    .bind(&user.user_id)
    .bind(quantity)
    .bind(&hash)
    .bind(&now)
    .execute(&mut **tx)
    .await;

    match insert {
        Ok(_) => Ok(ApplyOutcome {
            entry_id: Some(entry_id),
            status: MutationStatus::Accepted,
        }),
        Err(e) if is_unique_violation(&e) => {
            // Dedup: fold this add into the existing active row for the same
            // (list, canonical item, constraint hash). Increment its quantity.
            let surviving: Option<String> = sqlx::query_scalar(
                "UPDATE list_entries \
                 SET quantity = quantity + ?4, updated_at = ?5 \
                 WHERE list_id = ?1 AND canonical_item_id = ?2 \
                   AND effective_constraint_hash = ?3 AND status = 'active' \
                 RETURNING id",
            )
            .bind(&req.list_id)
            .bind(&canonical_item_id)
            .bind(&hash)
            .bind(quantity)
            .bind(&now)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_error("apply_add_item.merge"))?;

            // The conflict proves a colliding active row exists, so the UPDATE
            // must have matched it.
            let surviving = surviving.ok_or_else(|| {
                tracing::error!("mutations: add_item conflict but no row to merge into");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Ok(ApplyOutcome {
                entry_id: Some(surviving),
                status: MutationStatus::Merged,
            })
        }
        Err(e) => Err(db_error("apply_add_item.insert")(e)),
    }
}

/// `delete_item`: last-write-wins delete. If the entry was already `bought`, the
/// delete still wins but the bought action is preserved in `list_entry_events`
/// (the delete-vs-mark_bought conflict rule).
async fn apply_delete_item(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();

    let prior_status = entry_status(tx, &entry_id)
        .await
        .map_err(db_error("apply_delete_item.lookup"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // A delete arriving for an already-bought entry: keep the delete, but record
    // the bought action so it is not silently lost.
    if prior_status == "bought" {
        record_event(
            tx,
            &entry_id,
            &user.user_id,
            "bought_then_deleted",
            &req.payload,
            &now,
        )
        .await
        .map_err(db_error("apply_delete_item.event"))?;
    }

    sqlx::query("UPDATE list_entries SET status = 'deleted', updated_at = ?2 WHERE id = ?1")
        .bind(&entry_id)
        .bind(&now)
        .execute(&mut **tx)
        .await
        .map_err(db_error("apply_delete_item.update"))?;

    Ok(ApplyOutcome {
        entry_id: Some(entry_id),
        status: MutationStatus::Accepted,
    })
}

/// `mark_bought`: set status `bought`, stamp `bought_by`/`bought_at`. If the
/// entry was already `deleted`, the delete wins (status stays `deleted`) and the
/// bought action is recorded in `list_entry_events` instead.
async fn apply_mark_bought(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();
    let bought_at = string_field(&req.payload, "bought_at").unwrap_or_else(|| now.clone());

    let prior_status = entry_status(tx, &entry_id)
        .await
        .map_err(db_error("apply_mark_bought.lookup"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Delete wins over a concurrent mark_bought: do not resurrect a deleted entry;
    // log the bought action so the signal survives.
    if prior_status == "deleted" {
        record_event(
            tx,
            &entry_id,
            &user.user_id,
            "bought_after_delete",
            &req.payload,
            &now,
        )
        .await
        .map_err(db_error("apply_mark_bought.event"))?;
        return Ok(ApplyOutcome {
            entry_id: Some(entry_id),
            status: MutationStatus::Accepted,
        });
    }

    sqlx::query(
        "UPDATE list_entries \
         SET status = 'bought', bought_by = ?2, bought_at = ?3, updated_at = ?4 \
         WHERE id = ?1",
    )
    .bind(&entry_id)
    .bind(&user.user_id)
    .bind(&bought_at)
    .bind(&now)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_mark_bought.update"))?;

    Ok(ApplyOutcome {
        entry_id: Some(entry_id),
        status: MutationStatus::Accepted,
    })
}

/// `mark_skipped`: set status `skipped` (last-write-wins).
async fn apply_mark_skipped(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();

    let affected =
        sqlx::query("UPDATE list_entries SET status = 'skipped', updated_at = ?2 WHERE id = ?1")
            .bind(&entry_id)
            .bind(&now)
            .execute(&mut **tx)
            .await
            .map_err(db_error("apply_mark_skipped.update"))?
            .rows_affected();
    if affected == 0 {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(ApplyOutcome {
        entry_id: Some(entry_id),
        status: MutationStatus::Accepted,
    })
}

/// `rename_item`: update `display_name` (and `raw_text` when supplied),
/// last-write-wins.
async fn apply_rename_item(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let display_name = string_field(&req.payload, "display_name")
        .filter(|s| !s.trim().is_empty())
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let raw_text = string_field(&req.payload, "raw_text");
    let now = buywise_core::db::now_iso();

    let affected = sqlx::query(
        "UPDATE list_entries \
         SET display_name = ?2, raw_text = COALESCE(?3, raw_text), updated_at = ?4 \
         WHERE id = ?1",
    )
    .bind(&entry_id)
    .bind(&display_name)
    .bind(&raw_text)
    .bind(&now)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_rename_item.update"))?
    .rows_affected();
    if affected == 0 {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(ApplyOutcome {
        entry_id: Some(entry_id),
        status: MutationStatus::Accepted,
    })
}

// ---------------------------------------------------------------------------
// Small DB helpers (transaction-scoped + pool-scoped)
// ---------------------------------------------------------------------------

/// The stored `response_json` for a `client_mutation_id`, or `None` if unseen.
async fn stored_response(
    pool: &sqlx::SqlitePool,
    client_mutation_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT response_json FROM mutations WHERE client_mutation_id = ?1")
        .bind(client_mutation_id)
        .fetch_optional(pool)
        .await
        .map(Option::flatten)
}

/// The household owning a list, or `None` if the list does not exist.
async fn list_household(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT household_id FROM shopping_lists WHERE id = ?1")
        .bind(list_id)
        .fetch_optional(pool)
        .await
}

/// True if `user_id` is a member of `household_id` (mirrors `lists::is_member`).
async fn is_member(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    household_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM household_members WHERE user_id = ?1 AND household_id = ?2",
    )
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Atomically bump a list's `current_revision` by one and return the new value.
///
/// The `RETURNING` clause reads the incremented value on the same statement, so
/// concurrent writers (serialised by SQLite's write lock) each see a distinct
/// revision.
async fn increment_revision(
    tx: &mut Transaction<'_, Sqlite>,
    list_id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE shopping_lists SET current_revision = current_revision + 1 \
         WHERE id = ?1 RETURNING current_revision",
    )
    .bind(list_id)
    .fetch_one(&mut **tx)
    .await
}

/// Stamp an entry's `last_updated_revision` with the mutation's server revision.
async fn stamp_entry_revision(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    revision: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE list_entries SET last_updated_revision = ?2 WHERE id = ?1")
        .bind(entry_id)
        .bind(revision)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The current `status` of a `list_entries` row, or `None` if it does not exist.
async fn entry_status(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM list_entries WHERE id = ?1")
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await
}

/// Load a full `list_entries` row by id (transaction-scoped).
async fn load_entry(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
) -> Result<Option<EntryRow>, sqlx::Error> {
    let sql = format!("SELECT {ENTRY_COLUMNS} FROM list_entries WHERE id = ?1");
    sqlx::query_as::<_, EntryRow>(&sql)
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await
}

/// Append a `list_entry_events` row (used to preserve a losing action under a
/// delete-vs-mark_bought conflict). The mutation payload is stored verbatim.
async fn record_event(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    user_id: &str,
    event_type: &str,
    payload: &Value,
    created_at: &str,
) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let payload_json = if payload.is_null() {
        None
    } else {
        Some(payload.to_string())
    };
    sqlx::query(
        "INSERT INTO list_entry_events (id, entry_id, user_id, event_type, payload, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&id)
    .bind(entry_id)
    .bind(user_id)
    .bind(event_type)
    .bind(&payload_json)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Insert the append-only `mutations` row recording this accepted/merged apply.
async fn insert_mutation_row(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
    user: &AuthenticatedUser,
    server_revision: i64,
    outcome: &ApplyOutcome,
    response_json: &str,
) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let received_at = buywise_core::db::now_iso();
    let payload_json = req.payload.to_string();
    sqlx::query(
        "INSERT INTO mutations \
             (id, client_mutation_id, device_id, user_id, list_id, entity_type, entity_id, \
              operation_type, payload, base_revision, created_at_client, received_at_server, \
              server_revision, status, response_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
    )
    .bind(&id)
    .bind(&req.client_mutation_id)
    .bind(&req.device_id)
    .bind(&user.user_id)
    .bind(&req.list_id)
    .bind(&req.entity_type)
    .bind(&req.entity_id)
    .bind(&req.operation_type)
    .bind(&payload_json)
    .bind(req.base_revision)
    .bind(&req.created_at_client)
    .bind(&received_at)
    .bind(server_revision)
    .bind(outcome.status.as_str())
    .bind(response_json)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// True for one of the eight `mutations.operation_type` CHECK values.
fn is_known_operation(op: &str) -> bool {
    matches!(
        op,
        "add_item"
            | "mark_bought"
            | "mark_skipped"
            | "delete_item"
            | "rename_item"
            | "resolve_alias"
            | "reject_match"
            | "set_constraint"
    )
}

/// True if a sqlx error is a SQLite UNIQUE/PRIMARY KEY constraint violation.
///
/// SQLite reports both unique-index conflicts and primary-key conflicts with
/// extended result codes 2067 (`SQLITE_CONSTRAINT_UNIQUE`) and 1555
/// (`SQLITE_CONSTRAINT_PRIMARYKEY`). Matching the code is more robust than string
/// matching the message.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "2067" || code == "1555")
}

/// The non-empty `entity_id` an entry op targets, or `None` for an empty id.
fn entry_target(req: &MutationRequest) -> Option<String> {
    if req.entity_id.is_empty() {
        None
    } else {
        Some(req.entity_id.clone())
    }
}

/// The required target entry id, or 422 if the request carries none.
fn require_entry(req: &MutationRequest) -> Result<String, StatusCode> {
    entry_target(req).ok_or(StatusCode::UNPROCESSABLE_ENTITY)
}

/// A string field of a JSON object payload, if present and a string.
fn string_field(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Broadcast a committed mutation to SSE subscribers (P1-T5).
///
/// A deliberate no-op stub until the SSE stream lands: it is the seam the SSE
/// task will read from. Kept as a named function (rather than inlined) so the
/// post-commit broadcast point is explicit and testable later.
fn broadcast_mutation(
    _state: &AppState,
    _list_id: &str,
    _server_revision: i64,
    _response: &MutationResponse,
) {
    // Intentionally empty: wired in P1-T5.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_operations_are_exactly_the_eight_check_values() {
        for op in [
            "add_item",
            "mark_bought",
            "mark_skipped",
            "delete_item",
            "rename_item",
            "resolve_alias",
            "reject_match",
            "set_constraint",
        ] {
            assert!(is_known_operation(op), "{op} should be known");
        }
        assert!(!is_known_operation("frobnicate"));
        assert!(!is_known_operation(""));
        assert!(!is_known_operation("ADD_ITEM"));
    }

    #[test]
    fn entry_target_rejects_empty_id() {
        let mut req = sample_request();
        req.entity_id = String::new();
        assert!(entry_target(&req).is_none());
        assert_eq!(require_entry(&req), Err(StatusCode::UNPROCESSABLE_ENTITY));

        req.entity_id = "entry-1".to_string();
        assert_eq!(entry_target(&req).as_deref(), Some("entry-1"));
    }

    #[test]
    fn string_field_reads_only_present_strings() {
        let payload = serde_json::json!({ "display_name": "Milk", "quantity": 2 });
        assert_eq!(
            string_field(&payload, "display_name").as_deref(),
            Some("Milk")
        );
        // A non-string field is not a string.
        assert_eq!(string_field(&payload, "quantity"), None);
        assert_eq!(string_field(&payload, "missing"), None);
    }

    fn sample_request() -> MutationRequest {
        MutationRequest {
            client_mutation_id: "cm-1".into(),
            device_id: "dev-1".into(),
            list_id: "list-1".into(),
            base_revision: 0,
            entity_type: "list_entry".into(),
            entity_id: "entry-1".into(),
            operation_type: "mark_bought".into(),
            payload: Value::Null,
            created_at_client: "2026-05-31T00:00:00Z".into(),
        }
    }
}
