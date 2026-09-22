//! One drive or one charge, for the story card on the grouped pages.
//!
//! Grafana's trip dashboard filters a time range, not a session id, and the
//! drive-details / charge-details dashboards are not in this repo. This is the
//! single session: places, the battery, and a short path or power curve.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::live::{self, address_label};

pub fn drive_detail(
    conn: &Connection,
    car_id: i64,
    drive_id: i64,
) -> rusqlite::Result<Option<Value>> {
    let (length, temp_unit, _pressure, preferred) = units(conn);
    let length_unit = if length == "mi" { "mi" } else { "km" };
    let show_temp = if temp_unit == "F" { "F" } else { "C" };

    let row = conn
        .query_row(
            "SELECT d.start_date, d.end_date, d.distance, d.duration_min,
                    d.start_rated_range_km, d.end_rated_range_km,
                    d.start_ideal_range_km, d.end_ideal_range_km,
                    c.efficiency,
                    sg.name, eg.name,
                    sa.name, sa.road, sa.city,
                    ea.name, ea.road, ea.city,
                    sp.latitude, sp.longitude, sp.battery_level,
                    ep.latitude, ep.longitude, ep.battery_level
             FROM drives d
             LEFT JOIN cars c ON c.id = d.car_id
             LEFT JOIN geofences sg ON sg.id = d.start_geofence_id
             LEFT JOIN geofences eg ON eg.id = d.end_geofence_id
             LEFT JOIN addresses sa ON sa.id = d.start_address_id
             LEFT JOIN addresses ea ON ea.id = d.end_address_id
             LEFT JOIN positions sp ON sp.id = d.start_position_id
             LEFT JOIN positions ep ON ep.id = d.end_position_id
             WHERE d.id = ?1 AND d.car_id = ?2",
            params![drive_id, car_id],
            |r| {
                Ok(DriveRow {
                    start: r.get(0)?,
                    end: r.get(1)?,
                    distance_km: r.get(2)?,
                    duration_min: r.get(3)?,
                    start_rated: r.get(4)?,
                    end_rated: r.get(5)?,
                    start_ideal: r.get(6)?,
                    end_ideal: r.get(7)?,
                    efficiency: r.get(8)?,
                    start_geo: r.get(9)?,
                    end_geo: r.get(10)?,
                    start_name: r.get(11)?,
                    start_road: r.get(12)?,
                    start_city: r.get(13)?,
                    end_name: r.get(14)?,
                    end_road: r.get(15)?,
                    end_city: r.get(16)?,
                    start_lat: r.get(17)?,
                    start_lon: r.get(18)?,
                    start_soc: r.get(19)?,
                    end_lat: r.get(20)?,
                    end_lon: r.get(21)?,
                    end_soc: r.get(22)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };

    let from = place_or(
        conn,
        row.start_geo,
        row.start_name,
        row.start_road,
        row.start_city,
        row.start_lat,
        row.start_lon,
    )?;
    let to = place_or(
        conn,
        row.end_geo,
        row.end_name,
        row.end_road,
        row.end_city,
        row.end_lat,
        row.end_lon,
    )?;

    let (range_start, range_end) = if preferred == "ideal" {
        (
            row.start_ideal.or(row.start_rated),
            row.end_ideal.or(row.end_rated),
        )
    } else {
        (
            row.start_rated.or(row.start_ideal),
            row.end_rated.or(row.end_ideal),
        )
    };
    let consumption = wh_per_unit(
        range_start,
        range_end,
        row.efficiency,
        row.distance_km,
        length_unit,
    );

    let mut pts = track(conn, car_id, drive_id)?;
    if pts.is_empty() {
        if let (Some(lat), Some(lon)) = (row.start_lat, row.start_lon) {
            pts.push(Pt {
                lat,
                lon,
                soc: row.start_soc,
                elev: None,
            });
        }
        if let (Some(lat), Some(lon)) = (row.end_lat, row.end_lon) {
            pts.push(Pt {
                lat,
                lon,
                soc: row.end_soc,
                elev: None,
            });
        }
    }
    let pts = downsample(pts, 360);
    let path: Vec<Value> = pts
        .iter()
        .map(|p| json!([round5(p.lat), round5(p.lon)]))
        .collect();
    let soc: Vec<Value> = pts.iter().map(|p| json!(p.soc)).collect();
    let elevation: Vec<Value> = pts
        .iter()
        .map(|p| match p.elev {
            Some(m) => json!(live::elevation_in(m as f64, length_unit)),
            None => Value::Null,
        })
        .collect();

    Ok(Some(json!({
        "kind": "drive",
        "id": drive_id,
        "start": row.start,
        "end": row.end,
        "open": row.end.is_none(),
        "from": from,
        "to": to,
        "distance": row.distance_km.map(|km| live::km_to(km, length_unit)),
        "lengthUnit": length_unit,
        "durationMin": row.duration_min,
        "socStart": row.start_soc,
        "socEnd": row.end_soc,
        "consumption": consumption,
        "tempUnit": show_temp,
        "path": path,
        "soc": soc,
        "elevation": elevation,
    })))
}

pub fn charge_detail(
    conn: &Connection,
    car_id: i64,
    charge_id: i64,
) -> rusqlite::Result<Option<Value>> {
    let (length, temp_unit, _pressure, _preferred) = units(conn);
    let length_unit = if length == "mi" { "mi" } else { "km" };
    let show_temp = if temp_unit == "F" { "F" } else { "C" };

    let row = conn
        .query_row(
            "SELECT cp.start_date, cp.end_date, cp.charge_energy_added, cp.charge_energy_used,
                    cp.start_battery_level, cp.end_battery_level, cp.duration_min, cp.cost,
                    g.name, a.name, a.road, a.city, p.latitude, p.longitude
             FROM charging_processes cp
             LEFT JOIN geofences g ON g.id = cp.geofence_id
             LEFT JOIN addresses a ON a.id = cp.address_id
             LEFT JOIN positions p ON p.id = cp.position_id
             WHERE cp.id = ?1 AND cp.car_id = ?2",
            params![charge_id, car_id],
            |r| {
                Ok(ChargeRow {
                    start: r.get(0)?,
                    end: r.get(1)?,
                    added: r.get(2)?,
                    used: r.get(3)?,
                    soc_start: r.get(4)?,
                    soc_end: r.get(5)?,
                    duration_min: r.get(6)?,
                    cost: r.get(7)?,
                    geo: r.get(8)?,
                    name: r.get(9)?,
                    road: r.get(10)?,
                    city: r.get(11)?,
                    lat: r.get(12)?,
                    lon: r.get(13)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };

    let place = place_or(
        conn, row.geo, row.name, row.road, row.city, row.lat, row.lon,
    )?;
    let curve_pts = downsample(curve(conn, charge_id)?, 240);
    let power_max = curve_pts.iter().filter_map(|p| p.power).max();
    let curve_json: Vec<Value> = curve_pts
        .iter()
        .map(|p| json!({"soc": p.soc, "power": p.power}))
        .collect();
    let path = match (row.lat, row.lon) {
        (Some(lat), Some(lon)) => vec![json!([round5(lat), round5(lon)])],
        _ => Vec::new(),
    };

    Ok(Some(json!({
        "kind": "charge",
        "id": charge_id,
        "start": row.start,
        "end": row.end,
        "open": row.end.is_none(),
        "place": place,
        "energyAddedKwh": row.added.map(live::round1),
        "energyUsedKwh": row.used.map(live::round1),
        "durationMin": row.duration_min,
        "socStart": row.soc_start,
        "socEnd": row.soc_end,
        "cost": row.cost.map(round2),
        "powerMax": power_max,
        "lengthUnit": length_unit,
        "tempUnit": show_temp,
        "path": path,
        "curve": curve_json,
    })))
}

struct DriveRow {
    start: String,
    end: Option<String>,
    distance_km: Option<f64>,
    duration_min: Option<i64>,
    start_rated: Option<f64>,
    end_rated: Option<f64>,
    start_ideal: Option<f64>,
    end_ideal: Option<f64>,
    efficiency: Option<f64>,
    start_geo: Option<String>,
    end_geo: Option<String>,
    start_name: Option<String>,
    start_road: Option<String>,
    start_city: Option<String>,
    end_name: Option<String>,
    end_road: Option<String>,
    end_city: Option<String>,
    start_lat: Option<f64>,
    start_lon: Option<f64>,
    start_soc: Option<i64>,
    end_lat: Option<f64>,
    end_lon: Option<f64>,
    end_soc: Option<i64>,
}

struct ChargeRow {
    start: String,
    end: Option<String>,
    added: Option<f64>,
    used: Option<f64>,
    soc_start: Option<i64>,
    soc_end: Option<i64>,
    duration_min: Option<i64>,
    cost: Option<f64>,
    geo: Option<String>,
    name: Option<String>,
    road: Option<String>,
    city: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}

struct Pt {
    lat: f64,
    lon: f64,
    soc: Option<i64>,
    elev: Option<i64>,
}

struct Tick {
    soc: Option<i64>,
    power: Option<i64>,
}

fn units(conn: &Connection) -> (String, String, String, String) {
    conn.query_row(
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
    .unwrap_or_else(|_| ("km".into(), "C".into(), "bar".into(), "rated".into()))
}

fn place_or(
    conn: &Connection,
    geo: Option<String>,
    name: Option<String>,
    road: Option<String>,
    city: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
) -> rusqlite::Result<Option<String>> {
    if let Some(geo) = geo.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Ok(Some(geo));
    }
    if let Some(label) = address_label(name, road, city) {
        return Ok(Some(label));
    }
    match (lat, lon) {
        (Some(lat), Some(lon)) => live::place_name(conn, lat, lon),
        _ => Ok(None),
    }
}

fn track(conn: &Connection, car_id: i64, drive_id: i64) -> rusqlite::Result<Vec<Pt>> {
    let mut stmt = conn.prepare(
        "SELECT latitude, longitude, battery_level, elevation
         FROM positions WHERE drive_id = ?1 AND car_id = ?2 ORDER BY date",
    )?;
    let rows = stmt.query_map(params![drive_id, car_id], |r| {
        Ok(Pt {
            lat: r.get(0)?,
            lon: r.get(1)?,
            soc: r.get(2)?,
            elev: r.get(3)?,
        })
    })?;
    rows.collect()
}

fn curve(conn: &Connection, charge_id: i64) -> rusqlite::Result<Vec<Tick>> {
    let mut stmt = conn.prepare(
        "SELECT battery_level, charger_power FROM charges
         WHERE charging_process_id = ?1 ORDER BY date",
    )?;
    let rows = stmt.query_map(params![charge_id], |r| {
        Ok(Tick {
            soc: r.get(0)?,
            power: r.get(1)?,
        })
    })?;
    rows.collect()
}

fn wh_per_unit(
    start_km: Option<f64>,
    end_km: Option<f64>,
    efficiency: Option<f64>,
    distance_km: Option<f64>,
    length: &str,
) -> Option<f64> {
    let delta = start_km? - end_km?;
    let eff = efficiency?;
    let dist_km = distance_km.filter(|d| *d > 0.05)?;
    if delta <= 0.0 || eff <= 0.0 {
        return None;
    }
    let dist = if length == "mi" {
        dist_km / 1.609344
    } else {
        dist_km
    };
    if dist <= 0.0 {
        return None;
    }
    Some(live::round1(delta * eff * 1000.0 / dist))
}

fn downsample<T>(pts: Vec<T>, max: usize) -> Vec<T> {
    if max < 2 || pts.len() <= max {
        return pts;
    }
    let last = pts.len() - 1;
    let mut slots: Vec<Option<T>> = pts.into_iter().map(Some).collect();
    let mut picked = Vec::with_capacity(max);
    let mut prev = usize::MAX;
    for i in 0..max {
        let idx = (i * last) / (max - 1);
        if idx == prev {
            continue;
        }
        prev = idx;
        if let Some(pt) = slots.get_mut(idx).and_then(|s| s.take()) {
            picked.push(pt);
        }
    }
    picked
}

fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

fn round5(n: f64) -> f64 {
    (n * 100_000.0).round() / 100_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO settings (id, inserted_at, updated_at, unit_of_length, unit_of_temperature, preferred_range, unit_of_pressure)
             VALUES (1, '2026-01-01', '2026-01-01', 'mi', 'C', 'rated', 'psi')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO car_settings (id) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO cars (id, eid, vid, inserted_at, updated_at, vin, name, model, efficiency, settings_id)
             VALUES (1, 1, 2, '2026-01-01', '2026-01-01', 'VIN', 'Red S', 'S', 0.15471, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO geofences (id, name, latitude, longitude, radius, inserted_at, updated_at)
             VALUES (1, 'Home', 51.5074, -0.1278, 80, '2026-01-01', '2026-01-01'),
                    (2, 'Work', 51.5155, -0.0922, 80, '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn drive_story_names_both_ends_and_the_battery() {
        let conn = mem();
        for (id, lat, lon, soc, elev) in [
            (1, 51.5074, -0.1278, 80, 20),
            (2, 51.5100, -0.1100, 74, 28),
            (3, 51.5155, -0.0922, 70, 15),
        ] {
            conn.execute(
                "INSERT INTO positions (id, date, latitude, longitude, battery_level, elevation, car_id, drive_id, odometer)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 7, 100)",
                params![id, format!("2026-09-19 08:0{id}:00"), lat, lon, soc, elev],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO drives (id, start_date, end_date, distance, duration_min, car_id,
                start_rated_range_km, end_rated_range_km, start_position_id, end_position_id,
                start_geofence_id, end_geofence_id)
             VALUES (7, '2026-09-19 08:00:00', '2026-09-19 08:28:00', 10.0, 28, 1, 100, 90, 1, 3, 1, 2)",
            [],
        )
        .unwrap();
        let view = drive_detail(&conn, 1, 7).unwrap().unwrap();
        assert_eq!(view["from"], json!("Home"));
        assert_eq!(view["to"], json!("Work"));
        assert_eq!(view["socStart"], json!(80));
        assert_eq!(view["socEnd"], json!(70));
        assert_eq!(view["durationMin"], json!(28));
        assert!((view["distance"].as_f64().unwrap() - 6.2).abs() < 0.05);
        let wh = view["consumption"].as_f64().unwrap();
        assert!((wh - 249.0).abs() < 1.5, "wh/mi {wh}");
        assert_eq!(view["path"].as_array().unwrap().len(), 3);
        assert_eq!(view["soc"][0], json!(80));
        assert!(view["elevation"][0].as_f64().unwrap() > 60.0);
        assert!(drive_detail(&conn, 1, 99).unwrap().is_none());
        assert!(drive_detail(&conn, 2, 7).unwrap().is_none());
    }

    #[test]
    fn charge_story_uses_the_geofence_and_the_curve() {
        let conn = mem();
        conn.execute(
            "INSERT INTO positions (id, date, latitude, longitude, car_id, battery_level)
             VALUES (1, '2026-09-18 21:10:00', 51.5074, -0.1278, 1, 45)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO charging_processes (id, start_date, end_date, charge_energy_added, charge_energy_used,
                start_battery_level, end_battery_level, duration_min, car_id, position_id, geofence_id, cost)
             VALUES (4, '2026-09-18 21:10:00', '2026-09-19 00:10:00', 33.25, 36.0, 45, 80, 180, 1, 1, 1, 9.296)",
            [],
        )
        .unwrap();
        for (id, soc, power) in [(1, 45, 7), (2, 60, 7), (3, 80, 5)] {
            conn.execute(
                "INSERT INTO charges (id, date, battery_level, charge_energy_added, charger_power, ideal_battery_range_km, charging_process_id)
                 VALUES (?1, ?2, ?3, 1, ?4, 100, 4)",
                params![id, format!("2026-09-18 2{id}:00:00"), soc, power],
            )
            .unwrap();
        }
        let view = charge_detail(&conn, 1, 4).unwrap().unwrap();
        assert_eq!(view["place"], json!("Home"));
        assert_eq!(view["socStart"], json!(45));
        assert_eq!(view["socEnd"], json!(80));
        assert!((view["energyAddedKwh"].as_f64().unwrap() - 33.3).abs() < 0.05);
        assert!((view["cost"].as_f64().unwrap() - 9.30).abs() < 0.001);
        assert_eq!(view["powerMax"], json!(7));
        assert_eq!(view["curve"].as_array().unwrap().len(), 3);
        assert_eq!(view["path"][0][0], json!(51.5074));
        assert!(charge_detail(&conn, 9, 4).unwrap().is_none());
    }
}
