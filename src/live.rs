//! Latest Owner API snapshot, shaped for the grouped vehicle page.
//!
//! Grafana never had TeslaMate's car summary. The logger keeps the last
//! `vehicle_data` payload so the UI can show locks, closures, tires, climate
//! keeper, software update, and the active route without a second copy of the
//! historical charts.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::tesla;

pub fn record_snapshot(
    conn: &Connection,
    car_id: i64,
    state: &str,
    detail: Option<&Value>,
) -> Result<()> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    if let Some(detail) = detail {
        conn.execute(
            "INSERT INTO vehicle_snapshots (car_id, state, fetched_at, detail_at, data_json)
             VALUES (?1, ?2, ?3, ?3, ?4)
             ON CONFLICT(car_id) DO UPDATE SET
               state = excluded.state,
               fetched_at = excluded.fetched_at,
               detail_at = excluded.detail_at,
               data_json = excluded.data_json",
            params![car_id, state, now, detail.to_string()],
        )?;
    } else {
        conn.execute(
            "INSERT INTO vehicle_snapshots (car_id, state, fetched_at, detail_at, data_json)
             VALUES (?1, ?2, ?3, NULL, NULL)
             ON CONFLICT(car_id) DO UPDATE SET
               state = excluded.state,
               fetched_at = excluded.fetched_at",
            params![car_id, state, now],
        )?;
    }
    Ok(())
}

