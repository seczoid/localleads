//! The owner's account, sign-in sessions and MCP API keys.

use super::Database;
use crate::models::now;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub email: String,
}

#[derive(Clone, Debug)]
pub struct ApiKeyRow {
    pub id: String,
    pub name: String,
    /// The first characters of the key, to tell keys apart.
    pub shown: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

pub(super) fn create_tables(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id VARCHAR PRIMARY KEY,
            email VARCHAR NOT NULL UNIQUE,
            password_hash VARCHAR NOT NULL,
            created_at VARCHAR NOT NULL,
            updated_at VARCHAR NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            token_hash VARCHAR PRIMARY KEY,
            user_id VARCHAR NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            created_at VARCHAR NOT NULL,
            expires_at VARCHAR NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions(user_id);
        "#,
    )?;
    connection.execute("DELETE FROM sessions WHERE expires_at < ?", params![now()])?;
    create_key_table(connection)
}

/// Also used by `localleads mcp install`, which may run before the
/// dashboard ever opened this database.
fn create_key_table(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS api_keys (
            id VARCHAR PRIMARY KEY,
            name VARCHAR NOT NULL,
            shown VARCHAR NOT NULL,
            key_hash VARCHAR NOT NULL UNIQUE,
            created_at VARCHAR NOT NULL,
            last_used_at VARCHAR,
            revoked_at VARCHAR
        );
        "#,
    )?;
    Ok(())
}

fn insert_key(connection: &Connection, name: &str, shown: &str, key_hash: &str) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    connection.execute(
        "INSERT INTO api_keys (id, name, shown, key_hash, created_at) VALUES (?, ?, ?, ?, ?)",
        params![id, name, shown, key_hash, now()],
    )?;
    Ok(id)
}

/// Adds an API key to a database file without opening it as the app does
/// (that would mark a running dashboard's searches interrupted).
pub fn add_api_key_to_file(path: &Path, name: &str, shown: &str, key_hash: &str) -> Result<()> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(10))?;
    create_key_table(&connection)?;
    insert_key(&connection, name, shown, key_hash)?;
    Ok(())
}

