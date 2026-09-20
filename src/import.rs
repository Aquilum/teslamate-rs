use anyhow::{bail, Context, Result};
use rusqlite::{params_from_iter, types::Value, Connection, Transaction};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const TABLES: &[&str] = &[
    "car_settings",
    "settings",
    "cars",
    "addresses",
    "geofences",
    "positions",
    "drives",
    "charging_processes",
    "charges",
    "states",
    "updates",
];

const BOOL_COLS: &[&str] = &[
    "req_not_unlocked",
    "free_supercharging",
    "use_streaming_api",
    "enabled",
    "lfp_battery",
    "is_climate_on",
    "is_rear_defroster_on",
    "is_front_defroster_on",
    "battery_heater",
    "battery_heater_on",
    "battery_heater_no_power",
    "fast_charger_present",
    "not_enough_power_to_heat",
];

pub struct ImportOpts {
    pub docker: String,
    pub user: String,
    pub dbname: String,
}

pub fn import_from_docker(conn: &mut Connection, opts: &ImportOpts) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "cache_size", "-262144")?; // 256 MB
    for table in TABLES {
        eprintln!("importing {table} …");
        let tx = conn.transaction()?;
        import_table(&tx, opts, table)?;
        tx.commit()?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch("ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

fn import_table(tx: &Transaction, opts: &ImportOpts, table: &str) -> Result<()> {
    let pg_cols = pg_columns(opts, table)?;
    if pg_cols.is_empty() {
        eprintln!("  skip {table} (not present in source)");
        return Ok(());
    }
    let sqlite_cols = sqlite_columns(tx, table)?;
    let cols: Vec<String> = pg_cols
        .into_iter()
        .filter(|c| sqlite_cols.iter().any(|s| s.eq_ignore_ascii_case(c)))
        .collect();
    if cols.is_empty() {
        eprintln!("  skip {table} (no overlapping columns)");
        return Ok(());
    }
    tx.execute(&format!("DELETE FROM {table}"), [])?;
    let placeholders = cols.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let col_list = cols.join(",");
    let insert_sql = format!("INSERT INTO {table} ({col_list}) VALUES ({placeholders})");
    let mut stmt = tx.prepare(&insert_sql)?;

    let copy_sql = format!(
        "COPY public.{table} ({col_list}) TO STDOUT WITH (FORMAT csv, NULL '\\N', ENCODING 'UTF8')"
    );
    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            &opts.docker,
            "psql",
            "-U",
            &opts.user,
            "-d",
            &opts.dbname,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &copy_sql,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("docker exec {} COPY {table}", opts.docker))?;
    let stdout = child.stdout.take().context("capture COPY stdout")?;
    let mut stderr = child.stderr.take().context("capture COPY stderr")?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(stdout);

    let mut n = 0u64;
    for rec in rdr.records() {
        let rec = rec?;
        let values: Vec<Value> = cols
            .iter()
            .enumerate()
            .map(|(i, col)| csv_value(col, rec.get(i).unwrap_or("")))
            .collect();
        stmt.execute(params_from_iter(values))?;
        n += 1;
        if n % 100_000 == 0 {
            eprint!("\r  {table}: {n} rows");
            let _ = std::io::stderr().flush();
        }
    }
    drop(rdr);
    let status = child.wait()?;
    let mut err = String::new();
    let _ = stderr.read_to_string(&mut err);
    if !status.success() {
        bail!("COPY {table} failed: {err}");
    }
    if n >= 100_000 {
        eprintln!();
    }
    eprintln!("  {table}: {n} rows");
    Ok(())
}

fn sqlite_columns(tx: &Transaction, table: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

fn pg_columns(opts: &ImportOpts, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT attname FROM pg_attribute \
         WHERE attrelid = 'public.{table}'::regclass \
           AND attnum > 0 AND NOT attisdropped \
         ORDER BY attnum"
    );
    let output = Command::new("docker")
        .args([
            "exec",
            &opts.docker,
            "psql",
            "-U",
            &opts.user,
            "-d",
            &opts.dbname,
            "-At",
            "-c",
            &sql,
        ])
        .output()
        .context("list postgres columns")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        if err.contains("does not exist") {
            return Ok(vec![]);
        }
        bail!("column list for {table}: {err}");
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

fn csv_value(col: &str, raw: &str) -> Value {
    if raw.is_empty() || raw == "\\N" {
        return Value::Null;
    }
    if BOOL_COLS.contains(&col) {
        return match raw {
            "t" | "true" | "1" | "yes" => Value::Integer(1),
            "f" | "false" | "0" | "no" => Value::Integer(0),
            _ => Value::Text(raw.to_string()),
        };
    }
    Value::Text(raw.to_string())
}

pub fn summarize(conn: &Connection) -> Result<String> {
    let mut out = String::new();
    for table in TABLES {
        let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        out.push_str(&format!("  {table}: {n}\n"));
    }
    if let Some(path) = conn.path() {
        let p = Path::new(path);
        if p.exists() {
            out.push_str(&format!(
                "  file: {} ({:.1} MB)\n",
                p.display(),
                p.metadata()?.len() as f64 / 1_048_576.0
            ));
        }
    }
    Ok(out)
}