pub fn live_view(conn: &Connection, car_id: i64) -> rusqlite::Result<Option<Value>> {
    let car = conn
        .query_row(
            "SELECT name, model, trim_badging, marketing_name, vin, exterior_color, wheel_type
             FROM cars WHERE id=?1",
            [car_id],
            |r| {
                Ok(json!({
                    "id": car_id,
                    "name": r.get::<_, Option<String>>(0)?,
                    "model": r.get::<_, Option<String>>(1)?,
                    "trim": r.get::<_, Option<String>>(2)?,
                    "marketingName": r.get::<_, Option<String>>(3)?,
                    "vin": r.get::<_, String>(4)?,
                    "color": r.get::<_, Option<String>>(5)?,
                    "wheels": r.get::<_, Option<String>>(6)?,
                }))
            },
        )
        .optional()?;
    let Some(car) = car else {
        return Ok(None);
    };

    let (length, temp_unit, pressure, preferred): (String, String, String, String) = conn
        .query_row(
            "SELECT unit_of_length, unit_of_temperature, unit_of_pressure, preferred_range
             FROM settings ORDER BY id LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap_or_else(|_| ("km".into(), "C".into(), "bar".into(), "rated".into()));

    let snap: Option<(Option<String>, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT state, fetched_at, detail_at, data_json FROM vehicle_snapshots WHERE car_id=?1",
            [car_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let state_row: Option<(String, String)> = conn
        .query_row(
            "SELECT state, start_date FROM states
             WHERE car_id=?1 AND end_date IS NULL ORDER BY start_date DESC LIMIT 1",
            [car_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let pos = latest_position(conn, car_id)?;
    let detail = snap
        .as_ref()
        .and_then(|(_, _, _, json)| json.as_ref())
        .and_then(|s| serde_json::from_str::<Value>(s).ok());

    let state = state_row
        .as_ref()
        .map(|(s, _)| s.clone())
        .or_else(|| snap.as_ref().and_then(|(s, _, _, _)| s.clone()))
        .unwrap_or_else(|| "unknown".into());
    let since = state_row.as_ref().map(|(_, t)| t.clone());

    let length_unit = if length == "mi" { "mi" } else { "km" };
    let speed_unit = if length_unit == "mi" { "mph" } else { "km/h" };
    let show_temp = if temp_unit == "F" { "F" } else { "C" };
    let show_pressure = if pressure == "psi" { "psi" } else { "bar" };

    let mut extras = Vec::new();

    let battery = battery_block(
        detail.as_ref(),
        pos.as_ref(),
        length_unit,
        &preferred,
        &mut extras,
    );
    let drive = drive_block(detail.as_ref(), pos.as_ref(), length_unit, speed_unit, &mut extras);
    let climate = climate_block(detail.as_ref(), pos.as_ref(), show_temp, &mut extras);
    let body = body_block(detail.as_ref(), &mut extras);
    let tires = tires_block(detail.as_ref(), pos.as_ref(), show_pressure);
    let software = software_block(detail.as_ref(), &mut extras);
    let odometer = odometer_block(detail.as_ref(), pos.as_ref(), length_unit);

    if let Some(d) = detail.as_ref() {
        if let Some(color) = tesla::str_field(d, &["vehicle_config", "exterior_color"]) {
            if car.get("color").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                extras.push(json!({"label": "Exterior", "value": color}));
            }
        }
        if let Some(wheels) = tesla::str_field(d, &["vehicle_config", "wheel_type"]) {
            extras.push(json!({"label": "Wheels", "value": wheels}));
        }
    }

    Ok(Some(json!({
        "car": car,
        "state": state,
        "since": since,
        "fetchedAt": snap.as_ref().map(|(_, t, _, _)| t.clone()),
        "detailAt": snap.as_ref().and_then(|(_, _, t, _)| t.clone()),
        "hasDetail": detail.is_some(),
        "lengthUnit": length_unit,
        "tempUnit": show_temp,
        "pressureUnit": show_pressure,
        "preferredRange": preferred,
        "battery": battery,
        "drive": drive,
        "climate": climate,
        "body": body,
        "tires": tires,
        "software": software,
        "odometer": odometer,
        "extras": extras,
    })))
}

struct Pos {
    battery_level: Option<i64>,
    usable: Option<i64>,
    rated_km: Option<f64>,
    ideal_km: Option<f64>,
    est_km: Option<f64>,
    outside: Option<f64>,
    inside: Option<f64>,
    lat: Option<f64>,
    lon: Option<f64>,
    speed_kmh: Option<i64>,
    power: Option<i64>,
    odometer_km: Option<f64>,
    tpms: [Option<f64>; 4],
}

fn latest_position(conn: &Connection, car_id: i64) -> rusqlite::Result<Option<Pos>> {
    conn.query_row(
        "SELECT battery_level, usable_battery_level, rated_battery_range_km, ideal_battery_range_km,
                est_battery_range_km, outside_temp, inside_temp, latitude, longitude, speed, power,
                odometer, tpms_pressure_fl, tpms_pressure_fr, tpms_pressure_rl, tpms_pressure_rr
         FROM positions WHERE car_id=?1 ORDER BY date DESC LIMIT 1",
        [car_id],
        |r| {
            Ok(Pos {
                battery_level: r.get(0)?,
                usable: r.get(1)?,
                rated_km: r.get(2)?,
                ideal_km: r.get(3)?,
                est_km: r.get(4)?,
                outside: r.get(5)?,
                inside: r.get(6)?,
                lat: r.get(7)?,
                lon: r.get(8)?,
                speed_kmh: r.get(9)?,
                power: r.get(10)?,
                odometer_km: r.get(11)?,
                tpms: [r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?],
            })
        },
    )
    .optional()
}

fn battery_block(
    detail: Option<&Value>,
    pos: Option<&Pos>,
    length: &str,
    preferred: &str,
    extras: &mut Vec<Value>,
) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let level = tesla::i64_field(d, &["charge_state", "battery_level"])
        .or_else(|| pos.and_then(|p| p.battery_level));
    let usable = tesla::i64_field(d, &["charge_state", "usable_battery_level"])
        .or_else(|| pos.and_then(|p| p.usable));
    let rated = range_miles(d, "battery_range").map(|mi| miles(mi, length)).or_else(|| {
        pos.and_then(|p| p.rated_km)
            .map(|km| km_to(km, length))
    });
    let ideal = range_miles(d, "ideal_battery_range")
        .map(|mi| miles(mi, length))
        .or_else(|| pos.and_then(|p| p.ideal_km).map(|km| km_to(km, length)));
    let est = range_miles(d, "est_battery_range")
        .map(|mi| miles(mi, length))
        .or_else(|| pos.and_then(|p| p.est_km).map(|km| km_to(km, length)));
    let preferred_range = match preferred {
        "ideal" => ideal.or(rated),
        _ => rated.or(ideal),
    };
    let charging = meaningful(tesla::str_field(d, &["charge_state", "charging_state"]));
    let cable = meaningful(tesla::str_field(d, &["charge_state", "conn_charge_cable"]));
    let fast = meaningful(tesla::str_field(d, &["charge_state", "fast_charger_brand"]))
        .or_else(|| meaningful(tesla::str_field(d, &["charge_state", "fast_charger_type"])));
    if let Some(c) = cable {
        extras.push(json!({"label": "Cable", "value": c}));
    }
    if let Some(f) = fast {
        extras.push(json!({"label": "Fast charger", "value": f}));
    }
    if tesla::bool_field(d, &["charge_state", "charge_port_door_open"]) == Some(true) {
        extras.push(json!({"label": "Charge port", "value": "open"}));
    }
    if tesla::bool_field(d, &["charge_state", "scheduled_charging_pending"]) == Some(true) {
        let when = tesla::str_field(d, &["charge_state", "scheduled_charging_start_time"])
            .unwrap_or("pending");
        extras.push(json!({"label": "Scheduled charging", "value": when}));
    }
    if let Some(rate) = tesla::f64_field(d, &["charge_state", "charge_rate"]) {
        if rate > 0.0 {
            extras.push(json!({"label": "Charge rate", "value": format!("{rate:.0} {length}/h")}));
        }
    }
    if let Some(phases) = tesla::i64_field(d, &["charge_state", "charger_phases"]) {
        if phases > 0 {
            extras.push(json!({"label": "Phases", "value": phases.to_string()}));
        }
    }
    json!({
        "level": level,
        "usable": usable,
        "range": preferred_range,
        "rated": rated,
        "ideal": ideal,
        "est": est,
        "limit": tesla::i64_field(d, &["charge_state", "charge_limit_soc"]),
        "chargingState": charging,
        "powerKw": tesla::i64_field(d, &["charge_state", "charger_power"]),
        "voltage": tesla::i64_field(d, &["charge_state", "charger_voltage"]),
        "current": tesla::i64_field(d, &["charge_state", "charger_actual_current"]),
        "energyAddedKwh": tesla::f64_field(d, &["charge_state", "charge_energy_added"]),
        "hoursToFull": tesla::f64_field(d, &["charge_state", "time_to_full_charge"]),
        "minutesToFull": tesla::i64_field(d, &["charge_state", "minutes_to_full_charge"]),
        "heaterOn": tesla::bool_field(d, &["charge_state", "battery_heater_on"]),
    })
}

fn drive_block(
    detail: Option<&Value>,
    pos: Option<&Pos>,
    length: &str,
    speed_unit: &str,
    extras: &mut Vec<Value>,
) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let speed_mph = tesla::f64_field(d, &["drive_state", "speed"]);
    let speed = speed_mph
        .map(|mph| miles(mph, if speed_unit == "mph" { "mi" } else { "km" }))
        .or_else(|| {
            pos.and_then(|p| p.speed_kmh)
                .map(|kmh| km_to(kmh as f64, if speed_unit == "mph" { "mi" } else { "km" }))
        });
    let lat = tesla::f64_field(d, &["drive_state", "latitude"]).or_else(|| pos.and_then(|p| p.lat));
    let lon = tesla::f64_field(d, &["drive_state", "longitude"]).or_else(|| pos.and_then(|p| p.lon));
    let dest = meaningful(tesla::str_field(d, &["drive_state", "active_route", "destination"]));
    let miles_left = tesla::f64_field(d, &["drive_state", "active_route", "miles_to_arrival"]);
    let minutes = tesla::f64_field(d, &["drive_state", "active_route", "minutes_to_arrival"]);
    if let Some(energy) = tesla::f64_field(d, &["drive_state", "active_route", "energy_at_arrival"]) {
        extras.push(json!({"label": "Energy at arrival", "value": format!("{energy:.0}%")}));
    }
    json!({
        "shift": meaningful(tesla::str_field(d, &["drive_state", "shift_state"])),
        "speed": speed,
        "speedUnit": speed_unit,
        "powerKw": tesla::i64_field(d, &["drive_state", "power"]).or_else(|| pos.and_then(|p| p.power)),
        "heading": tesla::i64_field(d, &["drive_state", "heading"]),
        "lat": lat,
        "lon": lon,
        "destination": dest,
        "distanceToArrival": miles_left.map(|mi| miles(mi, length)),
        "minutesToArrival": minutes,
    })
}

fn climate_block(
    detail: Option<&Value>,
    pos: Option<&Pos>,
    unit: &str,
    extras: &mut Vec<Value>,
) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let inside = tesla::f64_field(d, &["climate_state", "inside_temp"])
        .or_else(|| pos.and_then(|p| p.inside))
        .map(|c| c_to(c, unit));
    let outside = tesla::f64_field(d, &["climate_state", "outside_temp"])
        .or_else(|| pos.and_then(|p| p.outside))
        .map(|c| c_to(c, unit));
    let set = tesla::f64_field(d, &["climate_state", "driver_temp_setting"]).map(|c| c_to(c, unit));
    if tesla::bool_field(d, &["climate_state", "is_preconditioning"]) == Some(true) {
        extras.push(json!({"label": "Preconditioning", "value": "on"}));
    }
    if let Some(keeper) = meaningful(tesla::str_field(d, &["climate_state", "climate_keeper_mode"])) {
        if !keeper.eq_ignore_ascii_case("off") {
            extras.push(json!({"label": "Climate keeper", "value": keeper}));
        }
    }
    if let Some(oh) = meaningful(tesla::str_field(d, &["climate_state", "cabin_overheat_protection"]))
    {
        if !oh.eq_ignore_ascii_case("off") {
            extras.push(json!({"label": "Cabin overheat", "value": oh}));
        }
    }
    let seats = [
        ("Driver seat", "seat_heater_left"),
        ("Passenger seat", "seat_heater_right"),
        ("Rear left seat", "seat_heater_rear_left"),
        ("Rear right seat", "seat_heater_rear_right"),
    ];
    for (label, key) in seats {
        if tesla::i64_field(d, &["climate_state", key]).unwrap_or(0) > 0 {
            let n = tesla::i64_field(d, &["climate_state", key]).unwrap_or(0);
            extras.push(json!({"label": label, "value": format!("level {n}")}));
        }
    }
    if tesla::bool_field(d, &["climate_state", "steering_wheel_heater"]) == Some(true) {
        extras.push(json!({"label": "Steering wheel", "value": "heat on"}));
    }
    json!({
        "inside": inside,
        "outside": outside,
        "setpoint": set,
        "on": tesla::bool_field(d, &["climate_state", "is_climate_on"]),
        "defrostFront": tesla::bool_field(d, &["climate_state", "is_front_defroster_on"]),
        "defrostRear": tesla::bool_field(d, &["climate_state", "is_rear_defroster_on"]),
    })
}

fn body_block(detail: Option<&Value>, extras: &mut Vec<Value>) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let door = |key: &str, label: &str| -> Option<String> {
        if open_flag(d, &["vehicle_state", key]) {
            Some(label.into())
        } else {
            None
        }
    };
    let doors: Vec<String> = [
        door("df", "Driver front"),
        door("pf", "Passenger front"),
        door("dr", "Driver rear"),
        door("pr", "Passenger rear"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let windows: Vec<String> = [
        ("fd_window", "Driver front"),
        ("fp_window", "Passenger front"),
        ("rd_window", "Driver rear"),
        ("rp_window", "Passenger rear"),
    ]
    .into_iter()
    .filter(|(k, _)| open_flag(d, &["vehicle_state", k]))
    .map(|(_, label)| label.to_string())
    .collect();
    if tesla::bool_field(d, &["vehicle_state", "is_user_present"]) == Some(true) {
        extras.push(json!({"label": "Driver", "value": "present"}));
    }
    if tesla::bool_field(d, &["vehicle_state", "valet_mode"]) == Some(true) {
        extras.push(json!({"label": "Valet", "value": "on"}));
    }
    if let Some(cam) = meaningful(tesla::str_field(d, &["vehicle_state", "dashcam_state"])) {
        extras.push(json!({"label": "Dashcam", "value": cam}));
    }
    if let Some(display) = tesla::i64_field(d, &["vehicle_state", "center_display_state"]) {
        let label = match display {
            0 => "off",
            1 | 2 => "dim",
            _ => "on",
        };
        extras.push(json!({"label": "Center display", "value": label}));
    }
    json!({
        "locked": tesla::bool_field(d, &["vehicle_state", "locked"]),
        "sentry": tesla::bool_field(d, &["vehicle_state", "sentry_mode"]),
        "doorsOpen": doors,
        "windowsOpen": windows,
        "frunkOpen": open_flag(d, &["vehicle_state", "ft"]),
        "trunkOpen": open_flag(d, &["vehicle_state", "rt"]),
    })
}

fn tires_block(detail: Option<&Value>, pos: Option<&Pos>, unit: &str) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let keys = [
        "tpms_pressure_fl",
        "tpms_pressure_fr",
        "tpms_pressure_rl",
        "tpms_pressure_rr",
    ];
    let warns = [
        "tpms_soft_warning_fl",
        "tpms_soft_warning_fr",
        "tpms_soft_warning_rl",
        "tpms_soft_warning_rr",
    ];
    let names = ["fl", "fr", "rl", "rr"];
    let mut out = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        let raw = tesla::f64_field(d, &["vehicle_state", keys[i]])
            .or_else(|| pos.and_then(|p| p.tpms[i]));
        let warning = tesla::bool_field(d, &["vehicle_state", warns[i]]) == Some(true);
        out.insert(
            (*name).into(),
            json!({
                "pressure": raw.map(|v| pressure_to(v, unit)),
                "warning": warning,
            }),
        );
    }
    Value::Object(out)
}

