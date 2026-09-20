use crate::logger;
use crate::tesla::Tokens;
use anyhow::{bail, Context, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};
use std::process::Command;

const AAD: &[u8] = b"AES256GCM";
const TAG: &[u8] = b"AES.GCM.V1";
const IV_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// Pull Cloak-encrypted TeslaMate tokens from `private.tokens` and the running
/// app's ENCRYPTION_KEY, decrypt, and store. Never logs token values.
pub fn import_tokens(
    db: &crate::db::Db,
    db_container: &str,
    app_container: &str,
    user: &str,
    dbname: &str,
) -> Result<Tokens> {
    let key_material = docker_stdout(app_container, &["printenv", "ENCRYPTION_KEY"])
        .context("read ENCRYPTION_KEY from TeslaMate app container")?;
    let key_material = key_material.trim();
    if key_material.is_empty() {
        bail!("ENCRYPTION_KEY is empty on {app_container}");
    }
    let key = Sha256::digest(key_material.as_bytes());

    let row = docker_stdout(
        db_container,
        &[
            "psql",
            "-U",
            user,
            "-d",
            dbname,
            "-At",
            "-c",
            "SELECT encode(refresh, 'hex') || ',' || encode(access, 'hex') FROM private.tokens ORDER BY id LIMIT 1",
        ],
    )
    .context("read private.tokens")?;
    let row = row.trim();
    let (refresh_hex, access_hex) = row
        .split_once(',')
        .context("expected refresh,access hex row from private.tokens")?;
    let refresh = decrypt_cloak(&key, &hex::decode(refresh_hex.trim())?)
        .context("decrypt refresh token")?;
    let access = decrypt_cloak(&key, &hex::decode(access_hex.trim())?)
        .context("decrypt access token")?;

    let tokens = Tokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: 0,
    };
    logger::store_tokens(db, &tokens)?;
    tracing::info!("imported TeslaMate tokens into teslamate-rs sqlite (plaintext not logged)");
    Ok(tokens)
}

fn decrypt_cloak(key: &[u8], blob: &[u8]) -> Result<String> {
    // Cloak header: reserved (1) + length (1) + tag + iv + gcm_tag + ciphertext
    if blob.len() < 2 + TAG.len() + IV_LEN + GCM_TAG_LEN {
        bail!("ciphertext too short ({})", blob.len());
    }
    let reserved = blob[0];
    let tag_len = blob[1] as usize;
    if reserved != 1 {
        bail!("unexpected cloak tag type {reserved}");
    }
    let tag_start = 2;
    let tag_end = tag_start + tag_len;
    if tag_end > blob.len() || &blob[tag_start..tag_end] != TAG {
        bail!("unexpected cloak key tag");
    }
    let rest = &blob[tag_end..];
    let iv = &rest[..IV_LEN];
    let gcm_tag = &rest[IV_LEN..IV_LEN + GCM_TAG_LEN];
    let ciphertext = &rest[IV_LEN + GCM_TAG_LEN..];
    let mut combined = ciphertext.to_vec();
    combined.extend_from_slice(gcm_tag);

    let cipher = Aes256Gcm::new_from_slice(key).context("aes key")?;
    let nonce = Nonce::from_slice(iv);
    let plain = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &combined,
                aad: AAD,
            },
        )
        .map_err(|e| anyhow::anyhow!("aes-gcm decrypt failed: {e}"))?;
    String::from_utf8(plain).context("token utf8")
}

fn docker_stdout(container: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(args)
        .output()
        .with_context(|| format!("docker exec {container}"))?;
    if !out.status.success() {
        bail!(
            "docker exec {container} {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}
