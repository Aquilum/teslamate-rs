//! Login, passkeys, invites — AstonMate-shaped auth HTTP API.

use crate::app_state::App;
use crate::auth::{
    self, api_err, hash_password, internal_err, issue_session, normalize_username, pam_provision_user,
    passkeys_of, session_token_from_jar, store_passkey, update_stored_passkey, verify_password,
    AdminUser, AuthUser, OptionalUser, PasswordBackend, WA_COOKIE,
};
use crate::users::{self, User};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use uuid::Uuid;
use webauthn_rs::prelude::*;

pub fn router() -> Router<App> {
    Router::new()
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/setup", post(auth_setup))
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/logout-all", post(auth_logout_all))
        .route("/api/auth/webauthn/register/start", post(wa_register_start))
        .route("/api/auth/webauthn/register/finish", post(wa_register_finish))
        .route("/api/auth/webauthn/login/start", post(wa_login_start))
        .route("/api/auth/webauthn/login/finish", post(wa_login_finish))
        .route("/api/auth/passkeys", get(list_passkeys))
        .route("/api/auth/passkeys/{id}", delete(delete_passkey))
        .route("/api/admin/invites", get(list_invites).post(create_invite))
}

fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let peer_loopback = peer.map(|p| p.ip().is_loopback()).unwrap_or(false);
    if auth::trust_proxy() && peer_loopback {
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return ip.to_string();
        }
    }
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "local".to_string())
}

fn rate_limit(
    app: &App,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<(), (StatusCode, Json<Value>)> {
    if app.limiter.allow(&client_ip(headers, peer)) {
        Ok(())
    } else {
        Err(api_err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, try later",
        ))
    }
}

fn setup_conflict(err: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    let msg = err.to_string();
    if msg.contains("already set up") {
        api_err(StatusCode::CONFLICT, "already set up")
    } else {
        internal_err(msg)
    }
}

fn request_webauthn(app: &App, headers: &HeaderMap) -> Result<Webauthn, (StatusCode, Json<Value>)> {
    app.auth.webauthn_from(headers).map_err(|_| {
        api_err(
            StatusCode::BAD_REQUEST,
            "passkeys need the public HTTPS origin nginx advertises (Host + X-Forwarded-Proto)",
        )
    })
}

async fn auth_status(
    State(app): State<App>,
    headers: HeaderMap,
    user: OptionalUser,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let setup_required = {
        let conn = app.db.lock();
        users::setup_required(&conn).map_err(internal_err)?
    };
    let mut body = json!({
        "setupRequired": setup_required,
        "setupAllowed": app.setup_allowed,
        "authenticated": user.0.is_some(),
        "passwordBackend": app.password_backend.as_str(),
        "registerEnabled": app.password_backend == PasswordBackend::Local,
        "webauthn": {
            "rpId": app.auth.rp_id_from(&headers),
            "origin": app.auth.origin_from(&headers),
        },
    });
    if let Some(user) = user.0 {
        let passkeys = {
            let conn = app.db.lock();
            users::list_passkeys(&conn, user.id).map_err(internal_err)?
        };
        body["user"] = json!({
            "id": user.id,
            "username": user.username,
            "isAdmin": user.is_admin,
        });
        body["passkeys"] = serde_json::to_value(passkeys).unwrap_or(json!([]));
    }
    Ok(Json(body))
}

#[derive(Deserialize)]
struct UsernamePassword {
    username: String,
    password: Option<String>,
    invite: Option<String>,
}

