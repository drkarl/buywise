//! List-management routes for the BuyWise API (P1-T3).
//!
//! Five endpoints, all behind the [`AuthenticatedUser`] extractor, mounted by
//! [`router`] (see `crate::app` for where the prefixes are nested):
//!
//!   * `GET   /api/lists`     — every ACTIVE list in the user's household(s).
//!   * `POST  /api/lists`     — create a new list (CSRF-protected; it is a POST
//!     behind [`AuthenticatedUser`]).
//!   * `GET   /api/list/:id`  — full list state: list metadata, a top-level
//!     `current_revision`, and ALL `list_entries` (every status — active,
//!     bought, skipped, and deleted tombstones — so the payload doubles as a
//!     faithful sync snapshot; see [`list_snapshot`] and issue #62).
//!   * `PATCH /api/list/:id`  — rename or archive a list (CSRF-protected).
//!   * `GET   /api/items`     — the full canonical catalogue (items + aliases +
//!     constraints) plus the `categories` catalogue and the household's
//!     `store_category_orders`, for client-side resolution / category assignment
//!     and offline store-ordering (P2-T6). Returns an [`ItemsResponse`] envelope.
//!
//! Authorization model (see also `migrations/003_phase1_schema.sql`):
//!   * A user belongs to household(s) via `household_members`. Collection
//!     endpoints (`GET /api/lists`, `POST /api/lists`, `GET /api/items`) are
//!     scoped to the user's household(s).
//!   * For a single list (`GET`/`PATCH /api/list/:id`) the row is looked up
//!     first: a missing list is **404**, an existing list the user is not a
//!     member of the owning household for is **403** (an authorization failure,
//!     deliberately distinguished from "not found"). The same scope applies to
//!     the entries returned with the list.
//!
//! Error discipline mirrors `auth.rs`: a database error is logged via
//! `tracing::error!` and surfaced as 500 (never failing open); 401/403/404/422
//! are returned for the corresponding client conditions.
//!
//! Revision note: `current_revision` is owned by the offline-sync mutation log
//! (P1-T4) and is **not** touched here — `PATCH` only renames/archives.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::db::{self, DbError};
use buywise_core::domain::Cents;
use buywise_core::optimiser::prices_same_lines;

use crate::middleware::AuthenticatedUser;
use crate::savings::{STATUS_ESTIMATED, STATUS_UNRECONCILED};
use crate::AppState;

/// Build the list-management routes as a `Router<AppState>`.
///
/// `GET /api/lists` and `GET /api/list/:id` are different path shapes, so the
/// `/api/lists`, `/api/list/:id`, and `/api/items` groups are registered
/// individually and merged by the caller (`crate::app`).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/lists", get(list_lists).post(create_list))
        .route("/api/list/{id}", get(get_list).patch(patch_list))
        .route("/api/items", get(list_items))
}

// ---------------------------------------------------------------------------
// Response / request shapes
// ---------------------------------------------------------------------------

/// One list as returned in the `GET /api/lists` collection.
#[derive(Debug, Serialize)]
pub struct ListSummary {
    /// `shopping_lists.id`.
    pub id: String,
    /// `shopping_lists.household_id`.
    pub household_id: String,
    /// `shopping_lists.name`.
    pub name: String,
    /// `shopping_lists.status` (`active` here — the collection is active-only).
    pub status: String,
    /// `shopping_lists.current_revision`.
    pub current_revision: i64,
    /// `shopping_lists.created_at` (ISO-8601 UTC).
    pub created_at: String,
}

/// A single list entry as returned inside [`ListDetail`].
///
/// Mirrors `mutations::EntityView` field-for-field (same JSON shape) so a client
/// decodes a snapshot entry and a `POST /api/mutations` / SSE `mutation` entity
/// identically — including `list_id`, `bought_by`, and `bought_at`. The snapshot
/// is the COMPLETE list state (every `status`: active, bought, skipped, and
/// deleted tombstones), so these fields are all required for a client to
/// reconcile basket history from a snapshot alone (issue #62).
#[derive(Debug, Serialize)]
pub struct EntryView {
    /// `list_entries.id`.
    pub id: String,
    /// `list_entries.list_id`. Present so a client need not stamp it from the
    /// requested id (mirrors `EntityView`).
    pub list_id: String,
    /// `list_entries.canonical_item_id` (NULL until resolved).
    pub canonical_item_id: Option<String>,
    /// `list_entries.resolution_status` (`unresolved`|`resolved`|`ambiguous`).
    pub resolution_status: String,
    /// `list_entries.resolution_candidates_json` (raw JSON; ambiguous only).
    pub resolution_candidates_json: Option<String>,
    /// `list_entries.raw_text`.
    pub raw_text: String,
    /// `list_entries.display_name`.
    pub display_name: String,
    /// `list_entries.added_by`.
    pub added_by: String,
    /// `list_entries.quantity`.
    pub quantity: i64,
    /// `list_entries.status` (`active`|`bought`|`skipped`|`deleted`). See
    /// [`ListDetail::entries`] for why every status is carried.
    pub status: String,
    /// `list_entries.bought_by` (set only for `bought` entries).
    pub bought_by: Option<String>,
    /// `list_entries.bought_at` (set only for `bought` entries).
    pub bought_at: Option<String>,
    /// `list_entries.effective_constraint_hash`.
    pub effective_constraint_hash: String,
    /// `list_entries.last_updated_revision`.
    pub last_updated_revision: i64,
    /// `list_entries.created_at` (ISO-8601 UTC).
    pub created_at: String,
    /// `list_entries.updated_at` (ISO-8601 UTC).
    pub updated_at: String,
    /// Whether the entry is resolved but has no usable product match, so no price
    /// can be produced for it (issue #108). Surfaces missing matching coverage on
    /// the list itself, so an unmatched staple prompts for a match instead of
    /// silently having no price. Always `false` for unresolved/ambiguous entries
    /// (matching cannot apply) and for non-active entries; the client still reads
    /// it only for active ones.
    pub needs_match: bool,
}

