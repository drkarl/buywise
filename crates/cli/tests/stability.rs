//! Stability test: the generator must be deterministic.
//!
//! Runs `generate` twice into separate temp output directories (reading the same
//! committed `fixtures/dictionary.json`) and asserts the produced
//! `canonical_items.json` and `002_seed.sql` are byte-identical between runs.
//! This proves the deterministic ID derivation, fixed timestamp, and stable
//! output ordering.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use buywise_cli::generate::generate;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "buywise-stability-{}-{tag}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn generator_output_is_byte_identical_across_runs() {
    let dict = workspace_root().join("fixtures").join("dictionary.json");

    let dir_a = unique_temp_dir("a");
    let dir_b = unique_temp_dir("b");

    let items_a = dir_a.join("canonical_items.json");
    let sql_a = dir_a.join("002_seed.sql");
    let items_b = dir_b.join("canonical_items.json");
    let sql_b = dir_b.join("002_seed.sql");

    generate(&dict, &items_a, &sql_a).expect("first generation");
    generate(&dict, &items_b, &sql_b).expect("second generation");

    let items_bytes_a = std::fs::read(&items_a).expect("read items a");
    let items_bytes_b = std::fs::read(&items_b).expect("read items b");
    let sql_bytes_a = std::fs::read(&sql_a).expect("read sql a");
    let sql_bytes_b = std::fs::read(&sql_b).expect("read sql b");

    assert_eq!(
        items_bytes_a, items_bytes_b,
        "canonical_items.json must be byte-identical across runs"
    );
    assert_eq!(
        sql_bytes_a, sql_bytes_b,
        "002_seed.sql must be byte-identical across runs"
    );

    // Cleanup (temp dirs only — verified to live under the system temp dir).
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn committed_artifacts_match_a_fresh_generation() {
    // Regenerating into a temp dir must reproduce the committed fixtures/migration
    // byte-for-byte; otherwise the repo is out of date with dictionary.json.
    let root = workspace_root();
    let dict = root.join("fixtures").join("dictionary.json");

    let dir = unique_temp_dir("committed");
    let items_tmp = dir.join("canonical_items.json");
    let sql_tmp = dir.join("002_seed.sql");
    generate(&dict, &items_tmp, &sql_tmp).expect("generation");

    let committed_items =
        std::fs::read(root.join("fixtures").join("canonical_items.json")).expect("committed items");
    let fresh_items = std::fs::read(&items_tmp).expect("fresh items");
    assert_eq!(
        committed_items, fresh_items,
        "committed fixtures/canonical_items.json is stale; run `cargo run -p buywise-cli -- generate-fixtures`"
    );

    let committed_sql =
        std::fs::read(root.join("migrations").join("002_seed.sql")).expect("committed sql");
    let fresh_sql = std::fs::read(&sql_tmp).expect("fresh sql");
    assert_eq!(
        committed_sql, fresh_sql,
        "committed migrations/002_seed.sql is stale; run `cargo run -p buywise-cli -- generate-fixtures`"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
