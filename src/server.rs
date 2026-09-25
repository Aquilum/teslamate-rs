use crate::app_state::{App, AuthLimiter, QueryCache, QueryGate};
use crate::auth::{AdminUser, AuthState, AuthUser, PasswordBackend};
use crate::db::Db;
use crate::sql::{self, QueryVars};
use anyhow::Result;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
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
        query_gate: QueryGate::default(),
        setup_allowed,
        location_jobs: crate::app_state::LocationJobs::default(),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/cars", get(cars))
        .route("/api/cars/{id}/live", get(car_live))
        .route("/api/cars/{car_id}/drives/{drive_id}", get(car_drive))
        .route("/api/cars/{car_id}/charges/{charge_id}", get(car_charge))
        .route("/api/settings", get(settings))
        .route("/api/geofences", get(list_geofences).post(create_geofence))
        .route("/api/geofences/rematch", post(rematch_geofences))
        .route("/api/geofences/job", get(location_job_status))
        .route("/api/geofences/{id}", put(update_geofence).delete(delete_geofence))
        .route("/api/dashboards", get(list_dashboards))
        .route("/api/dashboards/{*path}", get(get_dashboard))
        .route("/api/query", post(run_query))
        .route("/api/dbinfo", get(dbinfo))
        .merge(crate::auth_http::router())
        .route("/{*path}", get(static_file))
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    tracing::info!("teslamate-rs listening on http://{bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn csrf_guard(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let mutating = matches!(
        method.as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    );
    if mutating {
        let has_session = req
            .headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains(crate::auth::SESSION_COOKIE));
        if has_session && !csrf_origin_ok(req.headers()) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "ok": false, "error": "cross-origin request blocked" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

fn csrf_origin_ok(headers: &axum::http::HeaderMap) -> bool {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
    {
        if site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none") {
            return true;
        }
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    let expected = crate::auth::public_origin(headers, "", None);
    if expected.is_empty() {
        return true;
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return origin_matches(origin, &expected);
    }
    if let Some(referer) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
        return url::Url::parse(referer)
            .ok()
            .is_some_and(|url| origin_matches(url.origin().ascii_serialization().as_str(), &expected));
    }
    // Browser fetch to same origin usually sends Origin; allow missing for non-browser clients on loopback-style Host.
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| h.starts_with("127.0.0.1") || h.starts_with("localhost"))
}

fn origin_matches(origin: &str, expected: &str) -> bool {
    origin.trim_end_matches('/') == expected.trim_end_matches('/')
}

async fn security_headers(req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let https = request_is_https(req.headers(), peer);
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
    if https {
        headers.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    res
}

fn request_is_https(headers: &axum::http::HeaderMap, peer: Option<SocketAddr>) -> bool {
    if let Ok(origin) = std::env::var("TESLAMATE_RS_WEBAUTHN_ORIGIN") {
        if origin.starts_with("https://") {
            return true;
        }
    }
    if crate::auth::trust_forwarded_proto(peer) {
        return headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    }
    false
}

/// Reject traversal tricks before rust-embed lookups.
pub fn safe_embed_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != ".." && seg != ".")
}

fn dashboard_path_allowed(path: &str) -> bool {
    safe_embed_path(path) && path.ends_with(".json") && !path.starts_with("internal/")
}

async fn index() -> impl IntoResponse {
    match Web::get("index.html") {
        Some(f) => ([(header::CACHE_CONTROL, "no-store")], Html(f.data.to_vec())).into_response(),
        None => (StatusCode::NOT_FOUND, "missing web/index.html").into_response(),
    }
}

async fn static_file(Path(path): Path<String>) -> Response {
    if !safe_embed_path(&path) {
        return StatusCode::NOT_FOUND.into_response();
    }
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

async fn list_geofences(
    _user: AuthUser,
    State(app): State<App>,
) -> Result<Json<Value>, AppError> {
    spawn_db(app.db.clone(), |conn| crate::locations::list(conn)).await.map(Json)
}

async fn create_geofence(
    _user: AuthUser,
    State(app): State<App>,
    Json(body): Json<crate::locations::GeofenceInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Err(e) = crate::locations::validate(&body) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e.to_string()}))));
    }
    let db = app.db.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = db.lock();
        crate::locations::save(&mut conn, None, body)
    })
    .await
    .map_err(|e| crate::auth::internal_err(format!("location task: {e}")))?
    .map_err(crate::auth::internal_err)?;
    let job = app.location_jobs.start(app.db.clone());
    Ok(Json(json!({"ok": true, "location": result, "job": job})))
}