impl Database {
    pub fn has_account(&self) -> Result<bool> {
        let connection = self.read()?;
        Ok(connection.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |row| row.get(0))?)
    }

    /// Creates the one account. Returns `None` if an account already exists.
    pub fn create_account(&self, email: &str, password_hash: &str) -> Result<Option<Account>> {
        let connection = self.lock()?;
        let id = uuid::Uuid::new_v4().to_string();
        let at = now();
        let created = connection.execute(
            "INSERT INTO users (id, email, password_hash, created_at, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?4 WHERE NOT EXISTS (SELECT 1 FROM users)",
            params![id, email, password_hash, at],
        )?;
        Ok((created == 1).then(|| Account {
            id,
            email: email.to_owned(),
        }))
    }

    /// The account with this email and its password hash.
    pub fn account_by_email(&self, email: &str) -> Result<Option<(Account, String)>> {
        let connection = self.read()?;
        Ok(connection
            .query_row(
                "SELECT id, email, password_hash FROM users WHERE email = ?",
                params![email],
                |row| Ok((Account { id: row.get(0)?, email: row.get(1)? }, row.get(2)?)),
            )
            .optional()?)
    }

    pub fn password_hash(&self, user_id: &str) -> Result<Option<String>> {
        let connection = self.read()?;
        Ok(connection
            .query_row("SELECT password_hash FROM users WHERE id = ?", params![user_id], |row| row.get(0))
            .optional()?)
    }

    /// Sets a new password and signs out every other session.
    pub fn set_password(&self, user_id: &str, password_hash: &str, keep_session: &str) -> Result<()> {
        let connection = self.lock()?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?",
            params![password_hash, now(), user_id],
        )?;
        transaction.execute(
            "DELETE FROM sessions WHERE user_id = ? AND token_hash <> ?",
            params![user_id, keep_session],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_session(&self, user_id: &str, token_hash: &str, expires_at: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
            params![token_hash, user_id, now(), expires_at],
        )?;
        Ok(())
    }

    /// The account signed in with this session, if it's still valid.
    pub fn session_account(&self, token_hash: &str) -> Result<Option<Account>> {
        let connection = self.read()?;
        Ok(connection
            .query_row(
                "SELECT u.id, u.email FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ? AND s.expires_at > ?",
                params![token_hash, now()],
                |row| Ok(Account { id: row.get(0)?, email: row.get(1)? }),
            )
            .optional()?)
    }

    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute("DELETE FROM sessions WHERE token_hash = ?", params![token_hash])?;
        Ok(())
    }

    pub fn create_api_key(&self, name: &str, shown: &str, key_hash: &str) -> Result<String> {
        let connection = self.lock()?;
        insert_key(&connection, name, shown, key_hash)
    }

    /// Keys that still work, newest first.
    pub fn api_keys(&self) -> Result<Vec<ApiKeyRow>> {
        let connection = self.read()?;
        let mut statement = connection.prepare(
            "SELECT id, name, shown, created_at, last_used_at FROM api_keys
             WHERE revoked_at IS NULL ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ApiKeyRow {
                id: row.get(0)?,
                name: row.get(1)?,
                shown: row.get(2)?,
                created_at: row.get(3)?,
                last_used_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke_api_key(&self, id: &str) -> Result<bool> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE api_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            params![now(), id],
        )?;
        Ok(changed == 1)
    }

    /// Whether a key with this hash works; notes when it was last used (at
    /// most once a minute, so busy agents don't write on every call).
    pub fn use_api_key(&self, key_hash: &str) -> Result<bool> {
        let last_used: Option<Option<String>> = {
            let connection = self.read()?;
            connection
                .query_row(
                    "SELECT last_used_at FROM api_keys WHERE key_hash = ? AND revoked_at IS NULL",
                    params![key_hash],
                    |row| row.get(0),
                )
                .optional()?
        };
        let Some(last_used) = last_used else {
            return Ok(false);
        };
        let stale = last_used
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(&at).ok())
            .is_none_or(|at| chrono::Utc::now().signed_duration_since(at) > chrono::Duration::minutes(1));
        if stale {
            let connection = self.lock()?;
            connection.execute(
                "UPDATE api_keys SET last_used_at = ? WHERE key_hash = ?",
                params![now(), key_hash],
            )?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_account_sessions_and_keys() {
        let db = Database::in_memory().unwrap();
        assert!(!db.has_account().unwrap());
        let account = db.create_account("jo@example.com", "hash").unwrap().unwrap();
        assert!(db.create_account("other@example.com", "hash").unwrap().is_none());
        assert!(db.has_account().unwrap());

        let later = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        let earlier = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        db.create_session(&account.id, "a", &later).unwrap();
        db.create_session(&account.id, "b", &later).unwrap();
        db.create_session(&account.id, "old", &earlier).unwrap();
        assert_eq!(db.session_account("a").unwrap().unwrap().email, "jo@example.com");
        assert!(db.session_account("old").unwrap().is_none());
        db.set_password(&account.id, "new", "a").unwrap();
        assert!(db.session_account("a").unwrap().is_some());
        assert!(db.session_account("b").unwrap().is_none());
        assert_eq!(db.password_hash(&account.id).unwrap().as_deref(), Some("new"));

        let id = db.create_api_key("laptop", "ll_abc", "keyhash").unwrap();
        assert!(db.use_api_key("keyhash").unwrap());
        assert!(!db.use_api_key("other").unwrap());
        assert!(db.api_keys().unwrap()[0].last_used_at.is_some());
        assert!(db.revoke_api_key(&id).unwrap());
        assert!(!db.use_api_key("keyhash").unwrap());
        assert!(db.api_keys().unwrap().is_empty());
    }
}