async fn auth_setup(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<UsernamePassword>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    rate_limit(&app, &headers, Some(peer))?;
    if !app.setup_allowed {
        return Err(api_err(
            StatusCode::FORBIDDEN,
            "first-admin setup is locked; bind to loopback or set TESLAMATE_RS_ALLOW_SETUP=1",
        ));
    }
    {
        let conn = app.db.lock();
        if !users::setup_required(&conn).map_err(internal_err)? {
            return Err(api_err(StatusCode::CONFLICT, "already set up"));
        }
    }
    if app.password_backend == PasswordBackend::Pam {
        let username = normalize_username(&body.username).map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
        let password = body.password.unwrap_or_default();
        let user = pam_provision_user(&app.db, &username, &password).await?;
        let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
        crate::audit::record(&app.db, Some(&user.username), "setup", "first admin (pam)");
        return Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))));
    }
    let username = normalize_username(&body.username).map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    let password = body.password.as_deref().unwrap_or("");
    let hash = hash_password(password).map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let user = {
        let conn = app.db.lock();
        users::create_first_admin(&conn, &username, Some(&hash), None).map_err(setup_conflict)?
    };
    let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
    crate::audit::record(&app.db, Some(&user.username), "setup", "first admin (local)");
    Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
}

async fn auth_register(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<UsernamePassword>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    rate_limit(&app, &headers, Some(peer))?;
    if app.password_backend == PasswordBackend::Pam {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "this server uses Linux accounts (PAM); sign in with your username",
        ));
    }
    {
        let conn = app.db.lock();
        if users::setup_required(&conn).map_err(internal_err)? {
            return Err(api_err(StatusCode::BAD_REQUEST, "create the first admin account first"));
        }
    }
    let invite = body
        .invite
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| api_err(StatusCode::FORBIDDEN, "an invite is required"))?;
    let username = normalize_username(&body.username).map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    let password = body.password.as_deref().unwrap_or("");
    let hash = hash_password(password).map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let user = {
        let conn = app.db.lock();
        users::insert_user_with_invite(&conn, &username, Some(&hash), invite, None).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("invite") || msg.contains("taken") {
                api_err(StatusCode::BAD_REQUEST, msg)
            } else {
                internal_err(e)
            }
        })?
    };
    let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
    crate::audit::record(&app.db, Some(&user.username), "register", "invite redeemed");
    Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
}

