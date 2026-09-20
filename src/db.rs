use anyhow::{Context, Result};
use chrono::Datelike;
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{functions::FunctionFlags, Connection};
use std::path::Path;
use std::sync::Arc;

pub type Db = Arc<Mutex<Connection>>;

pub fn open(path: &Path) -> Result<Db> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "cache_size", "-131072")?; // 128 MB
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute_batch(include_str!("../schema.sql"))?;
    register_functions(&conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

pub(crate) fn register_functions(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    conn.create_scalar_function("convert_km", 2, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        let unit: String = ctx.get(1)?;
        Ok(n.map(|v| match unit.as_str() {
            "mi" => v / 1.60934,
            _ => v,
        }))
    })?;

    conn.create_scalar_function("convert_celsius", 2, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        let unit: String = ctx.get(1)?;
        Ok(n.map(|v| match unit.as_str() {
            "F" => (v * 9.0 / 5.0) + 32.0,
            _ => v,
        }))
    })?;

    conn.create_scalar_function("convert_m", 2, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        let unit: String = ctx.get(1)?;
        Ok(n.map(|v| match unit.as_str() {
            "ft" => v * 3.28084,
            _ => v,
        }))
    })?;

    conn.create_scalar_function("convert_tire_pressure", 2, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        let unit: String = ctx.get(1)?;
        Ok(n.map(|v| match unit.as_str() {
            "psi" => v * 14.503773773,
            _ => v,
        }))
    })?;

    conn.create_scalar_function("split_part", 3, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let delim: String = ctx.get(1)?;
        let n: i64 = ctx.get(2)?;
        Ok(s.and_then(|s| {
            s.split(&delim)
                .nth((n.max(1) as usize) - 1)
                .map(|p| p.to_string())
        }))
    })?;

    conn.create_scalar_function("date_trunc", 2, flags, |ctx| {
        let unit: String = ctx.get(0)?;
        let ts: Option<String> = ctx.get(1)?;
        Ok(ts.map(|t| trunc_ts(&unit, &t)))
    })?;

    conn.create_scalar_function("date_trunc", 3, flags, |ctx| {
        let unit: String = ctx.get(0)?;
        let ts: Option<String> = ctx.get(1)?;
        Ok(ts.map(|t| trunc_ts(&unit, &t)))
    })?;

    conn.create_scalar_function("timezone", 2, flags, |ctx| {
        let ts: Option<String> = ctx.get(1)?;
        Ok(ts)
    })?;

    conn.create_scalar_function("to_timestamp", 1, flags, |ctx| {
        let epoch: Option<f64> = ctx.get(0)?;
        Ok(epoch.map(|e| {
            chrono::DateTime::from_timestamp(e as i64, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default()
        }))
    })?;

    conn.create_scalar_function("age_seconds", 2, flags, |ctx| {
        let a: Option<String> = ctx.get(0)?;
        let b: Option<String> = ctx.get(1)?;
        Ok(match (a, b) {
            (Some(a), Some(b)) => unix(&a).and_then(|aa| unix(&b).map(|bb| aa - bb)),
            _ => None,
        })
    })?;

    conn.create_scalar_function("floor", 1, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        Ok(n.map(|v| v.floor()))
    })?;
    conn.create_scalar_function("ceil", 1, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        Ok(n.map(|v| v.ceil()))
    })?;
    conn.create_scalar_function("ceiling", 1, flags, |ctx| {
        let n: Option<f64> = ctx.get(0)?;
        Ok(n.map(|v| v.ceil()))
    })?;
    conn.create_scalar_function("regexp", 2, flags, |ctx| {
        let pat: String = ctx.get(0)?;
        let s: Option<String> = ctx.get(1)?;
        Ok(s.and_then(|s| Regex::new(&pat).ok().map(|re| re.is_match(&s)))
            .unwrap_or(false))
    })?;

    conn.create_scalar_function("join_head", 3, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let delim: String = ctx.get(1)?;
        let n: i64 = ctx.get(2)?;
        Ok(s.map(|s| {
            s.split(&delim)
                .take(n.max(1) as usize)
                .collect::<Vec<_>>()
                .join(&delim)
        }))
    })?;

    conn.create_scalar_function("version", 0, flags, |_| Ok("SQLite".to_string()))?;
    conn.create_scalar_function("current_setting", 1, flags, |_| Ok("UTC".to_string()))?;

    conn.create_scalar_function("concat", -1, flags, |ctx| {
        let mut out = String::new();
        for i in 0..ctx.len() {
            if let Some(s) = sql_value_text(ctx.get_raw(i)) {
                out.push_str(&s);
            }
        }
        Ok(out)
    })?;

    conn.create_scalar_function("concat_ws", -1, flags, |ctx| {
        if ctx.len() == 0 {
            return Ok(String::new());
        }
        let sep = sql_value_text(ctx.get_raw(0)).unwrap_or_default();
        let mut parts = Vec::new();
        for i in 1..ctx.len() {
            if let Some(s) = sql_value_text(ctx.get_raw(i)) {
                if !s.is_empty() {
                    parts.push(s);
                }
            }
        }
        Ok(parts.join(&sep))
    })?;

    conn.create_scalar_function("left", 2, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let n: i64 = ctx.get(1)?;
        Ok(s.map(|s| {
            let n = n.max(0) as usize;
            s.chars().take(n).collect::<String>()
        }))
    })?;

    conn.create_scalar_function("right", 2, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let n: i64 = ctx.get(1)?;
        Ok(s.map(|s| {
            let n = n.max(0) as usize;
            let skip = s.chars().count().saturating_sub(n);
            s.chars().skip(skip).collect::<String>()
        }))
    })?;

    conn.create_scalar_function("to_char", 2, flags, |ctx| {
        let fmt: String = ctx.get(1)?;
        Ok(pg_to_char(ctx.get_raw(0), &fmt))
    })?;

    conn.create_scalar_function("regexp_replace", 3, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let pat: String = ctx.get(1)?;
        let repl: String = ctx.get(2)?;
        Ok(s.and_then(|s| Regex::new(&pat).ok().map(|re| re.replace(&s, repl.as_str()).into_owned())))
    })?;

    conn.create_scalar_function("regexp_replace", 4, flags, |ctx| {
        let s: Option<String> = ctx.get(0)?;
        let pat: String = ctx.get(1)?;
        let repl: String = ctx.get(2)?;
        Ok(s.and_then(|s| Regex::new(&pat).ok().map(|re| re.replace(&s, repl.as_str()).into_owned())))
    })?;

    conn.create_scalar_function("date_bin", 3, flags, |ctx| {
        let stride: String = ctx.get(0)?;
        let src: Option<String> = ctx.get(1)?;
        let origin: Option<String> = ctx.get(2)?;
        Ok(match (src, origin) {
            (Some(src), Some(origin)) => date_bin(&stride, &src, &origin),
            _ => None,
        })
    })?;

    Ok(())
}