/// Full state of a single list: metadata, a top-level `current_revision`, and
/// every entry in the list (all statuses — see [`ListDetail::entries`]).
#[derive(Debug, Serialize)]
pub struct ListDetail {
    /// `shopping_lists.id`.
    pub id: String,
    /// `shopping_lists.household_id`.
    pub household_id: String,
    /// `shopping_lists.name`.
    pub name: String,
    /// `shopping_lists.status`.
    pub status: String,
    /// `shopping_lists.current_revision`. Top-level per the contract so a client
    /// can read it without descending into the entries.
    pub current_revision: i64,
    /// `shopping_lists.created_at` (ISO-8601 UTC).
    pub created_at: String,
    /// `shopping_lists.archived_at` (set only for archived lists).
    pub archived_at: Option<String>,
    /// The COMPLETE set of list entries — every `status` (`active`, `bought`,
    /// `skipped`, and `deleted` tombstones), not active-only. A client uses this
    /// as a faithful full replacement: it can reconcile basket history
    /// (bought/skipped) and tombstones from the snapshot alone, which an
    /// active-only payload could not (issue #62).
    pub entries: Vec<EntryView>,
}

/// Body for `POST /api/lists`.
///
/// `household_id` is required and explicitly validated against the caller's
/// membership (a user may belong to several households, so the target cannot be
/// safely inferred). A non-member household yields 403.
#[derive(Debug, Deserialize)]
pub struct CreateListRequest {
    /// Name for the new list (must be non-empty after trimming).
    pub name: String,
    /// Household the list belongs to; must be one the caller is a member of.
    pub household_id: String,
}

/// Body for `PATCH /api/list/:id`. Both fields are optional; at least one must
/// be present. `status`, when given, must be `active` or `archived`.
#[derive(Debug, Deserialize)]
pub struct PatchListRequest {
    /// New name (non-empty after trimming) when present.
    pub name: Option<String>,
    /// New status (`active` | `archived`) when present.
    pub status: Option<String>,
}

/// One canonical item with its aliases and (optional) constraints, returned by
/// `GET /api/items`.
#[derive(Debug, Serialize)]
pub struct ItemView {
    /// `canonical_items.id`.
    pub id: String,
    /// `canonical_items.household_id`.
    pub household_id: String,
    /// `canonical_items.name_en`.
    pub name_en: String,
    /// `canonical_items.name_es`.
    pub name_es: String,
    /// `canonical_items.category_id`.
    pub category_id: String,
    /// `canonical_items.subcategory_id`, or `None` when the item has no
    /// sub-category and shopping mode should fall back to `category_id`.
    pub subcategory_id: Option<String>,
    /// `canonical_items.default_constraint_policy`.
    pub default_constraint_policy: String,
    /// All aliases for this item.
    pub aliases: Vec<AliasView>,
    /// The single constraints row for this item, if any.
    pub constraints: Option<ConstraintsView>,
}

/// One alias for a canonical item.
#[derive(Debug, Serialize)]
pub struct AliasView {
    /// `item_aliases.id`.
    pub id: String,
    /// `item_aliases.alias`.
    pub alias: String,
    /// `item_aliases.language` (`en` | `es`).
    pub language: String,
    /// `item_aliases.source` (`manual` | `accepted_match` | `llm`).
    pub source: String,
}

/// The (at most one) constraints row for a canonical item.
#[derive(Debug, Serialize)]
pub struct ConstraintsView {
    /// `item_constraints.brand`.
    pub brand: Option<String>,
    /// `item_constraints.allowed_brands` (raw stored value).
    pub allowed_brands: Option<String>,
    /// `item_constraints.excluded_brands` (raw stored value).
    pub excluded_brands: Option<String>,
    /// `item_constraints.preferred_size_value`.
    pub preferred_size_value: Option<f64>,
    /// `item_constraints.preferred_size_basis` (`g` | `ml` | `each`).
    pub preferred_size_basis: Option<String>,
    /// `item_constraints.min_size_value`.
    pub min_size_value: Option<f64>,
    /// `item_constraints.min_size_basis`.
    pub min_size_basis: Option<String>,
    /// `item_constraints.max_size_value`.
    pub max_size_value: Option<f64>,
    /// `item_constraints.max_size_basis`.
    pub max_size_basis: Option<String>,
    /// `item_constraints.pack_count`.
    pub pack_count: Option<i64>,
    /// `item_constraints.form_factor`.
    pub form_factor: Option<String>,
    /// `item_constraints.dietary_flags` (raw stored value).
    pub dietary_flags: Option<String>,
    /// `item_constraints.substitution_policy` (`any` | `ask` | `strict`).
    pub substitution_policy: String,
}

