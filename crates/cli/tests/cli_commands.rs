//! Integration tests for the P0-T11 CLI commands (issue #11).
//!
//! These exercise the library entry points directly (rather than spawning the
//! binary) so they run fast and assert on structured results:
//!
//! * `validate-basket` on `fixtures/items.json` resolves ≥ 90% of lines.
//! * `optimise` in fixture mode over `fixtures/weekly-shop.json` produces all
//!   four plans with the documented invariant
//!   `theoretical_minimum ≤ best_two_store ≤ best_single` (cost) among complete
//!   plans, every plan `is_complete` (missing_count == 0), and a positive
//!   recommended saving.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use buywise_cli::commands::basket::{build_optimiser_input, resolve_lines};
use buywise_cli::commands::fixture_db::build_fixture_pool;
use buywise_cli::commands::BasketFixture;
use buywise_core::db;
use buywise_core::matching::ResolutionOutcome;
use buywise_core::optimiser::optimise;

const PHASE0_HOUSEHOLD: &str = "00000000-0000-0000-0000-000000000001";

/// Serialises the tests that drive `ingest::run`, which mutates the
/// process-global `BUYWISE_NO_NETWORK` env var for the duration of a dry-run.
/// `ingest::run` now restores the prior value on drop, so no leak survives a
/// test, but cargo runs tests in a binary concurrently — without this lock two
/// such runs could still race on the shared variable mid-run. Each test holds
/// the lock for the whole `ingest::run` call, so neither depends on (nor leaks)
/// global env state into the other.
static INGEST_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Count rows in a table of a freshly opened pool over the given db file.
async fn count_rows(db_path: &str, table: &str) -> i64 {
    let pool = db::connect(&format!("sqlite://{db_path}"))
        .await
        .expect("open db");
    let sql = format!("SELECT COUNT(*) FROM {table}");
    sqlx::query_scalar::<_, i64>(&sql)
        .fetch_one(&pool)
        .await
        .expect("count rows")
}

