//! teslamate-rs account authentication: passwords, passkeys, sessions.

use std::sync::Arc;

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::CookieJar;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::db::Db;
use crate::pam_auth;
use crate::users::{self, User};

pub const SESSION_COOKIE: &str = "teslamate_sid";
pub const WA_COOKIE: &str = "teslamate_wa";

pub trait HasAuthDb: Send + Sync {
    fn auth_db(&self) -> &Db;
}

#[derive(Clone)]
pub struct AuthState {
    #[allow(dead_code)]
    pub webauthn: Arc<Webauthn>,
    pub secure_cookie: bool,
    pub rp_id: String,
    pub origin: String,
}

impl AuthState {
    pub fn new(rp_id: &str, origin: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let url = Url::parse(origin)?;
        let webauthn = WebauthnBuilder::new(rp_id, &url)?
            .rp_name("teslamate-rs")
            .build()?;
        Ok(Self {
            webauthn: Arc::new(webauthn),
            secure_cookie: origin.starts_with("https://"),
            rp_id: rp_id.to_string(),
            origin: origin.to_string(),
        })
    }

    pub fn session_cookie(&self, token: &str, headers: &HeaderMap) -> Cookie<'static> {
        apply_cookie(
            Cookie::new(SESSION_COOKIE, token.to_owned()),
            self.secure_from(headers),
        )
    }

    pub fn wa_cookie(&self, id: &str, headers: &HeaderMap) -> Cookie<'static> {
        apply_cookie(Cookie::new(WA_COOKIE, id.to_owned()), self.secure_from(headers))
    }

    pub fn session_cookie_key(&self, headers: &HeaderMap) -> Cookie<'static> {
        apply_cookie(Cookie::from(SESSION_COOKIE), self.secure_from(headers))
    }

    pub fn wa_cookie_key(&self, headers: &HeaderMap) -> Cookie<'static> {
        apply_cookie(Cookie::from(WA_COOKIE), self.secure_from(headers))
    }

    pub fn secure_from(&self, headers: &HeaderMap) -> bool {
        self.secure_cookie || public_origin(headers, &self.origin).starts_with("https://")
    }

    pub fn origin_from(&self, headers: &HeaderMap) -> String {
        public_origin(headers, &self.origin)
    }

    pub fn rp_id_from(&self, headers: &HeaderMap) -> String {
        rp_id_for_origin(&self.origin_from(headers)).unwrap_or_else(|_| self.rp_id.clone())
    }

    pub fn webauthn_from(
        &self,
        headers: &HeaderMap,
    ) -> Result<Webauthn, Box<dyn std::error::Error>> {
        let origin = self.origin_from(headers);
        let rp_id = rp_id_for_origin(&origin)?;
        let url = Url::parse(&origin)?;
        Ok(WebauthnBuilder::new(&rp_id, &url)?
            .rp_name("teslamate-rs")
            .build()?)
    }
}

pub fn public_origin(headers: &HeaderMap, fallback: &str) -> String {
    if let Ok(forced) = std::env::var("TESLAMATE_RS_WEBAUTHN_ORIGIN") {
        if !forced.is_empty() {
            return forced.trim_end_matches('/').to_string();
        }
    }
    let host = if trust_proxy() {
        headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    } else {
        headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let proto = if trust_proxy() {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if fallback.starts_with("https://") {
                    "https"
                } else {
                    "http"
                }
            })
    } else if fallback.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    if let Some(host) = host {
        return format!("{proto}://{host}");
    }
    fallback.trim_end_matches('/').to_string()
}

