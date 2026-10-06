//! Household membership: roles, shared user creation, and member removal
//! (issue #139, roadmap #114).
//!
//! One place for the membership rules the API and the CLI must agree on, so the
//! bootstrap command and the add-member endpoint cannot drift:
//!
//! * [`ROLE_OWNER`] / [`ROLE_MEMBER`] are the only two roles (migration 003's
//!   `household_members.role`); the creator is an owner and everyone added later
//!   is a member.
//! * [`create_user`] is the one user-creation path (argon2id hash via
//!   [`crate::password`], duplicate email refused); [`add_member`] and
//!   [`bootstrap_household`] both call it.
//! * [`remove_member`] deletes a member's sessions and membership, and their
//!   account only if they belong to no other household — the same account rule,
//!   and the same code
//!   ([`crate::household_data::delete_orphaned_accounts`]), as the household
//!   delete, so a member shared with another household keeps their login.
//! * The last owner of a household can never be removed.
//!
//! These functions expect a database the caller has already migrated (the API
//! migrates at startup; the CLI's `bootstrap-household` runs migrations first)
//! and never run migrations themselves.

use sqlx::{FromRow, SqliteConnection, SqlitePool};

use crate::db;
use crate::password::{hash_password_for_storage, validate_new_password};

/// The household's founding role: may add and remove members and delete the
/// household. Stored in `household_members.role`.
pub const ROLE_OWNER: &str = "owner";
/// Every non-founding member: full use of the household's data, minus member
/// administration and household deletion.
pub const ROLE_MEMBER: &str = "member";

/// Errors produced by the membership operations.
#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    /// A required user field contained only whitespace.
    #[error("{0} cannot be blank")]
    BlankUserField(&'static str),

    /// A `users` row with this email already exists (`users.email` is UNIQUE).
    /// The email is unique across the whole database, so this also covers a
    /// person who already belongs to another household.
    #[error("a user with email {0} already exists")]
    DuplicateEmail(String),

    /// The proposed password was refused by [`validate_new_password`].
    #[error(transparent)]
    Password(#[from] crate::password::PasswordPolicyError),

    /// No household exists with the requested id.
    #[error("no household with id {0}")]
    NoSuchHousehold(String),

    /// The user is not a member of the named household (so cannot be removed).
    #[error("user {user_id} is not a member of household {household_id}")]
    NotAMember {
        /// The user that was targeted.
        user_id: String,
        /// The household that was targeted.
        household_id: String,
    },

    /// Removing this member would leave the household with no owner.
    #[error("household {0} must keep at least one owner")]
    LastOwner(String),

    /// A user already exists, so the database is not fresh.
    #[error("a user already exists in this database; bootstrap-household is only for a fresh database")]
    AlreadyBootstrapped,

    /// Argon2id hashing failed.
    #[error("hashing password: {0}")]
    Hash(String),

    /// A query or transaction failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// One household member as the app lists it. Serialisable so the API can return
/// it directly; it never carries a password hash or a session token.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Member {
    /// `users.id`.
    pub user_id: String,
    /// `users.email`.
    pub email: String,
    /// `users.display_name`.
    pub display_name: String,
    /// `household_members.role` — [`ROLE_OWNER`] or [`ROLE_MEMBER`].
    pub role: String,
    /// `household_members.joined_at` (RFC 3339 UTC).
    pub joined_at: String,
}

/// A household created by [`bootstrap_household`].
#[derive(Debug, Clone)]
pub struct CreatedHousehold {
    /// `households.id` of the new household.
    pub household_id: String,
    /// The founding member, with role [`ROLE_OWNER`].
    pub owner: Member,
}

/// The outcome of [`remove_member`].
///
/// Serialisable so the API can return it directly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemberRemoval {
    /// The removed user's id.
    pub removed_user_id: String,
    /// How many of their session rows were deleted (open sessions stop working
    /// on their next request).
    pub sessions_ended: u64,
    /// Whether their `users` row was deleted (true only when they belonged to no
    /// other household).
    pub account_deleted: bool,
}

