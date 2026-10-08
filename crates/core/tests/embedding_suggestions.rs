//! Integration tests for the embedding suggestion store and confirmation path.
//!
//! Everything here uses invented fixtures and hand-written "vectors" (3
//! dimensions) through a fake [`Embedder`]; no model is loaded and no real data
//! is involved. The tests cover the three contracts the feature must keep:
//!
//!   1. ranking against an invented catalogue orders candidates by cosine and
//!      returns the top five;
//!   2. a line with no stored vector falls back to an empty candidate list (the
//!      batch step has not run), never to a guess;
//!   3. nothing is bound without an explicit confirmation — embedding and
//!      listing write no alias and no `canonical_item_id`.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use buywise_core::db::{connect, run_migrations};
use buywise_core::embedding::{
    self, CommandEmbedder, ConfirmError, EmbedError, EmbedKind, Embedder, EmbeddingConfig,
};
use sqlx::SqlitePool;

const HH: &str = "hh-1";
const MODEL: &str = "test-model";
const DIM: usize = 3;

/// A deterministic fake embedder: a fixed vector per known text, and a
/// directionally-distinct default so any name still produces a valid vector.
struct FakeEmbedder {
    vectors: HashMap<String, Vec<f32>>,
    enabled: bool,
}

impl FakeEmbedder {
    fn new() -> Self {
        let mut vectors = HashMap::new();
        // Catalogue strings.
        vectors.insert("Eggs".to_string(), vec![1.0, 0.0, 0.0]);
        vectors.insert("Huevos".to_string(), vec![0.98, 0.10, 0.0]);
        vectors.insert("eggs".to_string(), vec![0.97, 0.12, 0.0]);
        vectors.insert("Milk".to_string(), vec![0.0, 1.0, 0.0]);
        vectors.insert("Leche".to_string(), vec![0.0, 0.99, 0.0]);
        vectors.insert("Bread".to_string(), vec![0.0, 0.0, 1.0]);
        // Line names.
        vectors.insert(
            "Tesco Free Range Eggs Large Box Of 12".to_string(),
            vec![1.0, 0.05, 0.0],
        );
        vectors.insert("Own Brand Milk 2L".to_string(), vec![0.02, 1.0, 0.0]);
        FakeEmbedder {
            vectors,
            enabled: true,
        }
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn model_id(&self) -> String {
        MODEL.to_string()
    }

    fn dim(&self) -> usize {
        DIM
    }

    async fn embed(&self, _kind: EmbedKind, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                self.vectors
                    .get(text)
                    .cloned()
                    // A distinct direction for unknown texts so every name still
                    // yields a valid vector.
                    .unwrap_or_else(|| vec![0.2, 0.2, 0.2])
            })
            .collect())
    }
}

async fn setup() -> SqlitePool {
    let pool = connect("sqlite::memory:").await.expect("connect in-memory");
    run_migrations(&pool).await.expect("run migrations");
    sqlx::query(
        "INSERT INTO households (id, name, created_at) VALUES (?1, 'H', '2026-01-01T00:00:00Z')",
    )
    .bind(HH)
    .execute(&pool)
    .await
    .expect("household");
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES ('cat', 'Food', 'Comida', 'x', 1)",
    )
    .execute(&pool)
    .await
    .expect("category");
    for (id, en, es) in [
        ("ci-eggs", "Eggs", "Huevos"),
        ("ci-milk", "Milk", "Leche"),
        ("ci-bread", "Bread", "Pan"),
    ] {
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'cat', 'any', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(HH)
        .bind(en)
        .bind(es)
        .execute(&pool)
        .await
        .expect("canonical item");
    }
    sqlx::query(
        "INSERT INTO item_aliases \
             (id, household_id, canonical_item_id, alias, language, source, created_at) \
         VALUES ('al-1', ?1, 'ci-eggs', 'eggs', 'en', 'manual', '2026-01-01T00:00:00Z')",
    )
    .bind(HH)
    .execute(&pool)
    .await
    .expect("alias");
    pool
}