/// Honor `X-Forwarded-*` / `X-Real-IP` only when explicitly enabled (or when the
/// WebAuthn origin is pinned, which implies a trusted reverse proxy).
pub fn trust_proxy() -> bool {
    match std::env::var("TESLAMATE_RS_TRUST_PROXY") {
        Ok(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => std::env::var("TESLAMATE_RS_WEBAUTHN_ORIGIN")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some(),
    }
}

fn rp_id_for_origin(origin: &str) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(id) = std::env::var("TESLAMATE_RS_WEBAUTHN_RP_ID") {
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let host = Url::parse(origin)?
        .host_str()
        .unwrap_or("localhost")
        .to_string();
    Ok(host)
}

fn apply_cookie(mut cookie: Cookie<'static>, secure: bool) -> Cookie<'static> {
    cookie.set_http_only(true);
    cookie.set_path("/");
    cookie.set_same_site(SameSite::Strict);
    cookie.set_max_age(time::Duration::days(30));
    if secure {
        cookie.set_secure(true);
    }
    cookie
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordBackend {
    Local,
    Pam,
}

impl PasswordBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Pam => "pam",
        }
    }
}

pub fn resolve_password_backend(
    explicit: Option<&str>,
) -> Result<PasswordBackend, Box<dyn std::error::Error>> {
    let raw = explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("TESLAMATE_RS_AUTH")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    if let Some(raw) = raw {
        return parse_password_backend(&raw);
    }
    #[cfg(target_os = "linux")]
    {
        if std::path::Path::new("/etc/pam.d/teslamate-rs").exists() {
            return Ok(PasswordBackend::Pam);
        }
    }
    Ok(PasswordBackend::Local)
}

fn parse_password_backend(raw: &str) -> Result<PasswordBackend, Box<dyn std::error::Error>> {
    match raw.to_ascii_lowercase().as_str() {
        "local" => Ok(PasswordBackend::Local),
        "pam" => {
            #[cfg(target_os = "linux")]
            {
                Ok(PasswordBackend::Pam)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err("PAM password backend is only available on Linux".into())
            }
        }
        other => Err(format!("unknown password backend {other:?} (local or pam)").into()),
    }
}

pub async fn pam_provision_user(
    db: &Db,
    username: &str,
    password: &str,
) -> Result<User, (StatusCode, Json<Value>)> {
    let denied = || api_err(StatusCode::UNAUTHORIZED, "invalid username or password");
    if username.eq_ignore_ascii_case("root") && !pam_auth::allow_root() {
        return Err(denied());
    }
    let user = username.to_string();
    let pass = password.to_string();
    tokio::task::spawn_blocking(move || pam_auth::authenticate(&user, &pass))
        .await
        .map_err(internal_err)?
        .map_err(|_| denied())?;

    if let Some(group) = pam_auth::required_group() {
        let user = username.to_string();
        let grp = group.clone();
        let (exists, member) = tokio::task::spawn_blocking(move || {
            let exists = pam_auth::group_exists(&grp);
            let member = exists && pam_auth::user_in_group(&user, &grp).unwrap_or(false);
            (exists, member)
        })
        .await
        .map_err(internal_err)?;
        if !exists {
            tracing::error!(
                "PAM group {group} does not exist; refusing sign-in until it is created"
            );
            return Err(denied());
        }
        if !member {
            return Err(denied());
        }
    }

    let conn = db.lock();
    users::get_or_create_pam_user(&conn, username).map_err(internal_err)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthUser {
    pub id: i64,
    pub uuid: String,
    pub username: String,
    pub is_admin: bool,
}

impl From<&User> for AuthUser {
    fn from(user: &User) -> Self {
        Self {
            id: user.id,
            uuid: user.uuid.clone(),
            username: user.username.clone(),
            is_admin: user.is_admin,
        }
    }
}

pub struct OptionalUser(pub Option<AuthUser>);
pub struct AdminUser(pub AuthUser);

impl std::ops::Deref for AdminUser {
    type Target = AuthUser;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn api_err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub fn internal_err(err: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!("teslamate-rs auth: {err}");
    api_err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn session_token_from(jar: &CookieJar) -> Option<String> {
    jar.get(SESSION_COOKIE)
        .map(|c| c.value())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

pub fn current_user(jar: &CookieJar, db: &Db) -> Result<Option<AuthUser>, Box<dyn std::error::Error>> {
    let Some(token) = session_token_from(jar) else {
        return Ok(None);
    };
    let conn = db.lock();
    Ok(users::user_from_session(&conn, &token)?.map(|u| AuthUser::from(&u)))
}

impl<S: HasAuthDb> FromRequestParts<S> for AuthUser {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| api_err(StatusCode::UNAUTHORIZED, "sign in required"))?;
        current_user(&jar, state.auth_db())
            .map_err(internal_err)?
            .ok_or_else(|| api_err(StatusCode::UNAUTHORIZED, "sign in required"))
    }
}

impl<S: HasAuthDb> FromRequestParts<S> for OptionalUser {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| api_err(StatusCode::UNAUTHORIZED, "sign in required"))?;
        Ok(OptionalUser(
            current_user(&jar, state.auth_db()).map_err(internal_err)?,
        ))
    }
}

