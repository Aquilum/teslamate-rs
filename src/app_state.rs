use crate::auth::{AuthState, HasAuthDb, PasswordBackend};
use crate::db::Db;
use parking_lot::Mutex as ParkingMutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const QUERY_CACHE_TTL: Duration = Duration::from_secs(30);
const QUERY_CACHE_MAX: usize = 128;
const QUERY_CACHE_MAX_ROWS: usize = 20_000;
const QUERY_STAT_MAX: usize = 256;
const QUERY_STAT_SQL_LEN: usize = 240;

struct CachedQuery {
    at: Instant,
    payload: Value,
}

#[derive(Clone)]
struct QueryStat {
    sql: String,
    calls: u64,
    total_ms: f64,
}

/// Short-lived dashboard query results. Keyed by translated SQL so a return visit
/// (or overlapping panels) does not redo SQLite work.
#[derive(Clone)]
pub struct QueryCache {
    inner: Arc<ParkingMutex<HashMap<u64, CachedQuery>>>,
    stats: Arc<ParkingMutex<HashMap<u64, QueryStat>>>,
    started: SystemTime,
}

impl Default for QueryCache {
    fn default() -> Self {
        Self {
            inner: Arc::new(ParkingMutex::new(HashMap::new())),
            stats: Arc::new(ParkingMutex::new(HashMap::new())),
            started: SystemTime::now(),
        }
    }
}

impl QueryCache {
    pub fn get(&self, key: u64) -> Option<Value> {
        let mut map = self.inner.lock();
        let stale = map
            .get(&key)
            .map(|ent| ent.at.elapsed() > QUERY_CACHE_TTL)
            .unwrap_or(false);
        if stale {
            map.remove(&key);
            return None;
        }
        map.get(&key).map(|ent| ent.payload.clone())
    }

    pub fn put(&self, key: u64, payload: Value) {
        if payload
            .get("rows")
            .and_then(|r| r.as_array())
            .map(|rows| rows.len() > QUERY_CACHE_MAX_ROWS)
            .unwrap_or(false)
        {
            return;
        }
        let mut map = self.inner.lock();
        if map.len() >= QUERY_CACHE_MAX {
            if let Some(old) = map.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| *k) {
                map.remove(&old);
            }
        }
        map.insert(
            key,
            CachedQuery {
                at: Instant::now(),
                payload,
            },
        );
    }

    pub fn record_exec(&self, sql: &str, ms: f64) {
        let preview = preview_sql(sql);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        preview.hash(&mut h);
        let key = h.finish();
        let mut map = self.stats.lock();
        let ent = map.entry(key).or_insert_with(|| QueryStat {
            sql: preview,
            calls: 0,
            total_ms: 0.0,
        });
        ent.calls = ent.calls.saturating_add(1);
        ent.total_ms += ms.max(0.0);
        if map.len() > QUERY_STAT_MAX {
            if let Some(old) = map
                .iter()
                .min_by(|a, b| a.1.calls.cmp(&b.1.calls).then(a.0.cmp(b.0)))
                .map(|(k, _)| *k)
            {
                map.remove(&old);
            }
        }
    }

    pub fn grafana_statements(&self, kind: &str) -> Value {
        match kind {
            "count" => json!({
                "ok": true,
                "columns": ["count"],
                "rows": [{"count": self.stats.lock().len() as i64}],
                "sql": "SELECT count(*) FROM tm_query_stats",
            }),
            "reset" => {
                let reset = chrono::DateTime::<chrono::Utc>::from(self.started)
                    .format("%Y-%m-%d %H:%M:%S UTC")
                    .to_string();
                json!({
                    "ok": true,
                    "columns": ["stats_reset"],
                    "rows": [{"stats_reset": reset}],
                    "sql": "SELECT stats_reset FROM tm_query_stats",
                })
            }
            kind => {
                let by_mean = kind != "total";
                let mut rows: Vec<QueryStat> = self.stats.lock().values().cloned().collect();
                rows.sort_by(|a, b| {
                    let (ka, kb) = if by_mean {
                        (
                            a.total_ms / (a.calls as f64).max(1.0),
                            b.total_ms / (b.calls as f64).max(1.0),
                        )
                    } else {
                        (a.total_ms, b.total_ms)
                    };
                    kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
                });
                let rows: Vec<Value> = rows
                    .into_iter()
                    .take(20)
                    .map(|s| {
                        let mean = s.total_ms / (s.calls as f64).max(1.0);
                        json!({
                            "Calls": s.calls,
                            "Mean Exec Time": mean,
                            "Total Exec Time": s.total_ms,
                            "Query": s.sql,
                        })
                    })
                    .collect();
                json!({
                    "ok": true,
                    "columns": ["Calls", "Mean Exec Time", "Total Exec Time", "Query"],
                    "rows": rows,
                    "sql": "SELECT * FROM tm_query_stats",
                })
            }
        }
    }
}

