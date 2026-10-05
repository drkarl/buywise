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
//! Processing:
//!   1. Verify the caller is a member of the list's household (404 missing list,
//!      403 non-member — the same distinction `lists.rs` draws). This runs FIRST,
//!      before the replay lookup, so a guessed `client_mutation_id` can never leak
//!      another household's stored response.
//!   2. If `client_mutation_id` already exists AND it was recorded against this
//!      `list_id`, return the stored `response_json` verbatim (byte-identical
//!      idempotent replay — never re-applied). A key recorded for a different list
//!      is a 404 (it does not belong to `req.list_id`).
//!   3. Reserve the `mutations` row keyed by `client_mutation_id` as the FIRST
//!      write of the transaction (before any entry write). This makes the
//!      idempotency key authoritative: two concurrent identical requests that both
//!      missed the fast-path replay check serialise here, and the loser hits the
//!      `client_mutation_id` UNIQUE constraint, rolls back, and replays the
//!      winner's committed response — before it can hit any entry-id PK collision.
//!      (steps 3–6 inside ONE transaction.)
//!   4. Apply the mutation to `list_entries` (insert / field update), honouring
//!      the conflict rules below.
//!   5. Increment `shopping_lists.current_revision` atomically.
//!   6. Finalise the reserved `mutations` row with `server_revision = new
//!      current_revision`, the real status, and the serialised `response_json`.
//!   7. After COMMIT, broadcast over SSE (P1-T5). Broadcasting is deliberately
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
use chrono::{DateTime, Utc};
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
    ///
    /// Recorded verbatim in `mutations.base_revision` but NOT used to gate the
    /// apply: the conflict policy is last-write-wins, so a stale `base_revision`
    /// never rejects a mutation today. It is persisted for audit and for a future
    /// staleness/optimistic-concurrency check (e.g. surfacing "this changed since
    /// you last synced") if the policy tightens beyond LWW.
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
#[derive(Debug, Serialize, Deserialize, Clone)]
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
    /// Whether an active, resolved entry lacks an accepted or suggested match.
    #[serde(default)]
    pub needs_match: bool,
}

/// Every list-entry field plus current match coverage, decoded from a fixed SELECT.
#[derive(sqlx::FromRow)]
pub(crate) struct EntryRow {
    id: String,
    list_id: String,
    canonical_item_id: Option<String>,
    resolution_status: String,
    resolution_candidates_json: Option<String>,
    raw_text: String,
    display_name: String,
    added_by: String,
    quantity: i64,
    status: String,
    bought_by: Option<String>,
    bought_at: Option<String>,
    effective_constraint_hash: String,
    last_updated_revision: i64,
    created_at: String,
    updated_at: String,
    needs_match: i64,
}

/// The list-entry fields plus current match coverage, matching [`EntryRow`] order.
pub(crate) const ENTRY_COLUMNS: &str = "id, list_id, canonical_item_id, resolution_status, \
     resolution_candidates_json, raw_text, display_name, added_by, quantity, status, \
     bought_by, bought_at, effective_constraint_hash, last_updated_revision, created_at, updated_at, \
     CASE WHEN list_entries.status = 'active' AND list_entries.resolution_status = 'resolved' \
       AND list_entries.canonical_item_id IS NOT NULL AND NOT EXISTS ( \
         SELECT 1 FROM shopping_lists sl JOIN product_matches pm ON pm.household_id = sl.household_id \
         WHERE sl.id = list_entries.list_id AND pm.canonical_item_id = list_entries.canonical_item_id \
           AND pm.status IN ('accepted', 'suggested') \
       ) THEN 1 ELSE 0 END AS needs_match";

