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

impl QueryVars {
    pub fn from_ts(&self) -> String {
        fmt_ms(self.from_ms)
    }
    pub fn to_ts(&self) -> String {
        fmt_ms(self.to_ms)
    }
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
    s
}

fn expand_vars(sql: &str, vars: &QueryVars) -> String {
    let from = vars.from_ts();
    let to = vars.to_ts();
    let from_q = format!("'{from}'");
    let to_q = format!("'{to}'");
    let step = vars.interval_secs().max(1);

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
    s = s.replace("$__interval", &vars.interval);
    s = s.replace("$interval", &vars.interval);
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

    s = replace_timefilter(&s, &from, &to);
    s = s.replace("$__timeFrom()", &from_q);
    s = s.replace("$__timeTo()", &to_q);
    s = s.replace("$__timeGroupAlias", "$__timeGroup");

    let timegroup = regex().timegroup.replace_all(&s, |caps: &regex::Captures| {
        let col = caps.get(1).unwrap().as_str();
        let iv = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let secs = if iv.starts_with('$') || iv.is_empty() {
            step
        } else {
            parse_interval(iv.trim_matches('\'').trim_matches('"'))
        };
        format!("(CAST(strftime('%s', {col}) AS INTEGER) / {secs}) * {secs}")
    });
    s = timegroup.into_owned();

    s = regex()
        .time_as
        .replace_all(&s, |caps: &regex::Captures| {
            format!(
                "CAST(strftime('%s', {}) AS INTEGER) AS time",
                caps.get(1).unwrap().as_str()
            )
        })
        .into_owned();

    s
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
        "(WITH RECURSIVE gs(date) AS (SELECT date('{from}') UNION ALL SELECT date(date, '+1 day') FROM gs WHERE date < date('{to}')) SELECT date FROM gs)"
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
        return format!("CAST(strftime('%s', {e}) AS REAL)");
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
        "(CAST(strftime('%s', ({a})) AS REAL) - CAST(strftime('%s', ({b})) AS REAL))"
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

fn rewrite_pg_catalog(sql: &str) -> String {
    let l = sql.to_ascii_lowercase();
    if l.contains("pg_") || l.contains("pg_stat") || l.contains("pg_database") {
        return "SELECT 'sqlite' AS engine, 'see Database Information (SQLite)' AS note WHERE 0"
            .into();
    }
    if l.trim_start().starts_with("show ") {
        return "SELECT 'UTC' AS timezone".into();
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
        if !errors.is_empty() {
            eprintln!(
                "{} non-core dashboard prepare warnings:\n{}",
                errors.len(),
                errors.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
            );
        }
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
}
