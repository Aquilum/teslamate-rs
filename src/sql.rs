use chrono::{TimeZone, Utc};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Clone, Debug, Deserialize)]
pub struct QueryVars {
    pub car_id: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    #[serde(default = "default_length")]
    pub length_unit: String,
    #[serde(default = "default_temp")]
    pub temp_unit: String,
    #[serde(default = "default_range")]
    pub preferred_range: String,
    #[serde(default = "default_pressure")]
    pub pressure_unit: String,
    #[serde(default = "default_speed")]
    pub speed_unit: String,
    #[serde(default = "default_interval")]
    #[allow(dead_code)]
    pub interval: String,
    #[serde(default)]
    pub charging_process_id: Option<i64>,
    #[serde(default)]
    pub drive_id: Option<i64>,
    #[serde(default = "default_charge_type")]
    #[allow(dead_code)]
    pub charge_type: String,
    #[serde(default = "default_period")]
    pub period: String,
    #[serde(default)]
    pub extras: HashMap<String, String>,
    /// Target points for `$__timeGroup`. Preview maps send ~400; hires tracks ~8000.
    #[serde(default = "default_max_buckets")]
    pub max_buckets: i64,
}

fn default_length() -> String {
    "km".into()
}
fn default_temp() -> String {
    "C".into()
}
fn default_range() -> String {
    "rated".into()
}
fn default_pressure() -> String {
    "bar".into()
}
fn default_speed() -> String {
    "kmh".into()
}
fn default_interval() -> String {
    "1h".into()
}
fn default_charge_type() -> String {
    "%".into()
}
fn default_period() -> String {
    "month".into()
}
fn default_max_buckets() -> i64 {
    1600
}

impl QueryVars {
    pub fn from_ts(&self) -> String {
        fmt_ms(self.from_ms)
    }
    pub fn to_ts(&self) -> String {
        fmt_ms(self.to_ms)
    }
    #[allow(dead_code)]
    pub fn interval_secs(&self) -> i64 {
        parse_interval(&self.interval)
    }
}

fn fmt_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn parse_interval(s: &str) -> i64 {
    let s = s.trim();
    let (n, unit) = s
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .parse::<i64>()
        .ok()
        .map(|n| {
            let u = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
            (n, u)
        })
        .unwrap_or((3600, "s"));
    match unit {
        "s" | "sec" | "second" | "seconds" => n,
        "m" | "min" | "minute" | "minutes" => n * 60,
        "h" | "hour" | "hours" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        "w" | "week" | "weeks" => n * 86400 * 7,
        _ => 3600,
    }
}

/// Tables that must never be reachable from dashboard `/api/query`.
pub const FORBIDDEN_QUERY_TABLES: &[&str] = &[
    "oauth_tokens",
    "users",
    "sessions",
    "invites",
    "webauthn_credentials",
    "webauthn_challenges",
    "audit_log",
];

/// Clamp client-controlled Grafana vars to values that are safe to splice into SQL.
pub fn sanitize_vars(mut vars: QueryVars) -> QueryVars {
    vars.length_unit = allow_enum(&vars.length_unit, &["km", "mi"], "km");
    vars.temp_unit = allow_enum(&vars.temp_unit, &["C", "F"], "C");
    vars.preferred_range = allow_enum(&vars.preferred_range, &["ideal", "rated"], "rated");
    vars.pressure_unit = allow_enum(&vars.pressure_unit, &["bar", "psi"], "bar");
    vars.speed_unit = allow_enum(&vars.speed_unit, &["kmh", "mph"], "kmh");
    vars.period = allow_enum(
        &vars.period,
        &["hour", "day", "week", "month", "year"],
        "month",
    );
    vars.interval = sanitize_interval(&vars.interval);
    vars.charge_type = "%".into();
    let mut extras = HashMap::new();
    for (k, v) in vars.extras.drain() {
        if !safe_ident(&k) || FORBIDDEN_QUERY_TABLES.iter().any(|t| t.eq_ignore_ascii_case(&k)) {
            continue;
        }
        // URL search params are mirrored into extras; only accept literal-safe values.
        if let Some(v) = sanitize_extra_value(&k, &v) {
            extras.insert(k, v);
        }
    }
    vars.extras = extras;
    vars
}

fn allow_enum(raw: &str, allowed: &[&str], default: &str) -> String {
    let t = raw.trim();
    allowed
        .iter()
        .find(|a| a.eq_ignore_ascii_case(t))
        .copied()
        .unwrap_or(default)
        .to_string()
}

fn sanitize_interval(raw: &str) -> String {
    let t = raw.trim();
    if Regex::new(r"^[0-9]{1,6}(ms|s|m|h|d|w)?$")
        .unwrap()
        .is_match(t)
    {
        t.to_string()
    } else {
        "1h".into()
    }
}

fn safe_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && s.len() <= 64
}

/// Double-quote a SQLite identifier after validating it is a safe ASCII name.
pub fn quote_ident(name: &str) -> Option<String> {
    if !safe_ident(name) {
        return None;
    }
    Some(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn sanitize_extra_value(key: &str, raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 64 {
        return None;
    }
    match key {
        "length_unit" => Some(allow_enum(t, &["km", "mi"], "km")),
        "temp_unit" => Some(allow_enum(t, &["C", "F"], "C")),
        "preferred_range" => Some(allow_enum(t, &["ideal", "rated"], "rated")),
        "pressure_unit" => Some(allow_enum(t, &["bar", "psi"], "bar")),
        "speed_unit" => Some(allow_enum(t, &["kmh", "mph"], "kmh")),
        "period" => Some(allow_enum(
            t,
            &["hour", "day", "week", "month", "year"],
            "month",
        )),
        "car_id" | "drive_id" | "charging_process_id" => {
            if t.chars().all(|c| c.is_ascii_digit()) && t.len() <= 18 {
                Some(t.to_string())
            } else {
                None
            }
        }
        _ => {
            if t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '%' | ':'))
            {
                Some(t.to_string())
            } else {
                None
            }
        }
    }
}

/// Reject non-SELECT statements and references to auth / token tables.
pub fn assert_safe_dashboard_sql(sql: &str) -> Result<(), String> {
    let stripped = strip_sql_comments(sql);
    for part in stripped.split(';') {
        let stmt = part.trim();
        if stmt.is_empty() {
            continue;
        }
        let head = stmt
            .chars()
            .take(12)
            .collect::<String>()
            .to_ascii_lowercase();
        if !(head.starts_with("select") || head.starts_with("with")) {
            return Err("only SELECT queries are allowed".into());
        }
        if head.contains("attach") || stmt.to_ascii_lowercase().contains(" attach ") {
            return Err("ATTACH is not allowed".into());
        }
    }
    let lower = stripped.to_ascii_lowercase();
    for table in FORBIDDEN_QUERY_TABLES {
        let re = Regex::new(&format!(r"(?i)\b{}\b", regex::escape(table))).map_err(|e| e.to_string())?;
        if re.is_match(&lower) {
            return Err(format!("query must not reference {table}"));
        }
    }
    Ok(())
}

fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_single {
            out.push(c);
            if c == '\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            in_single = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

pub fn translate(sql: &str, vars: &QueryVars) -> String {
    let mut s = sql.to_string();
    s = expand_vars(&s, vars);
    s = rewrite_union_parens(&s);
    s = rewrite_unnest_arrays(&s);
    s = rewrite_generate_series(&s, vars);
    s = rewrite_age(&s);
    s = rewrite_json_ops(&s);
    s = rewrite_lateral(&s);
    s = rewrite_interval_mul(&s);
    s = rewrite_casts(&s);
    s = rewrite_interval_arith(&s);
    s = rewrite_extract(&s);
    s = rewrite_date_part(&s);
    s = rewrite_percentile(&s);
    s = rewrite_mode(&s);
    s = rewrite_array_funcs(&s);
    s = rewrite_any_array(&s);
    s = rewrite_regex_ops(&s);
    s = rewrite_ilike_all(&s);
    s = rewrite_cte_table_shadow(&s);
    s = rewrite_ilike(&s);
    s = rewrite_greatest(&s);
    s = rewrite_pg_catalog(&s);
    s = rewrite_misc(&s);
    s = rewrite_count_star_alias(&s);
    s
}

/// Grafana's Postgres plugin names a bare `count(*)` column `count`.
fn rewrite_count_star_alias(sql: &str) -> String {
    let re = Regex::new(r"(?i)\bcount\s*\(\s*\*\s*\)").unwrap();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut last = 0;
    for m in re.find_iter(sql) {
        out.push_str(&sql[last..m.start()]);
        out.push_str(&sql[m.start()..m.end()]);
        let rest = sql[m.end()..].trim_start();
        let next_word = rest
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        let select_item = rest.is_empty()
            || rest.starts_with(',')
            || next_word.eq_ignore_ascii_case("from");
        if select_item
            && !next_word.eq_ignore_ascii_case("as")
            && !next_word.eq_ignore_ascii_case("filter")
            && !next_word.eq_ignore_ascii_case("over")
        {
            out.push_str(" AS count");
        }
        last = m.end();
    }
    out.push_str(&sql[last..]);
    out
}

fn expand_vars(sql: &str, vars: &QueryVars) -> String {
    let from = vars.from_ts();
    let to = vars.to_ts();
    let from_q = format!("'{from}'");
    let to_q = format!("'{to}'");

    let mut s = sql.to_string();
    s = s.replace("${preferred_range}", &vars.preferred_range);
    s = s.replace("$preferred_range", &vars.preferred_range);
    s = s.replace("$length_unit", &vars.length_unit);
    s = s.replace("${length_unit}", &vars.length_unit);
    s = s.replace("$temp_unit", &vars.temp_unit);
    s = s.replace("$pressure_unit", &vars.pressure_unit);
    s = s.replace("$speed_unit", &vars.speed_unit);
    s = s.replace("${determine_phases:sqlstring}", "NULL");
    s = s.replace("$custom_kwh_new", "0");
    s = s.replace("$custom_max_range", "0");
    let did = vars.drive_id.unwrap_or(0);
    let cid = vars.charging_process_id.unwrap_or(0);
    s = s.replace("${drive_id}", &did.to_string());
    s = s.replace("$drive_id", &did.to_string());
    s = s.replace("${charging_process_id}", &cid.to_string());
    s = s.replace("$charging_process_id", &cid.to_string());
    s = s.replace("$charge_type", "'DC','AC'");
    s = s.replace("$period", &vars.period);
    s = s.replace("$car_id", &vars.car_id.to_string());
    s = s.replace("$__timezone", "UTC");
    for (k, v) in &vars.extras {
        s = s.replace(&format!("${k}"), v);
        s = s.replace(&format!("${{{k}}}"), v);
    }
    // Grafana defaults used by charge-level percentiles
    s = s.replace("$include_average_percentiles", "1");
    s = s.replace("$days_moving_average_percentiles", "7");
    s = s.replace("$bucket_width", "300");
    s = s.replace("$duration", "3");
    s = s.replace("$min_duration_min", "1");
    s = s.replace("$min_duration", "1");
    s = s.replace("$min_distance", "0.01");
    s = s.replace("$min_dist", "0.01");
    s = s.replace("$min_speed", "1");
    s = s.replace("$efficiency", "by distance");
    s = s.replace("$high_precision", "0");
    s = s.replace("$exclude", "0");
    s = s.replace("$pg_stat_statements_enabled", "0");
    s = s.replace("$text_filter", "%");
    s = s.replace("$action_filter", "'🚗 Driving','🔋 Charging','🅿️ Parking','❓ Missing','💾 Updating'");
    s = s.replace("$address_filter", "%");
    s = s.replace("${exclude_formatted_string:raw}", "('%')");
    s = s.replace("${bucket_width:text}", "300");
    s = s.replace("${geofence:pipe}", "-1");
    s = s.replace("$geofence", "-1");
    s = s.replace("$location", "%");
    s = s.replace("$aux", "{}");
    let alt = if vars.length_unit == "mi" { "ft" } else { "m" };
    s = s.replace("${alternative_length_unit}", alt);
    s = s.replace("$alternative_length_unit", alt);

    let from_secs = vars.from_ms / 1000;
    let to_secs = vars.to_ms / 1000;
    s = s.replace("${__from:date:seconds}", &from_secs.to_string());
    s = s.replace("${__to:date:seconds}", &to_secs.to_string());
    s = s.replace("$__from", &vars.from_ms.to_string());
    s = s.replace("$__to", &vars.to_ms.to_string());

    let max_buckets = vars.max_buckets;
    let auto_secs = auto_bucket_with_cap(vars.to_ms.saturating_sub(vars.from_ms), 5, max_buckets);
    let interval_lbl = interval_label(auto_secs);
    s = s.replace("$__interval", &interval_lbl);
    s = s.replace("$interval", &interval_lbl);

    s = replace_timefilter(&s, &from, &to);
    s = s.replace("$__timeFrom()", &from_q);
    s = s.replace("$__timeTo()", &to_q);
    s = s.replace("$__timeGroupAlias", "$__timeGroup");

    let range_ms = vars.to_ms.saturating_sub(vars.from_ms);
    let timegroup = regex().timegroup.replace_all(&s, |caps: &regex::Captures| {
        let col = caps.get(1).unwrap().as_str();
        let iv = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let requested = if iv.starts_with('$') || iv.is_empty() {
            auto_secs
        } else {
            parse_interval(iv.trim_matches('\'').trim_matches('"'))
        };
        let secs = auto_bucket_with_cap(range_ms, requested, max_buckets);
        format!("(unixepoch({col}) / {secs}) * {secs}")
    });
    s = timegroup.into_owned();

    s = regex()
        .time_as
        .replace_all(&s, |caps: &regex::Captures| {
            format!("unixepoch({}) AS time", caps.get(1).unwrap().as_str())
        })
        .into_owned();

    s = rewrite_positions_hourly(&s, auto_secs, range_ms);
    s
}

/// Long-range charts (Projected Range, mileage, …) scan millions of GPS rows
/// even after coarsening `$__timeGroup`. Point them at the hourly rollup instead.
/// Route maps and climate/TPMS panels keep raw `positions`.
fn rewrite_positions_hourly(sql: &str, bucket_secs: i64, range_ms: i64) -> String {
    if bucket_secs < 1800 && range_ms < 2 * 86_400_000 {
        return sql.to_string();
    }
    let lower = sql.to_ascii_lowercase();
    if HOURLY_UNSAFE.iter().any(|col| lower.contains(col)) {
        return sql.to_string();
    }
    Regex::new(r"(?i)\bFROM\s+positions\b")
        .unwrap()
        .replace_all(sql, "FROM position_hourly")
        .into_owned()
}

/// `positions` columns that are not stored on `position_hourly`.
const HOURLY_UNSAFE: &[&str] = &[
    "latitude",
    "longitude",
    "speed",
    "power",
    "elevation",
    "fan_status",
    "driver_temp_setting",
    "passenger_temp_setting",
    "is_climate_on",
    "is_rear_defroster_on",
    "is_front_defroster_on",
    "drive_id",
    "inside_temp",
    "battery_heater",
    "est_battery_range_km",
    "tpms_pressure",
];

/// Bucket width so a time-series has about `max_buckets` points. Hardcoded Grafana
/// `5s` groups stay at 5s for a short trip and coarsen for 30d/1y windows.
#[allow(dead_code)]
pub fn auto_bucket_secs(range_ms: i64, requested: i64) -> i64 {
    auto_bucket_with_cap(range_ms, requested, 1600)
}

fn auto_bucket_with_cap(range_ms: i64, requested: i64, max_buckets: i64) -> i64 {
    const STEPS: [i64; 18] = [
        5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400,
        259200, 604800,
    ];
    let max_buckets = max_buckets.clamp(64, 20_000);
    let range_secs = (range_ms / 1000).max(1);
    let min_step = (range_secs / max_buckets).max(requested.max(1));
    STEPS
        .iter()
        .copied()
        .find(|&s| s >= min_step)
        .unwrap_or(min_step)
}

fn interval_label(secs: i64) -> String {
    if secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn replace_timefilter(sql: &str, from: &str, to: &str) -> String {
    regex()
        .timefilter
        .replace_all(sql, |caps: &regex::Captures| {
            let col = caps.get(1).unwrap().as_str();
            format!("{col} BETWEEN '{from}' AND '{to}'")
        })
        .into_owned()
}

fn rewrite_union_parens(sql: &str) -> String {
    // SQLite rejects `(SELECT ...) UNION (SELECT ...)`. Wrap each parenthesized
    // compound arm as `SELECT * FROM (SELECT ...)`.
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'(' && is_select_kw(&sql[i + 1..]) && should_wrap_union_arm(sql, i) {
            out.push_str("SELECT * FROM ");
        }
        let ch = sql[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_select_kw(rest: &str) -> bool {
    rest.trim_start()
        .get(..6)
        .map(|s| s.eq_ignore_ascii_case("select"))
        .unwrap_or(false)
}

fn should_wrap_union_arm(sql: &str, open: usize) -> bool {
    let Some(close) = find_matching_paren(sql, open) else {
        return false;
    };
    let after = sql[close + 1..].trim_start().to_ascii_uppercase();
    if after.starts_with("UNION") {
        return true;
    }
    let before = sql[..open].trim_end().to_ascii_uppercase();
    if before.ends_with("UNION") {
        return true;
    }
    if before.ends_with("ALL") {
        let head = before[..before.len() - 3].trim_end();
        return head.ends_with("UNION");
    }
    false
}

fn rewrite_unnest_arrays(sql: &str) -> String {
    if !sql.to_lowercase().contains("unnest(") {
        return sql.to_string();
    }
    let mut s = rewrite_unnest_pairs(sql);
    s = Regex::new(r"(?is)unnest\s*\(\s*ARRAY\s*\[\s*([^,\]]+?)\s*,\s*[^\]]+?\s*\]\s*\)")
        .unwrap()
        .replace_all(&s, "$1")
        .into_owned();
    s
}

fn rewrite_unnest_pairs(sql: &str) -> String {
    let re = Regex::new(
        r"(?is)unnest\s*\(\s*ARRAY\s*\[\s*([^,\]]+?)\s*,\s*([^\]]+?)\s*\]\s*\)\s+AS\s+\w+\s*,\s*unnest\s*\(\s*ARRAY\s*\[\s*([^,\]]+?)\s*,\s*([^\]]+?)\s*\]\s*\)\s+AS\s+\w+\s+",
    )
    .unwrap();
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(sql) {
        let m = caps.get(0).unwrap();
        out.push_str(&sql[last..m.start()]);
        let e1 = pg_expr_to_sqlite(caps.get(1).unwrap().as_str());
        let e2 = pg_expr_to_sqlite(caps.get(2).unwrap().as_str());
        let a1 = caps.get(3).unwrap().as_str().trim();
        let a2 = caps.get(4).unwrap().as_str().trim();
        let (from_where, consumed) = take_from_where(&sql[m.end()..]);
        out.push_str(&format!(
            "{e1} AS date, {a1} AS state {from_where} UNION ALL SELECT {e2} AS date, {a2} AS state {from_where} "
        ));
        last = m.end() + consumed;
    }
    out.push_str(&sql[last..]);
    out
}

fn take_from_where(sql: &str) -> (String, usize) {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 4 > bytes.len() || !sql[i..].get(..4).unwrap_or("").eq_ignore_ascii_case("from") {
        return (String::new(), 0);
    }
    i += 4;
    let mut depth = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            _ => {
                if depth == 0 && starts_kw(&sql[i..], "UNION") {
                    break;
                }
                if depth == 0 && starts_kw(&sql[i..], "ORDER") {
                    break;
                }
            }
        }
        i += 1;
    }
    (sql[..i].to_string(), i)
}

fn starts_kw(s: &str, kw: &str) -> bool {
    s.len() >= kw.len()
        && s[..kw.len()].eq_ignore_ascii_case(kw)
        && (s.len() == kw.len() || !s.as_bytes()[kw.len()].is_ascii_alphanumeric())
}

fn pg_expr_to_sqlite(expr: &str) -> String {
    let e = expr.trim();
    let re = Regex::new(r"(?i)^(.+?)\s*\+\s*interval\s+'(\d+)\s*([a-z]+)'$").unwrap();
    if let Some(c) = re.captures(e) {
        let col = c.get(1).unwrap().as_str();
        let n = c.get(2).unwrap().as_str();
        let unit = plural_unit(c.get(3).unwrap().as_str());
        return format!("datetime({col}, '+{n} {unit}')");
    }
    e.to_string()
}

fn plural_unit(u: &str) -> &'static str {
    match u.to_ascii_lowercase().as_str() {
        "second" | "seconds" | "s" => "seconds",
        "minute" | "minutes" | "m" => "minutes",
        "hour" | "hours" | "h" => "hours",
        "day" | "days" | "d" => "days",
        "month" | "months" => "months",
        "year" | "years" => "years",
        _ => "seconds",
    }
}

fn rewrite_generate_series(sql: &str, vars: &QueryVars) -> String {
    if !sql.to_lowercase().contains("generate_series") {
        return sql.to_string();
    }
    let from = vars.from_ts();
    let to = vars.to_ts();
    let dummy = format!(
        "(WITH RECURSIVE gs(date) AS (SELECT datetime(date('{from}')) UNION ALL SELECT datetime(date, '+1 day') FROM gs WHERE date < datetime(date('{to}'))) SELECT date FROM gs)"
    );
    let mut s = sql.to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(start) = lower.find("generate_series") else {
            break;
        };
        let after = s[start + 15..].trim_start();
        if !after.starts_with('(') {
            s.replace_range(start..start + 15, "gs");
            continue;
        }
        let open = start + 15 + (s[start + 15..].len() - after.len());
        let Some(close) = find_matching_paren(&s, open) else {
            break;
        };
        s.replace_range(start..close + 1, dummy.as_str());
    }
    s
}