async fn auth_login(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<UsernamePassword>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    rate_limit(&app, &headers, Some(peer))?;
    let username = normalize_username(&body.username).map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    let password = body.password.unwrap_or_default();
    if app.password_backend == PasswordBackend::Pam {
        let user = pam_provision_user(&app.db, &username, &password).await?;
        let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
        crate::audit::record(&app.db, Some(&user.username), "login", "pam");
        return Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))));
    }
    let user = {
        let conn = app.db.lock();
        users::get_user_by_username(&conn, &username).map_err(internal_err)?
    };
    let Some(user) = user else {
        crate::audit::record(&app.db, Some(&username), "login_failed", "unknown user");
        return Err(api_err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    };
    let Some(hash) = user.password_hash.as_deref() else {
        crate::audit::record(&app.db, Some(&username), "login_failed", "no password");
        return Err(api_err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    };
    if !verify_password(&password, hash) {
        crate::audit::record(&app.db, Some(&username), "login_failed", "bad password");
        return Err(api_err(StatusCode::UNAUTHORIZED, "invalid username or password"));
    }
    let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
    crate::audit::record(&app.db, Some(&user.username), "login", "local");
    Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
}

async fn auth_logout(
    State(app): State<App>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    if let Some(token) = session_token_from_jar(&jar) {
        let conn = app.db.lock();
        let _ = users::delete_session(&conn, &token);
    }
    Ok((
        jar.remove(app.auth.session_cookie_key(&headers)),
        Json(json!({ "ok": true })),
    ))
}

async fn auth_logout_all(
    user: AuthUser,
    State(app): State<App>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    let n = {
        let conn = app.db.lock();
        users::delete_sessions_for_user(&conn, user.id).map_err(internal_err)?
    };
    crate::audit::record(
        &app.db,
        Some(&user.username),
        "logout_all",
        &format!("revoked {n} session(s)"),
    );
    Ok((
        jar.remove(app.auth.session_cookie_key(&headers)),
        Json(json!({ "ok": true, "revoked": n })),
    ))
}

#[derive(Deserialize)]
struct RegisterStart {
    username: Option<String>,
    invite: Option<String>,
}

async fn wa_register_start(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    user: OptionalUser,
    Json(body): Json<RegisterStart>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    if user.0.is_none() {
        rate_limit(&app, &headers, Some(peer))?;
    }
    let exclude: Option<Vec<CredentialID>> = None;
    let (purpose, user_id, username, invite_hash, uuid, display) = if let Some(user) = user.0 {
        let uuid = Uuid::parse_str(&user.uuid).map_err(internal_err)?;
        (
            "add",
            Some(user.id),
            Some(user.username.clone()),
            None,
            uuid,
            user.username,
        )
    } else if {
        let conn = app.db.lock();
        users::setup_required(&conn).map_err(internal_err)?
    } {
        if !app.setup_allowed {
            return Err(api_err(
                StatusCode::FORBIDDEN,
                "first-admin setup is locked; bind to loopback or set TESLAMATE_RS_ALLOW_SETUP=1",
            ));
        }
        if app.password_backend == PasswordBackend::Pam {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "sign in with your Linux username first",
            ));
        }
        let username = normalize_username(body.username.as_deref().unwrap_or(""))
            .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
        (
            "setup",
            None,
            Some(username.clone()),
            None,
            Uuid::new_v4(),
            username,
        )
    } else {
        let invite = body
            .invite
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| api_err(StatusCode::FORBIDDEN, "an invite is required"))?;
        if app.password_backend == PasswordBackend::Pam {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "this server uses Linux accounts (PAM); sign in with your username",
            ));
        }
        let username = normalize_username(body.username.as_deref().unwrap_or(""))
            .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
        {
            let conn = app.db.lock();
            if users::peek_invite(&conn, invite)
                .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?
                .is_none()
            {
                return Err(api_err(StatusCode::BAD_REQUEST, "invite is invalid or expired"));
            }
        }
        (
            "register",
            None,
            Some(username.clone()),
            Some(users::hash_secret(invite)),
            Uuid::new_v4(),
            username,
        )
    };

    let uuid_str = uuid.to_string();
    let webauthn = request_webauthn(&app, &headers)?;
    let (ccr, state) = webauthn
        .start_passkey_registration(uuid, &display, &display, exclude)
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let challenge_id = auth::random_token();
    let state_json = serde_json::to_string(&state).map_err(internal_err)?;
    {
        let conn = app.db.lock();
        users::store_webauthn_challenge(
            &conn,
            &challenge_id,
            purpose,
            user_id,
            username.as_deref(),
            Some(&uuid_str),
            invite_hash.as_deref(),
            &state_json,
        )
        .map_err(internal_err)?;
    }
    let jar = jar.add(app.auth.wa_cookie(&challenge_id, &headers));
    Ok((jar, Json(serde_json::to_value(ccr).map_err(internal_err)?)))
}

#[derive(Deserialize)]
struct WebauthnFinish {
    credential: Value,
    username: Option<String>,
    invite: Option<String>,
}