/// List a household's members, owners first, then in join order.
///
/// An unknown household yields an empty list; callers that need to distinguish
/// "no such household" check it separately (the API's owner check already
/// implies membership, so the household exists).
pub async fn list_members(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<Vec<Member>, MembershipError> {
    let members = sqlx::query_as::<_, Member>(
        "SELECT hm.user_id, u.email, u.display_name, hm.role, hm.joined_at \
         FROM household_members hm JOIN users u ON u.id = hm.user_id \
         WHERE hm.household_id = ?1 \
         ORDER BY CASE hm.role WHEN 'owner' THEN 0 ELSE 1 END, hm.joined_at, hm.user_id",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await?;
    Ok(members)
}

/// The role `user_id` holds in `household_id`, or `None` when they are not a
/// member. Used by callers that must tell an owner from a plain member.
pub async fn member_role(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
) -> Result<Option<String>, MembershipError> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM household_members WHERE household_id = ?1 AND user_id = ?2",
    )
    .bind(household_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(role)
}

/// Insert a `users` row for `email` with `password` hashed (argon2id via
/// [`crate::password::hash_password_for_storage`]).
///
/// This is the one user-creation path, shared by the CLI bootstrap and the API's
/// add-member endpoint. The password is validated first
/// ([`validate_new_password`]); a duplicate email is reported as
/// [`MembershipError::DuplicateEmail`] from the UNIQUE constraint rather than a
/// prior `SELECT`, so two concurrent creates cannot both pass a check.
pub async fn create_user(
    conn: &mut SqliteConnection,
    user_id: &str,
    email: &str,
    display_name: &str,
    password: &str,
) -> Result<(), MembershipError> {
    if email.trim().is_empty() {
        return Err(MembershipError::BlankUserField("email"));
    }
    if display_name.trim().is_empty() {
        return Err(MembershipError::BlankUserField("display name"));
    }
    validate_new_password(password)?;
    let hash = hash_password_for_storage(password).map_err(|e| MembershipError::Hash(e.to_string()))?;

    let result = sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&hash)
    .bind(display_name)
    .bind(db::now_iso())
    .execute(&mut *conn)
    .await;

    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
            Err(MembershipError::DuplicateEmail(email.to_string()))
        }
        Err(err) => Err(MembershipError::Sqlx(err)),
    }
}

/// Create the very first household and its owner (the `bootstrap-household`
/// command).
///
/// Refuses with [`MembershipError::AlreadyBootstrapped`] when any `users` row
/// already exists, so it is genuinely a one-time command for a fresh database.
/// (Migrations seed a default household with no members — `004_seed_phase1.sql`
/// — so "fresh" is measured by accounts, not by the presence of a household.)
/// The household, the owner's account and the owner membership are written in
/// one transaction.
pub async fn bootstrap_household(
    pool: &SqlitePool,
    household_name: &str,
    email: &str,
    display_name: &str,
    password: &str,
) -> Result<CreatedHousehold, MembershipError> {
    validate_new_password(password)?;

    let household_id = uuid::Uuid::new_v4().to_string();
    let user_id = uuid::Uuid::new_v4().to_string();
    let now = db::now_iso();

    let mut tx = pool.begin().await?;

    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_optional(&mut *tx)
        .await?;
    if existing.is_some() {
        return Err(MembershipError::AlreadyBootstrapped);
    }

    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(&household_id)
        .bind(household_name)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

    create_user(&mut tx, &user_id, email, display_name, password).await?;

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(&household_id)
    .bind(&user_id)
    .bind(ROLE_OWNER)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(CreatedHousehold {
        household_id,
        owner: Member {
            user_id,
            email: email.to_string(),
            display_name: display_name.to_string(),
            role: ROLE_OWNER.to_string(),
            joined_at: now,
        },
    })
}

