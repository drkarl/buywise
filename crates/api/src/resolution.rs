//! Server-side resolution cascade for `add_item` mutations (P3-T1, issue #25).
//!
//! When an `add_item` mutation commits as `unresolved` (no client-supplied
//! `canonical_item_id`), [`crate::mutations::post_mutation`] spawns
//! [`spawn_resolution`] to run the Phase-0 cascade
//! ([`buywise_core::matching::resolve`]: alias lookup → token/fuzzy match)
//! against the household catalogue, then writes the outcome back to the entry and
//! broadcasts the change over SSE. The mutation is already accepted, so the UI
//! never blocks on resolution.
//!
//! Outcomes (per `docs/implementation-plan.md` P3-T1 / §8):
//!   * [`ResolutionOutcome::SingleMatch`] → the entry becomes `resolved`, its
//!     `canonical_item_id` is bound and its `effective_constraint_hash` is
//!     recomputed from the entry's constraints (never hardcoded `default` unless
//!     the entry genuinely has none). Binding a canonical item makes the entry a
//!     dedup candidate, so the unique index `uq_list_entries_active_dedup` can
//!     fire *during* the UPDATE if a colliding active row already exists; that
//!     constraint violation is caught and the two entries are merged (see
//!     [`resolve_single_match`]).
//!   * [`ResolutionOutcome::MultipleCandidates`] → the entry becomes `ambiguous`
//!     with `resolution_candidates_json` set; `canonical_item_id` stays NULL, so
//!     no dedup conflict is possible.
//!   * [`ResolutionOutcome::NoMatch`] → the entry is left `unresolved` (the LLM
//!     fallback lands in P3-T2); nothing is written or broadcast.
//!
//! Error discipline: this runs OUTSIDE the mutation's request/transaction. A DB
//! error here is logged and swallowed — it never affects the already-committed
//! mutation state, and the entry simply stays `unresolved` until a later trigger
//! re-resolves it. The whole task is `tokio::spawn`ed so a panic is isolated too.

use buywise_core::constraint_hash::effective_constraint_hash;
use buywise_core::matching::{self, ResolutionOutcome};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use crate::AppState;

/// The inputs the async resolution task needs, captured from the committed
/// `add_item` mutation. Owned (not borrowed) so it can move into the spawned task.
#[derive(Debug, Clone)]
pub struct ResolutionJob {
    /// The `list_entries.id` to resolve.
    pub entry_id: String,
    /// The list the entry belongs to (scopes every write — IDOR guard).
    pub list_id: String,
    /// The household whose catalogue the cascade matches against.
    pub household_id: String,
    /// The user credited for the `merged` audit event on a dedup conflict.
    pub user_id: String,
    /// The raw term to resolve (already stored on the entry).
    pub raw_text: String,
    /// The entry's constraints payload, if any — fed into the recomputed
    /// `effective_constraint_hash` on a successful resolution.
    pub constraints: Option<Value>,
}

/// Spawn the resolution cascade for one committed `add_item`.
///
/// Returns immediately; the work runs on a detached `tokio::spawn` task so the
/// request handler does not wait for it (the mutation is already accepted). Any
/// error is logged and discarded — resolution is best-effort and must never
/// corrupt the committed mutation.
pub fn spawn_resolution(state: AppState, job: ResolutionJob) {
    tokio::spawn(async move {
        if let Err(e) = resolve_entry(&state, &job).await {
            // A failure leaves the entry `unresolved`; it is not fatal and must not
            // affect the committed mutation. Logged for observability only.
            tracing::error!(
                error = %e,
                entry_id = %job.entry_id,
                list_id = %job.list_id,
                "resolution: cascade failed; entry left unresolved"
            );
        }
    });
}

/// Run the cascade and apply its outcome to the entry.
///
/// Split out from [`spawn_resolution`] so the fallible body is a single
/// `Result`-returning function (clean `?` propagation); the spawn wrapper logs.
async fn resolve_entry(state: &AppState, job: &ResolutionJob) -> Result<(), sqlx::Error> {
    let outcome = matching::resolve(&job.raw_text, &job.household_id, &state.pool)
        .await
        .map_err(into_sqlx_error)?;

    match outcome {
        ResolutionOutcome::SingleMatch { canonical_item_id } => {
            resolve_single_match(state, job, &canonical_item_id).await
        }
        ResolutionOutcome::MultipleCandidates { candidates } => {
            resolve_ambiguous(state, job, &candidates).await
        }
        // NoMatch: leave the entry unresolved (P3-T2 adds the LLM fallback).
        ResolutionOutcome::NoMatch => Ok(()),
    }
}

