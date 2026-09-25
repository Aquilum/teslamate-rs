//! Web UI accounts: users, sessions, invites, passkeys.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const INVITE_DAYS: i64 = 7;
const WEBAUTHN_MINUTES: i64 = 10;
const INVITE_UNUSED_MAX_DEFAULT: i64 = 20;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: i64,
    pub uuid: String,
    pub username: String,
    pub is_admin: bool,
    #[serde(skip)]
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyInfo {
    pub id: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteInfo {
    pub id: i64,
    pub expires_at: String,
    pub created_at: String,
    pub used: bool,
}

pub struct WebauthnChallenge {
    pub purpose: String,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub user_uuid: Option<String>,
    pub invite_token_hash: Option<String>,
    pub state_json: String,
}

pub fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn map_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        uuid: row.get(1)?,
        username: row.get(2)?,
        password_hash: row.get(3)?,
        is_admin: row.get::<_, i64>(4)? != 0,
    })
}

pub fn user_count(conn: &Connection) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
}

pub fn setup_required(conn: &Connection) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(user_count(conn)? == 0)
}

pub fn get_user_by_id(conn: &Connection, id: i64) -> Result<Option<User>, Box<dyn std::error::Error>> {
    Ok(conn
        .query_row(
            "SELECT id, uuid, username, password_hash, is_admin FROM users WHERE id=?1",
            [id],
            map_user,
        )
        .optional()?)
}

pub fn get_user_by_username(
    conn: &Connection,
    username: &str,
) -> Result<Option<User>, Box<dyn std::error::Error>> {
    Ok(conn
        .query_row(
            "SELECT id, uuid, username, password_hash, is_admin FROM users WHERE username=?1",
            [username],
            map_user,
        )
        .optional()?)
}

pub fn insert_user(
    conn: &Connection,
    username: &str,
    password_hash: Option<&str>,
    is_admin: bool,
) -> Result<User, Box<dyn std::error::Error>> {
    insert_user_with_uuid(conn, username, password_hash, is_admin, None)
}

pub fn insert_user_with_uuid(
    conn: &Connection,
    username: &str,
    password_hash: Option<&str>,
    is_admin: bool,
    uuid: Option<&str>,
) -> Result<User, Box<dyn std::error::Error>> {
    let uuid = uuid
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    conn.execute(
        "INSERT INTO users (uuid, username, password_hash, is_admin, created_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        params![uuid, username, password_hash, if is_admin { 1 } else { 0 }],
    )?;
    let id = conn.last_insert_rowid();
    get_user_by_id(conn, id)?.ok_or_else(|| "user insert vanished".into())
}

fn with_immediate<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, Box<dyn std::error::Error>>,
) -> Result<T, Box<dyn std::error::Error>> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

pub fn create_first_admin(
    conn: &Connection,
    username: &str,
    password_hash: Option<&str>,
    uuid: Option<&str>,
) -> Result<User, Box<dyn std::error::Error>> {
    with_immediate(conn, || {
        if user_count(conn)? != 0 {
            return Err("already set up".into());
        }
        insert_user_with_uuid(conn, username, password_hash, true, uuid)
    })
}

pub fn get_or_create_pam_user(
    conn: &Connection,
    username: &str,
) -> Result<User, Box<dyn std::error::Error>> {
    with_immediate(conn, || {
        if let Some(user) = get_user_by_username(conn, username)? {
            return Ok(user);
        }
        let admin = user_count(conn)? == 0;
        insert_user(conn, username, None, admin)
    })
}

pub fn insert_user_with_invite(
    conn: &Connection,
    username: &str,
    password_hash: Option<&str>,
    invite: &str,
    uuid: Option<&str>,
) -> Result<User, Box<dyn std::error::Error>> {
    with_immediate(conn, || {
        // Validate the invite before revealing whether a username exists — otherwise
        // unauthenticated callers can probe accounts with any garbage invite token.
        if peek_invite(conn, invite)?.is_none() {
            return Err("invite is invalid or expired".into());
        }
        if get_user_by_username(conn, username)?.is_some() {
            return Err("username is taken".into());
        }
        let user = insert_user_with_uuid(conn, username, password_hash, false, uuid)?;
        if !consume_invite(conn, invite, user.id)? {
            return Err("invite is invalid or expired".into());
        }
        Ok(user)
    })
}

const SESSION_DAYS_DEFAULT: i64 = 7;
const SESSION_IDLE_HOURS_DEFAULT: i64 = 24;
const SESSION_MAX_PER_USER_DEFAULT: i64 = 10;