async fn wa_register_finish(
    State(app): State<App>,
    headers: HeaderMap,
    jar: CookieJar,
    user: OptionalUser,
    Json(body): Json<WebauthnFinish>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    let challenge_id = jar
        .get(WA_COOKIE)
        .map(|c| c.value().to_string())
        .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "missing passkey challenge"))?;
    let challenge = {
        let conn = app.db.lock();
        users::take_webauthn_challenge(&conn, &challenge_id).map_err(internal_err)?
    }
    .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "passkey challenge expired"))?;
    let state: PasskeyRegistration =
        serde_json::from_str(&challenge.state_json).map_err(internal_err)?;
    let cred: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let passkey = request_webauthn(&app, &headers)?
        .finish_passkey_registration(&cred, &state)
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;

    let mut jar = jar.remove(app.auth.wa_cookie_key(&headers));
    match challenge.purpose.as_str() {
        "add" => {
            let session = user
                .0
                .ok_or_else(|| api_err(StatusCode::UNAUTHORIZED, "sign in required"))?;
            let user_id = challenge
                .user_id
                .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "not signed in"))?;
            if session.id != user_id {
                return Err(api_err(
                    StatusCode::FORBIDDEN,
                    "passkey challenge does not match this session",
                ));
            }
            store_passkey(&app.db, user_id, &passkey).map_err(internal_err)?;
            Ok((jar, Json(json!({ "ok": true }))))
        }
        "setup" => {
            if !app.setup_allowed {
                return Err(api_err(
                    StatusCode::FORBIDDEN,
                    "first-admin setup is locked; bind to loopback or set TESLAMATE_RS_ALLOW_SETUP=1",
                ));
            }
            {
                let conn = app.db.lock();
                if !users::setup_required(&conn).map_err(internal_err)? {
                    return Err(api_err(StatusCode::CONFLICT, "already set up"));
                }
            }
            let username = challenge
                .username
                .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "username required"))?;
            let user = {
                let conn = app.db.lock();
                users::create_first_admin(&conn, &username, None, challenge.user_uuid.as_deref())
                    .map_err(setup_conflict)?
            };
            store_passkey(&app.db, user.id, &passkey).map_err(internal_err)?;
            jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
            crate::audit::record(&app.db, Some(&user.username), "setup", "first admin (passkey)");
            Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
        }
        "register" => {
            let username = challenge
                .username
                .or(body.username)
                .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "username required"))?;
            let invite_hash = challenge
                .invite_token_hash
                .ok_or_else(|| api_err(StatusCode::FORBIDDEN, "an invite is required"))?;
            let invite = body.invite.unwrap_or_default();
            if users::hash_secret(&invite) != invite_hash {
                return Err(api_err(StatusCode::BAD_REQUEST, "invite is invalid or expired"));
            }
            let user = {
                let conn = app.db.lock();
                users::insert_user_with_invite(
                    &conn,
                    &username,
                    None,
                    &invite,
                    challenge.user_uuid.as_deref(),
                )
                .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?
            };
            store_passkey(&app.db, user.id, &passkey).map_err(internal_err)?;
            jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
            crate::audit::record(&app.db, Some(&user.username), "register", "invite redeemed (passkey)");
            Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
        }
        other => Err(api_err(
            StatusCode::BAD_REQUEST,
            format!("unexpected passkey purpose {other}"),
        )),
    }
}

#[derive(Deserialize)]
struct LoginStart {
    username: Option<String>,
}

async fn wa_login_start(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<LoginStart>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    rate_limit(&app, &headers, Some(peer))?;
    let failed = || api_err(StatusCode::UNAUTHORIZED, "passkey sign-in failed");
    let raw = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "username is required for a passkey sign-in"))?;
    let name = normalize_username(raw).map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    let user = {
        let conn = app.db.lock();
        users::get_user_by_username(&conn, &name).map_err(internal_err)?
    }
    .ok_or_else(failed)?;
    let keys = passkeys_of(&app.db, user.id).map_err(internal_err)?;
    if keys.is_empty() {
        return Err(failed());
    }
    let (ccr, state) = request_webauthn(&app, &headers)?
        .start_passkey_authentication(&keys)
        .map_err(|_| api_err(StatusCode::BAD_REQUEST, "passkey sign-in failed"))?;
    let state_json = serde_json::to_string(&state).map_err(internal_err)?;
    let challenge_id = auth::random_token();
    {
        let conn = app.db.lock();
        users::store_webauthn_challenge(
            &conn,
            &challenge_id,
            "login",
            Some(user.id),
            Some(&name),
            Some(&user.uuid),
            None,
            &state_json,
        )
        .map_err(internal_err)?;
    }
    Ok((
        jar.add(app.auth.wa_cookie(&challenge_id, &headers)),
        Json(serde_json::to_value(ccr).map_err(internal_err)?),
    ))
}

