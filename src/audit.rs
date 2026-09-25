//! Lightweight security audit trail (SQLite + tracing).

use crate::db::Db;
use rusqlite::params;
use tracing::info;

pub fn record(db: &Db, actor: Option<&str>, action: &str, detail: &str) {
    let actor_s = actor.unwrap_or("-");
    info!(target: "teslamate_rs::audit", actor = actor_s, action, detail, "audit");
    if let Err(e) = insert(db, actor_s, action, detail) {
        tracing::warn!("audit log write failed: {e}");
    }
}

fn insert(db: &Db, actor: &str, action: &str, detail: &str) -> Result<(), rusqlite::Error> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO audit_log (at, actor, action, detail) VALUES (datetime('now'), ?1, ?2, ?3)",
        params![actor, action, detail],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn writes_audit_row() {
        let db = db::Db::from_write(rusqlite::Connection::open_in_memory().unwrap());
        {
            let conn = db.lock();
            conn.execute_batch(include_str!("../schema.sql")).unwrap();
        }
        record(&db, Some("admin"), "login", "ok");
        let conn = db.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_log WHERE action='login'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