pub fn session_days() -> i64 {
    std::env::var("TESLAMATE_RS_SESSION_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SESSION_DAYS_DEFAULT)
        .clamp(1, 90)
}

pub fn session_idle_hours() -> i64 {
    std::env::var("TESLAMATE_RS_SESSION_IDLE_HOURS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SESSION_IDLE_HOURS_DEFAULT)
        .clamp(1, 24 * 30)
}

pub fn session_max_per_user() -> i64 {
    std::env::var("TESLAMATE_RS_SESSION_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SESSION_MAX_PER_USER_DEFAULT)
        .clamp(1, 100)
}

pub fn create_session(
    conn: &Connection,
    user_id: i64,
    token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = Utc::now();
    let expires = (now + Duration::days(session_days())).to_rfc3339();
    let seen = now.to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (token_hash, user_id, expires_at, created_at, last_seen)
         VALUES (?1, ?2, ?3, datetime('now'), ?4)",
        params![hash_secret(token), user_id, expires, seen],
    )?;
    // Cap concurrent sessions per user (oldest absolute expiry / created first).
    let max = session_max_per_user();
    conn.execute(
        "DELETE FROM sessions WHERE user_id=?1 AND rowid NOT IN (
            SELECT rowid FROM sessions WHERE user_id=?1
            ORDER BY created_at DESC, rowid DESC LIMIT ?2
         )",
        params![user_id, max],
    )?;
    Ok(())
}

pub fn user_from_session(
    conn: &Connection,
    token: &str,
) -> Result<Option<User>, Box<dyn std::error::Error>> {
    let hash = hash_secret(token);
    let now = Utc::now();
    let now_s = now.to_rfc3339();
    conn.execute("DELETE FROM sessions WHERE expires_at < ?1", [&now_s])?;
    let idle_cut = (now - Duration::hours(session_idle_hours())).to_rfc3339();
    conn.execute(
        "DELETE FROM sessions WHERE last_seen IS NOT NULL AND last_seen < ?1",
        [&idle_cut],
    )?;
    // Legacy rows without last_seen: treat created_at as last_seen for idle.
    conn.execute(
        "DELETE FROM sessions WHERE last_seen IS NULL AND created_at < ?1",
        [&idle_cut],
    )?;
    let user = conn
        .query_row(
            "SELECT u.id, u.uuid, u.username, u.password_hash, u.is_admin
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash=?1 AND s.expires_at >= ?2",
            params![hash, now_s],
            map_user,
        )
        .optional()?;
    if user.is_some() {
        conn.execute(
            "UPDATE sessions SET last_seen=?1 WHERE token_hash=?2",
            params![now_s, hash],
        )?;
    }
    Ok(user)
}

pub fn delete_session(conn: &Connection, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute(
        "DELETE FROM sessions WHERE token_hash=?1",
        [hash_secret(token)],
    )?;
    Ok(())
}

pub fn delete_sessions_for_user(
    conn: &Connection,
    user_id: i64,
) -> Result<usize, Box<dyn std::error::Error>> {
    let n = conn.execute("DELETE FROM sessions WHERE user_id=?1", [user_id])?;
    Ok(n)
}

pub fn store_webauthn_challenge(
    conn: &Connection,
    id: &str,
    purpose: &str,
    user_id: Option<i64>,
    username: Option<&str>,
    user_uuid: Option<&str>,
    invite_token_hash: Option<&str>,
    state_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let expires = (Utc::now() + Duration::minutes(WEBAUTHN_MINUTES)).to_rfc3339();
    conn.execute(
        "INSERT INTO webauthn_challenges
            (id, purpose, user_id, username, user_uuid, invite_token_hash, state_json, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            purpose,
            user_id,
            username,
            user_uuid,
            invite_token_hash,
            state_json,
            expires
        ],
    )?;
    Ok(())
}

pub fn take_webauthn_challenge(
    conn: &Connection,
    id: &str,
) -> Result<Option<WebauthnChallenge>, Box<dyn std::error::Error>> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "DELETE FROM webauthn_challenges WHERE expires_at < ?1",
        [&now],
    )?;
    let row = conn
        .query_row(
            "SELECT purpose, user_id, username, user_uuid, invite_token_hash, state_json
             FROM webauthn_challenges WHERE id=?1 AND expires_at >= ?2",
            params![id, now],
            |row| {
                Ok(WebauthnChallenge {
                    purpose: row.get(0)?,
                    user_id: row.get(1)?,
                    username: row.get(2)?,
                    user_uuid: row.get(3)?,
                    invite_token_hash: row.get(4)?,
                    state_json: row.get(5)?,
                })
            },
        )
        .optional()?;
    if row.is_some() {
        conn.execute("DELETE FROM webauthn_challenges WHERE id=?1", [id])?;
    }
    Ok(row)
}