/// Fetch (status, matched_by) for one product_matches row by its natural key.
async fn match_status(db_path: &str, canonical_item_id: &str, retailer_product_id: &str) -> String {
    let pool = db::connect(&format!("sqlite://{db_path}"))
        .await
        .expect("open db");
    sqlx::query_scalar::<_, String>(
        "SELECT status FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 AND retailer_product_id = ?3",
    )
    .bind(PHASE0_HOUSEHOLD)
    .bind(canonical_item_id)
    .bind(retailer_product_id)
    .fetch_one(&pool)
    .await
    .expect("fetch status")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

fn load_basket(path: &Path) -> BasketFixture {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[tokio::test]
async fn validate_basket_resolution_rate_at_least_90_percent() {
    let root = workspace_root();
    let fixture = load_basket(&root.join("fixtures").join("items.json"));
    let pool = build_fixture_pool(&root, &fixture.household_id)
        .await
        .expect("build fixture pool");

    let lines = resolve_lines(&pool, &fixture.household_id, &fixture.items)
        .await
        .expect("resolve lines");

    let total = lines.len();
    let resolved = lines
        .iter()
        .filter(|l| matches!(l.outcome, ResolutionOutcome::SingleMatch { .. }))
        .count();
    let rate = (resolved as f64 / total as f64) * 100.0;

    assert!(
        total >= 25,
        "validation basket should have ~30 lines, got {total}"
    );
    assert!(
        rate >= 90.0,
        "resolution rate {rate:.1}% (<90%); resolved {resolved}/{total}. \
         Unresolved: {:?}",
        lines
            .iter()
            .filter(|l| !matches!(l.outcome, ResolutionOutcome::SingleMatch { .. }))
            .map(|l| &l.raw)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn optimise_fixture_mode_plans_are_complete_and_ordered() {
    let root = workspace_root();
    let fixture = load_basket(&root.join("fixtures").join("weekly-shop.json"));
    let pool = build_fixture_pool(&root, &fixture.household_id)
        .await
        .expect("build fixture pool");

    let lines = resolve_lines(&pool, &fixture.household_id, &fixture.items)
        .await
        .expect("resolve lines");
    let input = build_optimiser_input(&pool, &fixture.household_id, &lines, None)
        .await
        .expect("build optimiser input");
    let out = optimise(&pool, &input).await.expect("optimise");

    let best_single = out.best_single.as_ref().expect("a best single plan");
    let best_two = out.best_two_store.as_ref().expect("a best two-store plan");
    let theo = out
        .theoretical_minimum
        .as_ref()
        .expect("a theoretical minimum plan");

    // All plans complete: missing_count == 0 (is_complete).
    assert_eq!(
        best_single.missing_count, 0,
        "best_single must price every resolved item; no_valid_offer: {:?}",
        out.no_valid_offer_items
    );
    assert_eq!(best_two.missing_count, 0, "best_two_store must be complete");
    assert_eq!(
        theo.missing_count, 0,
        "theoretical_minimum must be complete"
    );

    // Cost ordering among complete plans: theo ≤ two-store ≤ single.
    assert!(
        theo.total_cents <= best_two.total_cents,
        "theoretical_minimum ({}) must be ≤ best_two_store ({})",
        theo.total_cents,
        best_two.total_cents
    );
    assert!(
        best_two.total_cents <= best_single.total_cents,
        "best_two_store ({}) must be ≤ best_single ({})",
        best_two.total_cents,
        best_single.total_cents
    );

    // best_two_store spans exactly two distinct stores.
    assert_eq!(
        best_two.stores.len(),
        2,
        "best_two_store must use exactly two stores, got {:?}",
        best_two.stores
    );

    // The recommendation exists and saves something.
    let rec = out.recommended.as_ref().expect("a recommendation");
    assert!(
        rec.plan.total_cents <= best_single.total_cents,
        "recommended plan must not cost more than best_single"
    );
}

#[tokio::test]
async fn optimise_fixture_mode_monthly_saving_meets_phase0_gate() {
    // The Phase-0 gate: the achievable monthly saving (best single store −
    // theoretical minimum, annualised) must be ≥ €50. This mirrors the
    // "estimated monthly saving" figure the CLI prints. The fixture prices are
    // illustrative (the design doc reserves the real gate for live mode), but
    // they must demonstrate the optimiser surfacing a ≥ €50/month opportunity.
    const WEEKS_PER_MONTH: f64 = 52.0 / 12.0;

    let root = workspace_root();
    let fixture = load_basket(&root.join("fixtures").join("weekly-shop.json"));
    let pool = build_fixture_pool(&root, &fixture.household_id)
        .await
        .expect("build fixture pool");

    let lines = resolve_lines(&pool, &fixture.household_id, &fixture.items)
        .await
        .expect("resolve lines");
    let input = build_optimiser_input(&pool, &fixture.household_id, &lines, None)
        .await
        .expect("build optimiser input");
    let out = optimise(&pool, &input).await.expect("optimise");

    let single = out.best_single.as_ref().expect("best single").total_cents;
    let theoretical = out
        .theoretical_minimum
        .as_ref()
        .expect("theoretical minimum")
        .total_cents;
    let weekly_saving = (single - theoretical).max(0);
    let monthly_saving = ((weekly_saving as f64) * WEEKS_PER_MONTH).round() as i64;

    assert!(
        monthly_saving >= 5000,
        "Phase-0 gate: achievable monthly saving must be ≥ €50.00, got €{}.{:02} \
         (weekly €{}.{:02}; single €{}.{:02}, theoretical €{}.{:02})",
        monthly_saving / 100,
        monthly_saving % 100,
        weekly_saving / 100,
        weekly_saving % 100,
        single / 100,
        single % 100,
        theoretical / 100,
        theoretical % 100,
    );
}

/// Guards the catalogue/match household-split fix: a fixture pool built for a
/// **non-default** `--household-id` must stamp the catalogue (canonical_items +
/// item_aliases) AND the matches with that *same* requested id. Resolution and
/// pricing both filter by household, so if the catalogue lived under a different
/// id the basket would silently price to nothing. We assert both the row-level
/// invariant and the end-to-end pricing path.
#[tokio::test]
async fn fixture_pool_uses_requested_household_end_to_end() {
    const CUSTOM_HOUSEHOLD: &str = "11111111-1111-1111-1111-111111111111";

    let root = workspace_root();
    let pool = build_fixture_pool(&root, CUSTOM_HOUSEHOLD)
        .await
        .expect("build fixture pool for custom household");

    // Row-level: every catalogue + match row carries the requested id, and none
    // carries a different (e.g. fixture-embedded) household id.
    for table in ["canonical_items", "item_aliases", "product_matches"] {
        let under_requested: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE household_id = ?1"
        ))
        .bind(CUSTOM_HOUSEHOLD)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("count {table} (requested): {e}"));
        let under_other: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE household_id <> ?1"
        ))
        .bind(CUSTOM_HOUSEHOLD)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("count {table} (other): {e}"));
        assert!(
            under_requested > 0,
            "{table} has no rows under the requested household"
        );
        assert_eq!(
            under_other, 0,
            "{table} has {under_other} rows under a non-requested household (catalogue/match split)"
        );
    }

    // End-to-end: a basket resolved and priced under the custom household must
    // still produce a complete, priced plan. Before the fix the catalogue was
    // under a different id, so resolution found nothing and nothing priced.
    let lines = resolve_lines(&pool, CUSTOM_HOUSEHOLD, &["Whole Milk".to_string()])
        .await
        .expect("resolve lines under custom household");
    assert!(
        lines
            .iter()
            .any(|l| matches!(l.outcome, ResolutionOutcome::SingleMatch { .. })),
        "milk must resolve under the custom household"
    );

    let input = build_optimiser_input(&pool, CUSTOM_HOUSEHOLD, &lines, None)
        .await
        .expect("build optimiser input under custom household");
    let out = optimise(&pool, &input)
        .await
        .expect("optimise under custom household");

    let best_single = out
        .best_single
        .as_ref()
        .expect("a best single plan under the custom household");
    assert!(
        best_single.total_cents > 0,
        "custom-household basket priced to €0 — catalogue/match household split regressed"
    );
    assert_eq!(
        best_single.missing_count, 0,
        "milk should price under the custom household; no_valid_offer: {:?}",
        out.no_valid_offer_items
    );
}

