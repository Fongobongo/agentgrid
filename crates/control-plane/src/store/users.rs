//! Local user storage (Stage 4.1). Extracted from `store.rs`.

use super::{now_iso, Store};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::Argon2;
    use rand::rngs::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string();
    Ok(hash)
}

/// Verify a password against an Argon2id hash string (Stage 4.1).
fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
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
    pub async fn create_user(&self, username: &str, password: &str) -> Result<bool> {
        let pw = password.to_string();
        let hash = tokio::task::spawn_blocking(move || hash_password(&pw)).await??;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        // No check-then-insert: the unique constraint on username decides, so
        // concurrent creates cannot both succeed.
        let res = sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(&hash)
        .bind(&now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(true),
            Err(e) if format!("{e}").contains("UNIQUE") => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn user_exists(&self, username: &str) -> Result<bool> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")? > 0)
    }

    /// Verify a username/password pair. Returns the user id on success.
    pub async fn verify_user(&self, username: &str, password: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let hash: String = row.try_get("password_hash")?;
        // Argon2 verification is CPU-heavy; keep it off the async executor.
        let pw = password.to_string();
        let ok = tokio::task::spawn_blocking(move || verify_password(&pw, &hash)).await?;
        Ok(if ok { Some(id) } else { None })
    }
}