pub fn insert_passkey(
    conn: &Connection,
    user_id: i64,
    credential_id: &str,
    passkey_json: &str,
) -> Result<i64, Box<dyn std::error::Error>> {
    conn.execute(
        "INSERT INTO webauthn_credentials (user_id, credential_id, passkey_json, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        params![user_id, credential_id, passkey_json],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn passkeys_for_user(
    conn: &Connection,
    user_id: i64,
) -> Result<Vec<(i64, String, String)>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT id, credential_id, passkey_json FROM webauthn_credentials WHERE user_id=?1",
    )?;
    let rows = stmt.query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn list_passkeys(
    conn: &Connection,
    user_id: i64,
) -> Result<Vec<PasskeyInfo>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at FROM webauthn_credentials WHERE user_id=?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([user_id], |row| {
        Ok(PasskeyInfo {
            id: row.get(0)?,
            created_at: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn passkey_count(conn: &Connection, user_id: i64) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM webauthn_credentials WHERE user_id=?1",
        [user_id],
        |row| row.get(0),
    )?)
}

pub fn update_passkey_json(
    conn: &Connection,
    user_id: i64,
    credential_id: &str,
    passkey_json: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute(
        "UPDATE webauthn_credentials SET passkey_json=?1 WHERE user_id=?2 AND credential_id=?3",
        params![passkey_json, user_id, credential_id],
    )?;
    Ok(())
}

pub fn delete_passkey(
    conn: &Connection,
    user_id: i64,
    passkey_id: i64,
) -> Result<bool, Box<dyn std::error::Error>> {
    let n = conn.execute(
        "DELETE FROM webauthn_credentials WHERE id=?1 AND user_id=?2",
        params![passkey_id, user_id],
    )?;
    Ok(n > 0)
}

pub fn invite_unused_max() -> i64 {
    std::env::var("TESLAMATE_RS_INVITE_UNUSED_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(INVITE_UNUSED_MAX_DEFAULT)
        .clamp(1, 200)
}

pub fn count_unused_invites(conn: &Connection) -> Result<i64, Box<dyn std::error::Error>> {
    let now = Utc::now().to_rfc3339();
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM invites WHERE used_by IS NULL AND expires_at >= ?1",
        [&now],
        |r| r.get(0),
    )?)
}

pub fn create_invite(
    conn: &Connection,
    created_by: i64,
    token: &str,
) -> Result<InviteInfo, Box<dyn std::error::Error>> {
    let unused = count_unused_invites(conn)?;
    let max = invite_unused_max();
    if unused >= max {
        return Err(format!(
            "too many unused invites ({unused}); redeem or wait for expiry (max {max})"
        )
        .into());
    }
    let expires = (Utc::now() + Duration::days(INVITE_DAYS)).to_rfc3339();
    conn.execute(
        "INSERT INTO invites (token_hash, created_by, expires_at, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        params![hash_secret(token), created_by, expires],
    )?;
    Ok(InviteInfo {
        id: conn.last_insert_rowid(),
        expires_at: expires,
        created_at: Utc::now().to_rfc3339(),
        used: false,
    })
}

