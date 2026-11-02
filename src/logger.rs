use crate::db::Db;
use crate::sleep::{self, Action, Activity, Phase, ProbeOutcome, ProbePurpose, StreamReading};
use crate::tesla::{self, Tesla, Tokens};
use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const STREAM_COLUMNS: &str =
    "speed,odometer,soc,elevation,est_heading,est_lat,est_lng,power,shift_state,range,est_range,heading";
const STREAM_IDLE: Duration = Duration::from_secs(90);

/// True when `now_unix` is at least one stream bucket after the previous stored point.
fn stream_sample_due(last_unix: Option<i64>, now_unix: i64) -> bool {
    match last_unix {
        None => true,
        Some(last) => now_unix.saturating_sub(last) >= crate::db::STREAM_SAMPLE_SECS,
    }
}

/// Strip HTML/script-friendly punctuation from Tesla display names before storage.
pub fn sanitize_vehicle_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | '"' | '\'' | '`' | '\\'))
        .take(64)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Vehicle".into()
    } else {
        trimmed.to_string()
    }
}

pub fn poll_secs() -> u64 {
    std::env::var("TESLAMATE_RS_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

fn fast_poll() -> Option<u64> {
    let secs = poll_secs();
    if secs <= 5 {
        Some(secs.max(1))
    } else {
        None
    }
}

pub fn load_tokens(db: &Db) -> Result<Option<Tokens>> {
    let conn = db.lock();
    let row = conn
        .query_row(
            "SELECT access_token, refresh_token, expires_at FROM oauth_tokens WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    drop(conn);
    let Some((access_enc, refresh_enc, expires_at)) = row else {
        return Ok(None);
    };
    let access_token = crate::token_crypto::decrypt(&access_enc)?;
    let refresh_token = crate::token_crypto::decrypt(&refresh_enc)?;
    let tokens = Tokens {
        access_token,
        refresh_token,
        expires_at,
    };
    // Migrate legacy plaintext rows to ciphertext on first read.
    if !crate::token_crypto::is_encrypted(&access_enc)
        || !crate::token_crypto::is_encrypted(&refresh_enc)
    {
        store_tokens(db, &tokens)?;
    }
    Ok(Some(tokens))
}

pub fn store_tokens(db: &Db, t: &Tokens) -> Result<()> {
    let access = crate::token_crypto::encrypt(&t.access_token)?;
    let refresh = crate::token_crypto::encrypt(&t.refresh_token)?;
    let conn = db.lock();
    conn.execute(
        "INSERT INTO oauth_tokens (id, access_token, refresh_token, expires_at, updated_at)
         VALUES (1, ?1, ?2, ?3, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET
           access_token = excluded.access_token,
           refresh_token = excluded.refresh_token,
           expires_at = excluded.expires_at,
           updated_at = excluded.updated_at",
        params![access, refresh, t.expires_at],
    )?;
    Ok(())
}

pub async fn login(db: &Db, refresh_token: &str) -> Result<()> {
    let tesla = Tesla::from_refresh_token(refresh_token).await?;
    store_tokens(db, tesla.tokens())?;
    upsert_vehicles(db, tesla).await?;
    Ok(())
}

async fn upsert_vehicles(db: &Db, mut tesla: Tesla) -> Result<()> {
    let products = tesla.products().await?;
    for p in products {
        if tesla::str_field(&p, &["vin"]).is_none() {
            continue;
        }
        let vid = tesla::i64_field(&p, &["id"]).context("vehicle id")?;
        let eid = tesla::i64_field(&p, &["vehicle_id"]).unwrap_or(vid);
        let vin = tesla::str_field(&p, &["vin"]).unwrap_or("").to_string();
        let name = tesla::str_field(&p, &["display_name"]).map(|s| sanitize_vehicle_name(s));
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let conn = db.lock();
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM cars WHERE vin = ?1", [&vin], |r| r.get(0))
            .optional()?;
        if existing.is_some() {
            conn.execute(
                "UPDATE cars SET eid=?1, vid=?2, name=COALESCE(?3, name), updated_at=?4 WHERE vin=?5",
                params![eid, vid, name, now, vin],
            )?;
            continue;
        }
        conn.execute(
            "INSERT INTO car_settings (suspend_min, suspend_after_idle_min, use_streaming_api, enabled)
             VALUES (21, 15, 1, 1)",
            [],
        )?;
        let settings_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cars (eid, vid, vin, name, inserted_at, updated_at, settings_id, display_priority)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 1)",
            params![eid, vid, vin, name, now, settings_id],
        )?;
        info!("registered vehicle {vin}");
    }
    Ok(())
}

struct EnabledCar {
    id: i64,
    vid: i64,
    eid: i64,
    cfg: sleep::IdleConfig,
}

struct Tracked {
    phase: Phase,
    next_poll: Instant,
}

pub async fn run(db: Db) {
    if let Err(e) = repair_open_sessions(&db) {
        warn!("logger: open sessions: {e:#}");
    }
    let mut tracked: HashMap<i64, Tracked> = HashMap::new();
    loop {
        let delay = match load_tokens(&db) {
            Ok(Some(tokens)) => match poll_due(&db, tokens, &mut tracked).await {
                Ok(delay) => delay,
                Err(e) => {
                    warn!("logger: {e:#}");
                    Duration::from_secs(poll_secs().max(1))
                }
            },
            Ok(None) => {
                tracked.clear();
                tracing::debug!("logger idle (no oauth tokens; run teslamate-rs login)");
                Duration::from_secs(poll_secs().max(1))
            }
            Err(e) => {
                warn!("logger tokens: {e:#}");
                Duration::from_secs(poll_secs().max(1))
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn poll_due(
    db: &Db,
    tokens: Tokens,
    tracked: &mut HashMap<i64, Tracked>,
) -> Result<Duration> {
    let mut tesla = Tesla::new(tokens)?;
    let cars = load_cars(db)?;
    let ids: Vec<i64> = cars.iter().map(|c| c.id).collect();
    tracked.retain(|id, _| ids.contains(id));
    let now = Instant::now();
    for car in cars {
        let phase = {
            let entry = tracked.entry(car.id).or_insert(Tracked {
                phase: Phase::Startup,
                next_poll: now,
            });
            if entry.next_poll > Instant::now() {
                None
            } else {
                Some(entry.phase)
            }
        };
        let Some(phase) = phase else {
            continue;
        };
        match poll_car(db, &mut tesla, &car, phase).await {
            Ok((phase, wait)) => {
                if let Some(entry) = tracked.get_mut(&car.id) {
                    entry.phase = phase;
                    entry.next_poll = Instant::now() + wait;
                }
            }
            Err(e) => {
                warn!("car {}: {e:#}", car.id);
                if let Some(entry) = tracked.get_mut(&car.id) {
                    entry.next_poll = Instant::now() + Duration::from_secs(poll_secs().max(1));
                }
            }
        }
        let _ = store_tokens(db, tesla.tokens());
    }
    Ok(next_delay(tracked))
}

fn next_delay(tracked: &HashMap<i64, Tracked>) -> Duration {
    let now = Instant::now();
    let cap = Duration::from_secs(poll_secs().max(1));
    let wait = tracked
        .values()
        .map(|t| t.next_poll)
        .min()
        .map(|t| t.saturating_duration_since(now))
        .unwrap_or(cap);
    wait.min(cap).max(Duration::from_millis(200))
}

fn load_cars(db: &Db) -> Result<Vec<EnabledCar>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT c.id, c.vid, c.eid, s.suspend_min, s.suspend_after_idle_min,
                s.use_streaming_api, s.req_not_unlocked
         FROM cars c JOIN car_settings s ON s.id = c.settings_id
         WHERE s.enabled = 1",
    )?;
    let active = poll_secs();
    let fast = fast_poll().is_some();
    let cars = stmt
        .query_map([], |r| {
            let use_streaming = r.get::<_, i64>(5)? == 1;
            let req_not_unlocked = r.get::<_, i64>(6)? == 1;
            Ok(EnabledCar {
                id: r.get(0)?,
                vid: r.get(1)?,
                eid: r.get(2)?,
                cfg: sleep::config_for(
                    r.get(3)?,
                    r.get(4)?,
                    use_streaming,
                    req_not_unlocked,
                    active,
                    fast,
                ),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cars)
}

async fn poll_car(
    db: &Db,
    tesla: &mut Tesla,
    car: &EnabledCar,
    phase: Phase,
) -> Result<(Phase, Duration)> {
    let summary = tesla.vehicle(car.vid).await?;
    let state = tesla::str_field(&summary, &["state"]).unwrap_or("unknown");
    record_state(db, car.id, state)?;
    let now = Instant::now();
    match sleep::plan(&phase, state, now, &car.cfg) {
        Action::SummaryOnly { phase: next, wait } => {
            record_summary(db, car.id, state);
            if matches!(next, Phase::Asleep) && !matches!(phase, Phase::Asleep) {
                info!("car {}: {state}; not requesting vehicle_data", car.id);
            }
            Ok((next, wait))
        }
        Action::Probe { purpose } => {
            let reading = probe_stream(tesla, car.eid, purpose).await;
            match sleep::after_probe(purpose, &reading, &phase, Instant::now(), &car.cfg) {
                ProbeOutcome::VehicleData => fetch_vehicle_data(db, tesla, car, phase).await,
                ProbeOutcome::Wait { phase: next, wait } => {
                    record_summary(db, car.id, state);
                    if purpose == ProbePurpose::Wake && matches!(next, Phase::Checking { .. }) {
                        tracing::debug!(
                            "car {}: online looks like a subsystem check; not calling vehicle_data",
                            car.id
                        );
                    }
                    Ok((next, wait))
                }
            }
        }
        Action::VehicleData => fetch_vehicle_data(db, tesla, car, phase).await,
    }
}

fn record_summary(db: &Db, car_id: i64, state: &str) {
    let conn = db.lock();
    if let Err(e) = crate::live::record_snapshot(&conn, car_id, state, None) {
        warn!("car {car_id}: snapshot: {e:#}");
    }
}

async fn fetch_vehicle_data(
    db: &Db,
    tesla: &mut Tesla,
    car: &EnabledCar,
    phase: Phase,
) -> Result<(Phase, Duration)> {
    let data = tesla.vehicle_data(car.vid).await?;
    {
        let conn = db.lock();
        crate::live::record_snapshot(&conn, car.id, "online", Some(&data))?;
    }
    record_update_history(db, car.id, &data);
    let applied = sleep::apply_vehicle_data(&data, &phase, Instant::now(), &car.cfg);
    match applied.activity {
        Activity::Charging => persist_charge_tick(db, car.id, &data)?,
        Activity::Driving => persist_drive_tick(db, car.id, &data)?,
        Activity::Idle => {
            let pos_id = persist_snapshot(db, car.id, &data)?;
            finish_idle(db, car.id, pos_id)?;
        }
    }
    let mut phase_out = applied.phase;
    if applied.activity == Activity::Driving && car.cfg.use_streaming {
        if matches!(
            stream_drive(db, tesla, car.id, car.eid).await,
            StreamEnd::Idle
        ) {
            phase_out = Phase::Awake {
                idle_since: Instant::now()
                    .checked_sub(STREAM_IDLE)
                    .unwrap_or_else(Instant::now),
            };
        }
    }
    if matches!(phase, Phase::Asleep | Phase::Suspended { .. })
        && !matches!(phase_out, Phase::Suspended { .. })
    {
        info!("car {}: resuming vehicle_data", car.id);
    }
    if let Some(block) = applied.block {
        info!(
            "car {}: not suspending ({block}); vehicle_data keeps the car awake",
            car.id
        );
    }
    if matches!(phase_out, Phase::Suspended { .. }) && !matches!(phase, Phase::Suspended { .. }) {
        info!("car {}: suspending logging so the car can sleep", car.id);
    }
    Ok((phase_out, applied.wait))
}

fn record_update_history(db: &Db, car_id: i64, data: &Value) {
    let conn = db.lock();
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    match sync_updates(&conn, car_id, data, &now) {
        Ok(Some(version)) => info!("car {car_id}: software {version}"),
        Ok(None) => {}
        Err(e) => warn!("car {car_id}: update history: {e:#}"),
    }
}

/// TeslaMate imports keep the build hash ("2020.48.26 e0b5bf7f4a9d"); the
/// car reports it too. Only the release number identifies an update.
fn release(version: &str) -> Option<&str> {
    version.split_whitespace().next()
}

/// Keeps `updates` in step with `vehicle_state.car_version`. An install that
/// is seen opens a row, which the next reading closes with the new version;
/// a version change that happened while the logger was not looking is
/// recorded as an instant update, as TeslaMate does.
fn sync_updates(conn: &Connection, car_id: i64, data: &Value, now: &str) -> Result<Option<String>> {
    let status = tesla::str_field(data, &["vehicle_state", "software_update", "status"]).unwrap_or("");
    let open: Option<i64> = conn
        .query_row(
            "SELECT id FROM updates WHERE car_id=?1 AND end_date IS NULL
             ORDER BY start_date DESC, id DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    if status == "installing" {
        if open.is_none() {
            conn.execute(
                "INSERT INTO updates (start_date, car_id) VALUES (?1, ?2)",
                params![now, car_id],
            )?;
        }
        return Ok(None);
    }
    let Some(version) = tesla::str_field(data, &["vehicle_state", "car_version"]).and_then(release)
    else {
        return Ok(None);
    };
    if let Some(id) = open {
        conn.execute(
            "UPDATE updates SET end_date=?1, version=?2 WHERE id=?3",
            params![now, version, id],
        )?;
        return Ok(Some(version.to_string()));
    }
    let last: Option<String> = conn
        .query_row(
            "SELECT version FROM updates
             WHERE car_id=?1 AND version IS NOT NULL AND TRIM(version) <> ''
             ORDER BY start_date DESC, id DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    if last.as_deref().and_then(release) == Some(version) {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO updates (start_date, end_date, version, car_id) VALUES (?1, ?1, ?2, ?3)",
        params![now, version, car_id],
    )?;
    Ok(Some(version.to_string()))
}

fn persist_snapshot(db: &Db, car_id: i64, data: &Value) -> Result<i64> {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let lat = tesla::f64_field(data, &["drive_state", "latitude"]).unwrap_or(0.0);
    let lon = tesla::f64_field(data, &["drive_state", "longitude"]).unwrap_or(0.0);
    let conn = db.lock();
    conn.execute(
        "INSERT INTO positions (
            date, latitude, longitude, speed, power, odometer,
            ideal_battery_range_km, battery_level, outside_temp, elevation,
            driver_temp_setting, passenger_temp_setting, is_climate_on,
            is_rear_defroster_on, is_front_defroster_on, car_id, inside_temp,
            battery_heater, battery_heater_on, est_battery_range_km,
            rated_battery_range_km, usable_battery_level,
            tpms_pressure_fl, tpms_pressure_fr, tpms_pressure_rl, tpms_pressure_rr
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)",
        params![
            now,
            lat,
            lon,
            tesla::i64_field(data, &["drive_state", "speed"]),
            tesla::i64_field(data, &["drive_state", "power"]),
            tesla::f64_field(data, &["vehicle_state", "odometer"]).map(|m| m * 1.60934),
            tesla::f64_field(data, &["charge_state", "ideal_battery_range"]).map(|m| m * 1.60934),
            tesla::i64_field(data, &["charge_state", "battery_level"]),
            tesla::f64_field(data, &["climate_state", "outside_temp"]),
            tesla::i64_field(data, &["drive_state", "native_elevation"])
                .or_else(|| tesla::i64_field(data, &["drive_state", "elevation"])),
            tesla::f64_field(data, &["climate_state", "driver_temp_setting"]),
            tesla::f64_field(data, &["climate_state", "passenger_temp_setting"]),
            tesla::bool_field(data, &["climate_state", "is_climate_on"]).map(|b| b as i64),
            tesla::bool_field(data, &["climate_state", "is_rear_defroster_on"]).map(|b| b as i64),
            tesla::bool_field(data, &["climate_state", "is_front_defroster_on"]).map(|b| b as i64),
            car_id,
            tesla::f64_field(data, &["climate_state", "inside_temp"]),
            tesla::bool_field(data, &["charge_state", "battery_heater"]).map(|b| b as i64),
            tesla::bool_field(data, &["charge_state", "battery_heater_on"]).map(|b| b as i64),
            tesla::f64_field(data, &["charge_state", "est_battery_range"]).map(|m| m * 1.60934),
            tesla::f64_field(data, &["charge_state", "battery_range"]).map(|m| m * 1.60934),
            tesla::i64_field(data, &["charge_state", "usable_battery_level"]),
            tesla::f64_field(data, &["vehicle_state", "tpms_pressure_fl"]),
            tesla::f64_field(data, &["vehicle_state", "tpms_pressure_fr"]),
            tesla::f64_field(data, &["vehicle_state", "tpms_pressure_rl"]),
            tesla::f64_field(data, &["vehicle_state", "tpms_pressure_rr"]),
        ],
    )?;
    let _ = crate::db::refresh_position_hour(&conn, car_id, &now);
    Ok(conn.last_insert_rowid())
}

fn finish_idle(db: &Db, car_id: i64, position_id: i64) -> Result<()> {
    let conn = db.lock();
    attach_recent_position(&conn, car_id, position_id)?;
    close_open_drives(&conn, car_id)?;
    complete_open_charges(&conn, car_id)?;
    Ok(())
}

/// One online poll: GPS snapshot plus drive/charge tick if that is what the car is doing.
pub fn ingest_online(db: &Db, car_id: i64, data: &Value) -> Result<()> {
    record_update_history(db, car_id, data);
    match sleep::activity(data) {
        Activity::Charging => persist_charge_tick(db, car_id, data),
        Activity::Driving => persist_drive_tick(db, car_id, data),
        Activity::Idle => {
            let pos_id = persist_snapshot(db, car_id, data)?;
            finish_idle(db, car_id, pos_id)
        }
    }
}

fn persist_drive_tick(db: &Db, car_id: i64, data: &Value) -> Result<()> {
    {
        let conn = db.lock();
        complete_open_charges(&conn, car_id)?;
    }
    let pos_id = persist_snapshot(db, car_id, data)?;
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let conn = db.lock();
    let open: Option<i64> = conn
        .query_row(
            "SELECT id FROM drives WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    let drive_id = if let Some(id) = open {
        id
    } else {
        conn.execute(
            "INSERT INTO drives (start_date, car_id, start_position_id, start_km, start_ideal_range_km, start_rated_range_km)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now,
                car_id,
                pos_id,
                tesla::f64_field(data, &["vehicle_state", "odometer"]).map(|m| m * 1.60934),
                tesla::f64_field(data, &["charge_state", "ideal_battery_range"]).map(|m| m * 1.60934),
                tesla::f64_field(data, &["charge_state", "battery_range"]).map(|m| m * 1.60934),
            ],
        )?;
        conn.last_insert_rowid()
    };
    conn.execute(
        "UPDATE positions SET drive_id=?1 WHERE id=?2",
        params![drive_id, pos_id],
    )?;
    Ok(())
}

fn persist_charge_tick(db: &Db, car_id: i64, data: &Value) -> Result<()> {
    let pos_id = persist_snapshot(db, car_id, data)?;
    {
        let conn = db.lock();
        attach_recent_position(&conn, car_id, pos_id)?;
        close_open_drives(&conn, car_id)?;
    }
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let conn = db.lock();
    let open: Option<i64> = conn
        .query_row(
            "SELECT id FROM charging_processes WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    let process_id = if let Some(id) = open {
        id
    } else {
        conn.execute(
            "INSERT INTO charging_processes (
                start_date, car_id, position_id, start_battery_level,
                start_ideal_range_km, start_rated_range_km, charge_energy_added
             ) VALUES (?1,?2,?3,?4,?5,?6,0)",
            params![
                now,
                car_id,
                pos_id,
                tesla::i64_field(data, &["charge_state", "battery_level"]),
                tesla::f64_field(data, &["charge_state", "ideal_battery_range"])
                    .map(|m| m * 1.60934),
                tesla::f64_field(data, &["charge_state", "battery_range"]).map(|m| m * 1.60934),
            ],
        )?;
        conn.last_insert_rowid()
    };
    let energy = tesla::f64_field(data, &["charge_state", "charge_energy_added"]).unwrap_or(0.0);
    let ideal = tesla::f64_field(data, &["charge_state", "ideal_battery_range"])
        .map(|m| m * 1.60934)
        .unwrap_or(0.0);
    conn.execute(
        "INSERT INTO charges (
            date, battery_heater_on, battery_level, charge_energy_added,
            charger_actual_current, charger_phases, charger_pilot_current,
            charger_power, charger_voltage, fast_charger_present, conn_charge_cable,
            fast_charger_brand, fast_charger_type, ideal_battery_range_km,
            outside_temp, charging_process_id, battery_heater, rated_battery_range_km,
            usable_battery_level
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
        params![
            now,
            tesla::bool_field(data, &["charge_state", "battery_heater_on"]).map(|b| b as i64),
            tesla::i64_field(data, &["charge_state", "battery_level"]),
            energy,
            tesla::i64_field(data, &["charge_state", "charger_actual_current"]),
            tesla::i64_field(data, &["charge_state", "charger_phases"]),
            tesla::i64_field(data, &["charge_state", "charger_pilot_current"]),
            tesla::i64_field(data, &["charge_state", "charger_power"]).unwrap_or(0),
            tesla::i64_field(data, &["charge_state", "charger_voltage"]),
            tesla::bool_field(data, &["charge_state", "fast_charger_present"]).map(|b| b as i64),
            tesla::str_field(data, &["charge_state", "conn_charge_cable"]),
            tesla::str_field(data, &["charge_state", "fast_charger_brand"]),
            tesla::str_field(data, &["charge_state", "fast_charger_type"]),
            ideal,
            tesla::f64_field(data, &["charge_state", "outside_temp"]),
            process_id,
            tesla::bool_field(data, &["charge_state", "battery_heater"]).map(|b| b as i64),
            tesla::f64_field(data, &["charge_state", "battery_range"]).map(|m| m * 1.60934),
            tesla::i64_field(data, &["charge_state", "usable_battery_level"]),
        ],
    )?;
    conn.execute(
        "UPDATE charging_processes SET charge_energy_added=?1, end_battery_level=?2, end_ideal_range_km=?3 WHERE id=?4",
        params![
            energy,
            tesla::i64_field(data, &["charge_state", "battery_level"]),
            tesla::f64_field(data, &["charge_state", "ideal_battery_range"]).map(|m| m * 1.60934),
            process_id
        ],
    )?;
    Ok(())
}

fn repair_open_sessions(db: &Db) -> Result<()> {
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id FROM cars")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for car_id in ids {
        close_open_drives(&conn, car_id)?;
        complete_open_charges(&conn, car_id)?;
    }
    Ok(())
}

/// Parked points from the last 15 minutes belong on the open drive. Older
/// open drives are closed on the last point they already have, so a restart
/// does not stamp today's time on a trip from weeks ago.
fn attach_recent_position(conn: &Connection, car_id: i64, position_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE positions SET drive_id = (
            SELECT d.id FROM drives d
            WHERE d.car_id = ?1 AND d.end_date IS NULL
            ORDER BY d.start_date DESC LIMIT 1
         )
         WHERE id = ?2
           AND drive_id IS NULL
           AND EXISTS (
             SELECT 1 FROM drives d
             JOIN positions p ON p.drive_id = d.id
             WHERE d.car_id = ?1 AND d.end_date IS NULL
               AND p.date >= datetime('now', '-15 minutes')
           )",
        params![car_id, position_id],
    )?;
    Ok(())
}

fn close_open_drives(conn: &Connection, car_id: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id FROM drives WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date",
    )?;
    let ids = stmt
        .query_map([car_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for id in ids {
        close_drive(conn, car_id, id)?;
    }
    Ok(())
}

fn close_drive(conn: &Connection, car_id: i64, drive_id: i64) -> Result<()> {
    let row: Option<DriveStats> = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM positions WHERE drive_id=?1),
                (SELECT date FROM positions WHERE drive_id=?1 ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT date FROM positions WHERE drive_id=?1 ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT id FROM positions WHERE drive_id=?1 ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT id FROM positions WHERE drive_id=?1 ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT odometer FROM positions WHERE drive_id=?1 AND odometer IS NOT NULL ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT odometer FROM positions WHERE drive_id=?1 AND odometer IS NOT NULL ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT ideal_battery_range_km FROM positions WHERE drive_id=?1 AND ideal_battery_range_km IS NOT NULL ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT ideal_battery_range_km FROM positions WHERE drive_id=?1 AND ideal_battery_range_km IS NOT NULL ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT rated_battery_range_km FROM positions WHERE drive_id=?1 AND rated_battery_range_km IS NOT NULL ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT rated_battery_range_km FROM positions WHERE drive_id=?1 AND rated_battery_range_km IS NOT NULL ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT MAX(speed) FROM positions WHERE drive_id=?1),
                (SELECT MAX(power) FROM positions WHERE drive_id=?1),
                (SELECT MIN(power) FROM positions WHERE drive_id=?1),
                (SELECT AVG(outside_temp) FROM positions WHERE drive_id=?1),
                (SELECT AVG(inside_temp) FROM positions WHERE drive_id=?1),
                (SELECT latitude FROM positions WHERE drive_id=?1 ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT longitude FROM positions WHERE drive_id=?1 ORDER BY date ASC, id ASC LIMIT 1),
                (SELECT latitude FROM positions WHERE drive_id=?1 ORDER BY date DESC, id DESC LIMIT 1),
                (SELECT longitude FROM positions WHERE drive_id=?1 ORDER BY date DESC, id DESC LIMIT 1)",
            [drive_id],
            |r| {
                Ok(DriveStats {
                    count: r.get(0)?,
                    start_date: r.get(1)?,
                    end_date: r.get(2)?,
                    start_position_id: r.get(3)?,
                    end_position_id: r.get(4)?,
                    start_km: r.get(5)?,
                    end_km: r.get(6)?,
                    start_ideal: r.get(7)?,
                    end_ideal: r.get(8)?,
                    start_rated: r.get(9)?,
                    end_rated: r.get(10)?,
                    speed_max: r.get(11)?,
                    power_max: r.get(12)?,
                    power_min: r.get(13)?,
                    outside_temp_avg: r.get(14)?,
                    inside_temp_avg: r.get(15)?,
                    start_lat: r.get(16)?,
                    start_lon: r.get(17)?,
                    end_lat: r.get(18)?,
                    end_lon: r.get(19)?,
                })
            },
        )
        .optional()?;
    let Some(stats) = row else {
        return Ok(());
    };
    let distance = match (stats.start_km, stats.end_km) {
        (Some(start), Some(end)) => Some(end - start),
        _ => None,
    };
    if stats.count < 2
        || distance.unwrap_or(0.0) < 0.01
        || stats.start_date.is_none()
        || stats.end_date.is_none()
        || stats.start_position_id.is_none()
        || stats.end_position_id.is_none()
    {
        conn.execute(
            "UPDATE positions SET drive_id=NULL WHERE drive_id=?1",
            [drive_id],
        )?;
        conn.execute("DELETE FROM drives WHERE id=?1", [drive_id])?;
        info!("car {car_id}: dropped drive {drive_id} (shorter than 10 m)");
        return Ok(());
    }
    let duration_min: i64 = conn.query_row(
        "SELECT CAST(ROUND((julianday(?1) - julianday(?2)) * 1440.0) AS INTEGER)",
        params![stats.end_date, stats.start_date],
        |r| r.get(0),
    )?;
    let start_geofence = match (stats.start_lat, stats.start_lon) {
        (Some(lat), Some(lon)) => crate::locations::geofence_at(conn, lat, lon)?,
        _ => None,
    };
    let end_geofence = match (stats.end_lat, stats.end_lon) {
        (Some(lat), Some(lon)) => crate::locations::geofence_at(conn, lat, lon)?,
        _ => None,
    };
    conn.execute(
        "UPDATE drives SET
            start_date=?1, end_date=?2, start_position_id=?3, end_position_id=?4,
            start_km=?5, end_km=?6, distance=?7, duration_min=?8,
            start_ideal_range_km=?9, end_ideal_range_km=?10,
            start_rated_range_km=?11, end_rated_range_km=?12,
            speed_max=?13, power_max=?14, power_min=?15,
            outside_temp_avg=?16, inside_temp_avg=?17,
            start_geofence_id=?18, end_geofence_id=?19
         WHERE id=?20",
        params![
            stats.start_date,
            stats.end_date,
            stats.start_position_id,
            stats.end_position_id,
            stats.start_km,
            stats.end_km,
            distance,
            duration_min,
            stats.start_ideal,
            stats.end_ideal,
            stats.start_rated,
            stats.end_rated,
            stats.speed_max,
            stats.power_max,
            stats.power_min,
            stats.outside_temp_avg,
            stats.inside_temp_avg,
            start_geofence,
            end_geofence,
            drive_id,
        ],
    )?;
    info!(
        "car {car_id}: drive {drive_id} ended, {:.1} km in {duration_min} min",
        distance.unwrap_or(0.0)
    );
    Ok(())
}

struct DriveStats {
    count: i64,
    start_date: Option<String>,
    end_date: Option<String>,
    start_position_id: Option<i64>,
    end_position_id: Option<i64>,
    start_km: Option<f64>,
    end_km: Option<f64>,
    start_ideal: Option<f64>,
    end_ideal: Option<f64>,
    start_rated: Option<f64>,
    end_rated: Option<f64>,
    speed_max: Option<i64>,
    power_max: Option<i64>,
    power_min: Option<i64>,
    outside_temp_avg: Option<f64>,
    inside_temp_avg: Option<f64>,
    start_lat: Option<f64>,
    start_lon: Option<f64>,
    end_lat: Option<f64>,
    end_lon: Option<f64>,
}

fn complete_open_charges(conn: &Connection, car_id: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id FROM charging_processes WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date",
    )?;
    let ids = stmt
        .query_map([car_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for id in ids {
        complete_charge(conn, car_id, id)?;
    }
    Ok(())
}

fn complete_charge(conn: &Connection, car_id: i64, process_id: i64) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM charges WHERE charging_process_id=?1",
        [process_id],
        |r| r.get(0),
    )?;
    if count == 0 {
        conn.execute("DELETE FROM charging_processes WHERE id=?1", [process_id])?;
        return Ok(());
    }
    let (start_date, end_date, end_battery, end_ideal, end_rated, start_rated, energy): (
        String,
        String,
        Option<i64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        f64,
    ) = conn.query_row(
        "SELECT
            (SELECT date FROM charges WHERE charging_process_id=?1 ORDER BY date ASC, id ASC LIMIT 1),
            (SELECT date FROM charges WHERE charging_process_id=?1 ORDER BY date DESC, id DESC LIMIT 1),
            (SELECT battery_level FROM charges WHERE charging_process_id=?1 ORDER BY date DESC, id DESC LIMIT 1),
            (SELECT ideal_battery_range_km FROM charges WHERE charging_process_id=?1 AND ideal_battery_range_km IS NOT NULL ORDER BY date DESC, id DESC LIMIT 1),
            (SELECT rated_battery_range_km FROM charges WHERE charging_process_id=?1 AND rated_battery_range_km IS NOT NULL ORDER BY date DESC, id DESC LIMIT 1),
            (SELECT rated_battery_range_km FROM charges WHERE charging_process_id=?1 AND rated_battery_range_km IS NOT NULL ORDER BY date ASC, id ASC LIMIT 1),
            MAX(charge_energy_added)
         FROM charges WHERE charging_process_id=?1",
        [process_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    let duration_min: i64 = conn.query_row(
        "SELECT CAST(ROUND((julianday(?1) - julianday(?2)) * 1440.0) AS INTEGER)",
        params![end_date, start_date],
        |r| r.get(0),
    )?;
    let efficiency: Option<f64> = conn
        .query_row("SELECT efficiency FROM cars WHERE id=?1", [car_id], |r| {
            r.get(0)
        })
        .optional()?;
    let used = match (start_rated, end_rated, efficiency) {
        (Some(start), Some(end), Some(eff)) if end > start && eff > 0.0 => {
            Some((end - start) * eff)
        }
        _ => None,
    };
    conn.execute(
        "UPDATE charging_processes SET
            end_date=?1, duration_min=?2, end_battery_level=?3,
            end_ideal_range_km=?4, end_rated_range_km=?5,
            charge_energy_added=?6, charge_energy_used=COALESCE(?7, charge_energy_used)
         WHERE id=?8",
        params![
            end_date,
            duration_min,
            end_battery,
            end_ideal,
            end_rated,
            energy,
            used,
            process_id,
        ],
    )?;
    if let Some((lat, lon)) = conn
        .query_row(
            "SELECT p.latitude, p.longitude
             FROM charging_processes cp JOIN positions p ON p.id = cp.position_id
             WHERE cp.id=?1",
            [process_id],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)),
        )
        .optional()?
    {
        crate::locations::price_charge_geofence(conn, process_id, lat, lon)?;
    }
    info!("car {car_id}: charge {process_id} ended, {energy:.1} kWh in {duration_min} min");
    Ok(())
}

fn record_state(db: &Db, car_id: i64, state: &str) -> Result<()> {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let conn = db.lock();
    let current: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, state FROM states WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date DESC LIMIT 1",
            [car_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match current {
        Some((_, ref s)) if s == state => {}
        Some((id, _)) => {
            conn.execute(
                "UPDATE states SET end_date=?1 WHERE id=?2",
                params![now, id],
            )?;
            conn.execute(
                "INSERT INTO states (state, start_date, car_id) VALUES (?1,?2,?3)",
                params![state, now, car_id],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO states (state, start_date, car_id) VALUES (?1,?2,?3)",
                params![state, now, car_id],
            )?;
        }
    }
    Ok(())
}

enum StreamEnd {
    /// No stream message for `STREAM_IDLE`. The car has likely been parked that long.
    Idle,
    Stopped,
}

fn stream_subscribe(tesla: &Tesla, eid: i64) -> Value {
    json!({
        "msg_type": "data:subscribe_oauth",
        "token": tesla.tokens().access_token,
        "value": STREAM_COLUMNS,
        "tag": eid.to_string(),
    })
}

async fn probe_stream(tesla: &Tesla, eid: i64, purpose: ProbePurpose) -> StreamReading {
    let deadline = Instant::now() + sleep::probe_timeout(purpose, fast_poll());
    let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
    let (mut ws, _) = match tokio::time::timeout(
        remaining(deadline),
        tokio_tungstenite::connect_async(Tesla::stream_url()),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            tracing::debug!("stream connect: {e}");
            return StreamReading::Error;
        }
        Err(_) => return StreamReading::Timeout,
    };
    match tokio::time::timeout(
        remaining(deadline),
        ws.send(Message::Text(
            stream_subscribe(tesla, eid).to_string().into(),
        )),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::debug!("stream subscribe: {e}");
            return StreamReading::Error;
        }
        Err(_) => return StreamReading::Timeout,
    }
    // The first row is often a cached parked or nil-power packet. Keep
    // reading until the budget ends so a live drive behind it is visible.
    // A nil or parked row must still win over a timeout, or a subsystem
    // check that sends one empty packet would call vehicle_data.
    let mut quiet: Option<sleep::StreamSample> = None;
    loop {
        let left = remaining(deadline);
        if left.is_zero() {
            return quiet
                .map(StreamReading::Sample)
                .unwrap_or(StreamReading::Timeout);
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                match v.get("msg_type").and_then(|m| m.as_str()) {
                    Some("data:error") => {
                        tracing::debug!("stream error");
                        return quiet
                            .map(StreamReading::Sample)
                            .unwrap_or(StreamReading::Error);
                    }
                    Some("data:update") => {
                        if let Some(csv) = v.get("value").and_then(|x| x.as_str()) {
                            if let Some(sample) = sleep::parse_stream_csv(csv) {
                                let decisive = match purpose {
                                    ProbePurpose::Wake => sleep::wake_is_real(&sample),
                                    ProbePurpose::Suspend => sleep::suspend_is_active(&sample),
                                };
                                if decisive {
                                    return StreamReading::Sample(sample);
                                }
                                quiet = Some(sample);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                return quiet
                    .map(StreamReading::Sample)
                    .unwrap_or(StreamReading::Error);
            }
            Ok(Some(Err(e))) => {
                tracing::debug!("stream: {e}");
                return quiet
                    .map(StreamReading::Sample)
                    .unwrap_or(StreamReading::Error);
            }
            Err(_) => {
                return quiet
                    .map(StreamReading::Sample)
                    .unwrap_or(StreamReading::Timeout)
            }
            _ => {}
        }
    }
}

async fn stream_drive(db: &Db, tesla: &Tesla, car_id: i64, eid: i64) -> StreamEnd {
    let (mut ws, _) = match tokio_tungstenite::connect_async(Tesla::stream_url()).await {
        Ok(pair) => pair,
        Err(e) => {
            warn!("stream connect: {e}");
            return StreamEnd::Stopped;
        }
    };
    if let Err(e) = ws
        .send(Message::Text(
            stream_subscribe(tesla, eid).to_string().into(),
        ))
        .await
    {
        warn!("stream subscribe: {e}");
        return StreamEnd::Stopped;
    }
    let started = Instant::now();
    let mut saw_driving = false;
    loop {
        match tokio::time::timeout(STREAM_IDLE, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("msg_type").and_then(|m| m.as_str()) == Some("data:error") {
                    warn!("stream error");
                    return StreamEnd::Stopped;
                }
                if let Some(value) = v.get("value").and_then(|x| x.as_str()) {
                    if let Err(e) = ingest_stream_row(db, car_id, value) {
                        warn!("stream row: {e:#}");
                        return StreamEnd::Stopped;
                    }
                    if let Some(sample) = sleep::parse_stream_csv(value) {
                        if sleep::stream_is_driving(&sample) {
                            saw_driving = true;
                        } else if saw_driving {
                            finish_streamed_drive(db, car_id);
                            return StreamEnd::Idle;
                        } else if started.elapsed() >= Duration::from_secs(20) {
                            return StreamEnd::Stopped;
                        }
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return StreamEnd::Stopped,
            Ok(Some(Err(e))) => {
                warn!("stream: {e}");
                return StreamEnd::Stopped;
            }
            Err(_) => {
                if saw_driving {
                    finish_streamed_drive(db, car_id);
                }
                return StreamEnd::Idle;
            }
            _ => {}
        }
    }
}

fn finish_streamed_drive(db: &Db, car_id: i64) {
    let conn = db.lock();
    if let Err(e) = close_open_drives(&conn, car_id) {
        warn!("car {car_id}: close drive: {e:#}");
    }
}

fn ingest_stream_row(db: &Db, car_id: i64, csv: &str) -> Result<()> {
    // speed,odometer,soc,elevation,est_heading,est_lat,est_lng,power,shift_state,range,est_range,heading
    let cols: Vec<&str> = csv.split(',').collect();
    if cols.len() < 8 {
        return Ok(());
    }
    let speed: Option<i64> = cols[0].parse().ok();
    let odo_mi: Option<f64> = cols[1].parse().ok();
    let soc: Option<i64> = cols[2].parse().ok();
    let elev: Option<i64> = cols[3].parse().ok();
    let lat: f64 = cols[5].parse().unwrap_or(0.0);
    let lon: f64 = cols[6].parse().unwrap_or(0.0);
    let power: Option<i64> = cols[7].parse().ok();
    let now = Utc::now();
    let now_s = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let conn = db.lock();
    let drive_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM drives WHERE car_id=?1 AND end_date IS NULL ORDER BY id DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    let last_unix: Option<i64> = match drive_id {
        Some(id) => conn
            .query_row(
                "SELECT unixepoch(date) FROM positions WHERE drive_id=?1 ORDER BY date DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?,
        None => conn
            .query_row(
                "SELECT unixepoch(date) FROM positions
                 WHERE car_id=?1 AND drive_id IS NULL
                 ORDER BY date DESC LIMIT 1",
                [car_id],
                |r| r.get(0),
            )
            .optional()?,
    };
    if !stream_sample_due(last_unix, now.timestamp()) {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO positions (date, latitude, longitude, speed, power, odometer, battery_level, elevation, car_id, drive_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            now_s,
            lat,
            lon,
            speed,
            power,
            odo_mi.map(|m| m * 1.60934),
            soc,
            elev,
            car_id,
            drive_id
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn stream_keeps_a_point_every_five_seconds() {
        assert!(super::stream_sample_due(None, 1_000));
        assert!(!super::stream_sample_due(Some(1_000), 1_004));
        assert!(super::stream_sample_due(Some(1_000), 1_005));
    }

    #[test]
    fn records_software_updates_from_car_version() {
        let conn = memory();
        let reading = |status: &str, version: &str| {
            json!({"vehicle_state": {
                "car_version": version,
                "software_update": {"status": status, "version": " "}
            }})
        };
        let rows = |conn: &Connection| -> Vec<(String, Option<String>, Option<String>)> {
            let mut stmt = conn
                .prepare("SELECT start_date, end_date, version FROM updates ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        conn.execute(
            "INSERT INTO updates (start_date, end_date, version, car_id)
             VALUES ('2026-09-16 19:41:15', '2026-09-16 19:41:30', '2026.32.3 0a1b2c3d', 1)",
            [],
        )
        .unwrap();

        let same = sync_updates(&conn, 1, &reading("", "2026.32.3 9f8e7d6c"), "2026-11-01 08:00:00");
        assert_eq!(same.unwrap(), None);
        assert_eq!(rows(&conn).len(), 1);

        let missed = sync_updates(&conn, 1, &reading("", "2026.38.3.2 abcdef12"), "2026-11-02 09:18:33");
        assert_eq!(missed.unwrap().as_deref(), Some("2026.38.3.2"));
        assert_eq!(
            rows(&conn)[1],
            (
                "2026-11-02 09:18:33".into(),
                Some("2026-11-02 09:18:33".into()),
                Some("2026.38.3.2".into())
            )
        );
        assert_eq!(
            sync_updates(&conn, 1, &reading("", "2026.38.3.2 abcdef12"), "2026-11-02 10:00:00").unwrap(),
            None
        );

        sync_updates(&conn, 1, &reading("installing", "2026.38.3.2 abcdef12"), "2026-11-10 02:00:00").unwrap();
        sync_updates(&conn, 1, &reading("installing", "2026.38.3.2 abcdef12"), "2026-11-10 02:05:00").unwrap();
        assert_eq!(rows(&conn)[2], ("2026-11-10 02:00:00".into(), None, None));
        let done = sync_updates(&conn, 1, &reading("", "2026.44.1 1234abcd"), "2026-11-10 02:40:00");
        assert_eq!(done.unwrap().as_deref(), Some("2026.44.1"));
        let all = rows(&conn);
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[2],
            (
                "2026-11-10 02:00:00".into(),
                Some("2026-11-10 02:40:00".into()),
                Some("2026.44.1".into())
            )
        );
    }

    #[test]
    fn strips_html_metacharacters_from_vehicle_names() {
        assert_eq!(
            sanitize_vehicle_name(r#"</option><img src=x onerror=alert(1)>"#),
            "/optionimg src=x onerror=alert(1)"
        );
        assert_eq!(sanitize_vehicle_name("  Red S  "), "Red S");
        assert_eq!(sanitize_vehicle_name("<<<"), "Vehicle");
    }

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO car_settings (id, use_streaming_api) VALUES (1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cars (id, eid, vid, vin, inserted_at, updated_at, settings_id, efficiency)
             VALUES (1, 1, 1, '5YJ00000000000000', '2026-01-01 00:00:00', '2026-01-01 00:00:00', 1, 0.15)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO geofences (name, latitude, longitude, radius, inserted_at, updated_at, cost_per_unit, session_fee, billing_type)
             VALUES ('Home', 51.5, -0.12, 80, '2026-01-01 00:00:00', '2026-01-01 00:00:00', 0.30, 1.0, 'per_kwh')",
            [],
        )
        .unwrap();
        conn
    }

    fn position(
        conn: &Connection,
        date: &str,
        lat: f64,
        lon: f64,
        odometer: f64,
        drive_id: Option<i64>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO positions (
                date, latitude, longitude, odometer, speed, power, car_id, drive_id,
                ideal_battery_range_km, rated_battery_range_km, outside_temp
             ) VALUES (?1,?2,?3,?4,40,12,1,?5,200,180,11)",
            params![date, lat, lon, odometer, drive_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn closing_a_drive_fills_the_end_the_boards_filter_on() {
        let conn = memory();
        conn.execute(
            "INSERT INTO drives (start_date, car_id) VALUES ('2026-10-01 09:00:00', 1)",
            [],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();
        position(
            &conn,
            "2026-10-01 09:00:00",
            51.50,
            -0.12,
            1000.0,
            Some(drive_id),
        );
        position(
            &conn,
            "2026-10-01 09:30:00",
            51.55,
            -0.10,
            1025.0,
            Some(drive_id),
        );
        close_open_drives(&conn, 1).unwrap();
        let (end, distance, minutes, start_fence, end_fence): (String, f64, i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT end_date, distance, duration_min, start_geofence_id, end_geofence_id FROM drives WHERE id=?1",
                [drive_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(end, "2026-10-01 09:30:00");
        assert!((distance - 25.0).abs() < 0.01);
        assert_eq!(minutes, 30);
        assert_eq!(start_fence, 1);
        assert!(end_fence.is_none());
        let visible: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM drives WHERE car_id=1 AND end_date IS NOT NULL AND distance >= 0.01",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(visible, 1);
    }

    #[test]
    fn an_abandoned_drive_keeps_its_last_point_as_the_end() {
        let conn = memory();
        conn.execute(
            "INSERT INTO drives (start_date, car_id) VALUES ('2026-10-01 09:00:00', 1)",
            [],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();
        position(
            &conn,
            "2026-10-01 09:00:00",
            51.51,
            -0.11,
            1000.0,
            Some(drive_id),
        );
        position(
            &conn,
            "2026-10-01 09:20:00",
            51.60,
            -0.05,
            1012.0,
            Some(drive_id),
        );
        let parked = position(&conn, "2026-11-01 12:00:00", 51.70, 0.0, 2000.0, None);
        close_open_drives(&conn, 1).unwrap();
        let end: String = conn
            .query_row("SELECT end_date FROM drives WHERE id=?1", [drive_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(end, "2026-10-01 09:20:00");
        let linked: Option<i64> = conn
            .query_row(
                "SELECT drive_id FROM positions WHERE id=?1",
                [parked],
                |r| r.get(0),
            )
            .unwrap();
        assert!(linked.is_none());
    }

    #[test]
    fn a_recent_parked_point_is_the_end_of_the_drive() {
        let conn = memory();
        let start = (Utc::now() - chrono::Duration::minutes(20))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mid = (Utc::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        conn.execute(
            "INSERT INTO drives (start_date, car_id) VALUES (?1, 1)",
            [&start],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();
        position(&conn, &start, 51.51, -0.11, 1000.0, Some(drive_id));
        position(&conn, &mid, 51.53, -0.10, 1004.0, Some(drive_id));
        let parked_at = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let parked = position(&conn, &parked_at, 51.55, -0.08, 1008.0, None);
        attach_recent_position(&conn, 1, parked).unwrap();
        close_open_drives(&conn, 1).unwrap();
        let (end_id, distance): (i64, f64) = conn
            .query_row(
                "SELECT end_position_id, distance FROM drives WHERE id=?1",
                [drive_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(end_id, parked);
        assert!((distance - 8.0).abs() < 0.01);
    }

    #[test]
    fn a_drive_shorter_than_ten_metres_is_dropped() {
        let conn = memory();
        conn.execute(
            "INSERT INTO drives (start_date, car_id) VALUES ('2026-10-01 09:00:00', 1)",
            [],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();
        position(
            &conn,
            "2026-10-01 09:00:00",
            51.5,
            -0.12,
            1000.0,
            Some(drive_id),
        );
        close_open_drives(&conn, 1).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE drive_id IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0);
    }

    #[test]
    fn completing_a_charge_sets_the_end_and_the_geofence_price() {
        let conn = memory();
        let pos = position(&conn, "2026-10-02 22:00:00", 51.5, -0.12, 3000.0, None);
        conn.execute(
            "INSERT INTO charging_processes (start_date, car_id, position_id, charge_energy_added, start_rated_range_km)
             VALUES ('2026-10-02 22:00:00', 1, ?1, 0, 100)",
            [pos],
        )
        .unwrap();
        let process = conn.last_insert_rowid();
        for (date, energy, rated) in [
            ("2026-10-02 22:00:00", 4.0, 100.0),
            ("2026-10-02 22:40:00", 11.0, 140.0),
        ] {
            conn.execute(
                "INSERT INTO charges (
                    date, battery_level, charge_energy_added, charger_power,
                    ideal_battery_range_km, rated_battery_range_km, charging_process_id
                 ) VALUES (?1, 60, ?2, 7, ?3, ?3, ?4)",
                params![date, energy, rated, process],
            )
            .unwrap();
        }
        complete_open_charges(&conn, 1).unwrap();
        let (end, minutes, added, used, cost, fence): (String, i64, f64, f64, f64, i64) = conn
            .query_row(
                "SELECT end_date, duration_min, charge_energy_added, charge_energy_used, cost, geofence_id
                 FROM charging_processes WHERE id=?1",
                [process],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(end, "2026-10-02 22:40:00");
        assert_eq!(minutes, 40);
        assert!((added - 11.0).abs() < 0.01);
        assert!((used - 6.0).abs() < 0.01, "used {used}");
        assert!((cost - (1.0 + 0.30 * 11.0)).abs() < 0.01, "cost {cost}");
        assert_eq!(fence, 1);
    }
}