fn sql_value_text(v: ValueRef) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(_) => None,
    }
}

fn pg_to_char(val: ValueRef, fmt: &str) -> Option<String> {
    let time_only = !fmt.to_ascii_uppercase().contains("YYYY")
        && !fmt.to_ascii_uppercase().contains("MM")
        && !fmt.to_ascii_uppercase().contains("DD")
        && !fmt.to_ascii_uppercase().contains("WW")
        && !fmt.to_ascii_lowercase().contains("month");
    if time_only {
        let secs = match val {
            ValueRef::Integer(i) => i,
            ValueRef::Real(f) => f as i64,
            ValueRef::Text(t) => unix(&String::from_utf8_lossy(t))? as i64,
            _ => return None,
        };
        let sign = if secs < 0 { "-" } else { "" };
        let secs = secs.abs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        let u = fmt.to_ascii_uppercase();
        if u.contains("SS") {
            return Some(format!("{sign}{h:02}:{m:02}:{s:02}"));
        }
        return Some(format!("{sign}{h:02}:{m:02}"));
    }
    let ts = match val {
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Integer(i) => chrono::DateTime::from_timestamp(i, 0)?
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        ValueRef::Real(f) => chrono::DateTime::from_timestamp(f as i64, 0)?
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        _ => return None,
    };
    let t = ts.replace('T', " ").trim_end_matches('Z').to_string();
    let ndt = chrono::NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .ok()?;
    let mut f = fmt.to_string();
    f = f.replace("Month", "%B");
    f = f.replace("YYYY", "%Y");
    f = f.replace("HH24", "%H");
    f = f.replace("MI", "%M");
    f = f.replace("SS", "%S");
    f = f.replace("WW", "%U");
    f = f.replace("dd", "%d");
    f = f.replace("DD", "%d");
    f = f.replace("MM", "%m");
    Some(ndt.format(&f).to_string())
}

