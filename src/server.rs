use crate::app_state::{App, AuthLimiter, QueryCache};
use crate::auth::{AuthState, AuthUser, PasswordBackend};
use crate::db::Db;
use crate::sql::{self, QueryVars};
use anyhow::Result;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tower_http::trace::TraceLayer;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Web;

#[derive(RustEmbed)]
#[folder = "dashboards/"]
struct Dashboards;

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
                    tracing::warn!(
                        "group {group} does not exist yet; any PAM-authenticated user can sign in until you create it"
                    );
                }
            }
        }
        PasswordBackend::Local => tracing::info!("password backend: local (SQLite argon2)"),
    }
    let state = App {
        db,
        auth,
        password_backend,
        limiter: AuthLimiter::default(),
        query_cache: QueryCache::default(),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/cars", get(cars))
        .route("/api/cars/{id}/live", get(car_live))
        .route("/api/cars/{car_id}/drives/{drive_id}", get(car_drive))
        .route("/api/cars/{car_id}/charges/{charge_id}", get(car_charge))
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
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
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
        Some(f) => ([(header::CACHE_CONTROL, "no-store")], Html(f.data.to_vec())).into_response(),
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

async fn car_live(
    _user: AuthUser,
    State(app): State<App>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let view = spawn_db(app.db.clone(), move |conn| crate::live::live_view(conn, id)).await?;
    Ok(match view {
        Some(v) => Json(v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

async fn car_drive(
    _user: AuthUser,
    State(app): State<App>,
    Path((car_id, drive_id)): Path<(i64, i64)>,
) -> Result<Response, AppError> {
    let view = spawn_db(app.db.clone(), move |conn| {
        crate::detail::drive_detail(conn, car_id, drive_id)
    })
    .await?;
    Ok(match view {
        Some(v) => Json(v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
}

async fn car_charge(
    _user: AuthUser,
    State(app): State<App>,
    Path((car_id, charge_id)): Path<(i64, i64)>,
) -> Result<Response, AppError> {
    let view = spawn_db(app.db.clone(), move |conn| {
        crate::detail::charge_detail(conn, car_id, charge_id)
    })
    .await?;
    Ok(match view {
        Some(v) => Json(v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    })
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
) -> Value {
    if cancelled.load(Ordering::Relaxed) {
        return cancelled_json();
    }
    if let Some(payload) = query_stats_override(&sql, cache) {
        return payload;
    }
    if sql.contains("$aux") && !vars.extras.contains_key("aux") {
        let conn = db.read();
        if cancelled.load(Ordering::Relaxed) {
            return cancelled_json();
        }
        let aux =
            crate::db::battery_aux(&conn, vars.car_id, &vars.length_unit, &vars.preferred_range);
        drop(conn);
        vars.extras.insert("aux".into(), aux);
    }
    let translated =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sql::translate(&sql, &vars)))
            .unwrap_or_else(|_| sql.clone());
    let key = cache_key(&translated);
    let t0 = Instant::now();
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
    conn.progress_handler(250, Some(move || flag.load(Ordering::Relaxed)));
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
    loop {
        match mapped.next() {
            Ok(Some(row)) => {
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
    });
    cache.record_exec(&translated, t0.elapsed().as_secs_f64() * 1000.0);
    cache.put(key, payload.clone());
    payload
}

async fn run_query(
    _user: AuthUser,
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
    let payload =
        tokio::task::spawn_blocking(move || execute_dashboard_query(&db, &cache, sql, vars, &flag))
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

async fn dbinfo(_user: AuthUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
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
        );
        assert_eq!(v["cancelled"], json!(true));
    }

    #[test]
    fn select_populates_cache() {
        let db = crate::db::Db::from_write(Connection::open_in_memory().unwrap());
        let cache = QueryCache::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let v = execute_dashboard_query(&db, &cache, "select 1 as n".into(), vars(), &cancelled);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["rows"][0]["n"], json!(1));
        let again =
            execute_dashboard_query(&db, &cache, "select 1 as n".into(), vars(), &cancelled);
        assert_eq!(again["ok"], json!(true));
    }
}