async fn update_geofence(
    _user: AuthUser,
    State(app): State<App>,
    Path(id): Path<i64>,
    Json(body): Json<crate::locations::GeofenceInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Err(e) = crate::locations::validate(&body) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e.to_string()}))));
    }
    let db = app.db.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = db.lock();
        crate::locations::save(&mut conn, Some(id), body)
    })
    .await
    .map_err(|e| crate::auth::internal_err(format!("location task: {e}")))?
    .map_err(|e| {
        if e.to_string() == "location not found" {
            (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "location not found"})))
        } else {
            crate::auth::internal_err(e)
        }
    })?;
    let job = app.location_jobs.start(app.db.clone());
    Ok(Json(json!({"ok": true, "location": result, "job": job})))
}

async fn delete_geofence(
    _user: AuthUser,
    State(app): State<App>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = app.db.clone();
    let removed = tokio::task::spawn_blocking(move || {
        let mut conn = db.lock();
        crate::locations::delete(&mut conn, id)
    })
    .await
    .map_err(|e| crate::auth::internal_err(format!("location task: {e}")))?
    .map_err(crate::auth::internal_err)?;
    if !removed {
        return Err((StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "location not found"}))));
    }
    let job = app.location_jobs.start(app.db.clone());
    Ok(Json(json!({"ok": true, "job": job})))
}

async fn rematch_geofences(
    _user: AuthUser,
    State(app): State<App>,
) -> Json<Value> {
    Json(json!({"ok": true, "job": app.location_jobs.start(app.db.clone())}))
}

async fn location_job_status(
    _user: AuthUser,
    State(app): State<App>,
) -> Json<Value> {
    Json(app.location_jobs.status())
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
    if !dashboard_path_allowed(&path) {
        return StatusCode::NOT_FOUND.into_response();
    }
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
        let aux =
            crate::db::battery_aux(&conn, vars.car_id, &vars.length_unit, &vars.preferred_range);
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
            tracing::warn!("query prepare failed: {e}; sql={}", preview_sql_log(&translated));
            return json!({ "ok": false, "error": "query failed" });
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
            tracing::warn!("query start failed: {e}; sql={}", preview_sql_log(&translated));
            return json!({ "ok": false, "error": "query failed" });
        }
    };
    let mut truncated = false;
    loop {
        if Instant::now() >= deadline {
            tracing::warn!(
                "query timed out after {}ms; sql={}",
                query_timeout().as_millis(),
                preview_sql_log(&translated)
            );
            return json!({
                "ok": false,
                "error": format!("query timed out after {}ms", query_timeout().as_millis()),
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
                            tracing::warn!("query row failed: {e}");
                            return json!({ "ok": false, "error": "query failed" });
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
                tracing::warn!("query iterate failed: {e}");
                return json!({ "ok": false, "error": "query failed" });
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

fn preview_sql_log(sql: &str) -> String {
    sql.chars().take(160).collect::<String>().replace('\n', " ")
}

async fn run_query(
    user: AuthUser,
    State(app): State<App>,
    Json(body): Json<QueryBody>,
) -> Result<Response, AppError> {
    let rate_key = format!("query:{}", user.id);
    if !app.limiter.allow_budget(
        &rate_key,
        crate::app_state::query_rate_max(),
        crate::app_state::query_rate_window(),
    ) {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "ok": false, "error": "query rate limit exceeded" })),
        )
            .into_response());
    }
    let _permits = match app.query_gate.try_acquire(user.id).await {
        Ok(p) => p,
        Err(()) => {
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "ok": false, "error": "too many queries" })),
            )
                .into_response());
        }
    };
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
    Ok(Json(payload).into_response())
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
            let Some(quoted) = sql::quote_ident(&name) else {
                continue;
            };
            let n: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM {quoted}"),
                [],
                |r| r.get(0),
            )?;
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
        tracing::error!("teslamate-rs api: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": "internal error" })),
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

    #[test]
    fn embed_paths_reject_traversal() {
        assert!(safe_embed_path("app.js"));
        assert!(safe_embed_path("reports/dutch-tax.json"));
        assert!(!safe_embed_path(""));
        assert!(!safe_embed_path("/app.js"));
        assert!(!safe_embed_path("../index.html"));
        assert!(!safe_embed_path("foo/../bar.json"));
        assert!(dashboard_path_allowed("overview.json"));
        assert!(!dashboard_path_allowed("internal/hidden.json"));
        assert!(!dashboard_path_allowed("overview.json/.."));
    }

    #[test]
    fn csrf_referer_requires_matching_origin() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::HOST, "localhost:4010".parse().unwrap());
        headers.insert(header::REFERER, "http://localhost:4010.evil.example/page".parse().unwrap());
        assert!(!csrf_origin_ok(&headers));
        headers.insert(header::REFERER, "http://localhost:4010/page".parse().unwrap());
        assert!(csrf_origin_ok(&headers));
    }
}
