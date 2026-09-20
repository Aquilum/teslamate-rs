use crate::app_state::{App, AuthLimiter};
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
use rust_embed::RustEmbed;
use rusqlite::types::ValueRef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
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

async fn cars(_user: AuthUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
    let conn = app.db.lock();
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
}

async fn settings(_user: AuthUser, State(app): State<App>) -> Result<Json<Value>, AppError> {
    let conn = app.db.lock();
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

async fn run_query(
    _user: AuthUser,
    State(app): State<App>,
    Json(body): Json<QueryBody>,
) -> Result<Json<Value>, AppError> {
    let mut vars = body.vars.clone();
    if body.sql.contains("$aux") && !vars.extras.contains_key("aux") {
        let conn = app.db.lock();
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
        sql::translate(&body.sql, &vars)
    }))
    .unwrap_or_else(|_| body.sql.clone());
    let conn = app.db.lock();
    let mut stmt = match conn.prepare(&translated) {
        Ok(s) => s,
        Err(e) => {
            return Ok(Json(json!({
                "ok": false,
                "error": e.to_string(),
                "sql": translated,
            })));
        }
    };
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = Vec::new();
    let mut mapped = stmt.query([])?;
    while let Some(row) = mapped.next()? {
        let mut obj = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            obj.insert(name.clone(), sqlite_to_json(row.get_ref(i)?));
        }
        rows.push(Value::Object(obj));
    }
    Ok(Json(json!({
        "ok": true,
        "columns": names,
        "rows": rows,
        "sql": translated,
    })))
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
    let conn = app.db.lock();
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