impl From<EntryRow> for EntityView {
    fn from(r: EntryRow) -> Self {
        EntityView {
            id: r.id,
            list_id: r.list_id,
            canonical_item_id: r.canonical_item_id,
            resolution_status: r.resolution_status,
            resolution_candidates_json: r.resolution_candidates_json,
            raw_text: r.raw_text,
            display_name: r.display_name,
            added_by: r.added_by,
            quantity: r.quantity,
            status: r.status,
            bought_by: r.bought_by,
            bought_at: r.bought_at,
            effective_constraint_hash: r.effective_constraint_hash,
            last_updated_revision: r.last_updated_revision,
            created_at: r.created_at,
            updated_at: r.updated_at,
            needs_match: r.needs_match != 0,
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

    // 1. Authorisation: the list must exist and belong to a household the caller
    //    is a member of. 404 vs 403 mirrors lists.rs. This runs BEFORE the
    //    idempotent-replay lookup so a guessed `client_mutation_id` can never leak
    //    another household's stored response (H4): a non-member is rejected before
    //    any stored bytes are read.
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

    // 2. Idempotent replay: a known client_mutation_id returns the stored bytes
    //    without touching any state. The stored row is only replayed when its
    //    recorded `list_id` matches `req.list_id`; a mismatch means the key was
    //    reused against a different list and is rejected (H4), so the response of
    //    one list can never be served for another.
    if let Some(replay) = replay_response(&state.pool, &req.client_mutation_id, &req.list_id)
        .await
        .map_err(db_error("post_mutation.idempotency"))?
    {
        return replay;
    }

    // 3–5. Reserve the idempotency key, apply, bump the revision, and finalise the
    //       mutation row — all in one transaction.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(db_error("post_mutation.begin"))?;

    // Reserve the `mutations` row keyed by `client_mutation_id` as the FIRST write,
    // before any entry write. The pre-transaction replay check is only a fast path;
    // two concurrent identical requests can both miss it, so the key must be made
    // authoritative inside the transaction here. SQLite serialises writers, so the
    // loser's reserve-insert blocks until the winner commits and then loses the
    // `client_mutation_id` UNIQUE race (code 2067): it rolls back and replays the
    // winner's committed response as a byte-identical 200. Reserving first means a
    // duplicate replays BEFORE `apply_add_item` can hit the entry-id PK collision,
    // so a duplicate never spuriously 409s (only a genuinely different
    // `client_mutation_id` re-using a live entry id reaches that 409 path).
    if let Err(e) = reserve_mutation_row(&mut tx, &req, &user).await {
        if is_client_mutation_id_conflict(&e) {
            let _ = tx.rollback().await;
            return replay_after_conflict(&state.pool, &req.client_mutation_id, &req.list_id).await;
        }
        let _ = tx.rollback().await;
        return Err(db_error("post_mutation.reserve_mutation")(e));
    }

    let outcome = match apply_mutation(&mut tx, &user, &household_id, &req).await {
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
    // detail view agree on which revision last changed it. Both the stamp and the
    // re-read are scoped to `(id, list_id)`: this is the single shared post-apply
    // path, so scoping it here closes cross-list IDOR for EVERY op (including the
    // accept-and-log ops that do not run their own UPDATE) rather than relying on
    // each op to scope individually. A non-matching entry id is a 404.
    if let Some(entry_id) = &outcome.entry_id {
        let stamped = stamp_entry_revision(&mut tx, entry_id, &req.list_id, server_revision)
            .await
            .map_err(db_error("post_mutation.stamp_entry"))?;
        if !stamped {
            let _ = tx.rollback().await;
            return Err(StatusCode::NOT_FOUND);
        }
    }

    // Re-read the affected entry so the response reflects the committed state
    // (including the revision stamp and any merge increment), scoped to the list.
    let entity = match &outcome.entry_id {
        Some(entry_id) => load_entry(&mut tx, entry_id, &req.list_id)
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

    // Finalise the reserved mutation row with the real server_revision, status, and
    // response_json. The UNIQUE-conflict replay was already handled at reserve time
    // above, so this UPDATE only ever stamps our own reserved row.
    finalize_mutation_row(
        &mut tx,
        &req.client_mutation_id,
        server_revision,
        &outcome,
        &response_json,
    )
    .await
    .map_err(db_error("post_mutation.finalize_mutation"))?;

    // A dedup-merge that deleted a PRE-EXISTING row clients already hold (the
    // `resolve_alias` ambiguous loser) must broadcast that deletion too. The
    // survivor was stamped/finalised at R above; the loser gets its OWN distinct
    // revision R+1, stamp, and replayable `mutations` row — INSIDE this same
    // transaction, so both bumps commit atomically — exactly mirroring
    // `resolution::merge_into_survivor`. Without this the deletion carries no
    // revision and is never broadcast, so peers and `?since=` reconnects show the
    // ambiguous row forever (the SSE-cursor partial-delivery failure mode).
    let loser_broadcast = if let Some(loser_id) = &outcome.deleted_loser_entry_id {
        let loser_revision = increment_revision(&mut tx, &req.list_id)
            .await
            .map_err(db_error("post_mutation.increment_revision_loser"))?;
        let stamped = stamp_entry_revision(&mut tx, loser_id, &req.list_id, loser_revision)
            .await
            .map_err(db_error("post_mutation.stamp_loser"))?;
        if !stamped {
            let _ = tx.rollback().await;
            return Err(StatusCode::NOT_FOUND);
        }
        // The survivor the loser merged into is the op's primary `entry_id` (set by
        // the dedup-merge branch). Record it in the loser's `merged_into` audit
        // payload so the row never points at itself.
        let survivor_id = outcome.entry_id.as_deref().ok_or_else(|| {
            tracing::error!("mutations: dedup-merge loser without a survivor entry_id");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        insert_loser_replay_row(&mut tx, &req, &user, loser_id, survivor_id, loser_revision)
            .await
            .map_err(db_error("post_mutation.loser_replay_row"))?;
        Some(loser_revision)
    } else {
        None
    };

    tx.commit()
        .await
        .map_err(db_error("post_mutation.commit"))?;

    // 6. Broadcast over SSE (P1-T5). Intentionally after COMMIT so subscribers
    //    never observe an uncommitted mutation.
    broadcast_mutation(&state, &req.list_id, server_revision, &response);

    // Broadcast the merged-away loser's deletion at R+1, after R, matching the
    // ascending-revision order `?since=` backfill replays (so live and replay order
    // are identical). Read-after-commit and best-effort, like the cascade's loser
    // broadcast: a failure here is logged, never propagated.
    if let Some(loser_revision) = loser_broadcast {
        if let Some(loser_id) = &outcome.deleted_loser_entry_id {
            crate::resolution::broadcast_entry_id(&state, &req.list_id, loser_id, loser_revision)
                .await;
        }
    }

    // 7. Server-side resolution cascade (P3-T1). An `add_item` that committed as
    //    `unresolved` is resolved asynchronously: the mutation is already
    //    accepted, so the UI never blocks on it. Anything else (a resolved add, a
    //    field update, or a merge into an existing row) needs no cascade.
    if req.operation_type == "add_item" {
        if let Some(entity) = &response.entity {
            if entity.resolution_status == "unresolved" {
                crate::resolution::spawn_resolution(
                    state.clone(),
                    crate::resolution::ResolutionJob {
                        entry_id: entity.id.clone(),
                        list_id: req.list_id.clone(),
                        household_id: household_id.clone(),
                        user_id: user.user_id.clone(),
                        raw_text: entity.raw_text.clone(),
                        constraints: req.payload.get("constraints").cloned(),
                    },
                );
            }
        }
    }

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
    /// A SECOND entry the apply marked `deleted` that must get its OWN distinct
    /// revision stamp, replayable `mutations` row, and SSE broadcast — set only by
    /// the `resolve_alias` dedup-merge branch for the ambiguous loser row.
    ///
    /// The shared post-apply path stamps/broadcasts `entry_id` (the survivor) at
    /// revision R; when this is `Some`, it stamps this loser at a distinct R+1,
    /// writes its replayable row, and broadcasts it — mirroring the resolution
    /// cascade's `merge_into_survivor` (two distinct revisions, both broadcast) so
    /// peers and `?since=R` reconnects learn the row was deleted (the SSE-cursor
    /// partial-delivery failure mode a shared revision would re-introduce).
    deleted_loser_entry_id: Option<String>,
}

impl ApplyOutcome {
    /// The common case: an op touched one entry and has no merged-away loser.
    fn touched(entry_id: Option<String>, status: MutationStatus) -> Self {
        ApplyOutcome {
            entry_id,
            status,
            deleted_loser_entry_id: None,
        }
    }
}

/// Apply one mutation to `list_entries`, returning which entry was affected and
/// the resulting status. Pure DB errors are propagated as 500 via the caller;
/// client-fault conditions short-circuit with the mapped status here.
async fn apply_mutation(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    household_id: &str,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    match req.operation_type.as_str() {
        "add_item" => apply_add_item(tx, user, household_id, req).await,
        "delete_item" => apply_delete_item(tx, user, req).await,
        "mark_bought" => apply_mark_bought(tx, user, household_id, req).await,
        "mark_skipped" => apply_mark_skipped(tx, req).await,
        "move_to_list" => apply_move_to_list(tx, user, req).await,
        "rename_item" => apply_rename_item(tx, req).await,
        "set_quantity" => apply_set_quantity(tx, req).await,
        // resolve_alias is the user-confirmation step of the resolution flow
        // (P3-T3): it binds the chosen canonical item to an `ambiguous` entry,
        // transitions it to `resolved`, and persists the confirmed raw-text →
        // canonical mapping as an `accepted_match` alias — both writes inside this
        // one transaction (atomic; never a half-applied bind/alias).
        "resolve_alias" => apply_resolve_alias(tx, household_id, req).await,
        // dismiss_suggestion records the household's "not now" for a computed
        // purchase suggestion (issue #106). It touches no list entry: the
        // dismissal table gains a row, and the response carries a null entity
        // (the client advances the revision and refreshes suggestions).
        "dismiss_suggestion" => apply_dismiss_suggestion(tx, user, household_id, req).await,
        // reject_match / set_constraint remain accept-and-log (last-write-wins,
        // no extra row mutation): the richer field semantics land later. The
        // targeted entry is still verified to belong to `req.list_id` (the shared
        // post-apply stamp/load path 404s a cross-list id), so these ops cannot
        // touch or read a foreign entry.
        "reject_match" | "set_constraint" => Ok(ApplyOutcome::touched(
            entry_target(req),
            MutationStatus::Accepted,
        )),
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
    household_id: &str,
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

    // canonical_items are household-scoped: a resolved add may only bind a catalogue
    // item belonging to the target list's household. Without this check a caller
    // could attach another household's item by guessing its id (cross-household
    // catalogue leak). An unknown/foreign id is a 404 (the item does not exist for
    // this household), matching how other entity-not-found cases are surfaced.
    if let Some(item_id) = &canonical_item_id {
        if !canonical_item_in_household(tx, item_id, household_id)
            .await
            .map_err(db_error("apply_add_item.canonical_item_check"))?
        {
            return Err(StatusCode::NOT_FOUND);
        }
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
        Ok(_) => Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        )),
        // A PRIMARY KEY collision (1555) means this exact `entity_id` already
        // exists — it is NOT a dedup boundary. An add re-using a live id is a
        // client conflict (e.g. a botched offline replay that did not carry the
        // original `client_mutation_id`); surface 409 rather than silently merging
        // into an unrelated row. True idempotent replay is handled earlier by the
        // `client_mutation_id` path.
        Err(e) if is_primary_key_violation(&e) => Err(StatusCode::CONFLICT),
        // Only the partial dedup index (2067, uq_list_entries_active_dedup) drives
        // the merge: fold this add into the existing active row for the same
        // (list, canonical item, constraint hash) and increment its quantity.
        Err(e) if is_dedup_index_violation(&e) => {
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
            // No loser broadcast here: `apply_add_item`'s loser was the INSERT that
            // just failed the dedup index, so it never created a row any client
            // holds. (Contrast `apply_resolve_alias`, whose loser is a pre-existing
            // ambiguous row clients DO hold — that one must be broadcast.)
            Ok(ApplyOutcome::touched(
                Some(surviving),
                MutationStatus::Merged,
            ))
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

    let prior_status = entry_status(tx, &entry_id, &req.list_id)
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

    sqlx::query(
        "UPDATE list_entries SET status = 'deleted', updated_at = ?2 WHERE id = ?1 AND list_id = ?3",
    )
    .bind(&entry_id)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_delete_item.update"))?;

    Ok(ApplyOutcome::touched(
        Some(entry_id),
        MutationStatus::Accepted,
    ))
}

/// `mark_bought`: set status `bought`, stamp `bought_by`/`bought_at`. If the
/// entry was already `deleted`, the delete wins (status stays `deleted`) and the
/// bought action is recorded in `list_entry_events` instead.
///
/// A successful check-off of a RESOLVED entry also appends a `purchase_events`
/// row (source `check_off`) so the household's purchase history builds up from
/// normal use (issue #106). `move_to_list` removes that evidence again when the
/// check-off is undone. An unresolved entry has no canonical item to attribute
/// the purchase to, so no evidence is written for it.
async fn apply_mark_bought(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    household_id: &str,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();
    let server_now = DateTime::parse_from_rfc3339(&now)
        .expect("now_iso returns RFC3339")
        .with_timezone(&Utc);
    let bought_at = string_field(&req.payload, "bought_at")
        .and_then(|raw| DateTime::parse_from_rfc3339(&raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc).min(server_now.clone()))
        .unwrap_or(server_now)
        .to_rfc3339();

    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, canonical_item_id FROM list_entries WHERE id = ?1 AND list_id = ?2",
    )
    .bind(&entry_id)
    .bind(&req.list_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error("apply_mark_bought.lookup"))?;
    let (prior_status, canonical_item_id) = row.ok_or(StatusCode::NOT_FOUND)?;

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
        return Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        ));
    }

    sqlx::query(
        "UPDATE list_entries \
         SET status = 'bought', bought_by = ?2, bought_at = ?3, updated_at = ?4 \
         WHERE id = ?1 AND list_id = ?5",
    )
    .bind(&entry_id)
    .bind(&user.user_id)
    .bind(&bought_at)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_mark_bought.update"))?;

    // Purchase evidence, only for a resolved entry (see this fn's doc comment).
    if let Some(canonical_item_id) = canonical_item_id {
        sqlx::query("DELETE FROM purchase_events WHERE entry_id = ?1")
            .bind(&entry_id)
            .execute(&mut **tx)
            .await
            .map_err(db_error("apply_mark_bought.replace_evidence"))?;

        sqlx::query(
            "INSERT INTO purchase_events \
                 (id, household_id, canonical_item_id, list_id, entry_id, source, \
                  occurred_at, recorded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'check_off', ?6, ?7)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(household_id)
        .bind(&canonical_item_id)
        .bind(&req.list_id)
        .bind(&entry_id)
        .bind(&bought_at)
        .bind(&now)
        .execute(&mut **tx)
        .await
        .map_err(db_error("apply_mark_bought.evidence"))?;
    }

    Ok(ApplyOutcome::touched(
        Some(entry_id),
        MutationStatus::Accepted,
    ))
}

/// `mark_skipped`: set status `skipped` (last-write-wins). Like `mark_bought`, a
/// delete wins: a deleted entry is not resurrected (M1). The lookup and UPDATE are
/// scoped to `(id, list_id)` so a cross-list id is a 404 (H3).
async fn apply_mark_skipped(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();

    let prior_status = entry_status(tx, &entry_id, &req.list_id)
        .await
        .map_err(db_error("apply_mark_skipped.lookup"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Delete wins: do not resurrect a deleted entry to 'skipped'. The mutation is
    // still recorded (accepted, no-op on the row), keeping the revision counter
    // and audit log consistent with mark_bought's delete-wins handling.
    if prior_status == "deleted" {
        return Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        ));
    }

    sqlx::query(
        "UPDATE list_entries SET status = 'skipped', updated_at = ?2 WHERE id = ?1 AND list_id = ?3",
    )
    .bind(&entry_id)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_mark_skipped.update"))?;

    sqlx::query("DELETE FROM purchase_events WHERE entry_id = ?1")
        .bind(&entry_id)
        .execute(&mut **tx)
        .await
        .map_err(db_error("apply_mark_skipped.evidence"))?;

    Ok(ApplyOutcome::touched(
        Some(entry_id),
        MutationStatus::Accepted,
    ))
}

/// `dismiss_suggestion`: record the household's "not now" for a purchase
/// suggestion (issue #106) so the item is suppressed for one typical interval.
///
/// A dismissal is household-scoped evidence keyed by canonical item, not a list
/// entry, so `entity_type` is `canonical_item` and `entity_id` carries the
/// canonical item id. The item must belong to the caller's household (a guessed
/// foreign id is a 404, mirroring `add_item`). The response entity is `None`; the
/// shared post-apply path skips the entry stamp, still bumps the list revision so
/// the mutation replays in order, and the client refreshes its suggestions.
async fn apply_dismiss_suggestion(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    household_id: &str,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let canonical_item_id = req.entity_id.trim();
    if canonical_item_id.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if !canonical_item_in_household(tx, canonical_item_id, household_id)
        .await
        .map_err(db_error("apply_dismiss_suggestion.canonical_item_check"))?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    sqlx::query(
        "INSERT INTO purchase_dismissals \
             (id, household_id, canonical_item_id, dismissed_by, dismissed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(household_id)
    .bind(canonical_item_id)
    .bind(&user.user_id)
    .bind(buywise_core::db::now_iso())
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_dismiss_suggestion.insert"))?;

    Ok(ApplyOutcome::touched(None, MutationStatus::Accepted))
}

/// `move_to_list`: move a `bought` or `skipped` entry back to `active` — the
/// "undo, I checked this off / skipped it by mistake" action — clearing
/// `bought_by`/`bought_at`. Last-write-wins, scoped to `(id, list_id)` so a
/// cross-list id is a 404 (H3).
///
/// Delete wins (M1): a deleted entry is never resurrected. As with
/// `mark_skipped`, the mutation is still recorded as an accepted no-op on the row
/// so the revision counter and idempotency log stay consistent.
///
/// Dedup merge: reactivating a RESOLVED entry can collide with the partial unique
/// index `uq_list_entries_active_dedup` when another active row for the same
/// `(list, canonical item, constraint hash)` already exists (buy Milk, add Milk
/// again, then undo the buy). Rather than surface that as a retryable 500, we fold
/// this entry's quantity into the surviving active row and tombstone this one —
/// mirroring `apply_resolve_alias`'s merge. The loser is a row clients already
/// hold (it sat in their basket), so its deletion is broadcast at its own revision
/// via `deleted_loser_entry_id`. Unresolved entries carry a per-entry sentinel
/// hash and are excluded by the index, so they never take this branch.
async fn apply_move_to_list(
    tx: &mut Transaction<'_, Sqlite>,
    user: &AuthenticatedUser,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let now = buywise_core::db::now_iso();

    // Load the status plus the dedup key straight off the row (the entry is
    // already resolved or not; we never recompute the hash here).
    let row: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT status, canonical_item_id, effective_constraint_hash \
         FROM list_entries WHERE id = ?1 AND list_id = ?2",
    )
    .bind(&entry_id)
    .bind(&req.list_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error("apply_move_to_list.lookup"))?;
    let (prior_status, canonical_item_id, hash) = row.ok_or(StatusCode::NOT_FOUND)?;

    if prior_status == "deleted" {
        record_event(
            tx,
            &entry_id,
            &user.user_id,
            "move_to_list_after_delete",
            &req.payload,
            &now,
        )
        .await
        .map_err(db_error("apply_move_to_list.event"))?;
        return Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        ));
    }

    // Undoing a check-off also undoes its purchase evidence: a mistaken
    // check-off must not pollute the interval history (issue #106). Runs before
    // the status write so both the plain reactivate and the dedup-merge branch
    // leave no check_off evidence behind.
    sqlx::query("DELETE FROM purchase_events WHERE entry_id = ?1")
        .bind(&entry_id)
        .execute(&mut **tx)
        .await
        .map_err(db_error("apply_move_to_list.evidence"))?;

    let update = sqlx::query(
        "UPDATE list_entries \
         SET status = 'active', bought_by = NULL, bought_at = NULL, updated_at = ?2 \
         WHERE id = ?1 AND list_id = ?3",
    )
    .bind(&entry_id)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await;

    match update {
        Ok(_) => Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        )),
        // The partial dedup index fired: a resolved active row for the same
        // (list, canonical item, hash) already exists. Fold this entry into it and
        // tombstone this one, mirroring `apply_resolve_alias`'s merge. The active
        // UPDATE above failed (and rolled back), so this row still carries its
        // original quantity for the SELECT below.
        Err(e) if is_dedup_index_violation(&e) => {
            let surviving: Option<String> = sqlx::query_scalar(
                "UPDATE list_entries \
                 SET quantity = quantity + ( \
                         SELECT quantity FROM list_entries WHERE id = ?4 AND list_id = ?1 \
                     ), \
                     updated_at = ?3 \
                 WHERE list_id = ?1 AND canonical_item_id = ?2 \
                   AND effective_constraint_hash = ?5 AND status = 'active' \
                 RETURNING id",
            )
            .bind(&req.list_id)
            .bind(&canonical_item_id)
            .bind(&now)
            .bind(&entry_id)
            .bind(&hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_error("apply_move_to_list.merge"))?;

            // The conflict proves a colliding active row exists, so the UPDATE
            // must have matched it.
            let surviving = surviving.ok_or_else(|| {
                tracing::error!("mutations: move_to_list conflict but no row to merge into");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            // Tombstone the loser (still `bought`/`skipped` — the active UPDATE
            // rolled back). `merged` is not a valid `status`, so it becomes
            // `deleted`.
            sqlx::query(
                "UPDATE list_entries SET status = 'deleted', updated_at = ?2 \
                 WHERE id = ?1 AND list_id = ?3",
            )
            .bind(&entry_id)
            .bind(&now)
            .bind(&req.list_id)
            .execute(&mut **tx)
            .await
            .map_err(db_error("apply_move_to_list.merge_delete"))?;

            // The loser is a PRE-EXISTING row clients already hold (it sat in their
            // basket), so its deletion needs its own distinct revision + replayable
            // `mutations` row + SSE broadcast, or peers and `?since=` reconnects show
            // it forever. The shared post-apply path does this for
            // `deleted_loser_entry_id` at R+1 (same as `apply_resolve_alias`).
            Ok(ApplyOutcome {
                entry_id: Some(surviving),
                status: MutationStatus::Merged,
                deleted_loser_entry_id: Some(entry_id),
            })
        }
        Err(e) => Err(db_error("apply_move_to_list.update")(e)),
    }
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

    let prior_status = entry_status(tx, &entry_id, &req.list_id)
        .await
        .map_err(db_error("apply_rename_item.lookup"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Delete wins: a deleted entry is not resurrected by a rename (M1). Record the
    // mutation as a no-op accept, matching mark_bought/mark_skipped.
    if prior_status == "deleted" {
        return Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        ));
    }

    sqlx::query(
        "UPDATE list_entries \
         SET display_name = ?2, raw_text = COALESCE(?3, raw_text), updated_at = ?4 \
         WHERE id = ?1 AND list_id = ?5",
    )
    .bind(&entry_id)
    .bind(&display_name)
    .bind(&raw_text)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_rename_item.update"))?;

    Ok(ApplyOutcome::touched(
        Some(entry_id),
        MutationStatus::Accepted,
    ))
}

/// Upper bound for a `set_quantity` payload. A realistic shopping quantity never
/// approaches this; rejecting beyond it stops an absurd value (or a typo/overflow)
/// from being persisted. Keep this in sync with the client's `+`-button ceiling.
const MAX_QUANTITY: i64 = 9999;

/// `set_quantity`: set the entry's `quantity` to an ABSOLUTE value (issue #60),
/// last-write-wins. The payload carries the new value (`{ "quantity": <int >= 1> }`);
/// an absolute value is idempotent-friendly under retries/LWW (a replayed or stale
/// queued op can never double-count, unlike an `adjust_quantity` delta). The value
/// is the single syncable representation of the list UI's +/- controls.
///
/// Validation: `quantity` must be a present integer in `1..=MAX_QUANTITY`. A
/// missing/non-integer payload, a value `< 1`, or a value `> MAX_QUANTITY` is a
/// 422 (reject, not clamp — a clear client error beats a silent correction the
/// client never learns about). The client mirrors both bounds: it disables the
/// `-` button at 1 and the `+` button at `MAX_QUANTITY`, so a valid client never
/// sends an out-of-range value.
///
/// Like the other field updates, a delete wins: a deleted entry is not resurrected
/// (M1). The lookup and UPDATE are scoped to `(id, list_id)` so a cross-list id is
/// a 404 (H3).
async fn apply_set_quantity(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    // Absolute quantity, required and within `1..=MAX_QUANTITY`. `as_i64` rejects
    // floats/strings/missing; the range filter rejects `< 1` and absurd ceilings.
    let quantity = req
        .payload
        .get("quantity")
        .and_then(Value::as_i64)
        .filter(|q| (1..=MAX_QUANTITY).contains(q))
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let now = buywise_core::db::now_iso();

    let prior_status = entry_status(tx, &entry_id, &req.list_id)
        .await
        .map_err(db_error("apply_set_quantity.lookup"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Delete wins: a deleted entry is not resurrected by a quantity change (M1).
    // Record the mutation as a no-op accept, matching mark_bought/mark_skipped/rename.
    if prior_status == "deleted" {
        return Ok(ApplyOutcome::touched(
            Some(entry_id),
            MutationStatus::Accepted,
        ));
    }

    sqlx::query(
        "UPDATE list_entries SET quantity = ?2, updated_at = ?3 WHERE id = ?1 AND list_id = ?4",
    )
    .bind(&entry_id)
    .bind(quantity)
    .bind(&now)
    .bind(&req.list_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("apply_set_quantity.update"))?;

    Ok(ApplyOutcome::touched(
        Some(entry_id),
        MutationStatus::Accepted,
    ))
}

/// `resolve_alias`: the user-confirmation step (P3-T3). An `ambiguous` entry is
/// bound to the canonical item the user chose, transitioning it to `resolved`,
/// and the confirmed `raw_text` → canonical mapping is persisted as an
/// `accepted_match` alias so a later add of the same term resolves via the
/// step-1 alias lookup with no fuzzy/LLM work (docs/implementation-plan.md
/// P3-T3 / P3-T4 step 4).
///
/// Contract (payload, per the design doc — NOT the issue body's older field
/// name): `chosen_canonical_item_id` is the bound item; `raw_text` is the term
/// to learn as an alias.
///
/// Atomicity / TOCTOU: the bind UPDATE is guarded on the entry still being
/// `ambiguous` + `active` and scoped to `(id, list_id)`, and the alias write
/// runs INSIDE the same transaction — so the alias is committed iff the bind it
/// justifies also commits (never on a stale no-op). This mirrors the resolution
/// cascade's `resolve_single_match` ordering (the exact concern P3-T2's review
/// fixes hardened). Binding a `canonical_item_id` makes the row a dedup
/// candidate, so the partial unique index `uq_list_entries_active_dedup` can
/// fire *during* the UPDATE if a resolved active row for the same
/// `(list, canonical item, hash)` already exists; that violation is caught and
/// the two entries are merged (status `merged`), matching `apply_add_item`.
async fn apply_resolve_alias(
    tx: &mut Transaction<'_, Sqlite>,
    household_id: &str,
    req: &MutationRequest,
) -> Result<ApplyOutcome, StatusCode> {
    let entry_id = require_entry(req)?;
    let canonical_item_id = string_field(&req.payload, "chosen_canonical_item_id")
        .filter(|s| !s.trim().is_empty())
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    // `raw_text` is the term to learn as an alias. It is required for the
    // confirmation flow (the doc's payload always carries it); a blank/absent
    // value means we cannot learn anything, so we still bind but persist no alias
    // rather than writing an empty alias the unique index could not dedup on.
    let raw_text = string_field(&req.payload, "raw_text");

    // The chosen item must belong to the target list's household — without this a
    // caller could bind another household's catalogue item by guessing its id
    // (the same cross-household guard `apply_add_item` applies). Unknown/foreign
    // id → 404.
    if !canonical_item_in_household(tx, &canonical_item_id, household_id)
        .await
        .map_err(db_error("apply_resolve_alias.canonical_item_check"))?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    let hash = effective_constraint_hash(
        &entry_id,
        Some(&canonical_item_id),
        req.payload.get("constraints"),
    );
    let now = buywise_core::db::now_iso();

    // The non-empty `raw_text` we are about to learn as an alias, used to GUARD the
    // bind: we only bind/learn for the term the entry actually still carries. A
    // blank/absent `raw_text` learns nothing, so it does NOT gate the bind (the
    // legitimate bind-but-no-alias path stays a plain `ambiguous`→`resolved`
    // transition). `None` here means "do not guard on raw_text".
    let raw_text_guard = raw_text.as_deref().filter(|s| !s.trim().is_empty());

    // Candidate-membership validation (review Medium): the chosen item must be one
    // of the candidates the resolver offered for THIS row. But the no-op precedence
    // comes FIRST — we only enforce membership when the row is a LIVE, bindable
    // ambiguous row (active + ambiguous + raw_text still current, i.e. exactly the
    // rows the guarded UPDATE below would actually bind). For any other row
    // (already resolved, deleted, cross-list, renamed/stale, or a repeat confirm)
    // we skip validation entirely and let the UPDATE fall through to its existing
    // no-op accept — a stale/repeat confirm must never become a noisy 422.
    let current: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT resolution_status, status, resolution_candidates_json \
         FROM list_entries \
         WHERE id = ?1 AND list_id = ?2 \
           AND resolution_status = 'ambiguous' AND status = 'active' \
           AND (?3 IS NULL OR raw_text = ?3)",
    )
    .bind(&entry_id)
    .bind(&req.list_id)
    .bind(raw_text_guard)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error("apply_resolve_alias.candidate_lookup"))?;

    if let Some((_, _, candidates_json)) = current {
        // A live ambiguous row: the chosen id MUST be in its candidate list.
        // `resolution_candidates_json` is a flat JSON array of canonical-item-id
        // strings (see `resolution::resolve_ambiguous`); parse it the same way.
        if !candidate_list_contains(candidates_json.as_deref(), &canonical_item_id) {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    // Guarded bind: only an `ambiguous`, still-`active` entry on this list
    // transitions. A row already resolved/deleted (e.g. a concurrent confirm or a
    // stale replay that slipped past idempotency) matches 0 rows — a no-op accept,
    // so a double-confirm never errors. The dedup index fires here if a survivor
    // exists.
    //
    // TOCTOU/integrity guard (review Medium): when a non-empty `raw_text` is
    // supplied we additionally require the row's CURRENT `raw_text` to equal it
    // (`?6 IS NULL OR raw_text = ?6`). Without this, a rename arriving while the
    // confirmation modal is open (or a malicious client supplying an arbitrary
    // `raw_text`) would bind the entry AND persist an alias for a term the row no
    // longer holds. This mirrors `resolution::resolve_single_match`'s
    // `raw_text`-guarded UPDATE: a renamed row simply matches 0 rows → no-op accept,
    // never a stale bind/alias.
    let update = sqlx::query(
        "UPDATE list_entries \
         SET resolution_status = 'resolved', canonical_item_id = ?3, \
             effective_constraint_hash = ?4, resolution_candidates_json = NULL, \
             updated_at = ?5 \
         WHERE id = ?1 AND list_id = ?2 \
           AND resolution_status = 'ambiguous' AND status = 'active' \
           AND (?6 IS NULL OR raw_text = ?6)",
    )
    .bind(&entry_id)
    .bind(&req.list_id)
    .bind(&canonical_item_id)
    .bind(&hash)
    .bind(&now)
    .bind(raw_text_guard)
    .execute(&mut **tx)
    .await;

    match update {
        Ok(result) => {
            if result.rows_affected() == 0 {
                // Nothing to resolve (already resolved/deleted, a cross-list id —
                // the shared stamp/load path 404s a genuinely foreign id — or the
                // raw_text guard failed because the row was renamed). Accept as a
                // no-op so a repeat confirm is idempotent.
                return Ok(ApplyOutcome::touched(
                    Some(entry_id),
                    MutationStatus::Accepted,
                ));
            }
            // Bound cleanly — the guarded UPDATE proved the entry was current, so
            // persist the confirmed alias INSIDE this transaction (committed with
            // the bind, atomically, never on a no-op).
            insert_accepted_alias(tx, household_id, &canonical_item_id, raw_text.as_deref())
                .await?;
            Ok(ApplyOutcome::touched(
                Some(entry_id),
                MutationStatus::Accepted,
            ))
        }
        // The partial dedup index fired: a resolved active row for the same
        // (list, canonical item, hash) already exists. Fold this entry into it,
        // mirroring `apply_add_item`'s merge. The alias is still valid (the chosen
        // item is unchanged), so it is persisted on this path too.
        Err(e) if is_dedup_index_violation(&e) => {
            let surviving: Option<String> = sqlx::query_scalar(
                "UPDATE list_entries \
                 SET quantity = quantity + ( \
                         SELECT quantity FROM list_entries WHERE id = ?5 AND list_id = ?1 \
                     ), \
                     updated_at = ?4 \
                 WHERE list_id = ?1 AND canonical_item_id = ?2 \
                   AND effective_constraint_hash = ?3 AND status = 'active' \
                 RETURNING id",
            )
            .bind(&req.list_id)
            .bind(&canonical_item_id)
            .bind(&hash)
            .bind(&now)
            .bind(&entry_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_error("apply_resolve_alias.merge"))?;

            // The conflict proves a colliding active row exists, so the UPDATE
            // must have matched it.
            let surviving = surviving.ok_or_else(|| {
                tracing::error!("mutations: resolve_alias conflict but no row to merge into");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            // Mark the ambiguous entry deleted (it loses to the survivor; `merged`
            // is not a valid `status`), then learn the confirmed alias.
            sqlx::query(
                "UPDATE list_entries SET status = 'deleted', updated_at = ?2 \
                 WHERE id = ?1 AND list_id = ?3",
            )
            .bind(&entry_id)
            .bind(&now)
            .bind(&req.list_id)
            .execute(&mut **tx)
            .await
            .map_err(db_error("apply_resolve_alias.merge_delete"))?;

            insert_accepted_alias(tx, household_id, &canonical_item_id, raw_text.as_deref())
                .await?;

            // The loser is a PRE-EXISTING `ambiguous` row that clients already hold
            // (unlike `apply_add_item`'s merge, whose loser was the INSERT that just
            // failed and so was never visible). Its deletion MUST therefore get its
            // own distinct revision + replayable `mutations` row + SSE broadcast, or
            // peers and `?since=` reconnects show the deleted row forever (the
            // SSE-cursor partial-delivery failure mode). The shared post-apply path
            // does this for `deleted_loser_entry_id` at R+1, mirroring the cascade's
            // `resolution::merge_into_survivor`.
            Ok(ApplyOutcome {
                entry_id: Some(surviving),
                status: MutationStatus::Merged,
                deleted_loser_entry_id: Some(entry_id),
            })
        }
        Err(e) => Err(db_error("apply_resolve_alias.bind")(e)),
    }
}

/// True if `candidate_id` appears in the row's `resolution_candidates_json` — a
/// flat JSON array of canonical-item-id strings (the shape
/// `resolution::resolve_ambiguous` writes via `serde_json::json!(&[String])`, and
/// the frontend parses the same way). A NULL/absent or unparseable value, or one
/// that is not a JSON array, contains nothing (so the chosen id is rejected): an
/// ambiguous row always carries a candidate array, and a malformed one is treated
/// conservatively rather than waved through.
fn candidate_list_contains(candidates_json: Option<&str>, candidate_id: &str) -> bool {
    let Some(raw) = candidates_json else {
        return false;
    };
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(ids) => ids.iter().any(|id| id == candidate_id),
        Err(_) => false,
    }
}

/// Persist a confirmed `raw_text` → canonical-item mapping as an `item_aliases`
/// row with `source='accepted_match'` (P3-T3 / P3-T4 step 4), via the shared
/// [`buywise_core::matching::commit_alias`] write path used by every resolution
/// path (steps 2/3/4) so normalisation, language inference, and dedup semantics
/// are identical.
///
/// `commit_alias` stores the term NORMALISED (matching how step-1 alias lookup
/// compares), infers the language bucket (`'es'` for a Spanish term, else `'en'`),
/// and uses `INSERT OR IGNORE` against `uq_item_aliases_household_alias_lang` so
/// confirming the same term twice is a harmless no-op rather than a constraint
/// error.
///
/// Runs on the caller's transaction so it commits atomically with the guarded
/// bind/merge UPDATE that proved the entry was current. A blank/absent `raw_text`
/// (or one that normalises to empty) learns nothing and is skipped.
async fn insert_accepted_alias(
    tx: &mut Transaction<'_, Sqlite>,
    household_id: &str,
    canonical_item_id: &str,
    raw_text: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(raw_text) = raw_text else {
        return Ok(());
    };
    buywise_core::matching::commit_alias(
        tx,
        household_id,
        canonical_item_id,
        raw_text,
        buywise_core::matching::AliasSource::AcceptedMatch,
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "apply_resolve_alias.insert_alias");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Small DB helpers (transaction-scoped + pool-scoped)
// ---------------------------------------------------------------------------

/// The stored `(list_id, response_json)` for a `client_mutation_id`, or `None` if
/// unseen. The recorded `list_id` lets the caller reject a key replayed against a
/// different list (H4).
async fn stored_mutation(
    pool: &sqlx::SqlitePool,
    client_mutation_id: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT list_id, response_json FROM mutations WHERE client_mutation_id = ?1")
        .bind(client_mutation_id)
        .fetch_optional(pool)
        .await
}

/// Look up a prior mutation by idempotency key and, if it targeted `req_list_id`,
/// build its byte-identical 200 replay. Returns `None` when the key is unseen so
/// the caller proceeds to apply. A stored row whose `list_id` differs from
/// `req_list_id` is rejected with 404 (the key belongs to a different list — never
/// leak its response, H4).
async fn replay_response(
    pool: &sqlx::SqlitePool,
    client_mutation_id: &str,
    req_list_id: &str,
) -> Result<Option<Result<(StatusCode, Json<MutationResponse>), StatusCode>>, sqlx::Error> {
    Ok(stored_mutation(pool, client_mutation_id)
        .await?
        .map(|(stored_list_id, response_json)| {
            decode_replay(&stored_list_id, response_json, req_list_id)
        }))
}

/// Re-fetch and replay the response a concurrent winner committed after we lost
/// the `client_mutation_id` UNIQUE race (H1). The winner is guaranteed to exist;
/// a missing/mismatched row is an invariant break surfaced as 500.
async fn replay_after_conflict(
    pool: &sqlx::SqlitePool,
    client_mutation_id: &str,
    req_list_id: &str,
) -> Result<(StatusCode, Json<MutationResponse>), StatusCode> {
    match stored_mutation(pool, client_mutation_id)
        .await
        .map_err(db_error("post_mutation.replay_after_conflict"))?
    {
        Some((stored_list_id, response_json)) => {
            decode_replay(&stored_list_id, response_json, req_list_id)
        }
        None => {
            tracing::error!(
                "mutations: client_mutation_id conflict but no committed row to replay"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Validate a stored mutation belongs to `req_list_id` and decode its response for
/// a 200 replay. Mismatched list → 404; missing/invalid `response_json` → 500.
fn decode_replay(
    stored_list_id: &str,
    response_json: Option<String>,
    req_list_id: &str,
) -> Result<(StatusCode, Json<MutationResponse>), StatusCode> {
    if stored_list_id != req_list_id {
        return Err(StatusCode::NOT_FOUND);
    }
    let stored = response_json.ok_or_else(|| {
        tracing::error!("mutations: stored mutation has no response_json to replay");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let response: MutationResponse = serde_json::from_str(&stored).map_err(|e| {
        tracing::error!(error = %e, "mutations: stored response_json is not valid JSON");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok((StatusCode::OK, Json(response)))
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

/// Stamp an entry's `last_updated_revision` with the mutation's server revision,
/// scoped to `(id, list_id)`. Returns `true` if a row on that list was stamped,
/// `false` if no `(id, list_id)` row matched — letting the caller 404 a cross-list
/// id and never bump a foreign entry's revision (the shared IDOR guard).
async fn stamp_entry_revision(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    list_id: &str,
    revision: i64,
) -> Result<bool, sqlx::Error> {
    let affected = sqlx::query(
        "UPDATE list_entries SET last_updated_revision = ?2 WHERE id = ?1 AND list_id = ?3",
    )
    .bind(entry_id)
    .bind(revision)
    .bind(list_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// True if `item_id` is a `canonical_items` row owned by `household_id`. Used to
/// reject a resolved `add_item` that binds a catalogue item from another household.
async fn canonical_item_in_household(
    tx: &mut Transaction<'_, Sqlite>,
    item_id: &str,
    household_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM canonical_items WHERE id = ?1 AND household_id = ?2")
            .bind(item_id)
            .bind(household_id)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.is_some())
}

/// The current `status` of a `list_entries` row scoped to `(id, list_id)`, or
/// `None` if no such entry exists on that list. Scoping by `list_id` (not `id`
/// alone) prevents a caller authorised for one list from reaching an entry that
/// belongs to a different list (cross-list IDOR, H3).
async fn entry_status(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    list_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM list_entries WHERE id = ?1 AND list_id = ?2")
        .bind(entry_id)
        .bind(list_id)
        .fetch_optional(&mut **tx)
        .await
}

/// Load a full `list_entries` row scoped to `(id, list_id)` (transaction-scoped),
/// so the response can never echo an entry from another list (IDOR guard).
async fn load_entry(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    list_id: &str,
) -> Result<Option<EntryRow>, sqlx::Error> {
    let sql = format!("SELECT {ENTRY_COLUMNS} FROM list_entries WHERE id = ?1 AND list_id = ?2");
    sqlx::query_as::<_, EntryRow>(&sql)
        .bind(entry_id)
        .bind(list_id)
        .fetch_optional(&mut **tx)
        .await
}

/// Load a full `list_entries` row scoped to `(id, list_id)` on the pool (not a
/// transaction), for the async resolution task which runs after the original
/// mutation's transaction has committed. Returns the row as an [`EntityView`] so
/// the SSE broadcast matches the `entity` shape `POST /api/mutations` emits.
pub(crate) async fn load_entry_view(
    pool: &sqlx::SqlitePool,
    entry_id: &str,
    list_id: &str,
) -> Result<Option<EntityView>, sqlx::Error> {
    let sql = format!("SELECT {ENTRY_COLUMNS} FROM list_entries WHERE id = ?1 AND list_id = ?2");
    Ok(sqlx::query_as::<_, EntryRow>(&sql)
        .bind(entry_id)
        .bind(list_id)
        .fetch_optional(pool)
        .await?
        .map(EntityView::from))
}

/// Transaction-scoped variant of [`load_entry_view`], scoped to `(id, list_id)`,
/// for the resolution cascade ([`crate::resolution`]) which must read the
/// post-resolution row WITHIN the same transaction that wrote it — so the entity
/// stored in the synthetic replay `mutations` row is byte-identical to the live
/// broadcast (both reflect the committed state).
pub(crate) async fn load_entry_view_tx(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    list_id: &str,
) -> Result<Option<EntityView>, sqlx::Error> {
    Ok(load_entry(tx, entry_id, list_id)
        .await?
        .map(EntityView::from))
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

/// Reserve the append-only `mutations` row keyed by `client_mutation_id` as the
/// first write in the transaction, before any entry write.
///
/// `server_revision` and `response_json` are left NULL (the schema allows both);
/// they are stamped by [`finalize_mutation_row`] once the apply succeeds. Status is
/// the `accepted` default placeholder, also corrected at finalise. Reserving here
/// makes the `client_mutation_id` UNIQUE constraint the authoritative idempotency
/// guard: a duplicate loses this insert (code 2067) and replays the winner's
/// response before it can reach the entry-id PK-collision path in `apply_add_item`.
async fn reserve_mutation_row(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
    user: &AuthenticatedUser,
) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let received_at = buywise_core::db::now_iso();
    let payload_json = req.payload.to_string();
    sqlx::query(
        "INSERT INTO mutations \
             (id, client_mutation_id, device_id, user_id, list_id, entity_type, entity_id, \
              operation_type, payload, base_revision, created_at_client, received_at_server, \
              server_revision, status, response_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, 'accepted', NULL)",
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
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Stamp the reserved `mutations` row (see [`reserve_mutation_row`]) with the real
/// `server_revision`, `status`, and `response_json` once the apply has succeeded.
async fn finalize_mutation_row(
    tx: &mut Transaction<'_, Sqlite>,
    client_mutation_id: &str,
    server_revision: i64,
    outcome: &ApplyOutcome,
    response_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE mutations \
         SET server_revision = ?2, status = ?3, response_json = ?4 \
         WHERE client_mutation_id = ?1",
    )
    .bind(client_mutation_id)
    .bind(server_revision)
    .bind(outcome.status.as_str())
    .bind(response_json)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Write the replayable `mutations` row for a dedup-merge's deleted loser, INSIDE
/// the request transaction, at the loser's OWN distinct `revision` (R+1).
///
/// Closes the same invariant `resolution::insert_resolution_mutation_row` does:
/// every revision bump needs a `mutations` row so a client reconnecting via
/// `?since=` backfills the outcome (`sse::backfill_since` replays only `mutations`
/// rows). Without it the loser's R+1 deletion would be lost on reconnect — the
/// SSE-cursor partial-delivery bug.
///
/// The row is server-origin (a fresh `client_mutation_id` UUID in the server's own
/// idempotency space, `device_id='server'`) and honest about what it represents:
/// `delete_item` / `merged`, matching the loser side of the cascade's merge. The
/// `entity` is loaded WITHIN this transaction so the stored `response_json.entity`
/// is byte-identical to the live broadcast the post-commit step emits.
async fn insert_loser_replay_row(
    tx: &mut Transaction<'_, Sqlite>,
    req: &MutationRequest,
    user: &AuthenticatedUser,
    loser_entry_id: &str,
    survivor_entry_id: &str,
    revision: i64,
) -> Result<(), sqlx::Error> {
    let entity = load_entry_view_tx(tx, loser_entry_id, &req.list_id).await?;
    let response = MutationResponse {
        client_mutation_id: uuid::Uuid::new_v4().to_string(),
        status: "merged".to_string(),
        server_revision: revision,
        entity,
    };
    let response_json = serde_json::to_value(&response)
        .map(|v| v.to_string())
        .map_err(|e| sqlx::Error::Protocol(format!("loser replay serialise failed: {e}")))?;

    // `merged_into` points at the SURVIVOR (not `req.entity_id`, which in this merge
    // branch IS the loser — recording that would say the entry merged into itself).
    // This matches the cascade's loser payload (`{ "merged_into": survivor_id }`).
    let payload = serde_json::json!({ "merged_into": survivor_entry_id }).to_string();
    let now = buywise_core::db::now_iso();
    sqlx::query(
        "INSERT INTO mutations \
             (id, client_mutation_id, device_id, user_id, list_id, entity_type, entity_id, \
              operation_type, payload, base_revision, created_at_client, received_at_server, \
              server_revision, status, response_json) \
         VALUES (?1, ?2, 'server', ?3, ?4, 'list_entry', ?5, 'delete_item', ?6, ?7, ?8, ?8, ?9, 'merged', ?10)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&response.client_mutation_id)
    .bind(&user.user_id)
    .bind(&req.list_id)
    .bind(loser_entry_id)
    .bind(&payload)
    .bind(revision - 1)
    .bind(&now)
    .bind(revision)
    .bind(&response_json)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// True for one of the `mutations.operation_type` CHECK values.
fn is_known_operation(op: &str) -> bool {
    matches!(
        op,
        "add_item"
            | "mark_bought"
            | "mark_skipped"
            | "move_to_list"
            | "delete_item"
            | "rename_item"
            | "resolve_alias"
            | "reject_match"
            | "set_constraint"
            | "set_quantity"
            | "dismiss_suggestion"
    )
}

/// The SQLite extended result code of a sqlx error, if it is a DB error.
///
/// Matching the extended code is more robust than string-matching the message.
fn sqlite_extended_code(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|db| db.code())
        .map(|c| c.into_owned())
}

/// True if the error is a UNIQUE-index violation (`SQLITE_CONSTRAINT_UNIQUE`,
/// 2067) raised by an INSERT/UPDATE on `list_entries`, where the only unique
/// index is the partial dedup index `uq_list_entries_active_dedup` — the conflict
/// that triggers a merge. Shared with the resolution cascade
/// ([`crate::resolution`]), which catches the same violation when it binds a
/// `canonical_item_id`.
///
/// NB: this is deliberately distinct from [`is_client_mutation_id_conflict`],
/// which matches the same extended code 2067 but for the unrelated UNIQUE
/// constraint on `mutations.client_mutation_id`; the two are kept separate so each
/// call site documents *which* constraint it expects to fire.
pub(crate) fn is_dedup_index_violation(e: &sqlx::Error) -> bool {
    sqlite_extended_code(e).as_deref() == Some("2067")
}

/// True if the error is a PRIMARY KEY violation (`SQLITE_CONSTRAINT_PRIMARYKEY`,
/// 1555) — a collision on `list_entries.id`, which is a client conflict, not a
/// dedup boundary (see [`apply_add_item`]).
fn is_primary_key_violation(e: &sqlx::Error) -> bool {
    sqlite_extended_code(e).as_deref() == Some("1555")
}

/// True if the error is the UNIQUE violation on `mutations.client_mutation_id`
/// (2067) — the in-flight idempotency race resolved by replaying the winner's
/// stored response (see the H1 handling in [`post_mutation`]).
fn is_client_mutation_id_conflict(e: &sqlx::Error) -> bool {
    sqlite_extended_code(e).as_deref() == Some("2067")
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
/// Called after COMMIT so subscribers never observe an uncommitted mutation. If
/// no client is currently streaming this list there is no channel in the
/// registry (or it has zero receivers); the send is then a silent no-op — a
/// disconnected client simply backfills via `?since=` on reconnect, so a dropped
/// live event is never lost. The broadcast carries the same `server_revision`
/// and `entity` shape the SSE backfill path emits, so a live event and a
/// replayed one are byte-identical to the client.
fn broadcast_mutation(
    state: &AppState,
    list_id: &str,
    server_revision: i64,
    response: &MutationResponse,
) {
    let entity = match serde_json::to_value(&response.entity) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, list_id, "sse: failed to serialise broadcast entity");
            return;
        }
    };
    crate::sse::broadcast_entity(state, list_id, server_revision, entity);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_operations_are_exactly_the_check_values() {
        for op in [
            "add_item",
            "mark_bought",
            "mark_skipped",
            "move_to_list",
            "delete_item",
            "rename_item",
            "resolve_alias",
            "reject_match",
            "set_constraint",
            "set_quantity",
            "dismiss_suggestion",
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

    #[test]
    fn candidate_list_contains_matches_only_array_members() {
        let json = r#"["ci-single","ci-double"]"#;
        assert!(candidate_list_contains(Some(json), "ci-double"));
        assert!(candidate_list_contains(Some(json), "ci-single"));
        // Not a member.
        assert!(!candidate_list_contains(Some(json), "ci-sour"));
        // NULL/absent candidates contain nothing.
        assert!(!candidate_list_contains(None, "ci-double"));
        // Malformed / non-array JSON is treated conservatively (contains nothing).
        assert!(!candidate_list_contains(Some("not json"), "ci-double"));
        assert!(!candidate_list_contains(Some(r#"{"a":1}"#), "ci-double"));
        assert!(!candidate_list_contains(Some("[]"), "ci-double"));
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
