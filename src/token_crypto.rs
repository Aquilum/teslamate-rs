//! AES-256-GCM at-rest protection for Owner API tokens in SQLite.
//!
//! Key sources (first match wins):
//! 1. `TESLAMATE_RS_TOKEN_KEY` — 64 hex chars (32 bytes) or raw 32-byte string
//! 2. `$TESLAMATE_RS_HOME/oauth.key` — created on first use with mode 0600

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use parking_lot::Mutex;
use rand::RngCore;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const PREFIX: &str = "tmrs1.";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

static KEY_CACHE: OnceLock<Mutex<Option<[u8; KEY_LEN]>>> = OnceLock::new();

pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(PREFIX)
}

fn key_path() -> PathBuf {
    crate::db::default_home().join("oauth.key")
}

fn parse_env_key(raw: &str) -> Result<[u8; KEY_LEN]> {
    let raw = raw.trim();
    if raw.len() == KEY_LEN * 2 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = hex::decode(raw).context("decode TESLAMATE_RS_TOKEN_KEY hex")?;
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    if raw.as_bytes().len() == KEY_LEN {
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(raw.as_bytes());
        return Ok(key);
    }
    bail!("TESLAMATE_RS_TOKEN_KEY must be 64 hex chars or 32 raw bytes");
}

fn load_or_create_file_key(path: &Path) -> Result<[u8; KEY_LEN]> {
    if path.exists() {
        let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if data.len() != KEY_LEN {
            bail!("{} must be exactly {KEY_LEN} bytes", path.display());
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&data);
        return Ok(key);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut key = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(&key)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    tracing::info!("created Owner API token encryption key at {}", path.display());
    Ok(key)
}

fn resolve_key() -> Result<[u8; KEY_LEN]> {
    if let Ok(raw) = std::env::var("TESLAMATE_RS_TOKEN_KEY") {
        if !raw.is_empty() {
            return parse_env_key(&raw);
        }
    }
    load_or_create_file_key(&key_path())
}

fn encryption_key() -> Result<[u8; KEY_LEN]> {
    let cache = KEY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock();
    if let Some(key) = *guard {
        return Ok(key);
    }
    let key = resolve_key()?;
    *guard = Some(key);
    Ok(key)
}

/// Drop a cached key (tests only).
#[cfg(test)]
pub fn reset_cached_key_for_tests() {
    if let Some(cache) = KEY_CACHE.get() {
        *cache.lock() = None;
    }
}

pub fn encrypt(plain: &str) -> Result<String> {
    let key = encryption_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).context("aes key")?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plain.as_bytes())
        .map_err(|e| anyhow::anyhow!("encrypt token: {e}"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}", B64.encode(blob)))
}

pub fn decrypt(stored: &str) -> Result<String> {
    if !is_encrypted(stored) {
        return Ok(stored.to_string());
    }
    let key = encryption_key()?;
    let blob = B64
        .decode(stored[PREFIX.len()..].as_bytes())
        .context("decode encrypted token")?;
    if blob.len() < NONCE_LEN + 16 {
        bail!("encrypted token too short");
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(&key).context("aes key")?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher
        .decrypt(nonce, ct)
        .map_err(|e| anyhow::anyhow!("decrypt token: {e}"))?;
    String::from_utf8(plain).context("token utf8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn roundtrip_with_env_key() {
        let _g = LOCK.lock().unwrap();
        let prev = std::env::var("TESLAMATE_RS_TOKEN_KEY").ok();
        std::env::set_var(
            "TESLAMATE_RS_TOKEN_KEY",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        reset_cached_key_for_tests();
        let plain = "refresh-token-value";
        let enc = encrypt(plain).expect("encrypt");
        assert!(is_encrypted(&enc));
        assert_ne!(enc, plain);
        assert_eq!(decrypt(&enc).unwrap(), plain);
        assert_eq!(decrypt(plain).unwrap(), plain);
        match prev {
            Some(v) => std::env::set_var("TESLAMATE_RS_TOKEN_KEY", v),
            None => std::env::remove_var("TESLAMATE_RS_TOKEN_KEY"),
        }
        reset_cached_key_for_tests();
    }
}