/// Insert one imported transaction with the given lines. Each line is
/// `(id, name, line_total_cents)`, stored `unmatched`.
async fn seed_transaction(pool: &SqlitePool, tx_id: &str, date: &str, lines: &[(&str, &str, i64)]) {
    sqlx::query(
        "INSERT INTO purchase_transactions \
             (id, household_id, retailer, purchase_date, source_kind, source_transaction_id, \
              transaction_identity, identity_kind, fingerprint, currency, total_cents, revision, \
              source_payload, created_at, updated_at) \
         VALUES (?1, ?2, 'tesco', ?3, 'import', ?1, ?1, 'source_id', ?1, 'EUR', 100, 1, '{}', \
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(tx_id)
    .bind(HH)
    .bind(date)
    .execute(pool)
    .await
    .expect("transaction");
    for (ordinal, (id, name, total)) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO purchase_lines \
                 (id, transaction_id, line_ordinal, original_text, name, line_total_cents, \
                  match_status, revision, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, 'unmatched', 1, '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(tx_id)
        .bind(ordinal as i64)
        .bind(name)
        .bind(total)
        .execute(pool)
        .await
        .expect("line");
    }
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[test]
fn ranking_returns_the_top_five_and_prefers_the_best_string_per_item() {
    // Two strings for the same item: the max cosine must win, and the item must
    // appear once.
    let catalogue = vec![
        ("ci-eggs".to_string(), vec![1.0f32, 0.0, 0.0]),
        ("ci-eggs".to_string(), vec![0.5f32, 0.5, 0.0]),
        ("ci-milk".to_string(), vec![0.0f32, 1.0, 0.0]),
        ("ci-bread".to_string(), vec![0.0f32, 0.0, 1.0]),
    ];
    let ranked = embedding::rank_candidates(&[1.0, 0.1, 0.0], &catalogue, 5);
    assert_eq!(ranked[0].canonical_item_id, "ci-eggs");
    assert_eq!(ranked.len(), 3, "an item contributes one candidate");
    assert!(ranked[0].score > ranked[1].score);
}

#[test]
fn ranking_is_empty_without_a_usable_query_or_catalogue() {
    assert!(embedding::rank_candidates(&[1.0, 0.0, 0.0], &[], 5).is_empty());
    assert!(embedding::rank_candidates(&[], &[("ci".to_string(), vec![1.0])], 5).is_empty());
    // A dimension mismatch is skipped, not scored.
    assert!(
        embedding::rank_candidates(&[1.0, 0.0], &[("ci".to_string(), vec![1.0, 0.0, 0.0])], 5)
            .is_empty()
    );
}

#[test]
fn vector_codec_round_trips() {
    let vector = vec![0.0f32, 1.5, -2.25];
    let bytes = embedding::encode_vector(&vector);
    assert_eq!(bytes.len(), 12);
    assert_eq!(embedding::decode_vector(&bytes), Some(vector));
    assert_eq!(embedding::decode_vector(&[0, 1, 2]), None);
}

#[tokio::test]
async fn suggestions_rank_the_invented_catalogue_and_never_bind_before_confirmation() {
    let pool = setup().await;
    seed_transaction(
        &pool,
        "tx-1",
        "2026-02-01",
        &[("ln-eggs", "Tesco Free Range Eggs Large Box Of 12", 329)],
    )
    .await;
    // A second transaction with the same line name, to prove one confirmation
    // binds every unmatched line that shares the name.
    seed_transaction(
        &pool,
        "tx-2",
        "2026-02-02",
        &[("ln-eggs-2", "Tesco Free Range Eggs Large Box Of 12", 349)],
    )
    .await;

    let embedder = FakeEmbedder::new();
    let catalogue = embedding::embed_catalogue(&pool, HH, &embedder)
        .await
        .expect("embed catalogue");
    // 3 items x 2 names + 1 alias.
    assert_eq!(catalogue.embedded, 7);
    let names = embedding::embed_unmatched_names(&pool, HH, &embedder)
        .await
        .expect("embed names");
    assert_eq!(names.embedded, 1);

    let suggestions = embedding::unmatched_suggestions(&pool, HH, MODEL, 5, 50, 0)
        .await
        .expect("suggestions");
    assert_eq!(suggestions.len(), 2);
    let first = &suggestions[0];
    assert_eq!(first.name, "Tesco Free Range Eggs Large Box Of 12");
    assert_eq!(first.candidates.len(), 3);
    assert_eq!(first.candidates[0].canonical_item_id, "ci-eggs");
    assert_eq!(first.candidates[0].name_en, "Eggs");
    assert!(first.candidates[0].score > first.candidates[1].score);

    // Listing and embedding must not bind anything or learn an alias.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM item_aliases WHERE source = 'accepted_match'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM purchase_lines WHERE canonical_item_id IS NOT NULL"
        )
        .await,
        0
    );

    let outcome = embedding::confirm_suggestion(&pool, HH, "ln-eggs", "ci-eggs")
        .await
        .expect("confirm");
    assert_eq!(outcome.canonical_item_id, "ci-eggs");
    assert_eq!(outcome.bound_lines, 2);

    // The confirmation learns the alias through the shared write path...
    let alias: String = sqlx::query_scalar(
        "SELECT alias FROM item_aliases WHERE canonical_item_id = 'ci-eggs' AND source = 'accepted_match'",
    )
    .fetch_one(&pool)
    .await
    .expect("learned alias");
    assert_eq!(alias, "tesco free range eggs large box of 12");
    // ...and binds both lines.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM purchase_lines \
             WHERE canonical_item_id = 'ci-eggs' AND match_status = 'matched'",
        )
        .await,
        2
    );
}

