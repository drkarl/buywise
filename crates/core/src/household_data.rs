//! Household data controls: export and delete (issue #119, roadmap #114).
//!
//! Shared by the `buywise-cli` operator commands (`export-household` /
//! `delete-household`) and the member-facing API endpoints
//! (`crates/api/src/household.rs`, issue #136), so the two never drift:
//!
//! * [`export_household`] builds every household-scoped record as one JSON
//!   document, including the household's receipt-image METADATA (never the image
//!   bytes, which live on disk). The layout is documented in
//!   `docs/household-export.md`; it is
//!   plain `serde_json::Value` (no app domain types) so the document is a
//!   stable, self-describing interchange format rather than a dump of internal
//!   structs.
//! * [`delete_household`] removes every household-scoped record in one
//!   transaction, and unlinks the household's receipt-image files in that same
//!   transaction ([`crate::receipts`]). `dry_run` runs the whole delete inside a
//!   transaction and ROLLS IT BACK, so it reports the exact rows a real run
//!   would remove without guessing at per-table predicates.
//!
//! Both functions are READ-ONLY against the schema (neither runs migrations) and
//! expect a database the API or an operator has already migrated. What is
//! household-scoped and what is not is listed in [`HOUSEHOLD_TABLES`] and
//! documented in `docs/data-flows.md` (Retention and deletion). Shared reference
//! data — `categories`, `subcategories`, `retailer_products`, `product_offers`,
//! `source_runs`, `api_usage_events` — is deliberately untouched by deletion: it
//! carries no household identity.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Column, Row, SqliteConnection, SqlitePool};

use crate::db;
use crate::receipts;

/// `format` field of the export document. Bump-incompatible changes accompany a
/// [`EXPORT_FORMAT_VERSION`] bump; consumers MUST check both.
pub const EXPORT_FORMAT: &str = "buywise-household-export";

/// Version of the export layout. Documented in `docs/household-export.md`.
pub const EXPORT_FORMAT_VERSION: i64 = 1;

/// Errors produced by the household export and delete operations.
#[derive(Debug, thiserror::Error)]
pub enum HouseholdDataError {
    /// No household exists with the requested id (a typo, or already deleted).
    #[error("no household with id {0}")]
    NoSuchHousehold(String),

