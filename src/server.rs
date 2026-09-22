use crate::app_state::{App, AuthLimiter, QueryCache};
use crate::auth::{AdminUser, AuthState, AuthUser, PasswordBackend};
use crate::db::Db;
use crate::sql::{self, QueryVars};
use anyhow::Result;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::trace::TraceLayer;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Web;

#[derive(RustEmbed)]
#[folder = "dashboards/"]
struct Dashboards;

const DEFAULT_QUERY_MAX_ROWS: usize = 50_000;
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 15_000;

fn query_max_rows() -> usize {
    std::env::var("TESLAMATE_RS_QUERY_MAX_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QUERY_MAX_ROWS)
        .clamp(100, 500_000)
}

fn query_timeout() -> Duration {
    let ms = std::env::var("TESLAMATE_RS_QUERY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QUERY_TIMEOUT_MS)
        .clamp(500, 120_000);
    Duration::from_millis(ms)
}

pub fn setup_allowed_for_bind(bind: SocketAddr) -> bool {
    if bind.ip().is_loopback() {
        return true;
    }
    matches!(
        std::env::var("TESLAMATE_RS_ALLOW_SETUP")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub async fn serve(db: Db, bind: SocketAddr, password_backend: PasswordBackend) -> Result<()> {
    let origin = if bind.ip().is_loopback() {
        format!("http://localhost:{}", bind.port())
    } else {
        format!("http://{bind}")
    };
    let rp_id = origin
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or("localhost")
        .to_string();
    let auth = AuthState::new(&rp_id, &origin).map_err(|e| anyhow::anyhow!("{e}"))?;
    match password_backend {
        PasswordBackend::Pam => {
            let service = crate::pam_auth::service_name();
            let group = crate::pam_auth::required_group()
                .map(|g| format!("group={g}"))
                .unwrap_or_else(|| "group=(any PAM user)".into());
            tracing::info!("password backend: PAM (service={service}, {group})");
            if let Some(group) = crate::pam_auth::required_group() {
                if !crate::pam_auth::group_exists(&group) {
                    tracing::error!(
                        "group {group} does not exist; PAM sign-in is refused until you create it and add members"
                    );
                }
            }
        }
        PasswordBackend::Local => tracing::info!("password backend: local (SQLite argon2)"),
    }
    let setup_allowed = setup_allowed_for_bind(bind);
    if !setup_allowed {
        tracing::info!(
            "first-admin setup locked (non-loopback bind); set TESLAMATE_RS_ALLOW_SETUP=1 to unlock"
        );
    }
    tracing::info!(
        "dashboard SQL allowlist: {} templates",
        crate::query_allowlist::len()
    );
    let state = App {
        db,
        auth,
        password_backend,
        limiter: AuthLimiter::default(),
        query_cache: QueryCache::default(),
        setup_allowed,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/cars", get(cars))
        .route("/api/settings", get(settings))
        .route("/api/dashboards", get(list_dashboards))
        .route("/api/dashboards/{*path}", get(get_dashboard))
        .route("/api/query", post(run_query))
        .route("/api/dbinfo", get(dbinfo))
        .merge(crate::auth_http::router())
        .route("/{*path}", get(static_file))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    tracing::info!("teslamate-rs listening on http://{bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https://tile.openstreetmap.org https://*.tile.openstreetmap.org; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'; object-src 'none'",
        ),
    );
    res
}

async fn index() -> impl IntoResponse {
    match Web::get("index.html") {
        Some(f) => (
            [(header::CACHE_CONTROL, "no-store")],
            Html(f.data.to_vec()),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "missing web/index.html").into_response(),
    }
}

async fn static_file(Path(path): Path<String>) -> Response {
    match Web::get(&path) {
        Some(f) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.essence_str()),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                f.data.to_vec(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "name": "teslamate-rs" }))
}

async fn spawn_db<T, F>(db: Db, f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, rusqlite::Error> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let conn = db.read();
        f(&conn)
    })
    .await
    .map_err(|e| AppError(anyhow::anyhow!("db task: {e}")))?
    .map_err(AppError::from)
}

