//! Allowlist of dashboard `rawSql` templates accepted by `/api/query`.

use rust_embed::RustEmbed;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(RustEmbed)]
#[folder = "dashboards/"]
struct Dashboards;

/// Geomap preview query hardcoded in `web/app.js` (must stay in sync).
pub const PREVIEW_TRACK_SQL: &str = r#"SELECT latitude, longitude FROM (
  SELECT p.latitude AS latitude, p.longitude AS longitude, d.start_date AS sort_date, 0 AS seq
  FROM drives d
  JOIN positions p ON p.id = d.start_position_id
  WHERE d.car_id = $car_id AND $__timeFilter(d.start_date)
  UNION ALL
  SELECT p.latitude, p.longitude, COALESCE(d.end_date, d.start_date), 1
  FROM drives d
  JOIN positions p ON p.id = d.end_position_id
  WHERE d.car_id = $car_id AND $__timeFilter(d.start_date)
) ORDER BY sort_date, seq"#;

pub fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_raw_sql(value: &serde_json::Value, out: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(sql)) = map.get("rawSql") {
                let n = normalize_sql(sql);
                if !n.is_empty() {
                    out.insert(n);
                }
            }
            for child in map.values() {
                collect_raw_sql(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_raw_sql(child, out);
            }
        }
        _ => {}
    }
}

fn build_allowlist() -> HashSet<String> {
    let mut set = HashSet::new();
    set.insert(normalize_sql(PREVIEW_TRACK_SQL));
    for name in Dashboards::iter() {
        if !name.ends_with(".json") {
            continue;
        }
        let Some(file) = Dashboards::get(name.as_ref()) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&file.data) {
            collect_raw_sql(&parsed, &mut set);
        }
    }
    set
}

pub fn allowlist() -> &'static HashSet<String> {
    static LIST: OnceLock<HashSet<String>> = OnceLock::new();
    LIST.get_or_init(build_allowlist)
}

pub fn is_allowed(sql: &str) -> bool {
    allowlist().contains(&normalize_sql(sql))
}

pub fn len() -> usize {
    allowlist().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_includes_dashboards_and_preview() {
        assert!(len() > 10, "expected embedded dashboard SQL, got {}", len());
        assert!(is_allowed(PREVIEW_TRACK_SQL));
        assert!(!is_allowed("SELECT access_token FROM oauth_tokens"));
        assert!(!is_allowed("select 1 as n"));
    }
}