/// Apply a [`ResolutionOutcome::SingleMatch`]: resolve the entry, or — if a
/// resolved active row for the same `(list, canonical item, hash)` already exists
/// — merge this entry into it.
///
/// The UPDATE that binds `canonical_item_id` is what makes the row a dedup
/// candidate, so the partial unique index fires *during* that UPDATE if a
/// survivor exists. We attempt the UPDATE and catch the constraint violation
/// rather than checking for a survivor up front (the design's required ordering):
/// a concurrent resolution could otherwise slip a survivor in between the check
/// and the write. Everything runs in one transaction so a half-applied merge is
/// impossible.
async fn resolve_single_match(
    state: &AppState,
    job: &ResolutionJob,
    canonical_item_id: &str,
) -> Result<(), sqlx::Error> {
    let hash = effective_constraint_hash(
        &job.entry_id,
        Some(canonical_item_id),
        job.constraints.as_ref(),
    );
    let now = buywise_core::db::now_iso();

    let mut tx = state.pool.begin().await?;

    // Attempt to resolve in place. The entry must still be `active` and
    // `unresolved` — a concurrent delete or an earlier resolution makes this a
    // no-op (0 rows), which we treat as "already handled" and skip.
    let update = sqlx::query(
        "UPDATE list_entries \
         SET resolution_status = 'resolved', canonical_item_id = ?3, \
             effective_constraint_hash = ?4, resolution_candidates_json = NULL, \
             updated_at = ?5 \
         WHERE id = ?1 AND list_id = ?2 \
           AND resolution_status = 'unresolved' AND status = 'active'",
    )
    .bind(&job.entry_id)
    .bind(&job.list_id)
    .bind(canonical_item_id)
    .bind(&hash)
    .bind(&now)
    .execute(&mut *tx)
    .await;

    match update {
        Ok(result) => {
            if result.rows_affected() == 0 {
                // Nothing to resolve (already resolved, deleted, or gone). No-op.
                tx.rollback().await?;
                return Ok(());
            }
            // Resolved cleanly: bump the revision, stamp, broadcast the entry.
            let revision = increment_revision(&mut tx, &job.list_id).await?;
            stamp_entry_revision(&mut tx, &job.entry_id, &job.list_id, revision).await?;
            tx.commit().await?;
            // Broadcast is best-effort and runs AFTER commit: a read/serialise
            // failure here must not make the cascade report failure (the entry is
            // already resolved). Logged inside, never propagated.
            broadcast_entry(state, job, revision).await;
            Ok(())
        }
        // The partial dedup index fired: a resolved active row for the same
        // (list, canonical item, hash) already exists. Merge this entry into it.
        Err(e) if crate::mutations::is_dedup_index_violation(&e) => {
            merge_into_survivor(state, job, canonical_item_id, &hash, &now, tx).await
        }
        Err(e) => Err(e),
    }
}

