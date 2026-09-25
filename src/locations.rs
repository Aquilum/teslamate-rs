//! Geofences used to label places and calculate charging costs.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use std::f64::consts::PI;

#[derive(Debug, Deserialize)]
pub struct GeofenceInput {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius: i64,
    pub cost_per_unit: Option<f64>,
    pub session_fee: Option<f64>,
    pub billing_type: String,
}

#[derive(Clone)]
struct Fence {
    id: i64,
    latitude: f64,
    longitude: f64,
    radius: f64,
}

pub fn list(conn: &Connection) -> rusqlite::Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name, g.latitude, g.longitude, g.radius,
                g.cost_per_unit, g.session_fee, g.billing_type,
                (SELECT COUNT(*) FROM charging_processes cp WHERE cp.geofence_id=g.id),
                (SELECT COUNT(*) FROM drives d WHERE d.start_geofence_id=g.id OR d.end_geofence_id=g.id)
         FROM geofences g ORDER BY g.name COLLATE NOCASE, g.id",
    )?;
    let locations = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "latitude": r.get::<_, f64>(2)?,
                "longitude": r.get::<_, f64>(3)?,
                "radius": r.get::<_, i64>(4)?,
                "costPerUnit": r.get::<_, Option<f64>>(5)?,
                "sessionFee": r.get::<_, Option<f64>>(6)?,
                "billingType": r.get::<_, String>(7)?,
                "charges": r.get::<_, i64>(8)?,
                "driveEnds": r.get::<_, i64>(9)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let center = conn
        .query_row(
            "SELECT latitude, longitude FROM positions
             WHERE latitude IS NOT NULL AND longitude IS NOT NULL
               AND NOT (latitude=0 AND longitude=0)
             ORDER BY date DESC LIMIT 1",
            [],
            |r| Ok(json!([r.get::<_, f64>(0)?, r.get::<_, f64>(1)?])),
        )
        .unwrap_or_else(|_| json!([54.0, -2.0]));
    let unassigned_charges: i64 = conn.query_row(
        "SELECT COUNT(*) FROM charging_processes WHERE geofence_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(json!({"locations": locations, "center": center, "unassignedCharges": unassigned_charges}))
}

pub fn save(conn: &mut Connection, id: Option<i64>, input: GeofenceInput) -> Result<Value> {
    validate(&input)?;
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let tx = conn.transaction()?;
    let saved_id = if let Some(id) = id {
        let changed = tx.execute(
            "UPDATE geofences SET name=?1, latitude=?2, longitude=?3, radius=?4,
                 cost_per_unit=?5, session_fee=?6, billing_type=?7, updated_at=?8 WHERE id=?9",
            params![input.name.trim(), input.latitude, input.longitude, input.radius,
                input.cost_per_unit, input.session_fee, input.billing_type, now, id],
        )?;
        if changed == 0 {
            return Err(anyhow!("location not found"));
        }
        id
    } else {
        tx.execute(
            "INSERT INTO geofences (name, latitude, longitude, radius, inserted_at, updated_at,
                                    cost_per_unit, session_fee, billing_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)",
            params![input.name.trim(), input.latitude, input.longitude, input.radius, now,
                input.cost_per_unit, input.session_fee, input.billing_type],
        )?;
        tx.last_insert_rowid()
    };
    let value = tx.query_row(
        "SELECT id, name, latitude, longitude, radius, cost_per_unit, session_fee, billing_type
         FROM geofences WHERE id=?1",
        [saved_id],
        |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                "latitude": r.get::<_, f64>(2)?, "longitude": r.get::<_, f64>(3)?,
                "radius": r.get::<_, i64>(4)?, "costPerUnit": r.get::<_, Option<f64>>(5)?,
                "sessionFee": r.get::<_, Option<f64>>(6)?, "billingType": r.get::<_, String>(7)?,
            }))
        },
    )?;
    tx.commit()?;
    Ok(value)
}

pub fn delete(conn: &mut Connection, id: i64) -> Result<bool> {
    let tx = conn.transaction()?;
    let removed = tx.execute("DELETE FROM geofences WHERE id=?1", [id])? > 0;
    tx.commit()?;
    Ok(removed)
}