/// Add a member to an existing household with role [`ROLE_MEMBER`].
///
/// The household must exist ([`MembershipError::NoSuchHousehold`]); the email
/// must be free ([`MembershipError::DuplicateEmail`]); the password must pass
/// [`validate_new_password`]. The account and the membership are written in one
/// transaction, so a failed insert leaves no half-added user.
pub async fn add_member(
    pool: &SqlitePool,
    household_id: &str,
    email: &str,
    display_name: &str,
    password: &str,
) -> Result<Member, MembershipError> {
    validate_new_password(password)?;

    let user_id = uuid::Uuid::new_v4().to_string();
    let now = db::now_iso();

    let mut tx = pool.begin().await?;

    let household: Option<String> =
        sqlx::query_scalar("SELECT id FROM households WHERE id = ?1")
            .bind(household_id)
            .fetch_optional(&mut *tx)
            .await?;
    if household.is_none() {
        return Err(MembershipError::NoSuchHousehold(household_id.to_string()));
    }

    create_user(&mut tx, &user_id, email, display_name, password).await?;

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(household_id)
    .bind(&user_id)
    .bind(ROLE_MEMBER)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Member {
        user_id,
        email: email.to_string(),
        display_name: display_name.to_string(),
        role: ROLE_MEMBER.to_string(),
        joined_at: now,
    })
}