async fn cars(_user: AuthUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
    spawn_db(app.db.clone(), |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name, model, trim_badging, efficiency FROM cars ORDER BY display_priority, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "name": r.get::<_, Option<String>>(1)?,
                    "model": r.get::<_, Option<String>>(2)?,
                    "trim_badging": r.get::<_, Option<String>>(3)?,
                    "efficiency": r.get::<_, Option<f64>>(4)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Json(json!(rows)))
    })
    .await
}

async fn settings(_user: AuthUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
    spawn_db(app.db.clone(), |conn| {
        let v = conn
            .query_row(
                "SELECT unit_of_length, unit_of_temperature, preferred_range, unit_of_pressure, language, theme_mode
             FROM settings ORDER BY id LIMIT 1",
                [],
                |r| {
                    Ok(json!({
                        "unit_of_length": r.get::<_, String>(0)?,
                        "unit_of_temperature": r.get::<_, String>(1)?,
                        "preferred_range": r.get::<_, String>(2)?,
                        "unit_of_pressure": r.get::<_, String>(3)?,
                        "language": r.get::<_, String>(4)?,
                        "theme_mode": r.get::<_, String>(5)?,
                    }))
                },
            )
            .optional_json()?;
        Ok(Json(v.unwrap_or_else(|| {
            json!({
                "unit_of_length": "km",
                "unit_of_temperature": "C",
                "preferred_range": "rated",
                "unit_of_pressure": "bar",
                "language": "en",
                "theme_mode": "system"
            })
        })))
    })
    .await
}

trait OptionalJson {
    fn optional_json(self) -> rusqlite::Result<Option<Value>>;
}
impl OptionalJson for rusqlite::Result<Value> {
    fn optional_json(self) -> rusqlite::Result<Option<Value>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[derive(Serialize)]
struct DashMeta {
    path: String,
    title: String,
    uid: String,
    folder: String,
}

async fn list_dashboards(_user: AuthUser) -> Json<Vec<DashMeta>> {
    let mut out = Vec::new();
    for name in Dashboards::iter() {
        if !name.ends_with(".json") || name.starts_with("internal/") {
            continue;
        }
        let Some(file) = Dashboards::get(name.as_ref()) else {
            continue;
        };
        let parsed: Value = serde_json::from_slice(&file.data).unwrap_or(Value::Null);
        let folder = if name.starts_with("reports/") {
            "Reports"
        } else {
            "TeslaMate"
        };
        out.push(DashMeta {
            path: name.to_string(),
            title: parsed
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or(&name)
                .to_string(),
            uid: parsed
                .get("uid")
                .and_then(|t| t.as_str())
                .unwrap_or(&name)
                .to_string(),
            folder: folder.into(),
        });
    }
    out.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.title.cmp(&b.title)));
    Json(out)
}

async fn get_dashboard(_user: AuthUser, Path(path): Path<String>) -> Response {
    match Dashboards::get(&path) {
        Some(f) => (
            [(header::CONTENT_TYPE, "application/json")],
            f.data.to_vec(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
    #[serde(flatten)]
    vars: QueryVars,
}

fn cancelled_json() -> Value {
    json!({ "ok": false, "cancelled": true, "error": "cancelled" })
}

fn cache_key(sql: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut h);
    h.finish()
}

fn is_interrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::OperationInterrupted
    )
}

struct ClearProgress<'a>(&'a Connection);

impl Drop for ClearProgress<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn query_stats_override(sql: &str, cache: &QueryCache) -> Option<Value> {
    let l = sql.to_ascii_lowercase();
    if !l.contains("pg_stat_statements") {
        return None;
    }
    if l.contains("last_reset") || l.contains("pg_stat_statements_info") {
        return Some(cache.grafana_statements("reset"));
    }
    if l.contains("pg_stat_statements_count") {
        return Some(cache.grafana_statements("count"));
    }
    if l.contains("top_20_total") {
        return Some(cache.grafana_statements("total"));
    }
    if l.contains("top_20_mean") || l.contains("top_20") {
        return Some(cache.grafana_statements("mean"));
    }
    None
}

