//! SQLite-backed user store: signup, lookup, password verification.
//!
//! Single-file rusqlite database keeps the alpha deployment simple while
//! the auth contract (argon2id hash, uuid PK, plan state for billing)
//! stays fixed from day one.

use crate::auth;
use lumen_core::error::{LumenError, Result};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct UserStore {
    conn: std::sync::Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
}

/// Plan state granted via subscription fulfillment. Mirrors what the Dodo
/// dashboard shows so support queries line up without cross-referencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// No paid plan active; account exists with local-only access.
    None,
    Base,
}

impl Plan {
    pub fn as_str(&self) -> &'static str {
        match self {
            Plan::None => "none",
            Plan::Base => "base",
        }
    }
}

impl UserStore {
    pub fn open_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory())
    }

    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        Self::from_conn(Connection::open(path))
    }

    fn from_conn(conn: std::result::Result<Connection, rusqlite::Error>) -> Result<Self> {
        let conn = conn.map_err(|e| LumenError::Storage(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                plan TEXT NOT NULL DEFAULT 'base',
                dodo_customer_id TEXT,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    /// New store handle sharing the same underlying connection. Lets a
    /// caller keep inspection access after moving the primary store into
    /// shared state (webhook fulfillment assertions do exactly this).
    pub fn share(&self) -> UserStore {
        Self {
            conn: self.conn.clone(),
        }
    }

    /// Create a user with an argon2id hash. Duplicate email is a Conflict.
    pub fn signup(&self, email: &str, password: &str) -> Result<User> {
        let hash = auth::hash_password(password).map_err(|e| LumenError::Auth(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let created = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("user store poisoned");
        conn.execute(
            "INSERT INTO users (id, email, password_hash, plan, created_at) VALUES (?1, ?2, ?3, 'base', ?4)",
            params![id, email, hash, created],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                LumenError::Conflict(format!("email {email} already registered"))
            }
            other => LumenError::Storage(other.to_string()),
        })?;
        Ok(User {
            id,
            email: email.to_string(),
        })
    }

    /// Verify credentials and return the user. Constant-shape error on any
    /// failure path (unknown email vs bad password look identical to the
    /// caller's JSON response) so login cannot enumerate accounts.
    pub fn login(&self, email: &str, password: &str) -> Result<User> {
        let row = {
            let conn = self.conn.lock().expect("user store poisoned");
            conn.query_row(
                "SELECT id, email, password_hash FROM users WHERE email = ?1",
                params![email],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
        };
        let (id, stored_email, stored_hash) =
            row.map_err(|_| LumenError::Auth("invalid email or password".to_string()))?;
        auth::verify_password(password, &stored_hash)
            .map_err(|_| LumenError::Auth("invalid email or password".to_string()))?;
        Ok(User {
            id,
            email: stored_email,
        })
    }

    /// Link a user to their Dodo customer id. Webhook fulfillment matches on
    /// this column when subscription events arrive (the payload carries the
    /// provider-side id, not our uuid). Idempotent by nature of an UPDATE.
    pub fn set_dodo_customer(&self, user_id: &str, dodo_customer_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("user store poisoned");
        conn.execute(
            "UPDATE users SET dodo_customer_id = ?1 WHERE id = ?2",
            params![dodo_customer_id, user_id],
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Link + fulfill in one step for webhook events that carry checkout
    /// metadata: the user id comes from OUR metadata (set at checkout
    /// creation, authenticated by JWT), so no email/customer guesswork is
    /// involved. Returns Err(NotFound) when the metadata points at a user
    /// that does not exist (stale/garbage metadata) - callers surface that
    /// as acknowledged-but-unfulfilled rather than erroring the webhook.
    pub fn link_and_grant_by_user_id(
        &self,
        user_id: &str,
        dodo_customer_id: Option<&str>,
        plan: Plan,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("user store poisoned");
        let changed = conn
            .execute(
                "UPDATE users SET plan = ?1, dodo_customer_id = COALESCE(?2, dodo_customer_id) WHERE id = ?3",
                params![plan.as_str(), dodo_customer_id, user_id],
            )
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(changed > 0)
    }

    /// Grant a plan to whichever user owns this Dodo customer id. Returns
    /// false when no user matches (checkout completed before signup or a
    /// stale webhook) - callers treat that as acknowledged-but-unfulfilled,
    /// not an error, since retrying will not conjure the mapping.
    pub fn grant_plan_by_dodo_customer(&self, dodo_customer_id: &str, plan: Plan) -> Result<bool> {
        let conn = self.conn.lock().expect("user store poisoned");
        let n = conn
            .execute(
                "UPDATE users SET plan = ?1 WHERE dodo_customer_id = ?2",
                params![plan.as_str(), dodo_customer_id],
            )
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(n > 0)
    }

    /// Current plan for direct API exposure (/v1/me).
    pub fn plan_of(&self, user_id: &str) -> Result<Plan> {
        let conn = self.conn.lock().expect("user store poisoned");
        let plan: Option<String> = conn
            .query_row(
                "SELECT plan FROM users WHERE id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(match plan.as_deref() {
            Some("base") => Plan::Base,
            _ => Plan::None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signup_login_roundtrip() {
        let store = UserStore::open_memory().unwrap();
        let u = store
            .signup("chase@example.com", "correct horse battery staple")
            .unwrap();
        assert_eq!(u.email, "chase@example.com");

        let logged_in = store
            .login("chase@example.com", "correct horse battery staple")
            .unwrap();
        assert_eq!(logged_in.id, u.id);
    }

    #[test]
    fn wrong_password_and_unknown_email_fail_identically() {
        let store = UserStore::open_memory().unwrap();
        store
            .signup("chase@example.com", "correct horse battery staple")
            .unwrap();
        let e1 = format!("{}", store.login("chase@example.com", "wrong").unwrap_err());
        let e2 = format!(
            "{}",
            store.login("nobody@example.com", "wrong").unwrap_err()
        );
        // Both paths must produce the same client-facing message so the
        // endpoint cannot be used to probe which emails are registered.
        assert_eq!(e1, e2);
        assert!(e1.contains("invalid email or password"));
    }

    #[test]
    fn duplicate_email_conflicts() {
        let store = UserStore::open_memory().unwrap();
        store.signup("chase@example.com", "password one").unwrap();
        let err = store
            .signup("chase@example.com", "password two")
            .unwrap_err();
        assert!(matches!(err, LumenError::Conflict(_)));
    }

    #[test]
    fn stored_hash_never_contains_plaintext_password() {
        let store = UserStore::open_memory().unwrap();
        let pw = "hunter2-hunter2-hunter2";
        store.signup("a@b.c", pw).unwrap();
        let raw = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT password_hash FROM users", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap();
        assert!(!raw.contains(pw));
    }

    #[test]
    fn subscription_fulfillment_grants_plan_by_dodo_customer() {
        let store = UserStore::open_memory().unwrap();
        let u = store
            .signup("chase@example.com", "correct horse battery staple")
            .unwrap();
        store.set_dodo_customer(&u.id, "cst_9f2a").unwrap();

        // Fulfillment arrives with only the provider-side id in the payload.
        let granted = store
            .grant_plan_by_dodo_customer("cst_9f2a", Plan::Base)
            .unwrap();
        assert!(granted, "linked customer must be found");
        assert_eq!(store.plan_of(&u.id).unwrap(), Plan::Base);

        // Unknown customer acks as unfulfilled instead of erroring.
        let missing = store
            .grant_plan_by_dodo_customer("cst_unknown", Plan::Base)
            .unwrap();
        assert!(!missing);
        // The granted plan survives an unrelated miss - never downgraded.
        assert_eq!(store.plan_of(&u.id).unwrap(), Plan::Base);
    }

    #[test]
    fn replays_are_idempotent_for_fulfillment() {
        let store = UserStore::open_memory().unwrap();
        let u = store
            .signup("replay@example.com", "correct horse battery staple")
            .unwrap();
        store.set_dodo_customer(&u.id, "cst_replay_1").unwrap();
        for _ in 0..3 {
            assert!(store
                .grant_plan_by_dodo_customer("cst_replay_1", Plan::Base)
                .unwrap());
        }
        assert_eq!(store.plan_of(&u.id).unwrap(), Plan::Base);
    }
}