    /// A query or transaction failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    /// A receipt image file could not be removed while deleting a household.
    #[error(transparent)]
    Receipts(#[from] crate::receipts::ReceiptError),
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Build the full export document for `household_id`.
///
/// Errors with [`HouseholdDataError::NoSuchHousehold`] if the household does not
/// exist, so a typo in the id never yields a well-formed empty document that
/// looks like a real export.
pub async fn export_household(
    pool: &SqlitePool,
    household_id: &str,
    now: DateTime<Utc>,
) -> Result<Value, HouseholdDataError> {
    let household = fetch_rows_bound(
        pool,
        "SELECT id, name, created_at FROM households WHERE id = ?1",
        household_id,
    )
    .await?;
    let Some(household) = household.into_iter().next() else {
        return Err(HouseholdDataError::NoSuchHousehold(
            household_id.to_string(),
        ));
    };

    let members = fetch_rows_bound(
        pool,
        "SELECT hm.user_id, u.display_name, hm.role, hm.joined_at \
         FROM household_members hm JOIN users u ON u.id = hm.user_id \
         WHERE hm.household_id = ?1 ORDER BY hm.user_id",
        household_id,
    )
    .await?;

    // Shared reference data the household's records point at, so the document
    // can be read without a BuyWise database. Read whole: categories and
    // subcategories are a small, closed vocabulary.
    let categories = fetch_rows(
        pool,
        "SELECT id, name_en, name_es, icon, default_order, is_fresh \
         FROM categories ORDER BY id",
    )
    .await?;
    let subcategories = fetch_rows(
        pool,
        "SELECT id, category_id, name_en, name_es, default_order \
         FROM subcategories ORDER BY id",
    )
    .await?;

    let catalogue_items = fetch_rows_bound(
        pool,
        "SELECT id, name_en, name_es, category_id, subcategory_id, \
                default_constraint_policy, created_at \
         FROM canonical_items WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    let aliases = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, alias, language, source, created_at \
         FROM item_aliases WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    let constraints = fetch_rows_bound(
        pool,
        "SELECT c.id, c.canonical_item_id, c.brand, c.allowed_brands, \
                c.excluded_brands, c.preferred_size_value, c.preferred_size_basis, \
                c.min_size_value, c.min_size_basis, c.max_size_value, c.max_size_basis, \
                c.pack_count, c.form_factor, c.dietary_flags, c.substitution_policy, \
                c.updated_at \
         FROM item_constraints c JOIN canonical_items ci ON ci.id = c.canonical_item_id \
         WHERE ci.household_id = ?1 ORDER BY c.id",
        household_id,
    )
    .await?;

    let preferences = json!({
        "settings": fetch_rows_bound(
            pool,
            "SELECT convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at \
             FROM household_settings WHERE household_id = ?1",
            household_id,
        ).await?.into_iter().next().unwrap_or(Value::Null),
        "stores": fetch_rows_bound(
            pool,
            "SELECT retailer, status, nearby, updated_at \
             FROM household_store_settings WHERE household_id = ?1 ORDER BY retailer",
            household_id,
        ).await?,
        "store_category_rules": fetch_rows_bound(
            pool,
            "SELECT retailer, category_id, subcategory_id, allowed, updated_at \
             FROM household_store_category_rules WHERE household_id = ?1 \
             ORDER BY retailer, category_id, subcategory_id",
            household_id,
        ).await?,
        "store_category_orders": fetch_rows_bound(
            pool,
            "SELECT store, category_id, display_order, updated_at \
             FROM store_category_orders WHERE household_id = ?1 \
             ORDER BY store, display_order, category_id",
            household_id,
        ).await?,
        "purchase_dismissals": fetch_rows_bound(
            pool,
            "SELECT id, canonical_item_id, dismissed_by, dismissed_at \
             FROM purchase_dismissals WHERE household_id = ?1 ORDER BY id",
            household_id,
        ).await?,
    });

    let product_matches = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, retailer_product_id, status, matched_by, \
                created_at, updated_at \
         FROM product_matches WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    // Only the retailer products this household's matches point at, so the
    // document stays bounded by household size rather than the shared price
    // catalogue.
    let retailer_products = fetch_rows_bound(
        pool,
        "SELECT id, source, retailer, external_id, url, name, brand, category, \
                pack_size_text, size_value, size_unit, pack_count, form_factor, \
                dietary_flags, first_seen_at, last_seen_at \
         FROM retailer_products WHERE id IN \
             (SELECT retailer_product_id FROM product_matches WHERE household_id = ?1) \
         ORDER BY id",
        household_id,
    )
    .await?;
    let match_feedback = fetch_rows_bound(
        pool,
        "SELECT mf.id, mf.product_match_id, mf.user_id, mf.action, mf.created_at \
         FROM match_feedback mf JOIN product_matches pm ON pm.id = mf.product_match_id \
         WHERE pm.household_id = ?1 ORDER BY mf.id",
        household_id,
    )
    .await?;

    let purchase_events = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, list_id, entry_id, source, occurred_at, recorded_at \
         FROM purchase_events WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;

    // The household's purchase ledger (issue #148): transactions with their
    // lines and any superseded revisions nested under each transaction, like the
    // list-scoped arrays below.
    let mut purchase_transactions = fetch_rows_bound(
        pool,
        "SELECT id, household_id, retailer, purchase_date, source_kind, source_transaction_id, \
                transaction_identity, identity_kind, fingerprint, currency, total_cents, revision, \
                source_payload, created_at, updated_at, receipt_image_id \
         FROM purchase_transactions WHERE household_id = ?1 ORDER BY purchase_date, id",
        household_id,
    )
    .await?;
    attach_children_by(
        &mut purchase_transactions,
        "lines",
        "transaction_id",
        fetch_rows_bound(
            pool,
            "SELECT pl.id, pl.transaction_id, pl.line_ordinal, pl.original_text, pl.name, \
                    pl.brand, pl.quantity, pl.size_value, pl.size_unit, pl.unit_price_cents, \
                    pl.line_total_cents, pl.line_discount_cents, pl.deposit_cents, \
                    pl.retailer_product_id, pl.canonical_item_id, pl.match_status, pl.revision, \
                    pl.created_at \
             FROM purchase_lines pl JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
             WHERE pt.household_id = ?1 ORDER BY pl.transaction_id, pl.line_ordinal",
            household_id,
        )
        .await?,
    );
    attach_children_by(
        &mut purchase_transactions,
        "revisions",
        "transaction_id",
        fetch_rows_bound(
            pool,
            "SELECT pr.id, pr.transaction_id, pr.revision, pr.retailer, pr.purchase_date, \
                    pr.currency, pr.total_cents, pr.source_payload, pr.lines_json, pr.superseded_at \
             FROM purchase_transaction_revisions pr \
             JOIN purchase_transactions pt ON pt.id = pr.transaction_id \
             WHERE pt.household_id = ?1 ORDER BY pr.transaction_id, pr.revision",
            household_id,
        )
        .await?,
    );
    // Receipt-image METADATA only. The bytes are not in SQLite and are not part
    // of a database export; `storage_key` is deliberately omitted because it is
    // an internal file name, not something a consumer can resolve. Each image
    // stays retrievable through the app (its `id` is the handle) until its 30-day
    // expiry removes it — see `docs/household-export.md` and
    // `docs/data-flows.md` §4.
    let receipt_rows = sqlx::query(
        "SELECT id, uploaded_by, uploaded_at, expires_at, content_type, byte_size, \
                sha256_hex, processing \
         FROM receipt_images WHERE household_id = ?1 AND expires_at > ?2 \
         ORDER BY uploaded_at, id",
    )
    .bind(household_id)
    .bind(receipts::iso(now))
    .fetch_all(pool)
    .await?;
    let receipt_images: Vec<Value> = receipt_rows.iter().map(row_to_object).collect();

    let mut lists = fetch_rows_bound(
        pool,
        "SELECT id, name, status, current_revision, created_at, archived_at \
         FROM shopping_lists WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    attach_children(
        &mut lists,
        "entries",
        fetch_rows_bound(
            pool,
            "SELECT le.id, le.list_id, le.canonical_item_id, le.resolution_status, \
                    le.resolution_candidates_json, le.raw_text, le.display_name, \
                    le.added_by, le.quantity, le.status, le.bought_by, le.bought_at, \
                    le.effective_constraint_hash, le.last_updated_revision, \
                    le.created_at, le.updated_at \
             FROM list_entries le JOIN shopping_lists sl ON sl.id = le.list_id \
             WHERE sl.household_id = ?1 ORDER BY le.id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "entry_events",
        fetch_rows_bound(
            pool,
            "SELECT e.id, e.entry_id, le.list_id, e.user_id, e.event_type, e.payload, e.created_at \
             FROM list_entry_events e \
             JOIN list_entries le ON le.id = e.entry_id \
             JOIN shopping_lists sl ON sl.id = le.list_id \
             WHERE sl.household_id = ?1 ORDER BY e.id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "mutations",
        fetch_rows_bound(
            pool,
            "SELECT id, client_mutation_id, device_id, user_id, list_id, entity_type, \
                    entity_id, operation_type, payload, base_revision, created_at_client, \
                    received_at_server, server_revision, status, response_json \
             FROM mutations WHERE list_id IN \
                 (SELECT id FROM shopping_lists WHERE household_id = ?1) ORDER BY id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "recommendation_runs",
        fetch_rows_bound(
            pool,
            "SELECT id, list_id, run_at, offer_snapshot_date, plans_json, recommended_plan, \
                    convenience_threshold_2, convenience_threshold_3 \
             FROM recommendation_runs WHERE household_id = ?1 ORDER BY id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "savings_events",
        fetch_rows_bound(
            pool,
            "SELECT s.id, s.list_id, s.recommendation_run_id, s.baseline_cost_cents, \
                    s.recommended_cost_cents, s.theoretical_minimum_cost_cents, \
                    s.chosen_basket_cost_cents, s.chosen_plan, s.chosen_stores, \
                    s.gross_saving_cents, s.accepted_savings_cents, s.rejected_savings_cents, \
                    s.travel_cost_cents, s.subscription_cost_alloc_cents, s.net_saving_cents, \
                    s.status, s.created_at \
             FROM savings_events s JOIN shopping_lists sl ON sl.id = s.list_id \
             WHERE sl.household_id = ?1 ORDER BY s.id",
            household_id,
        )
        .await?,
    );

    let mut catalogue = nest_children(catalogue_items, &aliases, "canonical_item_id", "aliases");
    let mut constraints_by_item = group_by(constraints, "canonical_item_id");
    for item in catalogue.iter_mut() {
        if let Value::Object(map) = item {
            let id = map.get("id").and_then(Value::as_str).unwrap_or_default();
            let constraint = constraints_by_item
                .remove(id)
                .and_then(|mut rows| rows.pop())
                .unwrap_or(Value::Null);
            map.insert("constraint".to_string(), constraint);
        }
    }

    Ok(json!({
        "format": EXPORT_FORMAT,
        "format_version": EXPORT_FORMAT_VERSION,
        "exported_at": db::now_iso(),
        "household": household,
        "members": members,
        "reference_data": { "categories": categories, "subcategories": subcategories },
        "catalogue": catalogue,
        "preferences": preferences,
        "product_matches": product_matches,
        "retailer_products": retailer_products,
        "match_feedback": match_feedback,
        "lists": lists,
        "purchase_events": purchase_events,
        "purchase_transactions": purchase_transactions,
        "receipt_images": receipt_images,
    }))
}

/// Attach `children` (grouped by their `list_id`) to each list under `key`.
fn attach_children(lists: &mut [Value], key: &str, children: Vec<Value>) {
    attach_children_by(lists, key, "list_id", children);
}

/// Attach `children` grouped by `parent_key` to each parent under `key`.
///
/// [`attach_children`] is the list case (`list_id`); the purchase ledger nests
/// lines and revisions by `transaction_id`.
fn attach_children_by(parents: &mut [Value], key: &str, parent_key: &str, children: Vec<Value>) {
    let mut grouped = group_by(children, parent_key);
    for parent in parents.iter_mut() {
        if let Value::Object(map) = parent {
            let id = map
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let rows = grouped.remove(&id).unwrap_or_default();
            map.insert(key.to_string(), Value::Array(rows));
        }
    }
}

/// Group `children` by the string value of `parent_key`, preserving order.
fn group_by(children: Vec<Value>, parent_key: &str) -> HashMap<String, Vec<Value>> {
    let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
    for child in children {
        let Some(parent) = child.get(parent_key).and_then(Value::as_str) else {
            continue;
        };
        grouped.entry(parent.to_string()).or_default().push(child);
    }
    grouped
}

/// Nest each parent's children under `child_key`, keeping child order.
fn nest_children(
    parents: Vec<Value>,
    children: &[Value],
    parent_key: &str,
    child_key: &str,
) -> Vec<Value> {
    let mut grouped = group_by(children.to_vec(), parent_key);
    parents
        .into_iter()
        .map(|mut parent| {
            if let Value::Object(map) = &mut parent {
                let id = map
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let rows = grouped.remove(&id).unwrap_or_default();
                map.insert(child_key.to_string(), Value::Array(rows));
            }
            parent
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Every table row the delete removes for the target household, in dependency
/// order (children before parents, so immediate foreign keys hold).
///
/// Verification guard: `docs/data-flows.md` (Retention and deletion) lists this
/// set; `crates/cli/tests/household_data.rs` asserts a deleted household leaves
/// no row in any of them. `dry_run` executes every statement and rolls back, so
/// the reported counts are exactly what a real run removes.
///
/// Deliberately absent (shared, not household-identifiable): `categories`,
/// `subcategories`, `retailer_products`, `product_offers`, `source_runs`,
/// `api_usage_events`.
pub const HOUSEHOLD_TABLES: &[&str] = &[
    "list_entry_events",
    "savings_events",
    "purchase_events",
    "purchase_transaction_revisions",
    "purchase_lines",
    "purchase_transactions",
    "mutations",
    "list_entries",
    "recommendation_runs",
    "shopping_lists",
    "match_feedback",
    "product_matches",
    "item_constraints",
    "item_aliases",
    "purchase_dismissals",
    "canonical_items",
    "household_store_category_rules",
    "household_store_settings",
    "household_settings",
    "store_category_orders",
    // The receipt-image rows. Their bytes are unlinked in the same transaction
    // by the `DELETE ... RETURNING storage_key` step below, file before commit.
    "receipt_images",
    "household_members",
    "households",
    "sessions",
    "users",
];

/// One delete step: the table it clears and the statement that clears it. The
/// label MUST match the corresponding entry in [`HOUSEHOLD_TABLES`] (both are
/// asserted against the same order in the tests).
struct DeleteStep {
    table: &'static str,
    delete_sql: &'static str,
}

/// The delete statements, in dependency order. Every statement binds `?1` to the
/// household id (directly or through a subquery). The `sessions`/`users` cleanup
/// is not here: it runs last through [`delete_orphaned_sessions`] and
/// [`delete_orphaned_accounts`], which need the member ids captured before the
/// membership rows are removed.
const DELETE_STEPS: &[DeleteStep] = &[
    DeleteStep {
        table: "list_entry_events",
        delete_sql: "DELETE FROM list_entry_events WHERE entry_id IN \
             (SELECT le.id FROM list_entries le \
              JOIN shopping_lists sl ON sl.id = le.list_id WHERE sl.household_id = ?1)",
    },
    DeleteStep {
        table: "savings_events",
        delete_sql: "DELETE FROM savings_events WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "purchase_events",
        delete_sql: "DELETE FROM purchase_events WHERE household_id = ?1",
    },
    DeleteStep {
        table: "purchase_transaction_revisions",
        delete_sql: "DELETE FROM purchase_transaction_revisions WHERE transaction_id IN \
             (SELECT id FROM purchase_transactions WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "purchase_lines",
        delete_sql: "DELETE FROM purchase_lines WHERE transaction_id IN \
             (SELECT id FROM purchase_transactions WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "purchase_transactions",
        delete_sql: "DELETE FROM purchase_transactions WHERE household_id = ?1",
    },
    DeleteStep {
        table: "mutations",
        delete_sql: "DELETE FROM mutations WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "list_entries",
        delete_sql: "DELETE FROM list_entries WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "recommendation_runs",
        delete_sql: "DELETE FROM recommendation_runs WHERE household_id = ?1",
    },
    DeleteStep {
        table: "shopping_lists",
        delete_sql: "DELETE FROM shopping_lists WHERE household_id = ?1",
    },
    DeleteStep {
        table: "match_feedback",
        delete_sql: "DELETE FROM match_feedback WHERE product_match_id IN \
             (SELECT id FROM product_matches WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "product_matches",
        delete_sql: "DELETE FROM product_matches WHERE household_id = ?1",
    },
    DeleteStep {
        table: "item_constraints",
        delete_sql: "DELETE FROM item_constraints WHERE canonical_item_id IN \
             (SELECT id FROM canonical_items WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "item_aliases",
        delete_sql: "DELETE FROM item_aliases WHERE household_id = ?1",
    },
    DeleteStep {
        table: "purchase_dismissals",
        delete_sql: "DELETE FROM purchase_dismissals WHERE household_id = ?1",
    },
    DeleteStep {
        table: "canonical_items",
        delete_sql: "DELETE FROM canonical_items WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_store_category_rules",
        delete_sql: "DELETE FROM household_store_category_rules WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_store_settings",
        delete_sql: "DELETE FROM household_store_settings WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_settings",
        delete_sql: "DELETE FROM household_settings WHERE household_id = ?1",
    },
    DeleteStep {
        table: "store_category_orders",
        delete_sql: "DELETE FROM store_category_orders WHERE household_id = ?1",
    },
    // The one step whose statement RETURNS values: the storage keys of the rows
    // it removes, so `delete_household` can unlink those files in the same
    // transaction. `receipts::RECEIPT_IMAGES_TABLE` names this step.
    DeleteStep {
        table: receipts::RECEIPT_IMAGES_TABLE,
        delete_sql: "DELETE FROM receipt_images WHERE household_id = ?1 RETURNING storage_key",
    },
    DeleteStep {
        table: "household_members",
        delete_sql: "DELETE FROM household_members WHERE household_id = ?1",
    },
    DeleteStep {
        table: "households",
        delete_sql: "DELETE FROM households WHERE id = ?1",
    },
];

/// Delete every session row of each user in `user_ids`; returns the rows removed.
///
/// Used by member removal ([`crate::members::remove_member`]): a removed member's
/// open sessions must stop working on their very next request, even when they
/// still belong to another household. The account itself is handled separately
/// by [`delete_orphaned_accounts`].
pub async fn delete_sessions_for_users(
    conn: &mut SqliteConnection,
    user_ids: &[String],
) -> Result<u64, sqlx::Error> {
    let mut removed = 0u64;
    for user_id in user_ids {
        let result = sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(user_id)
            .execute(&mut *conn)
            .await?;
        removed += result.rows_affected();
    }
    Ok(removed)
}

/// Delete the session rows of each user in `user_ids` who belongs to no
/// household; returns the rows removed.
///
/// The household-delete rule: a member shared with another household keeps their
/// session (they are still signed in elsewhere), while a member left with no
/// household is signed out. Callers delete the relevant `household_members` rows
/// first, so a membership just removed no longer counts.
pub async fn delete_orphaned_sessions(
    conn: &mut SqliteConnection,
    user_ids: &[String],
) -> Result<u64, sqlx::Error> {
    let mut removed = 0u64;
    for user_id in user_ids {
        let result = sqlx::query(
            "DELETE FROM sessions WHERE user_id = ?1 AND NOT EXISTS \
             (SELECT 1 FROM household_members m WHERE m.user_id = ?1)",
        )
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
        removed += result.rows_affected();
    }
    Ok(removed)
}

/// The display name a deactivated account carries.
///
/// Views that join `users` to attribute a contribution read this in place of the
/// person's name: the household keeps the history, attributed to a former
/// member.
pub(crate) const FORMER_MEMBER_DISPLAY_NAME: &str = "Former member";

/// The reserved domain for a deactivated account's tombstone email.
///
/// `.invalid` is reserved by RFC 2606 and can never resolve, and the address
/// embeds the (unique) user id, so the tombstone is unique while the real
/// address becomes free to register again.
const DEACTIVATED_EMAIL_DOMAIN: &str = "deactivated.invalid";

/// The `password_hash` stored on a deactivated account.
///
/// Deliberately not a PHC string, so [`crate::password::verify_password`]
/// refuses every plaintext against it — no password can ever sign the account in
/// again.
const DEACTIVATED_PASSWORD_HASH: &str = "!deactivated";

/// The tombstone email written to a deactivated account's `users` row.
///
/// Derived only from the user id, so re-running a deactivation is idempotent and
/// two deactivations can never collide (`users.email` is UNIQUE).
pub(crate) fn deactivated_email(user_id: &str) -> String {
    format!("deactivated+{user_id}@{DEACTIVATED_EMAIL_DOMAIN}")
}

/// What [`delete_orphaned_accounts`] did with the accounts it was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanedAccountCleanup {
    /// Accounts whose `users` row was deleted, because nothing referenced it.
    pub deleted: u64,
    /// Accounts deactivated in place instead, because history rows still
    /// reference them.
    pub deactivated: u64,
}

/// Every `(table, column)` in the schema whose FOREIGN KEY targets `users(id)`.
///
/// Read from the database itself (`sqlite_master` joined with the table-valued
/// `pragma_foreign_key_list`), never a hand-kept table list, so a table a later
/// migration adds is found without a code change here.
pub(crate) async fn user_referencing_columns(
    conn: &mut SqliteConnection,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.name, fk.\"from\" \
         FROM sqlite_master m, pragma_foreign_key_list(m.name) fk \
         WHERE m.type = 'table' AND fk.\"table\" = 'users' \
         ORDER BY m.name, fk.\"from\"",
    )
    .fetch_all(&mut *conn)
    .await
}

/// Whether any row in a table that references `users(id)` still points at
/// `user_id`.
async fn account_is_referenced(
    conn: &mut SqliteConnection,
    user_id: &str,
    referencing: &[(String, String)],
) -> Result<bool, sqlx::Error> {
    for (table, column) in referencing {
        // Table and column names come from `sqlite_master` and cannot be bound
        // as parameters, so they are quoted (doubling any embedded quote).
        let sql = format!(
            "SELECT 1 FROM \"{}\" WHERE \"{}\" = ?1 LIMIT 1",
            table.replace('"', "\"\""),
            column.replace('"', "\"\""),
        );
        let found: Option<i64> = sqlx::query_scalar(&sql)
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Anonymise a `users` row in place: unusable password, tombstone email, former
/// member display name. The row stays, so every foreign key that points at it —
/// and the attribution of the history it keeps — still holds.
async fn deactivate_account(conn: &mut SqliteConnection, user_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET email = ?2, password_hash = ?3, display_name = ?4 WHERE id = ?1")
        .bind(user_id)
        .bind(deactivated_email(user_id))
        .bind(DEACTIVATED_PASSWORD_HASH)
        .bind(FORMER_MEMBER_DISPLAY_NAME)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Clean up the account of each user in `user_ids` who belongs to no household.
///
/// The one account-cleanup rule, shared by [`delete_household`] and by member
/// removal ([`crate::members::remove_member`]): a user who also belongs to
/// another household keeps their account untouched. Among the users left with no
/// household:
///
/// * an account that nothing references is DELETED — the behaviour for a member
///   who never contributed;
/// * an account that rows still reference (an item they added, a receipt they
///   uploaded, an audit event) is DEACTIVATED in place instead
///   ([`deactivate_account`]): the `users` row stays so the foreign keys hold and
///   the household keeps its history attributed to a former member, while the
///   password becomes unusable and the email becomes a unique tombstone.
///
/// Deleting such an account would violate the `NO ACTION` foreign keys several
/// tables declare against `users(id)` (issue #152). Which tables count is
/// discovered from the schema ([`user_referencing_columns`]), so a table added
/// later is handled automatically. Callers delete the relevant
/// `household_members` rows first.
pub async fn delete_orphaned_accounts(
    conn: &mut SqliteConnection,
    user_ids: &[String],
) -> Result<OrphanedAccountCleanup, sqlx::Error> {
    let referencing = user_referencing_columns(&mut *conn).await?;
    let mut cleanup = OrphanedAccountCleanup::default();
    for user_id in user_ids {
        let orphan: Option<String> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id = ?1 AND NOT EXISTS \
             (SELECT 1 FROM household_members m WHERE m.user_id = ?1)",
        )
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?;
        if orphan.is_none() {
            // Belongs to another household, or the row is already gone.
            continue;
        }
        if account_is_referenced(&mut *conn, user_id, &referencing).await? {
            deactivate_account(&mut *conn, user_id).await?;
            cleanup.deactivated += 1;
        } else {
            let result = sqlx::query("DELETE FROM users WHERE id = ?1")
                .bind(user_id)
                .execute(&mut *conn)
                .await?;
            cleanup.deleted += result.rows_affected();
        }
    }
    Ok(cleanup)
}

/// Delete every household-scoped row for `household_id` in one transaction,
/// including the household's receipt-image files on disk.
///
/// With `dry_run`, the same statements run and the transaction is rolled back,
/// so the returned `(table, rows_affected)` pairs are exactly what a real run
/// would remove — and no file is touched. Errors with
/// [`HouseholdDataError::NoSuchHousehold`] if the household does not exist.
///
/// `receipt_dir` is the directory holding receipt-image bytes (the same value
/// [`crate::receipts::ReceiptConfig::dir`] carries). It is required rather than
/// optional: the rows name those files, and a delete that skipped them would
/// leave every household image on disk after the household itself was gone.
/// The files are unlinked inside the transaction, before it commits, so a failed
/// unlink aborts the delete (leaving the rows to retry) rather than orphaning an
/// image.
pub async fn delete_household(
    pool: &SqlitePool,
    household_id: &str,
    dry_run: bool,
    receipt_dir: &Path,
) -> Result<Vec<(&'static str, u64)>, HouseholdDataError> {
    household_name(pool, household_id).await?;

    let mut tx = pool.begin().await?;

    // Capture the member ids before their membership rows go: the account and
    // session cleanup below needs to tell a member shared with another household
    // (keep the account) from one left membership-less (remove it).
    let candidate_user_ids: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM household_members WHERE household_id = ?1")
            .bind(household_id)
            .fetch_all(&mut *tx)
            .await?;

    let mut counts = Vec::with_capacity(DELETE_STEPS.len() + 2);
    for step in DELETE_STEPS {
        if step.table == receipts::RECEIPT_IMAGES_TABLE {
            // The one statement that returns values: the storage keys of the
            // rows it removes, so the bytes go with them (file first).
            let storage_keys: Vec<String> = sqlx::query_scalar(step.delete_sql)
                .bind(household_id)
                .fetch_all(&mut *tx)
                .await?;
            if !dry_run {
                receipts::delete_household_receipt_files(receipt_dir, &storage_keys)?;
            }
            counts.push((step.table, storage_keys.len() as u64));
            continue;
        }
        let result = sqlx::query(step.delete_sql)
            .bind(household_id)
            .execute(&mut *tx)
            .await?;
        counts.push((step.table, result.rows_affected()));
    }

    let sessions = delete_orphaned_sessions(&mut tx, &candidate_user_ids).await?;
    // `deleted` only: a deactivated account's row stays (see
    // [`delete_orphaned_accounts`]), so it is not a removed row.
    let accounts = delete_orphaned_accounts(&mut tx, &candidate_user_ids).await?;
    counts.push(("sessions", sessions));
    counts.push(("users", accounts.deleted));

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(counts)
}

/// The household's display name, or [`HouseholdDataError::NoSuchHousehold`] if
/// no such household exists.
pub async fn household_name(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<String, HouseholdDataError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM households WHERE id = ?1")
        .bind(household_id)
        .fetch_optional(pool)
        .await?;
    name.ok_or_else(|| HouseholdDataError::NoSuchHousehold(household_id.to_string()))
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Fetch rows as JSON objects, binding `?1` to `household_id`.
async fn fetch_rows_bound(
    pool: &SqlitePool,
    sql: &str,
    household_id: &str,
) -> Result<Vec<Value>, HouseholdDataError> {
    let rows = sqlx::query(sql).bind(household_id).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_object).collect())
}

/// Fetch rows as JSON objects from a statement that binds no parameters.
async fn fetch_rows(pool: &SqlitePool, sql: &str) -> Result<Vec<Value>, HouseholdDataError> {
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_object).collect())
}

/// Convert one SQLite row into a JSON object keyed by column name.
///
/// SQLite is dynamically typed, so the value is decoded by trying the column's
/// expected JSON shapes in order (integer, then real, then text). A NULL — or a
/// stored type none of the three accept — becomes JSON `null`, which keeps the
/// export total rather than failing the whole document on one odd value.
fn row_to_object(row: &SqliteRow) -> Value {
    let mut map = Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        map.insert(column.name().to_string(), column_value(row, index));
    }
    Value::Object(map)
}

fn column_value(row: &SqliteRow, index: usize) -> Value {
    if let Ok(Some(value)) = row.try_get::<Option<i64>, _>(index) {
        return Value::from(value);
    }
    if let Ok(Some(value)) = row.try_get::<Option<f64>, _>(index) {
        return Value::from(value);
    }
    if let Ok(Some(value)) = row.try_get::<Option<String>, _>(index) {
        return Value::from(value);
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn migrated_pool() -> SqlitePool {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        pool
    }

    /// The enumeration really is schema-driven: a table created after the
    /// migrations is reported, which is what lets a later migration's table be
    /// handled without a code change (issue #152).
    #[tokio::test]
    async fn user_referencing_columns_sees_a_table_added_after_migrations() {
        let pool = migrated_pool().await;
        sqlx::query(
            "CREATE TABLE later_table (id TEXT PRIMARY KEY, added_by TEXT REFERENCES users(id))",
        )
        .execute(&pool)
        .await
        .expect("create later table");

        let mut conn = pool.acquire().await.expect("acquire");
        let referencing = user_referencing_columns(&mut conn)
            .await
            .expect("enumerate");
        assert!(
            referencing.contains(&("later_table".to_string(), "added_by".to_string())),
            "{referencing:?}"
        );
    }

    /// Regression for issue #152: the household delete path is unchanged — it
    /// removes the household's rows first, so a member's account whose only
    /// references were those rows is still deleted, not deactivated.
    #[tokio::test]
    async fn delete_household_deletes_an_account_whose_history_goes_with_it() {
        let pool = migrated_pool().await;
        let dir =
            std::env::temp_dir().join(format!("buywise-delete-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create receipt dir");
        let now = db::now_iso();

        sqlx::query("INSERT INTO households (id, name, created_at) VALUES ('hh-1', 'Home', ?1)")
            .bind(&now)
            .execute(&pool)
            .await
            .expect("insert household");
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, display_name, created_at) \
             VALUES ('u-1', 'one@example.test', 'hash', 'One', ?1)",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert user");
        sqlx::query(
            "INSERT INTO household_members (household_id, user_id, role, joined_at) \
             VALUES ('hh-1', 'u-1', 'owner', ?1)",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert membership");
        sqlx::query(
            "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
             VALUES ('list-1', 'hh-1', 'Groceries', 'active', 1, ?1)",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert list");
        sqlx::query(
            "INSERT INTO list_entries (id, list_id, resolution_status, raw_text, display_name, \
                 added_by, status, effective_constraint_hash, created_at, updated_at) \
             VALUES ('e-1', 'list-1', 'unresolved', 'milk', 'Milk', 'u-1', 'active', 'default', ?1, ?1)",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert entry");
        sqlx::query(
            "INSERT INTO receipt_images (id, household_id, uploaded_by, uploaded_at, expires_at, \
                 content_type, byte_size, sha256_hex, storage_key) \
             VALUES ('ri-1', 'hh-1', 'u-1', ?1, '2999-01-01T00:00:00Z', 'image/png', 4, 'aa', '00112233445566778899aabbccddeeff.png')",
        )
        .bind(&now)
        .execute(&pool)
        .await
        .expect("insert receipt image");
        receipts::write_file(&dir, "00112233445566778899aabbccddeeff.png", b"img!")
            .expect("write receipt file");

        let counts = delete_household(&pool, "hh-1", false, &dir)
            .await
            .expect("delete household");
        let reported: std::collections::BTreeMap<&str, u64> = counts.into_iter().collect();
        assert_eq!(
            reported["users"], 1,
            "the orphaned account is still deleted"
        );
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = 'u-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