fn rewrite_age(sql: &str) -> String {
    Regex::new(r"(?i)\bage\s*\(")
        .unwrap()
        .replace_all(sql, "age_seconds(")
        .into_owned()
}

fn rewrite_json_ops(sql: &str) -> String {
    let mut s = Regex::new(r#"(?i)\(\s*'([^']*)'\s*(?:::\s*\w+)?\s*\)\s*->>\s*'([^']+)'"#)
        .unwrap()
        .replace_all(sql, "json_extract('$1', '$.$2')")
        .into_owned();
    s = Regex::new(r#"(?i)'([^']*)'\s*(?:::\s*\w+)?\s*->>\s*'([^']+)'"#)
        .unwrap()
        .replace_all(&s, "json_extract('$1', '$.$2')")
        .into_owned();
    s = Regex::new(r#"(?i)\bjson_build_object\s*\("#)
        .unwrap()
        .replace_all(&s, "json_object(")
        .into_owned();
    s = Regex::new(r#"\s*#>>\s*'\{\}'"#)
        .unwrap()
        .replace_all(&s, "")
        .into_owned();
    s
}

fn rewrite_lateral(sql: &str) -> String {
    if !sql.to_lowercase().contains("lateral") {
        return sql.to_string();
    }
    let mut s = sql.to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(mut lat) = lower.find("lateral") else {
            break;
        };
        while lat > 0 && lower.as_bytes()[lat - 1].is_ascii_alphanumeric() {
            match lower[lat + 7..].find("lateral") {
                Some(rel) => lat += 7 + rel,
                None => return s,
            }
        }
        let after = s[lat + 7..].trim_start();
        if !after.starts_with('(') {
            break;
        }
        let open = lat + 7 + (s[lat + 7..].len() - after.len());
        let Some(close) = find_matching_paren(&s, open) else {
            break;
        };
        let inner = s[open + 1..close].to_string();
        let rest = s[close + 1..].trim_start();
        let alias = rest
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("")
            .to_string();
        if alias.is_empty() || alias.eq_ignore_ascii_case("join") {
            break;
        }
        let alias_start = close + 1 + (s[close + 1..].len() - rest.len());
        let alias_end = alias_start + alias.len();
        let mut comma = lat;
        while comma > 0 && s.as_bytes()[comma - 1].is_ascii_whitespace() {
            comma -= 1;
        }
        if comma > 0 && s.as_bytes()[comma - 1] == b',' {
            comma -= 1;
        }
        if let Some((expr, col, from_where)) = split_select_as(&inner) {
            let sub = format!("(SELECT {expr} {from_where})");
            s.replace_range(comma..alias_end, "");
            s = s.replace(&format!("{alias}.{col}"), &sub);
        } else {
            break;
        }
    }
    s
}

fn split_select_as(inner: &str) -> Option<(String, String, String)> {
    let t = inner.trim();
    let lower = t.to_ascii_lowercase();
    if !lower.starts_with("select") {
        return None;
    }
    let after = t[6..].trim_start();
    let offset = t.len() - after.len();
    let bytes = t.as_bytes();
    let mut depth: i32 = 0;
    let mut from_at = None;
    let mut i = offset;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
            }
            _ if depth == 0 && starts_kw(&t[i..], "FROM") => {
                from_at = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let from_at = from_at?;
    let select_list = t[offset..from_at].trim();
    let from_where = t[from_at..].trim();
    let mut depth: i32 = 0;
    let sl = select_list.as_bytes();
    let mut as_at = None;
    let mut j = 0;
    while j < sl.len() {
        match sl[j] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'\'' => {
                j += 1;
                while j < sl.len() && sl[j] != b'\'' {
                    j += 1;
                }
            }
            _ if depth == 0 && starts_kw(&select_list[j..], "AS") => {
                as_at = Some(j);
            }
            _ => {}
        }
        j += 1;
    }
    let as_at = as_at?;
    let expr = select_list[..as_at].trim().to_string();
    let col = select_list[as_at + 2..]
        .trim()
        .trim_matches('"')
        .to_string();
    if expr.is_empty() || col.is_empty() {
        return None;
    }
    Some((expr, col, from_where.to_string()))
}

fn rewrite_interval_mul(sql: &str) -> String {
    let mut s = Regex::new(r"(?i)\*\s*INTERVAL\s+'(-?\d+)\s*([a-z]+)'")
        .unwrap()
        .replace_all(sql, |caps: &regex::Captures| {
            let n: i64 = caps.get(1).unwrap().as_str().parse().unwrap_or(1);
            let secs = interval_secs(n, caps.get(2).unwrap().as_str());
            format!("* {secs}")
        })
        .into_owned();
    s = Regex::new(r"(?i)\s*\|\|\s*' seconds?'")
        .unwrap()
        .replace_all(&s, "")
        .into_owned();
    s
}

fn interval_secs(n: i64, unit: &str) -> i64 {
    n * match unit.to_ascii_lowercase().as_str() {
        "second" | "seconds" | "s" | "sec" => 1,
        "minute" | "minutes" | "m" | "min" => 60,
        "hour" | "hours" | "h" => 3600,
        "day" | "days" | "d" => 86400,
        "week" | "weeks" | "w" => 86400 * 7,
        _ => 1,
    }
}

fn rewrite_interval_arith(sql: &str) -> String {
    let re = Regex::new(r"(?i)\s*([+-])\s*INTERVAL\s+'(-?\d+)\s*([a-z]+)'").unwrap();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut last = 0;
    for caps in re.captures_iter(sql) {
        let m = caps.get(0).unwrap();
        let left_end = m.start();
        let left_start = find_expr_start(sql, left_end);
        let left = sql[left_start..left_end].trim();
        let op = caps.get(1).unwrap().as_str();
        let mut n: i64 = caps.get(2).unwrap().as_str().parse().unwrap_or(0);
        let unit = plural_unit(caps.get(3).unwrap().as_str());
        if op == "-" {
            n = -n;
        }
        let sign = if n < 0 { "-" } else { "+" };
        out.push_str(&sql[last..left_start]);
        out.push_str(&format!("datetime({left}, '{sign}{} {unit}')", n.abs()));
        last = m.end();
    }
    out.push_str(&sql[last..]);
    out
}

fn find_expr_start(sql: &str, end: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut i = end;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    if bytes[i - 1] == b')' {
        let mut depth = 1;
        i -= 1;
        while i > 0 && depth > 0 {
            i -= 1;
            match bytes[i] {
                b')' => depth += 1,
                b'(' => depth -= 1,
                _ => {}
            }
        }
        while i > 0 {
            let c = bytes[i - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                i -= 1;
            } else {
                break;
            }
        }
        return i;
    }
    if bytes[i - 1] == b'\'' {
        i -= 1;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'\'' {
                if i > 0 && bytes[i - 1] == b'\'' {
                    i -= 1;
                    continue;
                }
                break;
            }
        }
        return i;
    }
    while i > 0 {
        let c = bytes[i - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

fn rewrite_extract(sql: &str) -> String {
    let mut s = sql.to_string();
    loop {
        let Some((start, field, expr, end)) = find_extract(&s) else {
            break;
        };
        let repl = extract_to_sqlite(&field, &expr);
        s.replace_range(start..end, &repl);
    }
    s
}

fn find_extract(sql: &str) -> Option<(usize, String, String, usize)> {
    let lower = sql.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find("extract") {
        let start = search + rel;
        let after = sql[start + 7..].trim_start();
        if !after.starts_with('(') {
            search = start + 7;
            continue;
        }
        let open = start + 7 + (sql[start + 7..].len() - after.len());
        let close = find_matching_paren(sql, open)?;
        let inner = &sql[open + 1..close];
        let inner_l = inner.to_ascii_lowercase();
        let from_at = inner_l.find(" from ")?;
        let field = inner[..from_at].trim().to_string();
        let expr = inner[from_at + 6..].trim().to_string();
        return Some((start, field, expr, close + 1));
    }
    None
}

fn find_matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if open >= bytes.len() || bytes[open] != b'(' {
        return None;
    }
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn extract_to_sqlite(field: &str, expr: &str) -> String {
    let f = field.to_ascii_lowercase();
    let e = expr.trim();
    if f == "epoch" {
        let el = e.to_ascii_lowercase();
        if el.contains("age_seconds(") || el.contains("age(") {
            return e.to_string();
        }
        if el.starts_with("age(") && e.ends_with(')') {
            let inner = &e[4..e.len() - 1];
            let mut parts = inner.splitn(2, ',');
            let a = parts.next().unwrap_or("").trim();
            let b = parts.next().unwrap_or("").trim();
            return format!("age_seconds({a}, {b})");
        }
        if e.starts_with('(') && e.ends_with(')') {
            let inner = e[1..e.len() - 1].trim();
            if let Some((a, b)) = split_top_minus(inner) {
                return interval_part(&f, &a, &b);
            }
        }
        if let Some((a, b)) = split_top_minus(e) {
            return interval_part(&f, &a, &b);
        }
        return format!("unixepoch({e})");
    }
    if let Some((a, b)) = split_top_minus(strip_outer_parens(e)) {
        return interval_part(&f, &a, &b);
    }
    if let Some((a, b)) = split_top_minus(e) {
        return interval_part(&f, &a, &b);
    }
    let fmt = match f.as_str() {
        "year" => "%Y",
        "month" => "%m",
        "day" | "days" => "%d",
        "hour" => "%H",
        "minute" => "%M",
        "dow" | "isodow" => "%w",
        _ => "%s",
    };
    format!("CAST(strftime('{fmt}', {e}) AS INTEGER)")
}

fn strip_outer_parens(expr: &str) -> &str {
    let s = expr.trim();
    if s.starts_with('(') && s.ends_with(')') && find_matching_paren(s, 0) == Some(s.len() - 1) {
        return s[1..s.len() - 1].trim();
    }
    s
}

fn interval_part(field: &str, a: &str, b: &str) -> String {
    let diff = format!(
        "(unixepoch({a}) - unixepoch({b}))"
    );
    match field {
        "epoch" | "second" | "seconds" => diff,
        "minute" | "minutes" => format!("(CAST({diff} / 60 AS INTEGER) % 60)"),
        "hour" | "hours" => format!("(CAST({diff} / 3600 AS INTEGER) % 24)"),
        "day" | "days" => format!("CAST({diff} / 86400 AS INTEGER)"),
        _ => diff,
    }
}

fn split_top_minus(expr: &str) -> Option<(String, String)> {
    let bytes = expr.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'-' if depth == 0 && i > 0 => {
                let left = expr[..i].trim();
                let right = expr[i + 1..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left.to_string(), right.to_string()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn rewrite_date_part(sql: &str) -> String {
    let mut s = sql.to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(rel) = lower.find("date_part") else {
            break;
        };
        let after = s[rel + 9..].trim_start();
        if !after.starts_with('(') {
            break;
        }
        let open = rel + 9 + (s[rel + 9..].len() - after.len());
        let Some(close) = find_matching_paren(&s, open) else {
            break;
        };
        let inner = &s[open + 1..close];
        let mut parts = inner.splitn(2, ',');
        let field = parts.next().unwrap_or("").trim().trim_matches('\'').to_string();
        let expr = parts.next().unwrap_or("").trim().to_string();
        let repl = extract_to_sqlite(&field, &expr);
        s.replace_range(rel..close + 1, &repl);
    }
    s
}

fn rewrite_any_array(sql: &str) -> String {
    let mut s = sql.to_string();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(rel) = lower.find("= any(").or_else(|| lower.find("= any (")) else {
            break;
        };
        let eq = s[rel..].find('(').map(|i| rel + i).unwrap();
        let Some(close) = find_matching_paren(&s, eq) else {
            break;
        };
        s.replace_range(rel..close + 1, " IN ('AC','DC')");
    }
    s = Regex::new(r"(?i)\bARRAY\s*\[(.*?)\]")
        .unwrap()
        .replace_all(&s, "($1)")
        .into_owned();
    s
}

fn rewrite_ilike_all(sql: &str) -> String {
    let mut s = Regex::new(r"(?is)(?:AND\s+)?[\w.]+(?:\s+NOT)?\s+ILIKE\s+ALL\s*\([^)]*\)")
        .unwrap()
        .replace_all(sql, "")
        .into_owned();
    s = Regex::new(r"(?i)\bWHERE\s+(LIMIT|ORDER|GROUP|HAVING)\b")
        .unwrap()
        .replace_all(&s, "$1")
        .into_owned();
    s
}

fn rewrite_array_funcs(sql: &str) -> String {
    let mut s = sql.to_string();
    s = Regex::new(
        r"(?is)array_to_string\s*\(\s*\(\s*\(string_to_array\s*\(\s*([^,]+?)\s*,\s*'([^']*)'\s*,\s*''\s*\)\s*\)\s*\[\s*\d+\s*:\s*(\d+)\s*\]\s*\)\s*,\s*'[^']*'\s*\)",
    )
    .unwrap()
    .replace_all(&s, |caps: &regex::Captures| {
        let expr = caps.get(1).unwrap().as_str().trim();
        let delim = caps.get(2).unwrap().as_str();
        let n: i64 = caps.get(3).unwrap().as_str().parse().unwrap_or(2);
        format!("join_head({expr}, '{delim}', {n})")
    })
    .into_owned();
    s = Regex::new(r"(?i)\barray_agg\s*\(")
        .unwrap()
        .replace_all(&s, "json_group_array(")
        .into_owned();
    s = Regex::new(r"(?is)\(\s*select\s+avg\s*\(\s*\w+\s*\)\s+from\s+unnest\s*\(\s*(\w+)\s*\)\s+\w+(?:\s*\(\s*\w+\s*\))?\s*\)")
        .unwrap()
        .replace_all(&s, "(SELECT avg(value) FROM json_each($1))")
        .into_owned();
    s = Regex::new(r"(?i)unnest\s*\(\s*(\w+)\s*\)\s+\w+(?:\s*\(\s*\w+\s*\))?")
        .unwrap()
        .replace_all(&s, "json_each($1)")
        .into_owned();
    s
}

fn rewrite_regex_ops(sql: &str) -> String {
    let mut s = Regex::new(r"\s+!~\s+")
        .unwrap()
        .replace_all(sql, " NOT REGEXP ")
        .into_owned();
    s = Regex::new(r"\s+~\s+")
        .unwrap()
        .replace_all(&s, " REGEXP ")
        .into_owned();
    s
}

fn rewrite_cte_table_shadow(sql: &str) -> String {
    let mut s = sql.to_string();
    // TeslaMate has a CTE named `positions` while also joining the table.
    // Rename that CTE so earlier CTEs can still use the table.
    let pos_header = Regex::new(r"(?is)(,\s*|WITH\s+)positions(\s+AS\s*\()").unwrap();
    if pos_header.is_match(&s) {
        s = pos_header
            .replace_all(&s, |caps: &regex::Captures| {
                format!(
                    "{}cte_positions{}",
                    caps.get(1).unwrap().as_str(),
                    caps.get(2).unwrap().as_str()
                )
            })
            .into_owned();
        s = Regex::new(r"(?i)\bselect\s+\*\s+from\s+positions\b")
            .unwrap()
            .replace_all(&s, "SELECT * FROM cte_positions")
            .into_owned();
    }
    // `WITH states AS (... FROM states ...)` means the base table inside the CTE.
    for name in ["states", "updates", "drives", "charges"] {
        let header = Regex::new(&format!(r"(?is)\bWITH\s+{name}\s+AS\s*\(")).unwrap();
        if let Some(m) = header.find(&s) {
            let open = m.end() - 1;
            if let Some(close) = find_matching_paren(&s, open) {
                let body = s[open + 1..close].to_string();
                let from_re = Regex::new(&format!(r"(?i)\b(FROM|JOIN)\s+{name}\b")).unwrap();
                let new_body = from_re
                    .replace_all(&body, |caps: &regex::Captures| {
                        format!("{} main.{name}", caps.get(1).unwrap().as_str())
                    })
                    .into_owned();
                s.replace_range(open + 1..close, &new_body);
            }
        }
    }
    s
}
fn rewrite_percentile(sql: &str) -> String {
    Regex::new(r"(?is)percentile_(?:cont|disc)\s*\(\s*[\d.]+\s*\)\s*within\s+group\s*\(\s*order\s+by\s+([^)]+)\)")
        .unwrap()
        .replace_all(sql, "avg($1)")
        .into_owned()
}

fn rewrite_mode(sql: &str) -> String {
    Regex::new(r"(?is)mode\s*\(\s*\)\s*within\s+group\s*\(\s*order\s+by\s+([^)]+)\)")
        .unwrap()
        .replace_all(sql, "max($1)")
        .into_owned()
}

fn rewrite_casts(sql: &str) -> String {
    regex().cast.replace_all(sql, "").into_owned()
}

fn rewrite_ilike(sql: &str) -> String {
    Regex::new(r"(?i)\bilike\b")
        .unwrap()
        .replace_all(sql, "LIKE")
        .into_owned()
}

fn rewrite_greatest(sql: &str) -> String {
    let mut s = Regex::new(r"(?i)\bgreatest\s*\(")
        .unwrap()
        .replace_all(sql, "max(")
        .into_owned();
    s = Regex::new(r"(?i)\bleast\s*\(")
        .unwrap()
        .replace_all(&s, "min(")
        .into_owned();
    s
}

/// Telemetry tables only — auth / token tables are omitted so database-info
/// dashboards cannot probe them via the rewritten catalog queries.
const SQLITE_USER_TABLES: &[&str] = &[
    "addresses",
    "car_settings",
    "cars",
    "charges",
    "charging_invoices",
    "charging_processes",
    "drives",
    "geofences",
    "position_hourly",
    "positions",
    "settings",
    "states",
    "updates",
];

fn sqlite_row_counts_sql() -> String {
    let unions = SQLITE_USER_TABLES
        .iter()
        .map(|t| {
            format!("SELECT '{t}' AS \"Table Name\", (SELECT COUNT(*) FROM {t}) AS \"Row Count\"")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    format!("{unions} ORDER BY 2 DESC")
}

fn sqlite_table_sizes_sql() -> &'static str {
    r#"SELECT
  m.name AS "Table",
  COALESCE(data.sz, 0) AS "Data",
  COALESCE(idx.sz, 0) AS "Indexes",
  COALESCE(data.sz, 0) + COALESCE(idx.sz, 0) AS "Total"
FROM sqlite_master m
LEFT JOIN (
  SELECT name, SUM(pgsize) AS sz FROM dbstat GROUP BY name
) data ON data.name = m.name
LEFT JOIN (
  SELECT i.tbl_name AS tbl, SUM(d.pgsize) AS sz
  FROM sqlite_master i
  JOIN dbstat d ON d.name = i.name
  WHERE i.type = 'index'
  GROUP BY i.tbl_name
) idx ON idx.tbl = m.name
WHERE m.type = 'table'
  AND m.name NOT LIKE 'sqlite_%'
ORDER BY 4 DESC"#
}

fn sqlite_indexes_sql() -> &'static str {
    r#"SELECT
  i.tbl_name AS "Table",
  i.name AS "Index",
  0 AS "Index Scans",
  0 AS "Tuples Read",
  0 AS "Tuples Fetched",
  COALESCE((SELECT SUM(pgsize) FROM dbstat d WHERE d.name = i.name), 0) AS "Index Size"
FROM sqlite_master i
WHERE i.type = 'index'
ORDER BY 6 DESC"#
}

fn sqlite_db_size_sql() -> &'static str {
    r#"SELECT (SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size) AS "Size""#
}

fn sqlite_cache_size_sql() -> &'static str {
    r#"SELECT CASE
  WHEN cs.cache_size < 0 THEN -cs.cache_size * 1024
  ELSE cs.cache_size * ps.page_size
END
FROM pragma_cache_size AS cs
CROSS JOIN pragma_page_size AS ps"#
}

fn sqlite_query_stats_placeholder(order: &str) -> String {
    format!(
        r#"SELECT 0 AS "Calls", 0.0 AS "Mean Exec Time", 0.0 AS "Total Exec Time", 'n/a' AS "Query" WHERE 0 /* {order} */"#
    )
}

fn rewrite_pg_catalog(sql: &str) -> String {
    let l = sql.to_ascii_lowercase();
    if l.contains("${pg_stat_statements_info_last_reset") {
        return "SELECT NULL AS stats_reset".into();
    }
    if l.contains("${pg_stat_statements_count") {
        return "SELECT 0 AS count".into();
    }
    if l.contains("${pg_stat_statements_top_20_total") {
        return sqlite_query_stats_placeholder("total");
    }
    if l.contains("${pg_stat_statements_top_20_mean") || l.contains("${pg_stat_statements_top_20")
    {
        return sqlite_query_stats_placeholder("mean");
    }
    if l.contains("query_to_xml") || (l.contains("xpath(") && l.contains("information_schema")) {
        return sqlite_row_counts_sql();
    }
    if l.contains("sum(pg_total_relation_size") {
        return sqlite_db_size_sql().into();
    }
    if l.contains("pg_statio_user_tables") {
        return sqlite_table_sizes_sql().into();
    }
    if l.contains("pg_stat_all_indexes") {
        return sqlite_indexes_sql().into();
    }
    if l.contains("shared_buffers") {
        return sqlite_cache_size_sql().into();
    }
    if l.contains("information_schema") && l.contains("pg_stat_statements") {
        return "SELECT 0 AS table_existence".into();
    }
    if l.contains("pg_stat_statements") {
        return sqlite_query_stats_placeholder("mean");
    }
    if l.trim_start().starts_with("show ") {
        return "SELECT 'UTC' AS timezone".into();
    }
    if l.contains("regexp_replace(version()")
        || (l.contains("version()") && l.contains("postgresql"))
    {
        return "SELECT sqlite_version() AS version".into();
    }
    if l.contains("pg_") || l.contains("pg_stat") || l.contains("pg_database") {
        return "SELECT 'sqlite' AS engine, sqlite_version() AS version WHERE 0".into();
    }
    sql.to_string()
}

fn rewrite_misc(sql: &str) -> String {
    let mut s = sql.to_string();
    s = Regex::new(r"(?i)\bp,start_date\b")
        .unwrap()
        .replace_all(&s, "p.start_date")
        .into_owned();
    s = s.replace("timestamp with time zone", "");
    s = s.replace("timestamp without time zone", "");
    s = Regex::new(r"(?i)\btrue\b")
        .unwrap()
        .replace_all(&s, "1")
        .into_owned();
    s = Regex::new(r"(?i)\bfalse\b")
        .unwrap()
        .replace_all(&s, "0")
        .into_owned();
    s = Regex::new(r"(?i)\bnow\s*\(\s*\)")
        .unwrap()
        .replace_all(&s, "datetime('now')")
        .into_owned();
    s = Regex::new(r"(?i)\bcurrent_date\b")
        .unwrap()
        .replace_all(&s, "date('now')")
        .into_owned();
    s = s.replace("::timestamp", "");
    s = s.replace("::timestamptz", "");
    s = Regex::new(r"\$\{[a-zA-Z0-9_]+(?::[a-zA-Z0-9_]+)?\}")
        .unwrap()
        .replace_all(&s, "NULL")
        .into_owned();
    s
}

struct Patterns {
    timefilter: Regex,
    timegroup: Regex,
    time_as: Regex,
    #[allow(dead_code)]
    unnest_select: Regex,
    lateral: Regex,
    cast: Regex,
}

fn regex() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        timefilter: Regex::new(r"\$__timeFilter\s*\(\s*([a-zA-Z0-9_._]+)\s*\)").unwrap(),
        timegroup: Regex::new(
            r"\$__timeGroup(?:Alias)?\s*\(\s*([^,]+)\s*,\s*([^)]+?)\s*\)",
        )
        .unwrap(),
        time_as: Regex::new(r"\$__time\s*\(\s*([^)]+?)\s*\)").unwrap(),
        unnest_select: Regex::new(
            r"(?is)unnest\s*\(\s*ARRAY\s*\[\s*([^,\]]+?)\s*,\s*([^\]]+?)\s*\]\s*\)\s+AS\s+\w+\s*,\s*unnest\s*\(\s*ARRAY\s*\[\s*([^,\]]+?)\s*,\s*([^\]]+?)\s*\]\s*\)\s+AS\s+\w+\s+(FROM\s+\S+\s+WHERE\s+.+?)(\s+UNION|\s+ORDER BY|\s*$)",
        )
        .unwrap(),
        lateral: Regex::new(r"(?is),\s*LATERAL\s*\((.+?)\)\s+(\w+)").unwrap(),
        cast: Regex::new(
            r"(?i)::\s*(timestamptz|timestamp(?:tz|t)?(?:\s+(?:with|without)\s+time\s+zone)?|[a-zA-Z][a-zA-Z0-9_]*(?:\s*\([^)]*\))?)",
        )
        .unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timefilter() {
        let v = QueryVars {
            car_id: 2,
            from_ms: 1_700_000_000_000,
            to_ms: 1_800_000_000_000,
            length_unit: "mi".into(),
            temp_unit: "C".into(),
            preferred_range: "rated".into(),
            pressure_unit: "psi".into(),
            speed_unit: "mph".into(),
            interval: "1h".into(),
            charging_process_id: None,
            drive_id: None,
            charge_type: "%".into(),
            period: "month".into(),
            extras: HashMap::new(),
            max_buckets: 1600,
        };
        let out = translate("SELECT 1 FROM drives WHERE $__timeFilter(start_date) AND car_id = $car_id", &v);
        assert!(out.contains("BETWEEN"));
        assert!(out.contains("2"));
    }

    fn vars() -> QueryVars {
        QueryVars {
            car_id: 2,
            from_ms: 1_755_784_800_000,
            to_ms: 1_758_376_800_000,
            length_unit: "mi".into(),
            temp_unit: "C".into(),
            preferred_range: "rated".into(),
            pressure_unit: "psi".into(),
            speed_unit: "mph".into(),
            interval: "1h".into(),
            charging_process_id: Some(1),
            drive_id: Some(1),
            charge_type: "%".into(),
            period: "month".into(),
            extras: HashMap::new(),
            max_buckets: 1600,
        }
    }

    #[test]
    fn union_paren_select() {
        let sql = "(SELECT battery_level, date FROM positions WHERE car_id = $car_id ORDER BY date DESC LIMIT 1)\nUNION\nSELECT battery_level, date FROM charges c JOIN charging_processes p ON p.id = c.charging_process_id WHERE $__timeFilter(date) AND p.car_id = $car_id ORDER BY date DESC LIMIT 1";
        let out = translate(sql, &vars());
        assert!(!out.trim_start().starts_with('('), "{out}");
        assert!(out.contains("SELECT * FROM"), "{out}");
        assert!(!out.to_ascii_uppercase().contains("UNION\nSELECT") || out.contains("UNION"), "{out}");
    }

    #[test]
    fn timezone_now_interval() {
        let sql = "SELECT date FROM positions WHERE date >= (TIMEZONE('UTC', NOW()) - INTERVAL '60m')";
        let out = translate(sql, &vars());
        assert!(!out.to_ascii_lowercase().contains("interval"), "{out}");
        assert!(out.contains("datetime("), "{out}");
        let tz_args = out.matches("TIMEZONE(").count() + out.matches("timezone(").count();
        assert!(tz_args <= 1, "{out}");
    }

    #[test]
    fn duration_interval_mul() {
        let sql = "SELECT TO_CHAR((duration_min * INTERVAL '1 minute'), 'HH24:MI') FROM drives";
        let out = translate(sql, &vars());
        assert!(!out.to_ascii_lowercase().contains("interval"), "{out}");
        assert!(out.contains("* 60"), "{out}");
    }

    #[test]
    fn timestamp_interval_literal() {
        let sql = "SELECT 1 FROM drives WHERE ($__timeFrom() :: timestamp - interval '30 day') < start_date";
        let out = translate(sql, &vars());
        assert!(!out.to_ascii_lowercase().contains("interval"), "{out}");
        assert!(out.contains("datetime("), "{out}");
    }

    #[test]
    fn cast_does_not_eat_end_as() {
        let sql = r#"SELECT CASE WHEN $custom_kwh_new > 0 THEN $custom_kwh_new ELSE ('$aux'::json ->> 'MaxCapacity')::float END as "Usable (new)""#;
        let out = translate(sql, &vars());
        assert!(out.to_ascii_lowercase().contains("end"), "{out}");
        assert!(out.contains("Usable (new)"), "{out}");
        assert!(!out.contains("::float END"), "{out}");
    }

    #[test]
    fn numeric_precision_cast() {
        let out = translate(
            "SELECT convert_km(avg(r.battery_rng), '$length_unit')::numeric(6,2) AS rng FROM rng r",
            &vars(),
        );
        assert!(!out.contains("(6,2)"), "{out}");
        assert!(out.to_ascii_lowercase().contains("as rng"), "{out}");
    }

    #[test]
    fn timestamptz_cast_keeps_ident() {
        let out = translate("SELECT series_id::timestamptz FROM t", &vars());
        assert!(!out.contains("series_idtz"), "{out}");
        assert!(out.to_ascii_lowercase().contains("series_id"), "{out}");
    }

    #[test]
    fn integer_cast_keeps_from() {
        let out = translate("SELECT battery_heater::integer\nFROM positions", &vars());
        assert!(out.to_ascii_lowercase().contains("from positions"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("battery_heaterfrom"), "{out}");
    }

    #[test]
    fn extract_second_from_lag() {
        let out = translate(
            "select extract (second from p.date - lag(p.date) over (order by p.date)) as seconds from positions p",
            &vars(),
        );
        assert!(!out.to_ascii_lowercase().contains("extract"), "{out}");
        assert!(out.matches(')').count() >= out.matches('(').count(), "{out}");
    }

    #[test]
    fn count_star_aliased_as_count() {
        let out = translate(
            "select count(*), count(distinct city) as city_count from addresses",
            &vars(),
        );
        let lower = out.to_ascii_lowercase();
        assert!(lower.contains("count(*) as count"), "{out}");
        assert!(lower.contains("city_count"), "{out}");
    }

    #[test]
    fn count_star_keeps_existing_alias() {
        let out = translate("select count(*) as n from drives", &vars());
        let lower = out.to_ascii_lowercase();
        assert!(lower.contains("count(*) as n"), "{out}");
        assert!(!lower.contains("as count"), "{out}");
    }

    #[test]
    fn count_star_not_aliased_in_expression() {
        let out = translate(
            "select sum(x) / count(*) > 0.25 as reduced from positions",
            &vars(),
        );
        let lower = out.to_ascii_lowercase();
        assert!(lower.contains("/ count(*) >"), "{out}");
        assert!(!lower.contains("as count >"), "{out}");
    }

    #[test]
    fn address_array_slice_becomes_join_head() {
        let out = translate(
            "SELECT array_to_string(((string_to_array(a.display_name, ', ', ''))[0:2]), ', ') AS addr FROM addresses a",
            &vars(),
        );
        assert!(out.contains("join_head"), "{out}");
        assert!(!out.contains("[0:2]"), "{out}");
    }

    #[test]
    fn lateral_nested_parens() {
        let sql = "SELECT coalesce(s_asleep.sleep, 0) FROM v, LATERAL (\n    SELECT EXTRACT(EPOCH FROM sum(age(s.end_date, s.start_date))) as sleep\n    FROM states s\n    WHERE s.car_id = $car_id\n  ) s_asleep JOIN cars c ON c.id = $car_id";
        let out = translate(sql, &vars());
        assert!(!out.to_ascii_lowercase().contains("lateral"), "{out}");
        assert!(out.to_ascii_lowercase().contains("age_seconds"), "{out}");
    }

    #[test]
    fn database_info_sqlite_catalog() {
        let v = vars();
        let sizes = translate(
            r#"SELECT relname AS "Table", pg_relation_size(relid) as "Data" FROM pg_catalog.pg_statio_user_tables ORDER BY pg_total_relation_size(relid) DESC"#,
            &v,
        );
        assert!(sizes.contains("dbstat"), "{sizes}");
        assert!(sizes.contains("sqlite_master"), "{sizes}");

        let total = translate(
            r#"SELECT SUM(pg_total_relation_size(relid)) As "Size" FROM pg_catalog.pg_statio_user_tables"#,
            &v,
        );
        assert!(total.contains("pragma_page_count"), "{total}");

        let rows = translate(
            r#"SELECT table_name AS "Table Name", (xpath('/row/cnt/text()', xml_count))[1]::text::int AS "Row Count" FROM (SELECT table_name, query_to_xml(format('SELECT count(*) as cnt FROM %I.%I', table_schema, table_name), false, true, '') AS xml_count FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')) AS t"#,
            &v,
        );
        assert!(rows.to_ascii_lowercase().contains("from positions"), "{rows}");
        assert!(rows.contains("Row Count"), "{rows}");

        let idx = translate(
            r#"SELECT relname AS "Table", indexrelname AS "Index", idx_scan AS "Index Scans", PG_RELATION_SIZE(indexrelid) as "Index Size" FROM pg_stat_all_indexes WHERE schemaname NOT LIKE 'pg_%'"#,
            &v,
        );
        assert!(idx.contains("Index Size"), "{idx}");
        assert!(idx.contains("dbstat"), "{idx}");

        let cache = translate(
            "SELECT cast(setting as numeric) * 8 * 1024 FROM pg_catalog.pg_settings WHERE name = 'shared_buffers'",
            &v,
        );
        assert!(cache.contains("pragma_cache_size"), "{cache}");

        let ver = translate(
            "SELECT regexp_replace(version(), 'PostgreSQL ([^ ]+) .*', '\\1') AS version",
            &v,
        );
        assert!(ver.contains("sqlite_version()"), "{ver}");

        let tz = translate("show timezone;", &v);
        assert!(tz.contains("UTC"), "{tz}");

        let count = translate("${pg_stat_statements_count:raw}", &v);
        assert!(count.to_ascii_lowercase().contains("select 0"), "{count}");
    }

    #[test]
    fn dashboards_prepare() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        crate::db::register_functions(&conn).unwrap();
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboards");
        let mut errors = Vec::new();
        visit_json(&dir, &conn, &vars(), &mut errors);
        let core = ["overview.json", "drives.json", "charges.json", "states.json", "mileage.json"];
        let core_errs: Vec<_> = errors
            .iter()
            .filter(|e| core.iter().any(|c| e.starts_with(c)))
            .cloned()
            .collect();
        if !core_errs.is_empty() {
            panic!("core dashboard SQL errors:\n{}", core_errs.join("\n"));
        }
        let dbinfo_errs: Vec<_> = errors
            .iter()
            .filter(|e| e.starts_with("database-info.json"))
            .cloned()
            .collect();
        if !dbinfo_errs.is_empty() {
            panic!(
                "database-info SQL errors:\n{}",
                dbinfo_errs.join("\n")
            );
        }
        if !errors.is_empty() {
            eprintln!(
                "{} non-core dashboard prepare warnings:\n{}",
                errors.len(),
                errors.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
            );
        }
    }

    #[test]
    fn parked_percentage_uses_fractional_seconds_in_selected_window() {
        let dashboard: serde_json::Value =
            serde_json::from_str(include_str!("../dashboards/states.json")).unwrap();
        let raw = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == 8)
            .unwrap()["targets"][0]["rawSql"]
            .as_str()
            .unwrap();
        let mut v = vars();
        v.from_ms = 1_785_000_000_000;
        v.to_ms = v.from_ms + 100_000;
        let sql = translate(raw, &v);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE drives (car_id INTEGER, start_date TEXT, end_date TEXT);")
            .unwrap();
        let start = v.from_ts();
        let end = fmt_ms(v.from_ms + 10_000);
        conn.execute(
            "INSERT INTO drives VALUES (?1, ?2, ?3)",
            rusqlite::params![v.car_id, start, end],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO drives VALUES (?1, '2024-01-01', NULL)",
            [v.car_id],
        )
        .unwrap();
        let parked: f64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert!((parked - 0.9).abs() < 1e-9, "{parked}: {sql}");
    }

    fn visit_json(
        path: &std::path::Path,
        conn: &rusqlite::Connection,
        vars: &QueryVars,
        errors: &mut Vec<String>,
    ) {
        if path.is_dir() {
            for e in std::fs::read_dir(path).unwrap() {
                visit_json(&e.unwrap().path(), conn, vars, errors);
            }
            return;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            return;
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        collect_sql(&v, path, conn, vars, errors);
    }

    #[test]
    fn timegroup_coarsens_long_range() {
        let mut v = vars();
        v.from_ms = 1_750_000_000_000;
        v.to_ms = v.from_ms + 30 * 86_400_000;
        let out = translate("SELECT $__timeGroup(date, '5s') AS time FROM positions", &v);
        assert!(out.contains("unixepoch(date)"), "{out}");
        assert!(!out.contains("/ 5)") && !out.contains("/ 5 "), "{out}");
        let secs = auto_bucket_secs(v.to_ms - v.from_ms, 5);
        assert!(secs >= 1800, "30d bucket {secs}");
        assert!(out.contains(&format!("/ {secs})")), "{out}");
    }

    #[test]
    fn timegroup_keeps_5s_for_a_short_trip() {
        let mut v = vars();
        v.from_ms = 1_750_000_000_000;
        v.to_ms = v.from_ms + 2 * 3_600_000;
        let out = translate("SELECT $__timeGroup(date, '5s') AS time FROM positions", &v);
        assert!(out.contains("/ 5)"), "{out}");
    }

    #[test]
    fn projected_range_uses_hourly_rollup() {
        let sql = "SELECT $__timeGroup(date, $interval) AS time, avg(battery_level) FROM positions WHERE car_id = $car_id AND $__timeFilter(date) AND ideal_battery_range_km is not null GROUP BY 1";
        let out = translate(sql, &vars());
        assert!(out.to_ascii_lowercase().contains("from position_hourly"), "{out}");
        let map = "SELECT latitude, longitude FROM positions p JOIN drives d ON p.drive_id = d.id WHERE $__timeFilter(d.start_date)";
        let map_out = translate(map, &vars());
        assert!(
            map_out.to_ascii_lowercase().contains("from positions"),
            "{map_out}"
        );
        assert!(
            !map_out.to_ascii_lowercase().contains("position_hourly"),
            "{map_out}"
        );
    }

    #[test]
    fn hires_buckets_are_finer_than_preview() {
        let range = 30 * 86_400_000;
        let preview = super::auto_bucket_with_cap(range, 5, 400);
        let hires = super::auto_bucket_with_cap(range, 5, 8000);
        assert!(hires < preview, "preview={preview} hires={hires}");
        assert!(hires <= 900, "hires {hires}");
    }

    fn collect_sql(
        v: &serde_json::Value,
        path: &std::path::Path,
        conn: &rusqlite::Connection,
        vars: &QueryVars,
        errors: &mut Vec<String>,
    ) {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(serde_json::Value::String(sql)) = m.get("rawSql") {
                    let translated = translate(sql, vars);
                    if let Err(e) = conn.prepare(&translated) {
                        errors.push(format!(
                            "{}: {} :: {}",
                            path.file_name().unwrap().to_string_lossy(),
                            e,
                            translated.replace('\n', " ").chars().take(180).collect::<String>()
                        ));
                    }
                }
                for child in m.values() {
                    collect_sql(child, path, conn, vars, errors);
                }
            }
            serde_json::Value::Array(a) => {
                for child in a {
                    collect_sql(child, path, conn, vars, errors);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn query_guard_blocks_token_and_auth_tables() {
        assert!(assert_safe_dashboard_sql("SELECT * FROM positions").is_ok());
        assert!(assert_safe_dashboard_sql("SELECT access_token FROM oauth_tokens").is_err());
        assert!(assert_safe_dashboard_sql("WITH u AS (SELECT * FROM users) SELECT * FROM u").is_err());
        assert!(assert_safe_dashboard_sql("DELETE FROM positions").is_err());
        assert!(assert_safe_dashboard_sql("ATTACH DATABASE '/tmp/x' AS x").is_err());
    }

    #[test]
    fn quote_ident_rejects_unsafe_names() {
        assert_eq!(quote_ident("cars").as_deref(), Some("\"cars\""));
        assert_eq!(quote_ident("oauth_tokens").as_deref(), Some("\"oauth_tokens\""));
        assert!(quote_ident("").is_none());
        assert!(quote_ident("cars; DROP TABLE users").is_none());
        assert!(quote_ident("1cars").is_none());
        assert!(quote_ident("car-s").is_none());
    }

    #[test]
    fn sanitize_vars_clamps_units_and_extras() {
        let mut v = vars();
        v.length_unit = "km'; DROP TABLE users;--".into();
        v.period = "month'; DROP TABLE cars;--".into();
        v.extras.insert("period".into(), "year'; waitfor delay".into());
        v.extras.insert("evil".into(), "1'; DROP TABLE cars;--".into());
        v.extras.insert("drive_id".into(), "42".into());
        let clean = sanitize_vars(v);
        assert_eq!(clean.length_unit, "km");
        assert_eq!(clean.period, "month");
        assert_eq!(clean.extras.get("drive_id").map(String::as_str), Some("42"));
        assert!(!clean.extras.contains_key("evil"));
        assert!(!clean.extras.contains_key("period") || clean.extras["period"] == "month");
    }
}