/// Remove `user_id` from `household_id`.
///
/// Deletes their membership and every session row (so their open sessions stop
/// working on the next request), and deletes the account only when they belong
/// to no other household — the same rule as the household delete, reusing
/// [`crate::household_data::delete_accounts_without_membership`].
///
/// Refuses with [`MembershipError::LastOwner`] when the target is the household's
/// only owner, and with [`MembershipError::NotAMember`] when they are not a
/// member at all. All of it runs in one transaction.
pub async fn remove_member(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
) -> Result<MemberRemoval, MembershipError> {
    let mut tx = pool.begin().await?;

    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM household_members WHERE household_id = ?1 AND user_id = ?2",
    )
    .bind(household_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(role) = role else {
        return Err(MembershipError::NotAMember {
            user_id: user_id.to_string(),
            household_id: household_id.to_string(),
        });
    };

    if role == ROLE_OWNER {
        let owners: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM household_members WHERE household_id = ?1 AND role = ?2",
        )
        .bind(household_id)
        .bind(ROLE_OWNER)
        .fetch_one(&mut *tx)
        .await?;
        if owners <= 1 {
            return Err(MembershipError::LastOwner(household_id.to_string()));
        }
    }

    sqlx::query("DELETE FROM household_members WHERE household_id = ?1 AND user_id = ?2")
        .bind(household_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    // A removed member's open sessions stop immediately, and their account is
    // removed only when no other household remains — the same account rule as the
    // household delete, reused from core.
    let candidate = [user_id.to_string()];
    let sessions_ended =
        crate::household_data::delete_sessions_for_users(&mut tx, &candidate).await?;
    let users_removed =
        crate::household_data::delete_orphaned_accounts(&mut tx, &candidate).await?;

    tx.commit().await?;

    Ok(MemberRemoval {
        removed_user_id: user_id.to_string(),
        sessions_ended,
        account_deleted: users_removed > 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        pool
    }

    #[tokio::test]
    async fn bootstrap_creates_a_household_owned_by_its_creator() {
        let pool = pool().await;
        let created = bootstrap_household(&pool, "Our Home", "owner@example.test", "Owner", "pw")
            .await
            .expect("bootstrap");

        assert_eq!(created.owner.role, ROLE_OWNER);
        let role: String = sqlx::query_scalar(
            "SELECT role FROM household_members WHERE household_id = ?1 AND user_id = ?2",
        )
        .bind(&created.household_id)
        .bind(&created.owner.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, ROLE_OWNER);

        // The stored hash verifies against the chosen password.
        let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?1")
            .bind(&created.owner.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(crate::password::verify_password("pw", &hash));
    }

    #[tokio::test]
    async fn bootstrap_refuses_when_an_account_already_exists() {
        let pool = pool().await;
        bootstrap_household(&pool, "First", "a@example.test", "A", "pw")
            .await
            .expect("first bootstrap");

        let err = bootstrap_household(&pool, "Second", "b@example.test", "B", "pw")
            .await
            .expect_err("a second bootstrap must be refused");
        assert!(matches!(err, MembershipError::AlreadyBootstrapped), "{err}");
    }

    #[tokio::test]
    async fn add_member_refuses_a_duplicate_email() {
        let pool = pool().await;
        let created = bootstrap_household(&pool, "Home", "owner@example.test", "Owner", "pw")
            .await
            .unwrap();

        add_member(
            &pool,
            &created.household_id,
            "member@example.test",
            "Member",
            "pw",
        )
        .await
        .expect("first add");

        let err = add_member(
            &pool,
            &created.household_id,
            "member@example.test",
            "Member again",
            "pw",
        )
        .await
        .expect_err("a duplicate email must be refused");
        assert!(matches!(err, MembershipError::DuplicateEmail(_)), "{err}");
    }

    #[tokio::test]
    async fn remove_member_ends_sessions_and_deletes_the_orphaned_account() {
        let pool = pool().await;
        let created = bootstrap_household(&pool, "Home", "owner@example.test", "Owner", "pw")
            .await
            .unwrap();
        let member = add_member(
            &pool,
            &created.household_id,
            "member@example.test",
            "Member",
            "pw",
        )
        .await
        .unwrap();

        // A live session that must stop working when the member is removed.
        sqlx::query(
            "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at) \
             VALUES ('sess-1', ?1, 'hash-1', 'csrf-1', ?2, ?3)",
        )
        .bind(&member.user_id)
        .bind(db::now_iso())
        .bind("2999-01-01T00:00:00Z")
        .execute(&pool)
        .await
        .unwrap();

        let removal = remove_member(&pool, &created.household_id, &member.user_id)
            .await
            .expect("remove");
        assert!(removal.account_deleted);
        assert_eq!(removal.sessions_ended, 1);

        let members: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM household_members WHERE user_id = ?1",
        )
        .bind(&member.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(members, 0);
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = ?1")
            .bind(&member.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 0, "an orphaned account is deleted");
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?1")
            .bind(&member.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0);
    }

    #[tokio::test]
    async fn remove_member_keeps_a_shared_account_but_ends_its_sessions() {
        let pool = pool().await;
        // A plain member of the first household (not its only owner), who also
        // belongs to a second household.
        let first = bootstrap_household(&pool, "First", "owner@example.test", "Owner", "pw")
            .await
            .unwrap();
        let shared = add_member(
            &pool,
            &first.household_id,
            "shared@example.test",
            "Shared",
            "pw",
        )
        .await
        .unwrap();

        let other_household = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Other', ?2)")
            .bind(&other_household)
            .bind(db::now_iso())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO household_members (household_id, user_id, role, joined_at) \
             VALUES (?1, ?2, 'owner', ?3)",
        )
        .bind(&other_household)
        .bind(&shared.user_id)
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .unwrap();

        // A live session that must still end even though the account survives.
        sqlx::query(
            "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at) \
             VALUES ('sess-shared', ?1, 'hash-shared', 'csrf-shared', ?2, ?3)",
        )
        .bind(&shared.user_id)
        .bind(db::now_iso())
        .bind("2999-01-01T00:00:00Z")
        .execute(&pool)
        .await
        .unwrap();

        let removal = remove_member(&pool, &first.household_id, &shared.user_id)
            .await
            .expect("remove shared member");
        assert!(!removal.account_deleted, "a shared account survives");
        assert_eq!(removal.sessions_ended, 1);

        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id = ?1")
            .bind(&shared.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 1);
        let other_membership: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM household_members WHERE household_id = ?1 AND user_id = ?2",
        )
        .bind(&other_household)
        .bind(&shared.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other_membership, 1);
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?1")
            .bind(&shared.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0, "the removed member's sessions must end");
    }

    #[tokio::test]
    async fn remove_member_refuses_the_last_owner() {
        let pool = pool().await;
        let created = bootstrap_household(&pool, "Home", "owner@example.test", "Owner", "pw")
            .await
            .unwrap();

        let err = remove_member(&pool, &created.household_id, &created.owner.user_id)
            .await
            .expect_err("the only owner must not be removable");
        assert!(matches!(err, MembershipError::LastOwner(_)), "{err}");
        let owners: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM household_members WHERE household_id = ?1 AND role = 'owner'",
        )
        .bind(&created.household_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owners, 1);
    }

    #[tokio::test]
    async fn remove_member_refuses_a_non_member() {
        let pool = pool().await;
        let created = bootstrap_household(&pool, "Home", "owner@example.test", "Owner", "pw")
            .await
            .unwrap();

        let err = remove_member(&pool, &created.household_id, "nobody")
            .await
            .expect_err("a non-member must not be removable");
        assert!(matches!(err, MembershipError::NotAMember { .. }), "{err}");
    }
}
