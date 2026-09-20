use crate::auth::{AuthState, HasAuthDb, PasswordBackend};
use crate::db::Db;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct AuthLimiter {
    hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl AuthLimiter {
    pub fn allow(&self, key: &str) -> bool {
        const WINDOW: Duration = Duration::from_secs(15 * 60);
        const MAX: usize = 30;
        let now = Instant::now();
        let mut map = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < WINDOW);
        if entry.len() >= MAX {
            return false;
        }
        entry.push(now);
        true
    }
}

#[derive(Clone)]
pub struct App {
    pub db: Db,
    pub auth: AuthState,
    pub password_backend: PasswordBackend,
    pub limiter: AuthLimiter,
}

impl HasAuthDb for App {
    fn auth_db(&self) -> &Db {
        &self.db
    }
}