fn software_block(detail: Option<&Value>, extras: &mut Vec<Value>) -> Value {
    let d = detail.unwrap_or(&Value::Null);
    let version = meaningful(tesla::str_field(d, &["vehicle_state", "car_version"]));
    let status = meaningful(tesla::str_field(
        d,
        &["vehicle_state", "software_update", "status"],
    ));
    let update_version = meaningful(tesla::str_field(
        d,
        &["vehicle_state", "software_update", "version"],
    ));
    if let Some(s) = status {
        if !s.is_empty() {
            let text = match update_version {
                Some(v) => format!("{s} · {v}"),
                None => s.to_string(),
            };
            extras.push(json!({"label": "Software update", "value": text}));
        }
    }
    json!({
        "version": version,
        "updateStatus": status,
        "updateVersion": update_version,
    })
}

fn odometer_block(detail: Option<&Value>, pos: Option<&Pos>, length: &str) -> Option<f64> {
    if let Some(mi) = tesla::f64_field(detail.unwrap_or(&Value::Null), &["vehicle_state", "odometer"])
    {
        return Some(round1(miles(mi, length)));
    }
    pos.and_then(|p| p.odometer_km).map(|km| round1(km_to(km, length)))
}

fn range_miles(v: &Value, key: &str) -> Option<f64> {
    tesla::f64_field(v, &["charge_state", key])
}