#[tokio::test]
async fn a_name_without_vectors_falls_back_to_an_empty_candidate_list() {
    let pool = setup().await;
    seed_transaction(
        &pool,
        "tx-1",
        "2026-02-01",
        &[("ln-eggs", "Tesco Free Range Eggs Large Box Of 12", 329)],
    )
    .await;
    let embedder = FakeEmbedder::new();
    embedding::embed_catalogue(&pool, HH, &embedder)
        .await
        .expect("embed catalogue");

    // The name vectors were never embedded for this model, so there are no
    // suggestions — never a guess.
    let suggestions = embedding::unmatched_suggestions(&pool, HH, MODEL, 5, 50, 0)
        .await
        .expect("suggestions");
    assert_eq!(suggestions.len(), 1);
    assert!(suggestions[0].candidates.is_empty());

    // The same is true for a model with no catalogue vectors at all.
    let other = embedding::unmatched_suggestions(&pool, HH, "unknown-model", 5, 50, 0)
        .await
        .expect("suggestions");
    assert!(other[0].candidates.is_empty());
}

#[tokio::test]
async fn embedding_is_idempotent_and_disabled_without_configuration() {
    let pool = setup().await;
    seed_transaction(
        &pool,
        "tx-1",
        "2026-02-01",
        &[("ln-eggs", "Tesco Free Range Eggs Large Box Of 12", 329)],
    )
    .await;
    let embedder = FakeEmbedder::new();
    embedding::embed_catalogue(&pool, HH, &embedder)
        .await
        .expect("first");
    let again = embedding::embed_catalogue(&pool, HH, &embedder)
        .await
        .expect("second");
    assert_eq!(again.embedded, 0, "a re-run embeds only what is new");
    assert_eq!(again.total, 7);

    // A disabled embedder refuses before doing any work.
    let disabled = FakeEmbedder {
        vectors: HashMap::new(),
        enabled: false,
    };
    assert!(matches!(
        embedding::embed_catalogue(&pool, HH, &disabled).await,
        Err(EmbedError::Disabled)
    ));
}

/// Write a throwaway shell script that speaks the embedder protocol, returning
/// its path. A fixture command, not a model.
fn write_script(body: &str) -> std::path::PathBuf {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("bw-embed-{}.sh", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(body.as_bytes()).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

fn command_embedder(path: &std::path::Path) -> CommandEmbedder {
    CommandEmbedder::new(EmbeddingConfig {
        enabled: true,
        command: Some(path.to_string_lossy().to_string()),
        model: MODEL.to_string(),
        model_path: None,
        dim: DIM,
        timeout: Duration::from_secs(30),
    })
}

#[tokio::test]
async fn command_embedder_round_trips_the_json_protocol() {
    let script = write_script(
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"dim\":3,\"vectors\":[[1,0,0],[0,1,0]]}'\n",
    );
    let embedder = command_embedder(&script);
    let vectors = embedder
        .embed(EmbedKind::Query, &["a".to_string(), "b".to_string()])
        .await
        .expect("embed");
    assert_eq!(vectors, vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]);
    let _ = std::fs::remove_file(&script);
}

#[tokio::test]
async fn command_embedder_refuses_a_shape_mismatch_and_a_disabled_config() {
    let script =
        write_script("#!/bin/sh\ncat >/dev/null\nprintf '{\"dim\":2,\"vectors\":[[1,0]]}'\n");
    let embedder = command_embedder(&script);
    assert!(matches!(
        embedder.embed(EmbedKind::Query, &["a".to_string()]).await,
        Err(EmbedError::Shape { .. })
    ));
    let _ = std::fs::remove_file(&script);

    let disabled = CommandEmbedder::new(EmbeddingConfig {
        enabled: false,
        command: None,
        model: MODEL.to_string(),
        model_path: None,
        dim: DIM,
        timeout: Duration::from_secs(30),
    });
    assert!(!disabled.is_enabled());
    assert!(matches!(
        disabled.embed(EmbedKind::Query, &["a".to_string()]).await,
        Err(EmbedError::Disabled)
    ));
}

#[tokio::test]
async fn confirmation_refuses_unknown_lines_and_items() {
    let pool = setup().await;
    seed_transaction(
        &pool,
        "tx-1",
        "2026-02-01",
        &[("ln-eggs", "Tesco Free Range Eggs Large Box Of 12", 329)],
    )
    .await;
    assert!(matches!(
        embedding::confirm_suggestion(&pool, HH, "nope", "ci-eggs").await,
        Err(ConfirmError::NoSuchLine)
    ));
    assert!(matches!(
        embedding::confirm_suggestion(&pool, HH, "ln-eggs", "ci-nope").await,
        Err(ConfirmError::NoSuchItem)
    ));
}