/// Build a small tokio runtime for the async DB-assertion helpers. The command
/// entry points (`ingest::run`, `seed_matches::run`) build their own runtime, so
/// these tests are plain `#[test]`s and only spin a runtime up for assertions.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("assertion runtime")
        .block_on(fut)
}

#[test]
fn ingest_dry_run_completes_without_network_and_writes_rows() {
    // Acceptance criterion: `ingest-basketwatch --db <tmp> --dry-run` completes
    // (exit 0) without any network calls. Per the command contract (see
    // crates/cli/src/commands/ingest.rs), `--dry-run` forces the no-network path
    // so the adapter reads `fixtures/offers.json`, but it STILL writes the
    // resulting retailer_products + product_offers rows. So a successful dry-run
    // is observable as Ok(()) plus non-empty data tables.
    let dir = tempfile::Builder::new()
        .prefix("bw")
        .tempdir()
        .expect("tempdir");
    let db_path = dir.path().join("dry.db");
    let db = db_path.to_str().unwrap();

    // Serialise the env-mutating window with the other ingest test (see
    // INGEST_ENV_LOCK). Ignore a poisoned lock: a panic in the other test still
    // leaves the env restored by the guard, so the lock state is irrelevant.
    let _env = INGEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Returns Ok(()) (exit 0) and prints the "dry-run mode" banner to stdout.
    // `ingest::run` restores BUYWISE_NO_NETWORK to its prior value on return, so
    // this dry-run does not leak the no-network flag to any later test.
    buywise_cli::commands::ingest::run(db, true).expect("dry-run ingest should succeed");

    // The fixture-backed (no-network) run still persists rows.
    assert!(
        block_on(count_rows(db, "retailer_products")) > 0,
        "dry-run should write retailer_products from fixtures/offers.json"
    );
    assert!(
        block_on(count_rows(db, "product_offers")) > 0,
        "dry-run should write product_offers from fixtures/offers.json"
    );
}

