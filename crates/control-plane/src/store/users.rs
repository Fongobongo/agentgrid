//! Local user storage (Stage 4.1). Extracted from `store.rs`.

use super::{now_iso, Store};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::PasswordHasher;
    use argon2::Argon2;
    // password-hash 0.6: the salt is generated internally (getrandom).
    let hash = Argon2::default()
        .hash_password(password.as_bytes())?
        .to_string();
    Ok(hash)
}

/// Verify a password against an Argon2id hash string (Stage 4.1).
fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::PasswordVerifier;
    use argon2::Argon2;
    use password_hash::phc::PasswordHash;
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A hash to verify against when the username does not exist (finding 12).
///
/// Returning early for an unknown user made the response measurably faster
/// than a wrong password for a real one — a username oracle that no amount of
/// generic error messages hides. Hashing this constant once at first use keeps
/// the cost identical on both paths (same `Argon2::default()` parameters, so
/// the parameters cannot drift away from the real hashes).
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("agentgrid-timing-equaliser")
            .expect("hash the constant dummy password (cannot fail short of OOM)")
    })
}

impl Store {
    /// Number of local users (0 means the install is in its open bootstrap window).
    pub async fn user_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    /// Create a local user. Returns false if the username already exists.
    pub async fn create_user(&self, username: &str, password: &str, role: &str) -> Result<bool> {
        let pw = password.to_string();
        let hash = tokio::task::spawn_blocking(move || hash_password(&pw)).await??;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        // No check-then-insert: the unique constraint on username decides, so
        // concurrent creates cannot both succeed.
        let res = self
            .write_one(
                sqlx::query(
                    "INSERT INTO users (id, username, password_hash, role, created_at) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(username)
                .bind(&hash)
                .bind(role)
                .bind(&now),
            )
            .await;
        match res {
            Ok(_) => Ok(true),
            Err(e) if format!("{e}").contains("UNIQUE") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// List users (username, role) for the admin users view (plan 5.2).
    pub async fn list_users(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT username, role FROM users ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| Ok((r.try_get("username")?, r.try_get("role")?)))
            .collect()
    }

    /// Authoritative auth state for a username (finding 7): the role that is in
    /// force *now*, and the session generation a token must match.
    ///
    /// `None` means the user no longer exists. `verify_token` treats both
    /// `None` and a stale generation as unauthenticated, so deleting a user,
    /// changing their role or rotating their password takes effect on the next
    /// request instead of at the end of the token's twelve hours.
    pub async fn user_auth_state(&self, username: &str) -> Result<Option<(String, i64)>> {
        let row = sqlx::query("SELECT role, sessions_gen FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(r) => Ok(Some((
                r.try_get("role")?,
                r.try_get::<i64, _>("sessions_gen")?,
            ))),
            None => Ok(None),
        }
    }

    /// Delete a user. Returns false when there was no such user.
    pub async fn delete_user(&self, username: &str) -> Result<bool> {
        let res = self
            .write_one(sqlx::query("DELETE FROM users WHERE username = ?").bind(username))
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Change a user's role. Returns false when there was no such user.
    pub async fn set_user_role(&self, username: &str, role: &str) -> Result<bool> {
        let res = self
            .write_one(
                sqlx::query("UPDATE users SET role = ? WHERE username = ?")
                    .bind(role)
                    .bind(username),
            )
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Replace a user's password hash. Returns false when there was no such
    /// user.
    pub async fn set_user_password(&self, username: &str, password: &str) -> Result<bool> {
        let pw = password.to_string();
        let hash = tokio::task::spawn_blocking(move || hash_password(&pw)).await??;
        let res = self
            .write_one(
                sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
                    .bind(&hash)
                    .bind(username),
            )
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Invalidate every session issued for this user so far by moving the
    /// generation forward.
    pub async fn bump_sessions_gen(&self, username: &str) -> Result<()> {
        self.write_one(
            sqlx::query("UPDATE users SET sessions_gen = sessions_gen + 1 WHERE username = ?")
                .bind(username),
        )
        .await?;
        Ok(())
    }

    /// Number of admins, so the last one cannot be deleted or demoted (that
    /// would leave an install nobody can administer).
    pub async fn admin_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE role = ?")
            .bind(agentgrid_common::ROLE_ADMIN)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    /// Verify a username/password pair. Returns the user id, role and session
    /// generation on success.
    pub async fn verify_user(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<(String, String, i64)>> {
        let row = sqlx::query(
            "SELECT id, password_hash, role, sessions_gen FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            // Spend the same Argon2 time an existing user would, so response
            // latency does not reveal which usernames exist (finding 12).
            let pw = password.to_string();
            let _ = tokio::task::spawn_blocking(move || verify_password(&pw, dummy_hash())).await?;
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let hash: String = row.try_get("password_hash")?;
        let role: String = row.try_get("role")?;
        let gen: i64 = row.try_get("sessions_gen")?;
        // Argon2 verification is CPU-heavy; keep it off the async executor.
        let pw = password.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_password(&pw, &hash)).await?;
        Ok(if ok { Some((id, role, gen)) } else { None })
    }
}
