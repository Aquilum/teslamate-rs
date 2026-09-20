use crate::db::Db;
use crate::tesla::{self, Tesla, Tokens};
use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

pub fn poll_secs() -> u64 {
    std::env::var("TESLAMATE_RS_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

fn asleep_secs(suspend_min: i64) -> u64 {
    if poll_secs() <= 5 {
        poll_secs()
    } else {
        (suspend_min.max(1) as u64) * 60
    }
}

pub fn load_tokens(db: &Db) -> Result<Option<Tokens>> {
    let conn = db.lock();
    let row = conn
        .query_row(
            "SELECT access_token, refresh_token, expires_at FROM oauth_tokens WHERE id = 1",
            [],
            |r| {
                Ok(Tokens {
                    access_token: r.get(0)?,
                    refresh_token: r.get(1)?,
                    expires_at: r.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub fn store_tokens(db: &Db, t: &Tokens) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO oauth_tokens (id, access_token, refresh_token, expires_at, updated_at)
         VALUES (1, ?1, ?2, ?3, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET
           access_token = excluded.access_token,
           refresh_token = excluded.refresh_token,
           expires_at = excluded.expires_at,
           updated_at = excluded.updated_at",
        params![t.access_token, t.refresh_token, t.expires_at],
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
        let name = tesla::str_field(&p, &["display_name"]).map(|s| s.to_string());
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

pub async fn run(db: Db) {
    loop {
        match load_tokens(&db) {
            Ok(Some(tokens)) => {
                if let Err(e) = run_once(db.clone(), tokens).await {
                    warn!("logger: {e:#}");
                }
            }
            Ok(None) => {
                tracing::debug!("logger idle (no oauth tokens; run teslamate-rs login)");
            }
            Err(e) => warn!("logger tokens: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(poll_secs())).await;
    }
}

async fn run_once(db: Db, tokens: Tokens) -> Result<()> {
    let mut tesla = Tesla::new(tokens)?;
    let cars: Vec<(i64, i64, i64, i64, i64)> = {
        let conn = db.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.vid, c.eid, s.suspend_min, s.use_streaming_api
             FROM cars c JOIN car_settings s ON s.id = c.settings_id
             WHERE s.enabled = 1",
        )?;
        let cars = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        cars
    };
    for (car_id, vid, eid, suspend_min, use_stream) in cars {
        if let Err(e) = poll_car(&db, &mut tesla, car_id, vid, eid, suspend_min, use_stream == 1)
            .await
        {
            warn!("car {car_id}: {e:#}");
        }
        let _ = store_tokens(&db, tesla.tokens());
    }
    Ok(())
}

async fn poll_car(
    db: &Db,
    tesla: &mut Tesla,
    car_id: i64,
    vid: i64,
    eid: i64,
    suspend_min: i64,
    use_stream: bool,
) -> Result<()> {
    let summary = tesla.vehicle(vid).await?;
    let state = tesla::str_field(&summary, &["state"]).unwrap_or("unknown");
    record_state(db, car_id, state)?;
    match state {
        "asleep" | "offline" => {
            tokio::time::sleep(Duration::from_secs(asleep_secs(suspend_min))).await;
        }
        "online" => {
            let data = tesla.vehicle_data(vid).await?;
            persist_snapshot(db, car_id, &data)?;
            if is_charging(&data) {
                persist_charge_tick(db, car_id, &data)?;
            } else if is_driving(&data) {
                persist_drive_tick(db, car_id, &data)?;
                if use_stream {
                    let _ = stream_drive(db, tesla, car_id, eid).await;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_charging(data: &Value) -> bool {
    matches!(
        tesla::str_field(data, &["charge_state", "charging_state"]),
        Some("Charging" | "Starting" | "Complete")
    ) && tesla::str_field(data, &["charge_state", "charging_state"]) != Some("Complete")
}

fn is_driving(data: &Value) -> bool {
    match tesla::str_field(data, &["drive_state", "shift_state"]) {
        Some("D" | "R" | "N") => true,
        _ => tesla::f64_field(data, &["drive_state", "speed"]).unwrap_or(0.0) > 0.0,
    }
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
    if is_charging(data) {
        persist_charge_tick(db, car_id, data)
    } else if is_driving(data) {
        persist_drive_tick(db, car_id, data)
    } else {
        persist_snapshot(db, car_id, data)?;
        Ok(())
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
                tesla::f64_field(data, &["charge_state", "ideal_battery_range"]).map(|m| m * 1.60934),
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
            conn.execute("UPDATE states SET end_date=?1 WHERE id=?2", params![now, id])?;
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

async fn stream_drive(db: &Db, tesla: &Tesla, car_id: i64, eid: i64) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(Tesla::stream_url()).await?;
    let sub = json!({
        "msg_type": "data:subscribe_oauth",
        "token": tesla.tokens().access_token,
        "value": "speed,odometer,soc,elevation,est_heading,est_lat,est_lng,power,shift_state,range,est_range,heading",
        "tag": eid.to_string(),
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    let idle = Duration::from_secs(90);
    loop {
        match tokio::time::timeout(idle, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                if v.get("msg_type").and_then(|m| m.as_str()) == Some("data:error") {
                    warn!("stream error: {t}");
                    break;
                }
                if let Some(value) = v.get("value").and_then(|x| x.as_str()) {
                    ingest_stream_row(db, car_id, value)?;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
            Ok(Some(Err(e))) => {
                warn!("stream: {e}");
                break;
            }
            Err(_) => break,
            _ => {}
        }
    }
    Ok(())
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