fn date_bin(stride: &str, src: &str, origin: &str) -> Option<String> {
    let secs = parse_stride_secs(stride).max(1);
    let src_u = unix(src)?;
    let origin_u = unix(origin).unwrap_or(0.0);
    let bucket = origin_u + ((src_u - origin_u) / secs as f64).floor() * secs as f64;
    chrono::DateTime::from_timestamp(bucket as i64, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn parse_stride_secs(s: &str) -> i64 {
    let s = s.trim().trim_matches('\'').trim();
    let num: i64 = s
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim()
        .parse()
        .unwrap_or(1);
    let unit = s
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace())
        .trim()
        .to_ascii_lowercase();
    match unit.as_str() {
        "s" | "sec" | "second" | "seconds" => num,
        "m" | "min" | "minute" | "minutes" => num * 60,
        "h" | "hour" | "hours" => num * 3600,
        "d" | "day" | "days" => num * 86400,
        _ => num,
    }
}

fn trunc_ts(unit: &str, ts: &str) -> String {
    let t = ts.replace('T', " ");
    let date = t.get(0..10).unwrap_or("1970-01-01");
    let hour = t.get(11..13).unwrap_or("00");
    let min = t.get(14..16).unwrap_or("00");
    match unit {
        "year" => format!("{}-01-01 00:00:00", &date[..4]),
        "month" => format!("{}-01 00:00:00", &date[..7]),
        "day" | "dec" => format!("{date} 00:00:00"),
        "hour" => format!("{date} {hour}:00:00"),
        "minute" => format!("{date} {hour}:{min}:00"),
        "week" => {
            if let Some(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok() {
                let wd = d.weekday().num_days_from_monday() as i64;
                let start = d
                    .checked_sub_signed(chrono::Duration::days(wd))
                    .unwrap_or(d);
                format!("{} 00:00:00", start)
            } else {
                format!("{date} 00:00:00")
            }
        }
        _ => t,
    }
}

fn unix(ts: &str) -> Option<f64> {
    let t = ts.replace('T', " ").trim_end_matches('Z').to_string();
    let t = if t.len() == 10 {
        format!("{t} 00:00:00")
    } else {
        t
    };
    chrono::NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&t, "%Y-%m-%d %H:%M:%S"))
        .ok()
        .map(|d| d.and_utc().timestamp() as f64)
}

pub fn battery_aux(conn: &Connection, car_id: i64, length_unit: &str, preferred_range: &str) -> String {
    let range_col = if preferred_range.eq_ignore_ascii_case("ideal") {
        "ideal_battery_range_km"
    } else {
        "rated_battery_range_km"
    };
    let eff: f64 = conn
        .query_row(
            "SELECT COALESCE((SELECT efficiency * 100.0 FROM cars WHERE id = ?1), 16.0)",
            [car_id],
            |r| r.get(0),
        )
        .unwrap_or(16.0);
    let max_cap: f64 = conn
        .query_row(
            "SELECT COALESCE(MAX(c.rated_battery_range_km * ?1 / c.usable_battery_level), 1)
             FROM charging_processes cp
             JOIN charges c ON c.charging_process_id = cp.id
             WHERE cp.car_id = ?2 AND c.usable_battery_level > 0 AND cp.end_date IS NOT NULL",
            rusqlite::params![eff, car_id],
            |r| r.get(0),
        )
        .unwrap_or(1.0);
    let cur_cap: f64 = conn
        .query_row(
            "SELECT COALESCE(AVG(x), ?1) FROM (
                SELECT c.rated_battery_range_km * ?2 / c.usable_battery_level AS x
                FROM charging_processes cp
                JOIN charges c ON c.charging_process_id = cp.id
                WHERE cp.car_id = ?3 AND c.usable_battery_level > 0 AND cp.end_date IS NOT NULL
                ORDER BY cp.end_date DESC
                LIMIT 100
             )",
            rusqlite::params![max_cap, eff, car_id],
            |r| r.get(0),
        )
        .unwrap_or(max_cap);
    let cur_range_sql = format!(
        "SELECT COALESCE(convert_km({range_col} * 100.0 / usable_battery_level, ?1), 0)
         FROM positions
         WHERE car_id = ?2 AND usable_battery_level > 0 AND {range_col} IS NOT NULL
         ORDER BY date DESC LIMIT 1"
    );
    let cur_range: f64 = conn
        .query_row(&cur_range_sql, rusqlite::params![length_unit, car_id], |r| r.get(0))
        .unwrap_or(0.0);
    let max_range_sql = format!(
        "SELECT COALESCE(convert_km(MAX(c.{range_col} * 100.0 / c.usable_battery_level), ?1), 0)
         FROM charges c
         JOIN charging_processes p ON p.id = c.charging_process_id
         WHERE p.car_id = ?2 AND c.usable_battery_level > 0 AND c.{range_col} IS NOT NULL"
    );
    let max_range: f64 = conn
        .query_row(&max_range_sql, rusqlite::params![length_unit, car_id], |r| r.get(0))
        .unwrap_or(0.0);
    format!(
        "{{\"MaxCapacity\":{max_cap},\"CurrentCapacity\":{cur_cap},\"MaxRange\":{max_range},\"CurrentRange\":{cur_range},\"RatedEfficiency\":{eff}}}"
    )
}

pub const DEFAULT_HOME: &str = "/srv/teslamate-rs";
pub const DEFAULT_DB_NAME: &str = "teslamate-rs.sqlite";

pub fn default_home() -> std::path::PathBuf {
    std::env::var("TESLAMATE_RS_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(DEFAULT_HOME))
}

pub fn default_db_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TESLAMATE_RS_DB") {
        if !p.is_empty() {
            return p.into();
        }
    }
    default_home().join(DEFAULT_DB_NAME)
}