async fn wa_login_finish(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<WebauthnFinish>,
) -> Result<(CookieJar, Json<Value>), (StatusCode, Json<Value>)> {
    rate_limit(&app, &headers, Some(peer))?;
    let challenge_id = jar
        .get(WA_COOKIE)
        .map(|c| c.value().to_string())
        .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "missing passkey challenge"))?;
    let challenge = {
        let conn = app.db.lock();
        users::take_webauthn_challenge(&conn, &challenge_id).map_err(internal_err)?
    }
    .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "passkey challenge expired"))?;
    let cred: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| api_err(StatusCode::BAD_REQUEST, "passkey sign-in failed"))?;
    let state: PasskeyAuthentication =
        serde_json::from_str(&challenge.state_json).map_err(internal_err)?;
    let result = request_webauthn(&app, &headers)?
        .finish_passkey_authentication(&cred, &state)
        .map_err(|_| api_err(StatusCode::UNAUTHORIZED, "passkey sign-in failed"))?;
    let user_id = challenge
        .user_id
        .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "username required"))?;
    let user = {
        let conn = app.db.lock();
        users::get_user_by_id(&conn, user_id).map_err(internal_err)?
    }
    .ok_or_else(|| api_err(StatusCode::UNAUTHORIZED, "passkey sign-in failed"))?;
    let keys = passkeys_of(&app.db, user.id).map_err(internal_err)?;
    apply_auth_result(&app, &user, &result, &keys)?;

    let jar = jar.remove(app.auth.wa_cookie_key(&headers));
    let jar = issue_session(jar, &app.auth, &app.db, user.id, &headers).map_err(internal_err)?;
    crate::audit::record(&app.db, Some(&user.username), "login", "passkey");
    Ok((jar, Json(json!({ "ok": true, "user": AuthUser::from(&user) }))))
}

fn apply_auth_result(
    app: &App,
    user: &User,
    result: &AuthenticationResult,
    keys: &[Passkey],
) -> Result<(), (StatusCode, Json<Value>)> {
    if !result.user_verified() {
        return Err(api_err(StatusCode::UNAUTHORIZED, "passkey was not verified"));
    }
    for key in keys {
        let mut updated = key.clone();
        if updated.update_credential(result) == Some(true) {
            update_stored_passkey(&app.db, user.id, &updated).map_err(internal_err)?;
            break;
        }
    }
    Ok(())
}

async fn list_passkeys(
    State(app): State<App>,
    user: AuthUser,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let keys = {
        let conn = app.db.lock();
        users::list_passkeys(&conn, user.id).map_err(internal_err)?
    };
    Ok(Json(json!({ "passkeys": keys })))
}

async fn delete_passkey(
    State(app): State<App>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    {
        let conn = app.db.lock();
        let full = users::get_user_by_id(&conn, user.id)
            .map_err(internal_err)?
            .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "missing user"))?;
        let n = users::passkey_count(&conn, user.id).map_err(internal_err)?;
        auth::user_can_drop_passkey(&full, n - 1, app.password_backend)
            .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
        if !users::delete_passkey(&conn, user.id, id).map_err(internal_err)? {
            return Err(api_err(StatusCode::BAD_REQUEST, "passkey not found"));
        }
    }
    Ok(Json(json!({ "ok": true })))
}

async fn list_invites(admin: AdminUser, State(app): State<App>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _ = admin;
    let invites = {
        let conn = app.db.lock();
        users::list_invites(&conn).map_err(internal_err)?
    };
    Ok(Json(json!({ "invites": invites })))
}

async fn create_invite(
    admin: AdminUser,
    State(app): State<App>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let token = auth::random_token();
    let info = {
        let conn = app.db.lock();
        users::create_invite(&conn, admin.id, &token).map_err(internal_err)?
    };
    crate::audit::record(
        &app.db,
        Some(&admin.username),
        "invite_create",
        &format!("id={}", info.id),
    );
    Ok(Json(json!({
        "id": info.id,
        "token": token,
        "expiresAt": info.expires_at,
        "url": format!("/#register?invite={token}"),
    })))
}