fn execute_dashboard_query(
    db: &Db,
    cache: &QueryCache,
    sql: String,
    mut vars: QueryVars,
    cancelled: &Arc<AtomicBool>,
    enforce_allowlist: bool,
    actor: Option<String>,
) -> Value {
    if cancelled.load(Ordering::Relaxed) {
        return cancelled_json();
    }
    if enforce_allowlist && !crate::query_allowlist::is_allowed(&sql) {
        let preview: String = sql.chars().take(120).collect();
        crate::audit::record(
            db,
            actor.as_deref(),
            "query_denied",
            &format!("not in dashboard allowlist: {preview}"),
        );
        return json!({
            "ok": false,
            "error": "query is not an allowed dashboard template",
        });
    }
    if let Some(payload) = query_stats_override(&sql, cache) {
        return payload;
    }
    vars = sql::sanitize_vars(vars);
    if sql.contains("$aux") && !vars.extras.contains_key("aux") {
        let conn = db.read();
        if cancelled.load(Ordering::Relaxed) {
            return cancelled_json();
        }
        let aux = crate::db::battery_aux(
            &conn,
            vars.car_id,
            &vars.length_unit,
            &vars.preferred_range,
        );
        drop(conn);
        vars.extras.insert("aux".into(), aux);
    }
    let translated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sql::translate(&sql, &vars)
    }))
    .unwrap_or_else(|_| sql.clone());
    if let Err(msg) = sql::assert_safe_dashboard_sql(&translated) {
        crate::audit::record(
            db,
            actor.as_deref(),
            "query_blocked",
            &msg,
        );
        return json!({
            "ok": false,
            "error": msg,
        });
    }
    let key = cache_key(&translated);
    let t0 = Instant::now();
    let deadline = t0 + query_timeout();
    let max_rows = query_max_rows();
    if let Some(hit) = cache.get(key) {
        cache.record_exec(&translated, t0.elapsed().as_secs_f64() * 1000.0);
        return hit;
    }
    if cancelled.load(Ordering::Relaxed) {
        return cancelled_json();
    }
    let conn = db.read();
    if cancelled.load(Ordering::Relaxed) {
        return cancelled_json();
    }
    let flag = cancelled.clone();
    conn.progress_handler(250, Some(move || {
        flag.load(Ordering::Relaxed) || Instant::now() >= deadline
    }));
    let _clear = ClearProgress(&conn);
    let mut stmt = match conn.prepare(&translated) {
        Ok(s) => s,
        Err(e) if is_interrupt(&e) || cancelled.load(Ordering::Relaxed) => {
            return cancelled_json();
        }
        Err(e) => {
            return json!({
                "ok": false,
                "error": e.to_string(),
                "sql": translated,
            });
        }
    };
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = Vec::new();
    let mut mapped = match stmt.query([]) {
        Ok(m) => m,
        Err(e) if is_interrupt(&e) || cancelled.load(Ordering::Relaxed) => {
            return cancelled_json();
        }
        Err(e) => {
            return json!({
                "ok": false,
                "error": e.to_string(),
                "sql": translated,
            });
        }
    };
    let mut truncated = false;
    loop {
        if Instant::now() >= deadline {
            return json!({
                "ok": false,
                "error": format!("query timed out after {}ms", query_timeout().as_millis()),
                "sql": translated,
            });
        }
        match mapped.next() {
            Ok(Some(row)) => {
                if rows.len() >= max_rows {
                    truncated = true;
                    break;
                }
                let mut obj = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    match row.get_ref(i) {
                        Ok(v) => {
                            obj.insert(name.clone(), sqlite_to_json(v));
                        }
                        Err(e) if is_interrupt(&e) => return cancelled_json(),
                        Err(e) => {
                            return json!({
                                "ok": false,
                                "error": e.to_string(),
                                "sql": translated,
                            });
                        }
                    }
                }
                rows.push(Value::Object(obj));
            }
            Ok(None) => break,
            Err(e) if is_interrupt(&e) || cancelled.load(Ordering::Relaxed) => {
                return cancelled_json();
            }
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": e.to_string(),
                    "sql": translated,
                });
            }
        }
    }
    drop(mapped);
    drop(stmt);
    drop(_clear);
    drop(conn);
    if cancelled.load(Ordering::Relaxed) {
        return cancelled_json();
    }
    let payload = json!({
        "ok": true,
        "columns": names,
        "rows": rows,
        "sql": translated,
        "truncated": truncated,
        "maxRows": max_rows,
    });
    if !truncated {
        cache.record_exec(&translated, t0.elapsed().as_secs_f64() * 1000.0);
        cache.put(key, payload.clone());
    } else {
        cache.record_exec(&translated, t0.elapsed().as_secs_f64() * 1000.0);
    }
    payload
}