impl<S: HasAuthDb> FromRequestParts<S> for AdminUser {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err(api_err(StatusCode::FORBIDDEN, "admin only"));
        }
        Ok(AdminUser(user))
    }
}

pub fn session_token_from_jar(jar: &CookieJar) -> Option<String> {
    session_token_from(jar)
}

pub fn random_token() -> String {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn normalize_username(raw: &str) -> Result<String, String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.len() < 3 || name.len() > 32 {
        return Err("username must be 3–32 characters".into());
    }
    let ok = name.chars().enumerate().all(|(i, c)| {
        c.is_ascii_alphanumeric() || ((c == '.' || c == '_' || c == '-') && i > 0)
    });
    if !ok {
        return Err("username may contain letters, numbers, '.', '_' and '-'".into());
    }
    Ok(name)
}

pub fn hash_password(password: &str) -> Result<String, Box<dyn std::error::Error>> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn issue_session(
    jar: CookieJar,
    state: &AuthState,
    db: &Db,
    user_id: i64,
    headers: &HeaderMap,
) -> Result<CookieJar, Box<dyn std::error::Error>> {
    let token = random_token();
    let conn = db.lock();
    users::create_session(&conn, user_id, &token)?;
    Ok(jar.add(state.session_cookie(&token, headers)))
}

#[allow(dead_code)]
pub fn uuid_from_user(user: &User) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(Uuid::parse_str(&user.uuid)?)
}

pub fn passkeys_of(db: &Db, user_id: i64) -> Result<Vec<Passkey>, Box<dyn std::error::Error>> {
    let conn = db.lock();
    let rows = users::passkeys_for_user(&conn, user_id)?;
    drop(conn);
    let mut out = Vec::new();
    for (_, _, json) in rows {
        out.push(serde_json::from_str(&json)?);
    }
    Ok(out)
}

pub fn store_passkey(
    db: &Db,
    user_id: i64,
    passkey: &Passkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let cred_id = URL_SAFE_NO_PAD.encode(passkey.cred_id());
    let json = serde_json::to_string(passkey)?;
    let conn = db.lock();
    users::insert_passkey(&conn, user_id, &cred_id, &json)?;
    Ok(())
}

pub fn update_stored_passkey(
    db: &Db,
    user_id: i64,
    passkey: &Passkey,
) -> Result<(), Box<dyn std::error::Error>> {
    let cred_id = URL_SAFE_NO_PAD.encode(passkey.cred_id());
    let json = serde_json::to_string(passkey)?;
    let conn = db.lock();
    users::update_passkey_json(&conn, user_id, &cred_id, &json)
}