/// Dedup-on-resolution merge: fold this entry into the surviving active row.
///
/// In one transaction (the same one whose UPDATE just hit the unique index):
///
/// 1. Increment the survivor's `quantity` (matched on the dedup key).
/// 2. Mark this entry `deleted` (`merged` is not a valid `status` per the schema
///    CHECK; the audit trail records the merge).
/// 3. Insert a `list_entry_events` row on this entry with `event_type='merged'`
///    and `payload={"merged_into":"<survivor_id>"}`.
///
/// Then bump the list revision once, stamp both rows with it, commit, and
/// broadcast two SSE events (the survivor's new quantity and this entry's
/// deletion) so every connected client converges.
async fn merge_into_survivor(
    state: &AppState,
    job: &ResolutionJob,
    canonical_item_id: &str,
    hash: &str,
    now: &str,
    mut tx: Transaction<'_, Sqlite>,
) -> Result<(), sqlx::Error> {
    // 1. Increment the survivor's quantity by this entry's quantity and return
    //    its id. The survivor is the existing active row on the dedup key.
    let survivor: Option<String> = sqlx::query_scalar(
        "UPDATE list_entries \
         SET quantity = quantity + ( \
                 SELECT quantity FROM list_entries WHERE id = ?4 AND list_id = ?1 \
             ), \
             updated_at = ?5 \
         WHERE list_id = ?1 AND canonical_item_id = ?2 \
           AND effective_constraint_hash = ?3 AND status = 'active' \
         RETURNING id",
    )
    .bind(&job.list_id)
    .bind(canonical_item_id)
    .bind(hash)
    .bind(&job.entry_id)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;

    // The unique-index violation proves a colliding active row existed, so the
    // UPDATE must have matched it. A None here is an invariant break.
    let Some(survivor_id) = survivor else {
        tx.rollback().await?;
        tracing::error!(
            entry_id = %job.entry_id,
            list_id = %job.list_id,
            "resolution: dedup conflict but no survivor row to merge into"
        );
        return Ok(());
    };

    // 2. Mark this entry deleted (it loses to the survivor).
    sqlx::query(
        "UPDATE list_entries SET status = 'deleted', updated_at = ?3 \
         WHERE id = ?1 AND list_id = ?2",
    )
    .bind(&job.entry_id)
    .bind(&job.list_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    // 3. Record the merge for the audit trail, pointing at the survivor.
    let event_id = uuid::Uuid::new_v4().to_string();
    let payload = serde_json::json!({ "merged_into": survivor_id }).to_string();
    sqlx::query(
        "INSERT INTO list_entry_events (id, entry_id, user_id, event_type, payload, created_at) \
         VALUES (?1, ?2, ?3, 'merged', ?4, ?5)",
    )
    .bind(&event_id)
    .bind(&job.entry_id)
    .bind(&job.user_id)
    .bind(&payload)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    // Bump the revision once for the whole merge and stamp both touched rows so
    // the broadcast entities carry the revision they were last changed at.
    let revision = increment_revision(&mut tx, &job.list_id).await?;
    stamp_entry_revision(&mut tx, &job.entry_id, &job.list_id, revision).await?;
    stamp_entry_revision(&mut tx, &survivor_id, &job.list_id, revision).await?;

    tx.commit().await?;

    // Broadcast both outcomes: the survivor's updated quantity and this entry's
    // deletion. Read-after-commit so each event reflects the committed row. Both
    // are best-effort (post-commit): a failure here is logged, never propagated.
    broadcast_entry_id(state, &job.list_id, &survivor_id, revision).await;
    broadcast_entry_id(state, &job.list_id, &job.entry_id, revision).await;
    Ok(())
}

/// Apply a [`ResolutionOutcome::MultipleCandidates`]: mark the entry `ambiguous`
/// and record the candidate ids as a JSON array. `canonical_item_id` stays NULL,
/// so no dedup conflict is possible and the entry is never a merge candidate.
async fn resolve_ambiguous(
    state: &AppState,
    job: &ResolutionJob,
    candidates: &[String],
) -> Result<(), sqlx::Error> {
    let candidates_json = serde_json::json!(candidates).to_string();
    let now = buywise_core::db::now_iso();

    let mut tx = state.pool.begin().await?;

    let affected = sqlx::query(
        "UPDATE list_entries \
         SET resolution_status = 'ambiguous', resolution_candidates_json = ?3, updated_at = ?4 \
         WHERE id = ?1 AND list_id = ?2 \
           AND resolution_status = 'unresolved' AND status = 'active'",
    )
    .bind(&job.entry_id)
    .bind(&job.list_id)
    .bind(&candidates_json)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if affected == 0 {
        // Already resolved/deleted by something else: nothing to do.
        tx.rollback().await?;
        return Ok(());
    }

    let revision = increment_revision(&mut tx, &job.list_id).await?;
    stamp_entry_revision(&mut tx, &job.entry_id, &job.list_id, revision).await?;
    tx.commit().await?;

    // Best-effort post-commit broadcast (see [`broadcast_entry_id`]).
    broadcast_entry(state, job, revision).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared DB + broadcast helpers
// ---------------------------------------------------------------------------

/// Atomically bump a list's `current_revision` by one and return the new value,
/// so a resolution-driven change is ordered in the same revision space as a
/// client mutation (and is therefore replayable over SSE backfill).
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

/// Stamp an entry's `last_updated_revision`, scoped to `(id, list_id)`.
async fn stamp_entry_revision(
    tx: &mut Transaction<'_, Sqlite>,
    entry_id: &str,
    list_id: &str,
    revision: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE list_entries SET last_updated_revision = ?2 WHERE id = ?1 AND list_id = ?3",
    )
    .bind(entry_id)
    .bind(revision)
    .bind(list_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Read the job's entry back (post-commit) and broadcast it over SSE.
async fn broadcast_entry(state: &AppState, job: &ResolutionJob, revision: i64) {
    broadcast_entry_id(state, &job.list_id, &job.entry_id, revision).await
}

/// Read `entry_id` back from the pool and broadcast it as a `mutation` SSE event
/// carrying `revision`, matching the wire shape `POST /api/mutations` emits.
///
/// This always runs AFTER the resolution transaction has committed, so the entry
/// is already in its final state. The broadcast is therefore best-effort: a
/// vanished entry (concurrent delete), a transient read failure, or a
/// serialisation failure is logged and swallowed — it must never make the cascade
/// report failure, because a disconnected client backfills via `?since=` on
/// reconnect and a dropped live event is never lost.
async fn broadcast_entry_id(state: &AppState, list_id: &str, entry_id: &str, revision: i64) {
    let view = match crate::mutations::load_entry_view(&state.pool, entry_id, list_id).await {
        Ok(Some(view)) => view,
        // Entry vanished (concurrent delete): nothing to broadcast.
        Ok(None) => return,
        Err(e) => {
            tracing::error!(error = %e, entry_id, "resolution: failed to read entry for broadcast (best-effort, skipped)");
            return;
        }
    };
    let entity = match serde_json::to_value(&view) {
        Ok(entity) => entity,
        Err(e) => {
            tracing::error!(error = %e, entry_id, "resolution: failed to serialise broadcast entity (best-effort, skipped)");
            return;
        }
    };
    crate::sse::broadcast_entity(state, list_id, revision, entity);
}

/// Map a [`buywise_core::db::DbError`] back to its underlying [`sqlx::Error`] so
/// the resolution task has a single error type to propagate and log. The cascade
/// only ever produces `DbError::Sqlx` (it runs plain queries), so other variants
/// are wrapped as a protocol error rather than silently lost.
fn into_sqlx_error(e: buywise_core::db::DbError) -> sqlx::Error {
    match e {
        buywise_core::db::DbError::Sqlx(inner) => inner,
        other => sqlx::Error::Protocol(other.to_string()),
    }
}