pub fn rematch(conn: &mut Connection) -> Result<Value> {
    let tx = conn.transaction()?;
    reassign(&tx)?;
    recalculate_costs(&tx)?;
    let matched: i64 = tx.query_row(
        "SELECT COUNT(*) FROM charging_processes WHERE geofence_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let unmatched: i64 = tx.query_row(
        "SELECT COUNT(*) FROM charging_processes WHERE geofence_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(json!({"matchedCharges": matched, "unmatchedCharges": unmatched}))
}

pub fn validate(input: &GeofenceInput) -> Result<()> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(anyhow!("name must contain 1 to 80 characters"));
    }
    if !input.latitude.is_finite() || !(-90.0..=90.0).contains(&input.latitude)
        || !input.longitude.is_finite() || !(-180.0..=180.0).contains(&input.longitude)
    {
        return Err(anyhow!("latitude or longitude is out of range"));
    }
    if !(1..=100_000).contains(&input.radius) {
        return Err(anyhow!("radius must be between 1 and 100000 metres"));
    }
    if input.billing_type != "per_kwh" && input.billing_type != "per_minute" {
        return Err(anyhow!("billing type must be per kWh or per minute"));
    }
    for amount in [input.cost_per_unit, input.session_fee].into_iter().flatten() {
        if !amount.is_finite() || !(0.0..=1_000_000.0).contains(&amount) {
            return Err(anyhow!("costs must be between 0 and 1000000"));
        }
    }
    Ok(())
}

fn reassign(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE charging_processes SET geofence_id=NULL;
         UPDATE drives SET start_geofence_id=NULL, end_geofence_id=NULL;",
    )?;
    let fences = {
        let mut stmt = conn.prepare("SELECT id, latitude, longitude, radius FROM geofences")?;
        let rows = stmt.query_map([], |r| {
            Ok(Fence { id: r.get(0)?, latitude: r.get(1)?, longitude: r.get(2)?, radius: r.get::<_, i64>(3)? as f64 })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let targets = {
        let mut stmt = conn.prepare(
            "SELECT 'charge', cp.id, p.latitude, p.longitude FROM charging_processes cp
             JOIN positions p ON p.id=cp.position_id
             UNION ALL
             SELECT 'drive_start', d.id, p.latitude, p.longitude FROM drives d
             JOIN positions p ON p.id=d.start_position_id
             UNION ALL
             SELECT 'drive_end', d.id, p.latitude, p.longitude FROM drives d
             JOIN positions p ON p.id=d.end_position_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut charge = conn.prepare("UPDATE charging_processes SET geofence_id=?1 WHERE id=?2")?;
    let mut start = conn.prepare("UPDATE drives SET start_geofence_id=?1 WHERE id=?2")?;
    let mut end = conn.prepare("UPDATE drives SET end_geofence_id=?1 WHERE id=?2")?;
    for (kind, id, lat, lon) in targets {
        let fence_id = fences.iter()
            .filter_map(|f| {
                let distance = distance_m(lat, lon, f.latitude, f.longitude);
                (distance <= f.radius).then_some((distance, f.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id);
        match kind.as_str() {
            "charge" => { charge.execute(params![fence_id, id])?; }
            "drive_start" => { start.execute(params![fence_id, id])?; }
            _ => { end.execute(params![fence_id, id])?; }
        }
    }
    Ok(())
}

fn recalculate_costs(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE charging_processes AS cp SET cost=(
           SELECT CASE
             WHEN g.cost_per_unit IS NULL AND g.session_fee IS NULL THEN NULL
             WHEN g.billing_type='per_kwh' THEN COALESCE(g.session_fee, 0) +
               COALESCE(g.cost_per_unit * MAX(COALESCE(cp.charge_energy_used, 0), COALESCE(cp.charge_energy_added, 0)), 0)
             WHEN g.billing_type='per_minute' THEN COALESCE(g.session_fee, 0) +
               COALESCE(g.cost_per_unit * cp.duration_min, 0)
             ELSE NULL END FROM geofences g WHERE g.id=cp.geofence_id
         ) WHERE NOT EXISTS (
           SELECT 1 FROM charging_invoices i WHERE i.charging_process_id=cp.id
         )",
        [],
    )?;
    Ok(())
}

fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let radians = |degrees: f64| degrees * PI / 180.0;
    let (lat1, lat2) = (radians(lat1), radians(lat2));
    let dlat = lat2 - lat1;
    let dlon = radians(lon2 - lon1);
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    6_371_000.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}