pub fn user_can_drop_passkey(
    user: &User,
    remaining_after_delete: i64,
    backend: PasswordBackend,
) -> Result<(), String> {
    if remaining_after_delete > 0 {
        return Ok(());
    }
    if user.password_hash.is_some() || backend == PasswordBackend::Pam {
        return Ok(());
    }
    Err("add a password before removing the last passkey".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    #[test]
    fn username_rules() {
        assert!(normalize_username("ab").is_err());
        assert_eq!(normalize_username("Admin").unwrap(), "admin");
        assert!(normalize_username("bad name").is_err());
        assert!(normalize_username("ok_user-1").is_ok());
        assert!(normalize_username(".admin").is_err());
        assert_eq!(normalize_username("  Ok_User-2  ").unwrap(), "ok_user-2");
    }

    #[test]
    fn origin_follows_nginx_forwarded_https() {
        let prev_trust = std::env::var("TESLAMATE_RS_TRUST_PROXY").ok();
        let prev_origin = std::env::var("TESLAMATE_RS_WEBAUTHN_ORIGIN").ok();
        std::env::set_var("TESLAMATE_RS_TRUST_PROXY", "1");
        std::env::remove_var("TESLAMATE_RS_WEBAUTHN_ORIGIN");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "tm.example.com".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:4010".parse().unwrap());
        let origin = public_origin(&headers, "http://localhost:4010");
        assert_eq!(origin, "https://tm.example.com");
        let state = AuthState::new("localhost", "http://localhost:4010").unwrap();
        assert!(state.secure_from(&headers));
        assert_eq!(state.rp_id_from(&headers), "tm.example.com");
        match prev_trust {
            Some(v) => std::env::set_var("TESLAMATE_RS_TRUST_PROXY", v),
            None => std::env::remove_var("TESLAMATE_RS_TRUST_PROXY"),
        }
        match prev_origin {
            Some(v) => std::env::set_var("TESLAMATE_RS_WEBAUTHN_ORIGIN", v),
            None => std::env::remove_var("TESLAMATE_RS_WEBAUTHN_ORIGIN"),
        }
    }

    #[test]
    fn forwarded_headers_ignored_without_trust_proxy() {
        let prev_trust = std::env::var("TESLAMATE_RS_TRUST_PROXY").ok();
        let prev_origin = std::env::var("TESLAMATE_RS_WEBAUTHN_ORIGIN").ok();
        std::env::remove_var("TESLAMATE_RS_TRUST_PROXY");
        std::env::remove_var("TESLAMATE_RS_WEBAUTHN_ORIGIN");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "evil.example".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:4010".parse().unwrap());
        assert_eq!(
            public_origin(&headers, "http://localhost:4010"),
            "http://127.0.0.1:4010"
        );
        match prev_trust {
            Some(v) => std::env::set_var("TESLAMATE_RS_TRUST_PROXY", v),
            None => std::env::remove_var("TESLAMATE_RS_TRUST_PROXY"),
        }
        match prev_origin {
            Some(v) => std::env::set_var("TESLAMATE_RS_WEBAUTHN_ORIGIN", v),
            None => std::env::remove_var("TESLAMATE_RS_WEBAUTHN_ORIGIN"),
        }
    }

    #[test]
    fn origin_header_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        headers.insert(header::HOST, "mate.lan".parse().unwrap());
        assert_eq!(
            public_origin(&headers, "http://localhost:4010"),
            "http://mate.lan"
        );
    }

    #[test]
    fn resolve_backend_local() {
        assert_eq!(
            resolve_password_backend(Some("local")).unwrap(),
            PasswordBackend::Local
        );
    }

    #[test]
    fn user_can_drop_passkey_rules() {
        let with_password = User {
            id: 1,
            uuid: "u".into(),
            username: "admin".into(),
            is_admin: true,
            password_hash: Some("hash".into()),
        };
        let passkey_only = User {
            password_hash: None,
            ..with_password.clone()
        };
        assert!(user_can_drop_passkey(&with_password, 0, PasswordBackend::Local).is_ok());
        assert!(user_can_drop_passkey(&passkey_only, 1, PasswordBackend::Local).is_ok());
        assert!(user_can_drop_passkey(&passkey_only, 0, PasswordBackend::Local).is_err());
        assert!(user_can_drop_passkey(&passkey_only, 0, PasswordBackend::Pam).is_ok());
    }

    #[test]
    fn random_token_is_unique() {
        assert_ne!(random_token(), random_token());
    }
}