async fn run_query(
    user: AuthUser,
    State(app): State<App>,
    Json(body): Json<QueryBody>,
) -> Result<Json<Value>, AppError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(cancelled.clone());
    let db = app.db.clone();
    let cache = app.query_cache.clone();
    let sql = body.sql;
    let vars = body.vars;
    let flag = cancelled.clone();
    let actor = Some(user.username.clone());
    let payload = tokio::task::spawn_blocking(move || {
        execute_dashboard_query(&db, &cache, sql, vars, &flag, true, actor)
    })
    .await
    .map_err(|e| AppError(anyhow::anyhow!("query task: {e}")))?;
    Ok(Json(payload))
}

fn sqlite_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(_) => json!("<blob>"),
    }
}

async fn dbinfo(_admin: AdminUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
    spawn_db(app.db.clone(), |conn| {
        let page_count: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut tables = Vec::new();
        for name in names {
            if sql::FORBIDDEN_QUERY_TABLES
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |r| r.get(0))?;
            tables.push(json!({ "name": name, "rows": n }));
        }
        Ok(Json(json!({
            "engine": "sqlite",
            "bytes": page_count * page_size,
            "tables": tables,
        })))
    })
    .await
}

struct AppError(anyhow::Error);
impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.into())
    }
}
impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": self.0.to_string() })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod query_cancel_tests {
    use super::*;

    fn vars() -> QueryVars {
        serde_json::from_value(json!({"car_id": 1, "from_ms": 0, "to_ms": 1})).unwrap()
    }

    #[test]
    fn cancelled_before_lock_skips_sqlite() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        let cancelled = Arc::new(AtomicBool::new(true));
        let v = execute_dashboard_query(
            &db,
            &QueryCache::default(),
            "select 1 as n".into(),
            vars(),
            &cancelled,
            false,
            None,
        );
        assert_eq!(v["cancelled"], json!(true));
    }

    #[test]
    fn select_populates_cache() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        let cache = QueryCache::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let v = execute_dashboard_query(
            &db,
            &cache,
            "select 1 as n".into(),
            vars(),
            &cancelled,
            false,
            None,
        );
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["rows"][0]["n"], json!(1));
        let again = execute_dashboard_query(
            &db,
            &cache,
            "select 1 as n".into(),
            vars(),
            &cancelled,
            false,
            None,
        );
        assert_eq!(again["ok"], json!(true));
    }

    #[test]
    fn query_rejects_oauth_tokens_table() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        {
            let conn = db.lock();
            conn.execute_batch(include_str!("../schema.sql")).unwrap();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let v = execute_dashboard_query(
            &db,
            &QueryCache::default(),
            "SELECT access_token, refresh_token FROM oauth_tokens".into(),
            vars(),
            &cancelled,
            false,
            Some("admin".into()),
        );
        assert_eq!(v["ok"], json!(false));
        assert!(
            v["error"].as_str().unwrap_or("").contains("oauth_tokens"),
            "{v}"
        );
    }

    #[test]
    fn query_rejects_unlisted_sql_when_allowlist_on() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        {
            let conn = db.lock();
            conn.execute_batch(include_str!("../schema.sql")).unwrap();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let v = execute_dashboard_query(
            &db,
            &QueryCache::default(),
            "select 1 as n".into(),
            vars(),
            &cancelled,
            true,
            Some("guest".into()),
        );
        assert_eq!(v["ok"], json!(false));
        assert!(
            v["error"]
                .as_str()
                .unwrap_or("")
                .contains("allowed dashboard"),
            "{v}"
        );
    }

    #[test]
    fn query_allows_preview_track_template() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        {
            let conn = db.lock();
            conn.execute_batch(include_str!("../schema.sql")).unwrap();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let v = execute_dashboard_query(
            &db,
            &QueryCache::default(),
            crate::query_allowlist::PREVIEW_TRACK_SQL.into(),
            vars(),
            &cancelled,
            true,
            Some("admin".into()),
        );
        assert_eq!(v["ok"], json!(true), "{v}");
    }
}