/// The full `GET /api/items` payload: the canonical catalogue plus the
/// reference tables shopping mode needs to group + order items by store entirely
/// offline (P2-T6, extended for sub-categories by issue #101). Canonical shape
/// per `docs/implementation-plan.md` ("Store category orders: fetched from
/// `GET /api/items` response").
///
/// Contract change (P2-T6): the endpoint previously returned a bare
/// `Vec<ItemView>`; it now returns this envelope so the client can cache the
/// `categories` catalogue and the per-store aisle orders alongside the items.
///
/// Contract change (issue #101): the envelope also carries the shared
/// `subcategories` catalogue; each item carries its optional `subcategory_id`.
#[derive(Debug, Serialize)]
pub struct ItemsResponse {
    /// The canonical items with their aliases and (optional) constraints.
    pub items: Vec<ItemView>,
    /// The shared `categories` catalogue (id, names, icon, default order).
    pub categories: Vec<CategoryView>,
    /// The shared `subcategories` catalogue (id, parent, names, default order).
    pub subcategories: Vec<SubcategoryView>,
    /// The caller's household per-store aisle orders (`store_category_orders`).
    pub store_category_orders: Vec<StoreCategoryOrderView>,
}

/// One `categories` row, mirroring `crates/core/src/domain.rs::Category`.
#[derive(Debug, Serialize)]
pub struct CategoryView {
    /// `categories.id`.
    pub id: String,
    /// `categories.name_en`.
    pub name_en: String,
    /// `categories.name_es`.
    pub name_es: String,
    /// `categories.icon`.
    pub icon: String,
    /// `categories.default_order`.
    pub default_order: i64,
}

/// One `subcategories` row, mirroring `crates/core/src/domain.rs::Subcategory`
/// (issue #101). Shared reference data like [`CategoryView`].
#[derive(Debug, Serialize)]
pub struct SubcategoryView {
    /// `subcategories.id`.
    pub id: String,
    /// `subcategories.category_id` — the parent category.
    pub category_id: String,
    /// `subcategories.name_en`.
    pub name_en: String,
    /// `subcategories.name_es`.
    pub name_es: String,
    /// `subcategories.default_order` (orders the split within its parent).
    pub default_order: i64,
}

/// One `store_category_orders` row (household-scoped to the caller). `store` is a
/// Retailer DB string (`tesco`|`dunnes`|`supervalu`|`aldi`|`lidl`); see
/// `crates/core/src/domain.rs::Retailer::as_db_str`.
#[derive(Debug, Serialize)]
pub struct StoreCategoryOrderView {
    /// `store_category_orders.id`.
    pub id: String,
    /// `store_category_orders.store`.
    pub store: String,
    /// `store_category_orders.category_id`.
    pub category_id: String,
    /// `store_category_orders.display_order`.
    pub display_order: i64,
}

// ---------------------------------------------------------------------------
// Authorization helpers
// ---------------------------------------------------------------------------

