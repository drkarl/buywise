//! `bootstrap-household --name <name> --email <email> [--display-name <name>]
//! --db <db>`.
//!
//! One-time command that creates the very first household and its owner on a
//! fresh database (issue #139, roadmap #114). Before this there was no supported
//! way — short of hand-written `INSERT`s plus `set-password` — to create a
//! household and its first member, so a new deployment had nobody to sign in as.
//!
//! The owner's initial password is read from a hidden prompt (twice on a TTY,
//! once from piped stdin), never taken as a CLI argument, using the same
//! [`super::password_prompt`] as `set-password`. The household, the owner account
//! and the `owner` membership are written through the shared core path
//! ([`buywise_core::members::bootstrap_household`]) that the API's add-member
//! endpoint also uses, so hashing, validation and the role cannot drift.
//!
//! Unlike the data-control commands, this one *creates* a missing database file
//! and runs the migrations: it is the first thing an operator runs on a new
//! server (see `deploy/README.md`).

use anyhow::{Context, Result};
use buywise_core::{db, members};

use super::password_prompt::read_password;

/// Entry point for `buywise-cli bootstrap-household`.
pub fn run(name: &str, email: &str, display_name: Option<&str>, db_path: &str) -> Result<()> {
    let display_name = display_name
        .map(str::to_string)
        .unwrap_or_else(|| default_display_name(email));

    // Acquire the password before touching the database, so a cancelled prompt
    // leaves no half-created database file.
    let password = read_password()?;

    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(name, email, &display_name, db_path, &password))
}

async fn run_async(
    name: &str,
    email: &str,
    display_name: &str,
    db_path: &str,
    password: &str,
) -> Result<()> {
    let created = bootstrap(db_path, name, email, display_name, password).await?;
    println!("created household {} ({name})", created.household_id);
    println!(
        "owner: {} <{}>",
        created.owner.display_name, created.owner.email
    );
    println!(
        "sign in with that email and the password you entered, then add the rest \
         of the household from Settings → Household members."
    );
    Ok(())
}

/// Create the household and owner in `db_path`, applying the migrations first
/// (creating the database file if it does not exist).
///
/// Pure over the path and the already-read password — no terminal I/O — so tests
/// exercise the whole path (create the file, migrate, bootstrap) without a TTY.
pub async fn bootstrap(
    db_path: &str,
    name: &str,
    email: &str,
    display_name: &str,
    password: &str,
) -> Result<members::CreatedHousehold> {
    let pool = db::connect(&format!("sqlite://{db_path}"))
        .await
        .with_context(|| format!("opening database {db_path}"))?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;
    let created = members::bootstrap_household(&pool, name, email, display_name, password).await?;
    Ok(created)
}

/// The owner's display name when `--display-name` is omitted: the local part of
/// the email, so a minimal `--name … --email …` invocation still records
/// something readable. (The display name is never used for authentication.)
fn default_display_name(email: &str) -> String {
    match email.split('@').next() {
        Some(local) if !local.is_empty() => local.to_string(),
        _ => email.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_display_name_uses_the_email_local_part() {
        assert_eq!(default_display_name("alice@example.test"), "alice");
        assert_eq!(default_display_name("no-at-sign"), "no-at-sign");
        assert_eq!(default_display_name("@example.test"), "@example.test");
    }
}
