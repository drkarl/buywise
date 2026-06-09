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
//!   * Off a TTY (piped stdin, for scripting), a single line is read from stdin;
//!     no confirmation is required and nothing is echoed.
//!
//! The password is **never** accepted as a plaintext CLI argument and never
//! printed. It is hashed with argon2id via the shared
//! [`buywise_core::password::hash_password_for_storage`] (the same helper the
//! API verifies against) and written with
//! `UPDATE users SET password_hash = ? WHERE email = ?`. An email that matches
//! no user is a hard error and exits non-zero.

use std::io::{IsTerminal, Read};

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::password::hash_password_for_storage;
use sqlx::SqlitePool;

/// Entry point for `buywise-cli set-password <email> --db <db>`.
pub fn run(email: &str, db_path: &str) -> Result<()> {
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

/// Read the new password without echoing it.
///
/// Interactive (stdin is a TTY): prompt twice with a hidden prompt and require
/// the two entries to match. Non-interactive (piped stdin): read one line from
/// stdin and use it verbatim (scripting path); no confirmation, since there is
/// no second stream to compare against. Either way the password is never echoed
/// and an empty password is rejected.
fn read_password() -> Result<String> {
    let password = if std::io::stdin().is_terminal() {
        let first = rpassword::prompt_password("New password: ")
            .context("reading password from terminal")?;
        let confirm = rpassword::prompt_password("Confirm password: ")
            .context("reading password confirmation from terminal")?;
        if first != confirm {
            bail!("passwords did not match");
        }
        first
    } else {
        read_password_from_stdin(&mut std::io::stdin())?
    };

    if password.is_empty() {
        bail!("password must not be empty");
    }
    Ok(password)
}

/// Read a password from a non-TTY reader (the scripting path).
///
/// Takes the first line, trimming a single trailing `\n`/`\r\n` (so
/// `echo secret | buywise-cli …` works) but preserving any other whitespace the
/// caller intended as part of the password. Factored out so it can be unit
/// tested without a real stdin.
fn read_password_from_stdin(reader: &mut impl Read) -> Result<String> {
    let mut buf = String::new();
    reader
        .read_to_string(&mut buf)
        .context("reading password from stdin")?;
    let line = buf.split('\n').next().unwrap_or("");
    Ok(line.strip_suffix('\r').unwrap_or(line).to_string())
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

    #[test]
    fn stdin_reader_takes_first_line_trimming_newline() {
        let mut input = "hunter2\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut input).unwrap(), "hunter2");

        let mut crlf = "hunter2\r\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut crlf).unwrap(), "hunter2");

        // Only the first line is used; a trailing second line is ignored.
        let mut multi = "secret\nignored\n".as_bytes();
        assert_eq!(read_password_from_stdin(&mut multi).unwrap(), "secret");

        // No trailing newline still works.
        let mut bare = "secret".as_bytes();
        assert_eq!(read_password_from_stdin(&mut bare).unwrap(), "secret");
    }
}