/// True if `user_id` is a member of `household_id` (via `household_members`).
///
/// A DB error is propagated so callers fail closed (500) rather than treating a
/// transient failure as "not a member".
///
/// `pub(crate)` so sibling modules (e.g. [`crate::savings`]) reuse the exact same
/// membership check rather than re-implementing it.
pub(crate) async fn is_member(
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

/// Map a DB error to a logged 500. Used by every handler so a transient failure
/// never fails open.
///
/// Generic over the error type so it covers both the raw `sqlx::Error` returned
/// by the list queries and the [`DbError`] surfaced by [`record_savings_event`].
fn db_error<E: std::fmt::Display>(context: &'static str) -> impl Fn(E) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "lists: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// savings_events recording on archive (P5-T3)
// ---------------------------------------------------------------------------

/// The subset of `recommendation_runs.plans_json` (written by
/// `optimise::persist_run`) that the savings ledger needs. Only `total_cents` is
/// read off each plan, so the other plan fields are deliberately ignored via
/// `#[serde(default)]` + a minimal [`PlanTotal`] shape — this stays decoupled
/// from the full optimiser `StorePlan`/`Recommendation` types and tolerant of
/// extra keys, so a forward-compatible plan schema change does not break reads.
#[derive(Debug, Deserialize)]
struct PlansJsonView {
    #[serde(default)]
    best_single: Option<PlanView>,
    #[serde(default)]
    theoretical_minimum: Option<PlanTotal>,
    #[serde(default)]
    recommended: Option<RecommendedView>,
}

/// A plan reduced to its `total_cents` and priced lines (the only figures the
/// ledger needs).
#[derive(Debug, Deserialize)]
struct PlanView {
    total_cents: Cents,
    /// The plan's priced lines; only `item_index` is read, so the other line
    /// fields are ignored (`#[serde(default)]` tolerates a plan stored without
    /// lines, which then compares as an empty line set).
    #[serde(default)]
    lines: Vec<PlanLineView>,
}

/// A plan reduced to its `total_cents` (the only figure the ledger needs).
#[derive(Debug, Deserialize)]
struct PlanTotal {
    total_cents: Cents,
}

/// One plan line reduced to the basket item it prices.
#[derive(Debug, Deserialize)]
struct PlanLineView {
    item_index: usize,
}

/// The recommended plan as stored: a `kind` plus a nested `plan` carrying the
/// total and lines. Mirrors `optimiser::Recommendation`'s serialised shape.
#[derive(Debug, Deserialize)]
struct RecommendedView {
    plan: PlanView,
}

/// Record a `savings_events` row for a just-archived list, IF a usable
/// recommendation run exists. Returns `Ok(())` in every non-error case.
///
/// Design decisions (issue #34 / P5-T3):
///   * **Which run?** The LATEST run for the list (`ORDER BY run_at DESC LIMIT 1`).
///   * **No run / no usable plan?** Silent no-op (not an error). The four NOT-NULL
///     cost columns (`baseline`, `recommended`, `theoretical_minimum`,
///     `gross_saving`) require a run whose `plans_json` carries `best_single`,
///     `recommended`, and `theoretical_minimum`. A run with any of these missing
///     (e.g. nothing priceable, `recommended_plan = 'none'`) cannot produce a
///     complete row, so we skip rather than insert a zeroed/partial one.
///   * **Idempotency / re-archive.** A list can be archived → reactivated →
///     archived again, possibly concurrently. The INSERT uses
///     `ON CONFLICT(list_id) DO NOTHING` against the UNIQUE index on
///     `savings_events(list_id)` (migration 006), so re-archiving never
///     duplicates the row and never errors even under a concurrent race. A cheap
///     pre-check short-circuits the common re-archive path before the run lookup.
///
/// Cost mapping (docs/design.md §savings): `baseline_cost_cents` = best single
/// store total (the comparison anchor), `recommended_cost_cents` = recommended
/// plan total, `theoretical_minimum_cost_cents` = theoretical minimum total,
/// `gross_saving_cents` = baseline − recommended. `chosen_basket_cost_cents` and
/// `net_saving_cents` stay NULL until the user posts actual spend (the P5-T3
/// `/actual-spend` endpoint). `chosen_plan`/`chosen_stores` describe what the
/// household ACTUALLY shopped (post-shop, paired with `chosen_basket_cost_cents`
/// per docs/design.md §savings); archive does not know that yet, so they stay
/// NULL here. The recommendation linkage is preserved via `recommendation_run_id`.
///
/// **Coverage guard (issue #117).** A gross figure is only a like-for-like
/// saving when the best single store and the recommended plan price the same
/// lines. When they do not, the row is still created — `baseline_cost_cents`
/// and `recommended_cost_cents` are known plan totals, and every cost column is
/// NOT NULL, so we keep the computed gross for audit rather than writing a zero
/// for an unknown saving — but its `status` is `unreconciled`, so the summary
/// never presents or aggregates it as a saving. A comparable row is `estimated`
/// (an opportunity estimate, never `verified`; see `crate::savings`).
async fn record_savings_event(pool: &sqlx::SqlitePool, list_id: &str) -> Result<(), DbError> {
    // Idempotency guard: never create a second row for the same list.
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM savings_events WHERE list_id = ?1 LIMIT 1")
            .bind(list_id)
            .fetch_optional(pool)
            .await?;
    if existing.is_some() {
        return Ok(());
    }

    // Latest run for the list, if any. "No run" => silent no-op.
    let run: Option<(String, String)> = sqlx::query_as(
        "SELECT id, plans_json FROM recommendation_runs \
         WHERE list_id = ?1 ORDER BY run_at DESC LIMIT 1",
    )
    .bind(list_id)
    .fetch_optional(pool)
    .await?;
    let Some((run_id, plans_json)) = run else {
        return Ok(());
    };

    // A malformed plans_json is a genuine error (our own writer produced it), so
    // surface it as a 500 via DbError rather than silently skipping.
    let plans: PlansJsonView = serde_json::from_str(&plans_json).map_err(|e| {
        DbError::from(sqlx::Error::Protocol(format!(
            "deserialise plans_json for run {run_id}: {e}"
        )))
    })?;

    // All three NOT-NULL cost figures must be present; otherwise the run is not
    // usable for a complete savings row — skip silently.
    let (Some(best_single), Some(theoretical_minimum), Some(recommended)) = (
        plans.best_single,
        plans.theoretical_minimum,
        plans.recommended,
    ) else {
        return Ok(());
    };

    let baseline_cost_cents = best_single.total_cents;
    let recommended_cost_cents = recommended.plan.total_cents;
    let theoretical_minimum_cost_cents = theoretical_minimum.total_cents;
    let gross_saving_cents = baseline_cost_cents - recommended_cost_cents;

    // Coverage guard (issue #117): the gross figure is only a saving when both
    // plans price the same lines. This is the same comparison the optimise
    // explanation uses to refuse a cross-plan cost comparison
    // (`optimise::recommendation_comparison`), so the two paths cannot drift.
    let comparable = prices_same_lines(
        best_single.lines.iter().map(|line| &line.item_index),
        recommended.plan.lines.iter().map(|line| &line.item_index),
    );
    let status = if comparable {
        STATUS_ESTIMATED
    } else {
        STATUS_UNRECONCILED
    };

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = db::now_iso();

    // `ON CONFLICT(list_id) DO NOTHING` is the structural idempotency guard: the
    // UNIQUE index on savings_events(list_id) (migration 006) makes a concurrent
    // double-archive race a no-op rather than a duplicate row. The cheap pre-check
    // above stays as an early-out so the common re-archive path skips the run
    // lookup + JSON parse entirely.
    sqlx::query(
        "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, chosen_basket_cost_cents, chosen_plan, \
              chosen_stores, gross_saving_cents, accepted_savings_cents, rejected_savings_cents, \
              travel_cost_cents, subscription_cost_alloc_cents, net_saving_cents, created_at, \
              status) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, ?7, NULL, NULL, NULL, NULL, NULL, ?8, ?9) \
         ON CONFLICT(list_id) DO NOTHING",
    )
    .bind(&id)
    .bind(list_id)
    .bind(&run_id)
    .bind(baseline_cost_cents)
    .bind(recommended_cost_cents)
    .bind(theoretical_minimum_cost_cents)
    .bind(gross_saving_cents)
    .bind(&created_at)
    .bind(status)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/lists` — all ACTIVE lists across the caller's household(s).
async fn list_lists(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<ListSummary>>, StatusCode> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64, String)>(
        "SELECT id, household_id, name, status, current_revision, created_at \
         FROM shopping_lists \
         WHERE status = 'active' \
           AND household_id IN \
               (SELECT household_id FROM household_members WHERE user_id = ?1) \
         ORDER BY created_at, id",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("list_lists"))?;

    let lists = rows
        .into_iter()
        .map(
            |(id, household_id, name, status, current_revision, created_at)| ListSummary {
                id,
                household_id,
                name,
                status,
                current_revision,
                created_at,
            },
        )
        .collect();
    Ok(Json(lists))
}

/// `POST /api/lists` — create a new ACTIVE list (revision 0) in a household the
/// caller is a member of.
///
/// 422 for an empty name; 403 if the caller is not a member of the requested
/// household. CSRF is enforced by the extractor (this is a POST).
async fn create_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateListRequest>,
) -> Result<(StatusCode, Json<ListDetail>), StatusCode> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // A list may only be created in a household the caller belongs to.
    if !is_member(&state.pool, &user.user_id, &body.household_id)
        .await
        .map_err(db_error("create_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = buywise_core::db::now_iso();

    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at, archived_at) \
         VALUES (?1, ?2, ?3, 'active', 0, ?4, NULL)",
    )
    .bind(&id)
    .bind(&body.household_id)
    .bind(name)
    .bind(&created_at)
    .execute(&state.pool)
    .await
    .map_err(db_error("create_list.insert"))?;

    let detail = ListDetail {
        id,
        household_id: body.household_id,
        name: name.to_string(),
        status: "active".to_string(),
        current_revision: 0,
        created_at,
        archived_at: None,
        entries: Vec::new(),
    };
    Ok((StatusCode::CREATED, Json(detail)))
}

/// A `shopping_lists` row as needed for authorization + detail rendering.
type ListRow = (String, String, String, String, i64, String, Option<String>);

/// Load a list by id, or `None` if no such row exists.
async fn load_list(pool: &sqlx::SqlitePool, id: &str) -> Result<Option<ListRow>, sqlx::Error> {
    sqlx::query_as::<_, ListRow>(
        "SELECT id, household_id, name, status, current_revision, created_at, archived_at \
         FROM shopping_lists WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Load the full [`ListDetail`] for a list, or `None` if no such list exists.
///
/// Public seam for the SSE stream (P1-T5): when a reconnecting client is below
/// the mutation-log retention floor, the stream emits this complete list state as
/// a `full_snapshot` event instead of an incremental backfill. Reuses
/// [`load_list`] + [`load_detail`] so the snapshot is byte-identical in shape to
/// `GET /api/list/:id`. The caller is responsible for the household-membership
/// authorization check (the SSE handler performs it before calling this).
///
/// As of issue #62 this carries the COMPLETE list state — every entry status,
/// not active-only — so a client can use a snapshot as a faithful full
/// replacement and reconcile bought/skipped/deleted rows it would otherwise have
/// to preserve from local state. The revision semantics are unchanged: the
/// snapshot is keyed to `current_revision`, which the SSE handler uses as the
/// high-water mark so the client resumes from the right `?since=`.
pub async fn list_snapshot(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<ListDetail>, sqlx::Error> {
    match load_list(pool, list_id).await? {
        Some(row) => Ok(Some(load_detail(pool, row).await?)),
        None => Ok(None),
    }
}

/// Render a [`ListDetail`] from an already-loaded `shopping_lists` row plus a
/// single query for ALL its entries (every `status`, not active-only — issue
/// #62), so the result is a faithful full snapshot a client can reconcile
/// against.
///
/// Shared by [`get_list`] and [`patch_list`] so neither re-runs [`load_list`]
/// nor a redundant [`is_member`] check after the row is already in hand.
async fn load_detail(pool: &sqlx::SqlitePool, row: ListRow) -> Result<ListDetail, sqlx::Error> {
    let (id, household_id, name, status, current_revision, created_at, archived_at) = row;

    let entry_rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            String,
            i64,
            String,
            String,
        ),
    >(
        // No `WHERE status = 'active'` filter — every entry status is selected
        // (see this fn's doc comment). `list_id`, `bought_by`, `bought_at` are
        // included to mirror `mutations::EntityView` field-for-field.
        "SELECT id, list_id, canonical_item_id, resolution_status, resolution_candidates_json, \
                raw_text, display_name, added_by, quantity, status, bought_by, bought_at, \
                effective_constraint_hash, last_updated_revision, created_at, updated_at \
         FROM list_entries \
         WHERE list_id = ?1 \
         ORDER BY created_at, id",
    )
    .bind(&id)
    .fetch_all(pool)
    .await?;

    // Canonical items in this household that already have a usable product match.
    // Reading it once means `needs_match` costs one extra query per snapshot, not
    // one per entry (issue #108).
    let matched_canonical: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT canonical_item_id FROM product_matches \
         WHERE household_id = ?1 AND status IN ('accepted', 'suggested')",
    )
    .bind(&household_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    let entries = entry_rows
        .into_iter()
        .map(|r| EntryView {
            needs_match: r.9 == "active"
                && r.3 == "resolved"
                && r.2
                    .as_deref()
                    .is_some_and(|c| !matched_canonical.contains(c)),
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
        })
        .collect();

    Ok(ListDetail {
        id,
        household_id,
        name,
        status,
        current_revision,
        created_at,
        archived_at,
        entries,
    })
}

/// `GET /api/list/:id` — full list state with a top-level `current_revision`
/// and ALL entries (every status, not active-only — issue #62). The read UI
/// buckets entries by `status` client-side (active vs the "In basket"
/// bought/skipped section), and the offline-sync client uses the same payload as
/// a faithful full snapshot, so the endpoint carries the complete set rather than
/// filtering server-side.
///
/// 404 if the list does not exist; **403** if it exists but belongs to a
/// household the caller is not a member of (deliberately distinct from 404).
async fn get_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<ListDetail>, StatusCode> {
    let row = load_list(&state.pool, &id)
        .await
        .map_err(db_error("get_list.load"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Existing-but-forbidden is 403, NOT 404.
    if !is_member(&state.pool, &user.user_id, &row.1)
        .await
        .map_err(db_error("get_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let detail = load_detail(&state.pool, row)
        .await
        .map_err(db_error("get_list.entries"))?;
    Ok(Json(detail))
}

/// `PATCH /api/list/:id` — rename and/or archive a list.
///
/// 404 if missing; **403** if not a member of the owning household; 422 if the
/// body changes nothing, has an empty name, or carries an unknown status. When
/// archiving, `archived_at` is set; reactivating clears it. `current_revision`
/// is intentionally left untouched (owned by the mutation log, P1-T4). CSRF is
/// enforced by the extractor (this is a PATCH).
///
/// Ordering note: body validation runs **before** the list lookup, so a
/// malformed body on a missing/forbidden list returns 422 ahead of 404/403.
/// This is intentional — input is validated at the boundary first.
async fn patch_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<PatchListRequest>,
) -> Result<Json<ListDetail>, StatusCode> {
    // Validate the body before any state lookup so a malformed request is a
    // clean 422 regardless of authorization.
    if body.name.is_none() && body.status.is_none() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if let Some(name) = &body.name {
        if name.trim().is_empty() {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }
    if let Some(status) = &body.status {
        if status != "active" && status != "archived" {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    let row = load_list(&state.pool, &id)
        .await
        .map_err(db_error("patch_list.load"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    let household_id = row.1.clone();

    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("patch_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Compute the effective new values, starting from the current row.
    let new_name = body.name.as_ref().map(|n| n.trim().to_string());
    let new_status = body.status.clone();

    // archived_at follows status when status changes: set on archive, cleared on
    // (re)activate. If status is unchanged, archived_at is left as-is.
    match new_status.as_deref() {
        Some("archived") => {
            let now = buywise_core::db::now_iso();
            sqlx::query(
                "UPDATE shopping_lists \
                 SET name = COALESCE(?2, name), status = 'archived', archived_at = ?3 \
                 WHERE id = ?1",
            )
            .bind(&id)
            .bind(&new_name)
            .bind(&now)
            .execute(&state.pool)
            .await
            .map_err(db_error("patch_list.archive"))?;

            // P5-T3: record a savings_events row for this archive, IF the list has
            // a usable recommendation run. "No run" / "no usable plan" / "row
            // already exists" are silent no-ops (not errors); only a genuine DB
            // failure is surfaced as 500. Done after the status flip commits so a
            // savings bookkeeping failure never loses the archive's primary effect.
            record_savings_event(&state.pool, &id)
                .await
                .map_err(db_error("patch_list.savings_event"))?;
        }
        Some("active") => {
            sqlx::query(
                "UPDATE shopping_lists \
                 SET name = COALESCE(?2, name), status = 'active', archived_at = NULL \
                 WHERE id = ?1",
            )
            .bind(&id)
            .bind(&new_name)
            .execute(&state.pool)
            .await
            .map_err(db_error("patch_list.activate"))?;
        }
        _ => {
            // Name-only change.
            sqlx::query("UPDATE shopping_lists SET name = COALESCE(?2, name) WHERE id = ?1")
                .bind(&id)
                .bind(&new_name)
                .execute(&state.pool)
                .await
                .map_err(db_error("patch_list.rename"))?;
        }
    }

    // Re-read so the response reflects the persisted state (including entries).
    // Membership was already verified above, so render directly from the
    // updated row rather than re-running get_list (which would repeat the
    // load_list + is_member queries).
    let updated = load_list(&state.pool, &id)
        .await
        .map_err(db_error("patch_list.reload"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    let detail = load_detail(&state.pool, updated)
        .await
        .map_err(db_error("patch_list.entries"))?;
    Ok(Json(detail))
}

/// `GET /api/items` — the full canonical catalogue for the caller's
/// household(s): each item with its aliases and (optional) constraints, plus the
/// shared `categories` catalogue and the household's per-store aisle orders
/// (`store_category_orders`). The two reference tables let shopping mode (P2-T6)
/// resolve `canonical_item_id -> category_id` and order categories per store
/// fully offline. See [`ItemsResponse`].
async fn list_items(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<ItemsResponse>, StatusCode> {
    let item_rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT id, household_id, name_en, name_es, category_id, subcategory_id, \
                default_constraint_policy \
         FROM canonical_items \
         WHERE household_id IN \
               (SELECT household_id FROM household_members WHERE user_id = ?1) \
         ORDER BY name_en",
    )
    .bind(&user.user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("list_items.items"))?;

    // The categories + subcategories catalogues are shared (not household-scoped)
    // and the per-store aisle orders are scoped to the caller's household set.
    // All are returned even when the caller has no canonical items yet, so a fresh
    // household can still render shopping mode's store ordering from its seeded
    // defaults.
    let categories = load_categories(&state.pool)
        .await
        .map_err(db_error("list_items.categories"))?;
    let subcategories = load_subcategories(&state.pool)
        .await
        .map_err(db_error("list_items.subcategories"))?;
    let store_category_orders = load_store_category_orders(&state.pool, &user.user_id)
        .await
        .map_err(db_error("list_items.store_category_orders"))?;

    if item_rows.is_empty() {
        return Ok(Json(ItemsResponse {
            items: Vec::new(),
            categories,
            subcategories,
            store_category_orders,
        }));
    }

    // Batch all aliases and constraints for the scoped item set in a small,
    // constant number of queries (no per-item N+1). Both helpers chunk their
    // `IN (...)` lists to stay under SQLite's bound-parameter limit.
    let item_ids: Vec<String> = item_rows.iter().map(|r| r.0.clone()).collect();

    let mut aliases_by_item = load_aliases_for_items(&state.pool, &user.user_id, &item_ids)
        .await
        .map_err(db_error("list_items.aliases"))?;
    let mut constraints_by_item = load_constraints_for_items(&state.pool, &item_ids)
        .await
        .map_err(db_error("list_items.constraints"))?;

    let items = item_rows
        .into_iter()
        .map(
            |(
                id,
                household_id,
                name_en,
                name_es,
                category_id,
                subcategory_id,
                default_constraint_policy,
            )| {
                let aliases = aliases_by_item.remove(&id).unwrap_or_default();
                let constraints = constraints_by_item.remove(&id);
                ItemView {
                    id,
                    household_id,
                    name_en,
                    name_es,
                    category_id,
                    subcategory_id,
                    default_constraint_policy,
                    aliases,
                    constraints,
                }
            },
        )
        .collect();

    Ok(Json(ItemsResponse {
        items,
        categories,
        subcategories,
        store_category_orders,
    }))
}

/// Load the shared `categories` catalogue (id, names, icon, default order).
///
/// Categories are global reference data (not household-scoped): every household
/// orders the same canonical category set per store.
async fn load_categories(pool: &sqlx::SqlitePool) -> Result<Vec<CategoryView>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64)>(
        "SELECT id, name_en, name_es, icon, default_order \
         FROM categories \
         ORDER BY default_order, id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, name_en, name_es, icon, default_order)| CategoryView {
            id,
            name_en,
            name_es,
            icon,
            default_order,
        })
        .collect())
}

/// Load the shared `subcategories` catalogue (id, parent, names, default order).
///
/// Shared reference data like `categories`: every household sees the same
/// sub-category split. Ordered by parent then display order so the client can
/// group a category's items without re-sorting the catalogue.
async fn load_subcategories(pool: &sqlx::SqlitePool) -> Result<Vec<SubcategoryView>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, String, String, i64)>(
        "SELECT id, category_id, name_en, name_es, default_order \
         FROM subcategories \
         ORDER BY category_id, default_order, id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, category_id, name_en, name_es, default_order)| SubcategoryView {
                id,
                category_id,
                name_en,
                name_es,
                default_order,
            },
        )
        .collect())
}

/// Load the per-store aisle orders for the caller's household(s).
///
/// Household-scoped via the same `household_members` sub-select the items query
/// uses, so a user only sees their own household's curated (or seeded) orders.
async fn load_store_category_orders(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Vec<StoreCategoryOrderView>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, store, category_id, display_order \
         FROM store_category_orders \
         WHERE household_id IN \
               (SELECT household_id FROM household_members WHERE user_id = ?1) \
         ORDER BY store, display_order, category_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, store, category_id, display_order)| StoreCategoryOrderView {
                id,
                store,
                category_id,
                display_order,
            },
        )
        .collect())
}

/// Max bound parameters per chunked `IN (...)` query. SQLite's compile-time
/// limit is 999 (pre-3.32) / 32766 (3.32+); 900 is comfortably below either and
/// leaves headroom for the per-chunk household-scope parameter.
const IN_CHUNK_SIZE: usize = 900;

/// Build all aliases for the given canonical item ids, grouped by
/// `canonical_item_id`.
///
/// Defense in depth: aliases are additionally constrained to the caller's
/// household set (`item_aliases.household_id`) so a mis-pointed alias from
/// another household cannot leak through the `canonical_item_id` join.
async fn load_aliases_for_items(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    item_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<AliasView>>, sqlx::Error> {
    let mut grouped: std::collections::HashMap<String, Vec<AliasView>> =
        std::collections::HashMap::new();

    for chunk in item_ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, id, alias, language, source FROM item_aliases \
             WHERE canonical_item_id IN ({placeholders}) \
               AND household_id IN \
                   (SELECT household_id FROM household_members WHERE user_id = ?) \
             ORDER BY alias"
        );
        let mut query = sqlx::query_as::<_, (String, String, String, String, String)>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        query = query.bind(user_id);

        for (canonical_item_id, id, alias, language, source) in query.fetch_all(pool).await? {
            grouped
                .entry(canonical_item_id)
                .or_default()
                .push(AliasView {
                    id,
                    alias,
                    language,
                    source,
                });
        }
    }

    Ok(grouped)
}

/// Build the (at most one) constraints row for each canonical item id, keyed by
/// `canonical_item_id`.
async fn load_constraints_for_items(
    pool: &sqlx::SqlitePool,
    item_ids: &[String],
) -> Result<std::collections::HashMap<String, ConstraintsView>, sqlx::Error> {
    let mut by_item: std::collections::HashMap<String, ConstraintsView> =
        std::collections::HashMap::new();

    for chunk in item_ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, brand, allowed_brands, excluded_brands, \
                    preferred_size_value, preferred_size_basis, min_size_value, min_size_basis, \
                    max_size_value, max_size_basis, pack_count, form_factor, dietary_flags, \
                    substitution_policy \
             FROM item_constraints WHERE canonical_item_id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, KeyedConstraintsRow>(&sql);
        for id in chunk {
            query = query.bind(id);
        }

        for r in query.fetch_all(pool).await? {
            by_item.insert(
                r.0,
                ConstraintsView {
                    brand: r.1,
                    allowed_brands: r.2,
                    excluded_brands: r.3,
                    preferred_size_value: r.4,
                    preferred_size_basis: r.5,
                    min_size_value: r.6,
                    min_size_basis: r.7,
                    max_size_value: r.8,
                    max_size_basis: r.9,
                    pack_count: r.10,
                    form_factor: r.11,
                    dietary_flags: r.12,
                    substitution_policy: r.13,
                },
            );
        }
    }

    Ok(by_item)
}

/// Build `n` comma-separated `?` placeholders for an `IN (...)` clause.
fn sql_placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n.saturating_mul(2));
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// An `item_constraints` row keyed by its `canonical_item_id` (first column),
/// followed by the columns in [`ConstraintsView`] order. Flat (not nested) so
/// sqlx can decode it via `FromRow`.
type KeyedConstraintsRow = (
    String,         // canonical_item_id
    Option<String>, // brand
    Option<String>, // allowed_brands
    Option<String>, // excluded_brands
    Option<f64>,    // preferred_size_value
    Option<String>, // preferred_size_basis
    Option<f64>,    // min_size_value
    Option<String>, // min_size_basis
    Option<f64>,    // max_size_value
    Option<String>, // max_size_basis
    Option<i64>,    // pack_count
    Option<String>, // form_factor
    Option<String>, // dietary_flags
    String,         // substitution_policy
);