fn preview_sql(sql: &str) -> String {
    let collapsed: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= QUERY_STAT_SQL_LEN {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(QUERY_STAT_SQL_LEN.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[derive(Clone, Default)]
pub struct AuthLimiter {
    hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl AuthLimiter {
    pub fn allow(&self, key: &str) -> bool {
        self.allow_budget(key, 30, Duration::from_secs(15 * 60))
    }

    pub fn allow_budget(&self, key: &str, max: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut map = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < window);
        if entry.len() >= max {
            return false;
        }
        entry.push(now);
        true
    }
}

const DEFAULT_QUERY_PER_USER: usize = 4;
const DEFAULT_QUERY_GLOBAL: usize = 32;
const DEFAULT_QUERY_RATE_MAX: usize = 300;
const QUERY_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);
const QUERY_RATE_WINDOW: Duration = Duration::from_secs(15 * 60);

pub fn query_rate_window() -> Duration {
    QUERY_RATE_WINDOW
}

pub fn query_rate_max() -> usize {
    std::env::var("TESLAMATE_RS_QUERY_RATE_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QUERY_RATE_MAX)
        .clamp(30, 10_000)
}

fn query_per_user_limit() -> usize {
    std::env::var("TESLAMATE_RS_QUERY_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QUERY_PER_USER)
        .clamp(1, 64)
}

fn query_global_limit() -> usize {
    std::env::var("TESLAMATE_RS_QUERY_GLOBAL_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QUERY_GLOBAL)
        .clamp(1, 256)
}

/// Caps in-flight `/api/query` work so one account cannot exhaust the SQLite
/// read pool / blocking thread pool.
#[derive(Clone)]
pub struct QueryGate {
    global: Arc<Semaphore>,
    per_user: Arc<Mutex<HashMap<i64, Arc<Semaphore>>>>,
    per_user_limit: usize,
}

pub struct QueryPermits {
    _user: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

impl Default for QueryGate {
    fn default() -> Self {
        Self::new(query_global_limit(), query_per_user_limit())
    }
}

impl QueryGate {
    pub fn new(global: usize, per_user: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global.max(1))),
            per_user: Arc::new(Mutex::new(HashMap::new())),
            per_user_limit: per_user.max(1),
        }
    }

    fn user_semaphore(&self, user_id: i64) -> Arc<Semaphore> {
        let mut map = self.per_user.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(user_id)
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_user_limit)))
            .clone()
    }

    /// Acquire per-user then global permits, or fail after a short wait (429).
    pub async fn try_acquire(&self, user_id: i64) -> Result<QueryPermits, ()> {
        let user_sem = self.user_semaphore(user_id);
        let user = tokio::time::timeout(QUERY_ACQUIRE_TIMEOUT, user_sem.acquire_owned())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let global = tokio::time::timeout(QUERY_ACQUIRE_TIMEOUT, self.global.clone().acquire_owned())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        Ok(QueryPermits {
            _user: user,
            _global: global,
        })
    }
}

#[derive(Clone)]
pub struct App {
    pub db: Db,
    pub auth: AuthState,
    pub password_backend: PasswordBackend,
    pub limiter: AuthLimiter,
    pub query_cache: QueryCache,
    pub query_gate: QueryGate,
    /// First-admin setup is allowed (loopback bind or TESLAMATE_RS_ALLOW_SETUP=1).
    pub setup_allowed: bool,
}

impl HasAuthDb for App {
    fn auth_db(&self) -> &Db {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_cache_roundtrip_and_skip_huge() {
        let cache = QueryCache::default();
        let small = json!({"ok": true, "rows": [{"a": 1}]});
        cache.put(1, small.clone());
        assert_eq!(cache.get(1), Some(small));

        let huge = json!({"ok": true, "rows": vec![json!({"a": 1}); QUERY_CACHE_MAX_ROWS + 1]});
        cache.put(2, huge);
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn query_stats_top_by_total() {
        let cache = QueryCache::default();
        cache.record_exec("SELECT 1", 10.0);
        cache.record_exec("SELECT 1", 20.0);
        cache.record_exec("SELECT 2", 100.0);
        let count = cache.grafana_statements("count");
        assert_eq!(count["rows"][0]["count"], json!(2));
        let top = cache.grafana_statements("total");
        assert_eq!(top["rows"][0]["Query"], json!("SELECT 2"));
        assert_eq!(top["rows"][0]["Calls"], json!(1));
        let reset = cache.grafana_statements("reset");
        assert!(reset["rows"][0]["stats_reset"].as_str().unwrap().contains("UTC"));
    }

    #[tokio::test]
    async fn query_gate_rejects_over_per_user_limit() {
        let gate = QueryGate::new(32, 2);
        let a = gate.try_acquire(7).await.expect("first");
        let b = gate.try_acquire(7).await.expect("second");
        let blocked = gate.try_acquire(7).await;
        assert!(blocked.is_err(), "third concurrent query for same user");
        // Other users still get a slot.
        let other = gate.try_acquire(8).await.expect("other user");
        drop(a);
        drop(b);
        drop(other);
        gate.try_acquire(7).await.expect("after release");
    }

    #[tokio::test]
    async fn query_gate_rejects_over_global_limit() {
        let gate = QueryGate::new(1, 8);
        let a = gate.try_acquire(1).await.expect("first");
        assert!(gate.try_acquire(2).await.is_err());
        drop(a);
        gate.try_acquire(2).await.expect("after release");
    }

    #[test]
    fn limiter_budget_is_independent_per_key() {
        let lim = AuthLimiter::default();
        let w = Duration::from_secs(60);
        assert!(lim.allow_budget("query:1", 2, w));
        assert!(lim.allow_budget("query:1", 2, w));
        assert!(!lim.allow_budget("query:1", 2, w));
        assert!(lim.allow_budget("query:2", 2, w));
    }
}
