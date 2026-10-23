use crate::db::Db;
use crate::sleep::{self, Action, Activity, Phase, ProbeOutcome, ProbePurpose, StreamReading};
use crate::tesla::{self, Tesla, Tokens};
use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const STREAM_COLUMNS: &str =
    "speed,odometer,soc,elevation,est_heading,est_lat,est_lng,power,shift_state,range,est_range,heading";
const STREAM_IDLE: Duration = Duration::from_secs(90);

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
                    if purpose == ProbePurpose::Wake && matches!(next, Phase::Asleep) {
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
    let applied = sleep::apply_vehicle_data(&data, &phase, Instant::now(), &car.cfg);
    match applied.activity {
        Activity::Charging => persist_charge_tick(db, car.id, &data)?,
        Activity::Driving => persist_drive_tick(db, car.id, &data)?,
        Activity::Idle => {
            persist_snapshot(db, car.id, &data)?;
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

/// One online poll: GPS snapshot plus drive/charge tick if that is what the car is doing.
pub fn ingest_online(db: &Db, car_id: i64, data: &Value) -> Result<()> {
    match sleep::activity(data) {
        Activity::Charging => persist_charge_tick(db, car_id, data),
        Activity::Driving => persist_drive_tick(db, car_id, data),
        Activity::Idle => {
            persist_snapshot(db, car_id, data)?;
            Ok(())
        }
    }
}

fn persist_drive_tick(db: &Db, car_id: i64, data: &Value) -> Result<()> {
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
    let timeout = sleep::probe_timeout(purpose, fast_poll());
    match tokio::time::timeout(timeout, read_stream_sample(tesla, eid)).await {
        Ok(reading) => reading,
        Err(_) => StreamReading::Timeout,
    }
}

async fn read_stream_sample(tesla: &Tesla, eid: i64) -> StreamReading {
    let (mut ws, _) = match tokio_tungstenite::connect_async(Tesla::stream_url()).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!("stream connect: {e}");
            return StreamReading::Error;
        }
    };
    if let Err(e) = ws
        .send(Message::Text(
            stream_subscribe(tesla, eid).to_string().into(),
        ))
        .await
    {
        tracing::debug!("stream subscribe: {e}");
        return StreamReading::Error;
    }
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                match v.get("msg_type").and_then(|m| m.as_str()) {
                    Some("data:error") => {
                        tracing::debug!("stream error");
                        return StreamReading::Error;
                    }
                    Some("data:update") => {
                        if let Some(csv) = v.get("value").and_then(|x| x.as_str()) {
                            if let Some(sample) = sleep::parse_stream_csv(csv) {
                                return StreamReading::Sample(sample);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some(Ok(Message::Close(_))) | None => return StreamReading::Error,
            Some(Err(e)) => {
                tracing::debug!("stream: {e}");
                return StreamReading::Error;
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
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return StreamEnd::Stopped,
            Ok(Some(Err(e))) => {
                warn!("stream: {e}");
                return StreamEnd::Stopped;
            }
            Err(_) => return StreamEnd::Idle,
            _ => {}
        }
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
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let conn = db.lock();
    let drive_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM drives WHERE car_id=?1 AND end_date IS NULL ORDER BY id DESC LIMIT 1",
            [car_id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO positions (date, latitude, longitude, speed, power, odometer, battery_level, elevation, car_id, drive_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            now,
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
    use super::sanitize_vehicle_name;

    #[test]
    fn strips_html_metacharacters_from_vehicle_names() {
        assert_eq!(
            sanitize_vehicle_name(r#"</option><img src=x onerror=alert(1)>"#),
            "/optionimg src=x onerror=alert(1)"
        );
        assert_eq!(sanitize_vehicle_name("  Red S  "), "Red S");
        assert_eq!(sanitize_vehicle_name("<<<"), "Vehicle");
    }
}