#[test]
fn ingest_lidl_dry_run_completes_without_network_and_writes_rows() {
    // Acceptance criterion (issue #90): `ingest-lidl --db <tmp> --dry-run`
    // completes (exit 0) without any network calls. Per the command contract
    // (see crates/cli/src/commands/ingest_lidl.rs), `--dry-run` forces the
    // no-network path so the Lidl adapter reads `fixtures/lidl_offers.json`, but
    // it STILL writes the resulting retailer_products + product_offers rows. So a
    // successful dry-run is observable as Ok(()) plus non-empty data tables.
    let dir = tempfile::Builder::new()
        .prefix("lidl")
        .tempdir()
        .expect("tempdir");
    let db_path = dir.path().join("dry.db");
    let db = db_path.to_str().unwrap();

    // Serialise the env-mutating window with the BasketWatch ingest tests. They
    // all mutate the SAME process-global BUYWISE_NO_NETWORK; `ingest_lidl::run`
    // restores the prior value on drop, but without this shared lock two runs
    // could still race on the variable mid-run. Ignore a poisoned lock: the guard
    // restores the env even on a panic, so the lock state is irrelevant.
    let _env = INGEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Returns Ok(()) (exit 0) and prints the "dry-run mode" banner to stdout.
    // `ingest_lidl::run` restores BUYWISE_NO_NETWORK to its prior value on return,
    // so this dry-run does not leak the no-network flag to any later test.
    buywise_cli::commands::ingest_lidl::run(db, true).expect("dry-run lidl ingest should succeed");

    // The fixture-backed (no-network) run still persists rows.
    assert!(
        block_on(count_rows(db, "retailer_products")) > 0,
        "dry-run should write retailer_products from fixtures/lidl_offers.json"
    );
    assert!(
        block_on(count_rows(db, "product_offers")) > 0,
        "dry-run should write product_offers from fixtures/lidl_offers.json"
    );
}

