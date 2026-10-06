//! `set-password <email> --db <db>`.
//!
//! Operator tool that seeds (or rotates) `users.password_hash` for an existing
//! user. Split out of issue #13 as issue #51: the auth endpoints verify against
//! `users.password_hash` but there was no supported way to populate it outside
//! the integration tests.
//!
//! Password acquisition:
//!   * On a TTY (interactive), the password is read twice with a hidden prompt
//!     (no echo) and the two entries must match.
//!   * Off a TTY (piped stdin, for scripting), only the first line of stdin is
//!     read and used as the password; no confirmation is required and nothing
//!     is echoed.
//!
//! The prompting and the password policy live in
//! [`super::password_prompt`] / [`buywise_core::password`], shared with
//! `bootstrap-household` so the two commands read and validate identically.
//!
//! The password is **never** accepted as a plaintext CLI argument and never
//! printed. It is hashed with argon2id via the shared
//! [`buywise_core::password::hash_password_for_storage`] (the same helper the
//! API verifies against) and written with
//! `UPDATE users SET password_hash = ? WHERE email = ?`. An email that matches
//! no user is a hard error and exits non-zero.

use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::password::hash_password_for_storage;
use sqlx::SqlitePool;

use super::password_prompt::read_password;

/// Entry point for `buywise-cli set-password <email> --db <db>`.
pub fn run(email: &str, db_path: &str) -> Result<()> {
    // Guard against a mistyped `--db`: `db::connect` uses `create_if_missing`,
    // so a wrong path would otherwise silently create an empty database and the
    // operator would see a misleading "no user with email" instead of "database
    // not found". Check existence up front for a clear, operator-facing error.
    if !Path::new(db_path).exists() {
        bail!("database not found: {db_path}");
    }

    // Acquire the password before touching the database: a mistyped/cancelled
    // prompt should fail fast without opening a connection.
    let password = read_password()?;

    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(email, db_path, &password))
}

async fn run_async(email: &str, db_path: &str, password: &str) -> Result<()> {
    let url = format!("sqlite://{db_path}");
    let pool = db::connect(&url)
        .await
        .with_context(|| format!("opening database {db_path}"))?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    set_user_password(&pool, email, password).await?;

    println!("updated password_hash for {email}");
    Ok(())
}

/// Hash `password` with argon2id and write it to the matching user's
/// `password_hash`, returning an error if no user has that email.
///
/// Pure over an open pool (no terminal I/O) so it is exercised directly by the
/// tests: unknown-email errors, and a successful update stores a valid argon2id
/// PHC string that verifies. The `UPDATE … WHERE email = ?` affects zero rows
/// for an unknown email; we detect that via `rows_affected()` rather than a
/// prior `SELECT`, so the check and the write are one statement.
pub async fn set_user_password(pool: &SqlitePool, email: &str, password: &str) -> Result<()> {
    let hash = hash_password_for_storage(password)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?;

    let result = sqlx::query("UPDATE users SET password_hash = ?1 WHERE email = ?2")
        .bind(&hash)
        .bind(email)
        .execute(pool)
        .await
        .context("updating users.password_hash")?;

    if result.rows_affected() == 0 {
        bail!("no user with email {email:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use buywise_core::password::verify_password;

    /// Build a fresh in-memory pool with the migrations applied and one user
    /// (with a placeholder hash) so the update path has a row to target.
    async fn setup_pool_with_user(email: &str) -> SqlitePool {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, display_name, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind("user-1")
        .bind(email)
        .bind("placeholder-not-a-real-hash")
        .bind("Test User")
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .expect("insert user");
        pool
    }

    #[tokio::test]
    async fn sets_a_valid_argon2id_hash_that_verifies() {
        let email = "alice@example.test";
        let pool = setup_pool_with_user(email).await;

        set_user_password(&pool, email, "correct horse battery staple")
            .await
            .expect("set password");

        let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?1")
            .bind(email)
            .fetch_one(&pool)
            .await
            .expect("read back hash");

        assert!(stored.starts_with("$argon2id$"), "got: {stored}");
        assert!(
            verify_password("correct horse battery staple", &stored),
            "stored hash must verify against the chosen password"
        );
        assert!(
            !verify_password("wrong password", &stored),
            "a different password must not verify"
        );
    }

    #[tokio::test]
    async fn rehashing_replaces_the_previous_hash() {
        let email = "alice@example.test";
        let pool = setup_pool_with_user(email).await;

        set_user_password(&pool, email, "first-password")
            .await
            .expect("set first");
        let first: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?1")
            .bind(email)
            .fetch_one(&pool)
            .await
            .unwrap();

        set_user_password(&pool, email, "second-password")
            .await
            .expect("set second");
        let second: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE email = ?1")
            .bind(email)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_ne!(first, second, "the hash must be replaced");
        assert!(verify_password("second-password", &second));
        assert!(!verify_password("first-password", &second));
    }

    #[tokio::test]
    async fn unknown_email_is_an_error_and_changes_nothing() {
        let pool = setup_pool_with_user("alice@example.test").await;

        let err = set_user_password(&pool, "nobody@example.test", "whatever")
            .await
            .expect_err("unknown email must error");
        assert!(err.to_string().contains("no user with email"), "got: {err}");

        // The existing user's placeholder hash must be untouched.
        let stored: String = sqlx::query_scalar(
            "SELECT password_hash FROM users WHERE email = 'alice@example.test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, "placeholder-not-a-real-hash");
    }
}