fn meaningful(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| {
        !t.is_empty() && *t != "<invalid>" && !t.eq_ignore_ascii_case("null")
    })
}

fn open_flag(v: &Value, path: &[&str]) -> bool {
    if tesla::bool_field(v, path) == Some(true) {
        return true;
    }
    tesla::i64_field(v, path).unwrap_or(0) != 0
}

fn miles(mi: f64, length: &str) -> f64 {
    round1(if length == "mi" { mi } else { mi * 1.609344 })
}

fn km_to(km: f64, length: &str) -> f64 {
    round1(if length == "mi" { km / 1.609344 } else { km })
}

fn c_to(c: f64, unit: &str) -> f64 {
    round1(if unit == "F" { c * 9.0 / 5.0 + 32.0 } else { c })
}

/// Owner API tire pressures are PSI. TeslaMate's imported rows are bar (~2–3.5).
fn pressure_to(raw: f64, unit: &str) -> f64 {
    let bar = if raw > 8.0 { raw * 0.0689476 } else { raw };
    round1(if unit == "psi" { bar / 0.0689476 } else { bar })
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO settings (id, inserted_at, updated_at, unit_of_length, unit_of_temperature, preferred_range, unit_of_pressure)
             VALUES (1, '2026-01-01', '2026-01-01', 'mi', 'C', 'rated', 'psi')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO car_settings (id) VALUES (1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cars (id, eid, vid, inserted_at, updated_at, vin, name, model, trim_badging, settings_id)
             VALUES (1, 1, 2, '2026-01-01', '2026-01-01', 'VIN', 'Red S', 'S', 'Plaid', 1)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn asleep_poll_keeps_previous_detail() {
        let conn = mem();
        let detail = json!({
            "charge_state": {"battery_level": 61, "battery_range": 180.0, "charge_limit_soc": 80,
                             "charging_state": "Disconnected"},
            "vehicle_state": {"locked": true, "sentry_mode": true, "car_version": "2026.32.4",
                              "odometer": 1000.0, "tpms_pressure_fl": 42.0,
                              "software_update": {"status": "available", "version": "2026.44.1"}},
            "drive_state": {"latitude": 51.5, "longitude": -0.12, "shift_state": null,
                            "active_route": {"destination": "Home", "miles_to_arrival": 4.0, "minutes_to_arrival": 11}},
            "climate_state": {"inside_temp": 21.0, "outside_temp": 9.0, "is_climate_on": false,
                              "climate_keeper_mode": "dog"}
        });
        record_snapshot(&conn, 1, "online", Some(&detail)).unwrap();
        record_snapshot(&conn, 1, "asleep", None).unwrap();
        let view = live_view(&conn, 1).unwrap().unwrap();
        assert_eq!(view["state"], json!("asleep"));
        assert_eq!(view["hasDetail"], json!(true));
        assert_eq!(view["battery"]["level"], json!(61));
        assert_eq!(view["battery"]["limit"], json!(80));
        assert_eq!(view["body"]["locked"], json!(true));
        assert_eq!(view["body"]["sentry"], json!(true));
        assert_eq!(view["software"]["version"], json!("2026.32.4"));
        assert_eq!(view["drive"]["destination"], json!("Home"));
        assert!((view["tires"]["fl"]["pressure"].as_f64().unwrap() - 42.0).abs() < 0.05);
        let labels: Vec<&str> = view["extras"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["label"].as_str())
            .collect();
        assert!(labels.contains(&"Software update"));
        assert!(labels.contains(&"Climate keeper"));
    }

    #[test]
    fn position_fallback_when_no_poll() {
        let conn = mem();
        conn.execute(
            "INSERT INTO positions (date, latitude, longitude, speed, power, odometer, battery_level,
                rated_battery_range_km, ideal_battery_range_km, outside_temp, inside_temp, car_id,
                tpms_pressure_fl)
             VALUES ('2026-09-19 22:00:00', 51.5, -0.1, 0, 0, 160.9344, 55, 160.9344, 180.0, 10, 20, 1, 2.9)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO states (state, start_date, car_id) VALUES ('asleep', '2026-09-19 23:00:00', 1)",
            [],
        )
        .unwrap();
        let view = live_view(&conn, 1).unwrap().unwrap();
        assert_eq!(view["hasDetail"], json!(false));
        assert_eq!(view["state"], json!("asleep"));
        assert_eq!(view["since"], json!("2026-09-19 23:00:00"));
        assert_eq!(view["battery"]["level"], json!(55));
        assert!((view["odometer"].as_f64().unwrap() - 100.0).abs() < 0.05);
        assert!((view["battery"]["range"].as_f64().unwrap() - 100.0).abs() < 0.05);
        assert!((view["tires"]["fl"]["pressure"].as_f64().unwrap() - 42.1).abs() < 0.15);
    }
}
