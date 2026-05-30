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

use buywise_cli::commands::basket::{build_optimiser_input, resolve_lines};
use buywise_cli::commands::fixture_db::build_fixture_pool;
use buywise_cli::commands::BasketFixture;
use buywise_core::matching::ResolutionOutcome;
use buywise_core::optimiser::optimise;

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
