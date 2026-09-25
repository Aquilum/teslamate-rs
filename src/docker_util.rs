//! Validate identifiers passed to `docker exec` so they cannot be parsed as flags.

use anyhow::{bail, Result};

pub fn validate_docker_ident(kind: &str, value: &str) -> Result<()> {
    let v = value.trim();
    if v.is_empty() {
        bail!("{kind} must not be empty");
    }
    if v.starts_with('-') {
        bail!("{kind} must not start with '-'");
    }
    if v.len() > 128 {
        bail!("{kind} is too long");
    }
    if !v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("{kind} contains invalid characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_names() {
        validate_docker_ident("container", "teslamate-database-1").unwrap();
        validate_docker_ident("user", "teslamate").unwrap();
    }

    #[test]
    fn rejects_flag_like() {
        assert!(validate_docker_ident("container", "--privileged").is_err());
        assert!(validate_docker_ident("container", "-e").is_err());
        assert!(validate_docker_ident("user", "bad;id").is_err());
    }
}