pub fn list_invites(conn: &Connection) -> Result<Vec<InviteInfo>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT id, expires_at, created_at, used_by FROM invites ORDER BY id DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(InviteInfo {
            id: row.get(0)?,
            expires_at: row.get(1)?,
            created_at: row.get(2)?,
            used: row.get::<_, Option<i64>>(3)?.is_some(),
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn peek_invite(conn: &Connection, token: &str) -> Result<Option<i64>, Box<dyn std::error::Error>> {
    let now = Utc::now().to_rfc3339();
    Ok(conn
        .query_row(
            "SELECT id FROM invites
             WHERE token_hash=?1 AND used_by IS NULL AND expires_at >= ?2",
            params![hash_secret(token), now],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn ui_layout(conn: &Connection, user_id: i64) -> Result<String, Box<dyn std::error::Error>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT ui_layout FROM user_prefs WHERE user_id=?1",
            [user_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match stored.as_deref() {
        Some("grouped") => "grouped".into(),
        _ => "classic".into(),
    })
}

pub fn set_ui_layout(
    conn: &Connection,
    user_id: i64,
    layout: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if layout != "classic" && layout != "grouped" {
        return Err("ui layout must be classic or grouped".into());
    }
    conn.execute(
        "INSERT INTO user_prefs (user_id, ui_layout) VALUES (?1, ?2)
         ON CONFLICT(user_id) DO UPDATE SET ui_layout = excluded.ui_layout",
        params![user_id, layout],
    )?;
    Ok(())
}

pub fn consume_invite(
    conn: &Connection,
    token: &str,
    used_by: i64,
) -> Result<bool, Box<dyn std::error::Error>> {
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE invites SET used_by=?1
         WHERE token_hash=?2 AND used_by IS NULL AND expires_at >= ?3",
        params![used_by, hash_secret(token), now],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        db::register_functions(&conn).unwrap();
        conn
    }

    #[test]
    fn first_admin_and_invite_user() {
        let conn = mem();
        assert!(setup_required(&conn).unwrap());
        let admin = create_first_admin(&conn, "admin", Some("hash"), None).unwrap();
        assert!(admin.is_admin);
        assert!(!setup_required(&conn).unwrap());
        assert!(create_first_admin(&conn, "other", Some("hash"), None).is_err());
        let token = "invite-token";
        create_invite(&conn, admin.id, token).unwrap();
        let guest = insert_user_with_invite(&conn, "guest", Some("hash"), token, None).unwrap();
        assert!(!guest.is_admin);
        assert!(insert_user_with_invite(&conn, "late", Some("hash"), token, None).is_err());
    }

    #[test]
    fn layout_is_per_account() {
        let conn = mem();
        let a = insert_user(&conn, "ada", Some("hash"), true).unwrap();
        let b = insert_user(&conn, "bea", Some("hash"), false).unwrap();
        assert_eq!(ui_layout(&conn, a.id).unwrap(), "classic");
        set_ui_layout(&conn, a.id, "grouped").unwrap();
        assert!(set_ui_layout(&conn, a.id, "grafana").is_err());
        assert_eq!(ui_layout(&conn, a.id).unwrap(), "grouped");
        assert_eq!(ui_layout(&conn, b.id).unwrap(), "classic");
    }

    #[test]
    fn register_hides_usernames_without_valid_invite() {
        let conn = mem();
        let admin = create_first_admin(&conn, "admin", Some("hash"), None).unwrap();
        insert_user(&conn, "alice", Some("hash"), false).unwrap();
        let err_taken = insert_user_with_invite(&conn, "alice", Some("hash"), "bogus", None)
            .unwrap_err()
            .to_string();
        let err_fresh = insert_user_with_invite(&conn, "bob", Some("hash"), "bogus", None)
            .unwrap_err()
            .to_string();
        assert!(
            err_taken.contains("invite"),
            "existing user must not leak via bad invite: {err_taken}"
        );
        assert!(
            err_fresh.contains("invite"),
            "unknown user must not differ from existing: {err_fresh}"
        );
        assert_eq!(err_taken, err_fresh);

        let token = "real-invite";
        create_invite(&conn, admin.id, token).unwrap();
        let taken = insert_user_with_invite(&conn, "alice", Some("hash"), token, None)
            .unwrap_err()
            .to_string();
        assert!(taken.contains("taken"), "{taken}");
    }

    #[test]
    fn session_roundtrip() {
        let conn = mem();
        let user = insert_user(&conn, "tom", Some("hash"), true).unwrap();
        create_session(&conn, user.id, "secret").unwrap();
        let got = user_from_session(&conn, "secret").unwrap().unwrap();
        assert_eq!(got.username, "tom");
        delete_session(&conn, "secret").unwrap();
        assert!(user_from_session(&conn, "secret").unwrap().is_none());
    }

    #[test]
    fn session_cap_evicts_oldest() {
        let conn = mem();
        let user = insert_user(&conn, "tom", Some("hash"), true).unwrap();
        let prev = std::env::var("TESLAMATE_RS_SESSION_MAX").ok();
        std::env::set_var("TESLAMATE_RS_SESSION_MAX", "2");
        create_session(&conn, user.id, "a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        create_session(&conn, user.id, "b").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        create_session(&conn, user.id, "c").unwrap();
        match prev {
            Some(v) => std::env::set_var("TESLAMATE_RS_SESSION_MAX", v),
            None => std::env::remove_var("TESLAMATE_RS_SESSION_MAX"),
        }
        assert!(user_from_session(&conn, "a").unwrap().is_none());
        assert!(user_from_session(&conn, "b").unwrap().is_some());
        assert!(user_from_session(&conn, "c").unwrap().is_some());
        assert_eq!(delete_sessions_for_user(&conn, user.id).unwrap(), 2);
        assert!(user_from_session(&conn, "c").unwrap().is_none());
    }

    #[test]
    fn unused_invite_cap_blocks_minting() {
        let conn = mem();
        let admin = create_first_admin(&conn, "admin", Some("hash"), None).unwrap();
        let prev = std::env::var("TESLAMATE_RS_INVITE_UNUSED_MAX").ok();
        std::env::set_var("TESLAMATE_RS_INVITE_UNUSED_MAX", "2");
        create_invite(&conn, admin.id, "one").unwrap();
        create_invite(&conn, admin.id, "two").unwrap();
        let err = create_invite(&conn, admin.id, "three")
            .unwrap_err()
            .to_string();
        match prev {
            Some(v) => std::env::set_var("TESLAMATE_RS_INVITE_UNUSED_MAX", v),
            None => std::env::remove_var("TESLAMATE_RS_INVITE_UNUSED_MAX"),
        }
        assert!(err.contains("too many unused"), "{err}");
        assert_eq!(count_unused_invites(&conn).unwrap(), 2);
    }
}
