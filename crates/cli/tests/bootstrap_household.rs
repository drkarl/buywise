//! Integration tests for `bootstrap-household` (issue #139, roadmap #114).
//!
//! Exercises the whole path the command runs — create the database file, apply
//! every migration, then create the household, the owner account and the `owner`
//! membership — against a real temporary database file, without a TTY (the
//! password is passed straight to [`bootstrap`], which is the piece the prompt
//! feeds).

use std::future::Future;

use buywise_core::db;
use sqlx::SqlitePool;
use tempfile::TempDir;

/// Run one async helper to completion on a small current-thread runtime. The test
/// bodies call it between steps, so a runtime is never nested.
fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
        .block_on(fut)
}

#[test]
fn bootstrap_creates_the_household_owner_and_a_signable_password() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("buywise.db");
    assert!(!db_path.exists(), "the file starts missing");
    let db_path = db_path.to_str().unwrap();

    let created = block_on(buywise_cli::commands::bootstrap::bootstrap(
        db_path,
        "Our Home",
        "owner@example.test",
        "Owner",
        "owner-password",
    ))
    .expect("bootstrap");
    assert_eq!(created.owner.role, "owner");

    // The command creates the file and migrates it.
    assert!(std::path::Path::new(db_path).exists(), "database created");

    let pool: SqlitePool = block_on(db::connect(&format!("sqlite://{db_path}"))).expect("reopen");

    // The household row carries the requested name.
    let name: String = block_on(
        sqlx::query_scalar("SELECT name FROM households WHERE id = ?1")
            .bind(&created.household_id)
            .fetch_one(&pool),
    )
    .expect("household row");
    assert_eq!(name, "Our Home");

    // The owner is the household's single, `owner`-role member.
    let (role, user_id): (String, String) = block_on(
        sqlx::query_as("SELECT role, user_id FROM household_members WHERE household_id = ?1")
            .bind(&created.household_id)
            .fetch_one(&pool),
    )
    .expect("membership row");
    assert_eq!(role, "owner");
    assert_eq!(user_id, created.owner.user_id);

    // ...and their stored hash verifies against the password the operator typed.
    let hash: String = block_on(
        sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?1")
            .bind("owner@example.test")
            .fetch_one(&pool),
    )
    .expect("user row");
    assert!(buywise_core::password::verify_password(
        "owner-password",
        &hash
    ));
    assert!(!buywise_core::password::verify_password("wrong", &hash));
}

#[test]
fn bootstrap_refuses_a_database_that_already_has_an_account() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("buywise.db");
    let db_path = db_path.to_str().unwrap();

    block_on(buywise_cli::commands::bootstrap::bootstrap(
        db_path,
        "First",
        "first@example.test",
        "First",
        "pw",
    ))
    .expect("first bootstrap");

    // A second run finds an account already present and refuses, changing nothing.
    let err = block_on(buywise_cli::commands::bootstrap::bootstrap(
        db_path,
        "Second",
        "second@example.test",
        "Second",
        "pw",
    ))
    .expect_err("a second bootstrap must be refused");
    assert!(
        err.to_string().contains("already exists"),
        "clear refusal message, got: {err}"
    );

    let pool: SqlitePool = block_on(db::connect(&format!("sqlite://{db_path}"))).expect("reopen");
    let users: i64 = block_on(sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&pool))
        .expect("count users");
    assert_eq!(users, 1, "the refused run must not create a second owner");
    let households: i64 =
        block_on(sqlx::query_scalar("SELECT COUNT(*) FROM households").fetch_one(&pool))
            .expect("count households");
    assert_eq!(
        households, 2,
        "the seeded default household plus the bootstrapped one"
    );
}

#[test]
fn bootstrap_refuses_blank_owner_email_or_display_name() {
    for (email, display_name) in [(" \t ", "Owner"), ("owner@example.test", " \n ")] {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("buywise.db");
        let db_path = db_path.to_str().unwrap();

        let err = block_on(buywise_cli::commands::bootstrap::bootstrap(
            db_path,
            "Our Home",
            email,
            display_name,
            "owner-password",
        ))
        .expect_err("blank owner fields must be refused");
        assert!(err.to_string().contains("cannot be blank"), "got: {err}");

        let pool: SqlitePool = block_on(db::connect(&format!("sqlite://{db_path}")))
            .expect("reopen migrated database");
        let users: i64 =
            block_on(sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&pool))
                .expect("count users");
        assert_eq!(users, 0, "no owner account is created");
        let household_count: i64 =
            block_on(sqlx::query_scalar("SELECT COUNT(*) FROM households").fetch_one(&pool))
                .expect("count households");
        assert_eq!(household_count, 1, "only the seeded household remains");
    }
}