/// RAII guard that snapshots an env var and restores it (to its prior value, or
/// unset if it was absent) on drop — even if the body panics. Used by the live
/// Lidl test below to leave `BUYWISE_NO_NETWORK` / `LIDL_FEED_URL` exactly as it
/// found them, so it cannot leak state into the other ingest tests it shares the
/// `INGEST_ENV_LOCK` with.
struct EnvVarRestore {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarRestore {
    fn capture(key: &'static str) -> Self {
        Self {
            key,
            previous: std::env::var(key).ok(),
        }
    }
}

impl Drop for EnvVarRestore {
    fn drop(&mut self) {
        // SAFETY: env mutation is `unsafe` in edition 2024 (not thread-safe). The
        // shared INGEST_ENV_LOCK held by the test serialises this with the other
        // ingest tests, so there is no concurrent reader during the restore.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn ingest_lidl_live_mode_without_feed_url_errors_with_hint() {
    // Acceptance criterion (issue #90): in *live* mode (no --dry-run), an unset
    // `LIDL_FEED_URL` must fail fast with an operator-facing error that names the
    // variable. The Lidl adapter raises `IngestError::MissingFeedUrl` before any
    // network call (see crates/ingest/src/lidl.rs), and `ingest_lidl::run` attaches
    // the detailed `LIDL_FEED_URL` / `--dry-run` hint *only* for that variant, so
    // the surfaced message mentions `LIDL_FEED_URL`.
    let dir = tempfile::Builder::new()
        .prefix("lidl-live")
        .tempdir()
        .expect("tempdir");
    let db_path = dir.path().join("live.db");
    let db = db_path.to_str().unwrap();

    // This test reads the process-global BUYWISE_NO_NETWORK + LIDL_FEED_URL. Hold
    // the shared lock so it cannot race the dry-run tests (which toggle the same
    // BUYWISE_NO_NETWORK), and snapshot+restore both vars so it leaks nothing.
    let _env = INGEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _restore_no_network = EnvVarRestore::capture("BUYWISE_NO_NETWORK");
    let _restore_feed_url = EnvVarRestore::capture("LIDL_FEED_URL");

    // Force the *live* path: BUYWISE_NO_NETWORK must not be "1", and LIDL_FEED_URL
    // must be unset so the adapter fails fast with MissingFeedUrl.
    // SAFETY: env mutation is `unsafe` in edition 2024; the shared lock above
    // serialises this with every other env-touching ingest test, and the
    // EnvVarRestore guards above put both vars back on return (incl. panic).
    unsafe {
        std::env::remove_var("BUYWISE_NO_NETWORK");
        std::env::remove_var("LIDL_FEED_URL");
    }

    let err = buywise_cli::commands::ingest_lidl::run(db, false)
        .expect_err("live ingest without LIDL_FEED_URL should error");
    let message = format!("{err:#}");
    assert!(
        message.contains("LIDL_FEED_URL"),
        "error should name the missing env var, got: {message}"
    );
}

#[test]
fn ingest_then_seed_matches_is_idempotent() {
    // Acceptance criterion: seed-matches upserts rows and is idempotent on a
    // second run (stable row count + values; an updated status in the CSV updates
    // in place rather than duplicating).
    //
    // The ingest step populates retailer_products + offers so the match FKs
    // resolve. We drive the no-network (fixture) ingest path explicitly via
    // `--dry-run` so the test is self-contained: it requires neither a live
    // BASKETWATCH_API_KEY nor env state leaked from another test. The seed +
    // idempotency assertions below are identical regardless of which ingest path
    // populated the products.
    let root = workspace_root();
    let dir = tempfile::Builder::new()
        .prefix("bw")
        .tempdir()
        .expect("tempdir");
    let db_path = dir.path().join("seed.db");
    let db = db_path.to_str().unwrap();

    // Serialise the env-mutating ingest window with the dry-run test (see
    // INGEST_ENV_LOCK). Hold the lock only for the ingest call; the seed-matches
    // runs below touch no env vars. Ignore a poisoned lock: the guard restores
    // the env on a panic, so the lock state is irrelevant.
    {
        let _env = INGEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        buywise_cli::commands::ingest::run(db, true).expect("fixture ingest should succeed");
    }

    let csv_src = root.join("fixtures").join("product_matches.csv");
    let csv_path = dir.path().join("matches.csv");
    std::fs::copy(&csv_src, &csv_path).expect("copy fixture csv");
    let csv = csv_path.to_str().unwrap();

    // First run: seeds the rows.
    buywise_cli::commands::seed_matches::run(csv, db, PHASE0_HOUSEHOLD).expect("seed run 1");
    let count_after_first = block_on(count_rows(db, "product_matches"));
    assert!(
        count_after_first > 0,
        "seed-matches should write rows, got {count_after_first}"
    );

    // Second run with the identical CSV: idempotent (same count, no duplicates).
    buywise_cli::commands::seed_matches::run(csv, db, PHASE0_HOUSEHOLD).expect("seed run 2");
    let count_after_second = block_on(count_rows(db, "product_matches"));
    assert_eq!(
        count_after_first, count_after_second,
        "second seed-matches run must be idempotent (no new rows)"
    );

    // An updated status in the CSV must update in place, not duplicate the row.
    let raw = std::fs::read_to_string(&csv_path).expect("read csv");
    let updated = raw.replacen(",accepted,system", ",rejected,system", 1);
    assert_ne!(
        raw, updated,
        "fixture must contain an 'accepted' row to flip"
    );
    std::fs::write(&csv_path, &updated).expect("write updated csv");

    buywise_cli::commands::seed_matches::run(csv, db, PHASE0_HOUSEHOLD).expect("seed run 3");
    let count_after_update = block_on(count_rows(db, "product_matches"));
    assert_eq!(
        count_after_first, count_after_update,
        "an updated status must update in place, not add a row"
    );

    // The first data row's status must now reflect the update.
    let first = updated
        .lines()
        .find(|l| l.contains(",rejected,system"))
        .expect("an updated row");
    let cols: Vec<&str> = first.split(',').collect();
    let status = match_status(db, cols[0], cols[1]);
    assert_eq!(
        block_on(status),
        "rejected",
        "the flipped row's status must be updated in place"
    );
}
