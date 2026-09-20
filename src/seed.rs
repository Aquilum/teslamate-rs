use anyhow::Result;
use rusqlite::{params, Connection};

const VIN: &str = "5YJSA7E2XNF000001";

pub fn seed(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.execute_batch(
        "         DELETE FROM charging_invoices;
         DELETE FROM charges;
         DELETE FROM charging_processes;
         DELETE FROM positions;
         DELETE FROM drives;
         DELETE FROM states;
         DELETE FROM updates;
         DELETE FROM addresses;
         DELETE FROM geofences;
         DELETE FROM cars;
         DELETE FROM car_settings;
         DELETE FROM settings;
         DELETE FROM oauth_tokens;",
    )?;

    conn.execute(
        "INSERT INTO settings (id, inserted_at, updated_at, unit_of_length, unit_of_temperature,
           preferred_range, language, unit_of_pressure, theme_mode)
         VALUES (1, '2026-01-01 00:00:00', '2026-01-01 00:00:00', 'mi', 'C', 'rated', 'en', 'psi', 'system')",
        [],
    )?;
    conn.execute(
        "INSERT INTO car_settings (id, suspend_min, suspend_after_idle_min, use_streaming_api, enabled, lfp_battery)
         VALUES (1, 21, 15, 0, 1, 0)",
        [],
    )?;
    conn.execute(
        "INSERT INTO cars (id, eid, vid, model, efficiency, inserted_at, updated_at, vin, name, trim_badging, settings_id, display_priority, marketing_name)
         VALUES (1, 8001, 9001, 'S', 0.15471, '2026-01-01 00:00:00', '2026-09-01 00:00:00', ?1, 'Mock S', '100D', 1, 1, 'Model S')",
        params![VIN],
    )?;

    conn.execute(
        "INSERT INTO addresses (id, display_name, latitude, longitude, name, road, city, country, inserted_at, updated_at)
         VALUES (1, 'Home, Mock Lane', 51.5074, -0.1278, 'Home', 'Mock Lane', 'London', 'United Kingdom', '2026-01-01 00:00:00', '2026-01-01 00:00:00'),
                (2, 'Work, Example Street', 51.5155, -0.0922, 'Work', 'Example Street', 'London', 'United Kingdom', '2026-01-01 00:00:00', '2026-01-01 00:00:00'),
                (3, 'Supercharger, Mock Retail Park', 51.4700, -0.4543, 'Mock Supercharger', 'Airport Way', 'Hounslow', 'United Kingdom', '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
        [],
    )?;
    conn.execute(
        "INSERT INTO geofences (id, name, latitude, longitude, radius, inserted_at, updated_at, cost_per_unit, billing_type)
         VALUES (1, 'Home', 51.5074, -0.1278, 50, '2026-01-01 00:00:00', '2026-01-01 00:00:00', NULL, 'per_kwh'),
                (2, 'Work', 51.5155, -0.0922, 40, '2026-01-01 00:00:00', '2026-01-01 00:00:00', NULL, 'per_kwh'),
                (3, 'Mock Supercharger', 51.4700, -0.4543, 80, '2026-01-01 00:00:00', '2026-01-01 00:00:00', 0.40, 'per_kwh')",
        [],
    )?;

    let mut pos_id = 1i64;
    let mut drive_id = 1i64;
    let mut charge_id = 1i64;
    let mut tick_id = 1i64;
    let mut odo = 42000.0_f64;

    // 14 days of commute + a longer weekend drive and DC charge.
    for day in 0..14 {
        let date = format!("2026-09-{:02}", 6 + day); // 6–19 Sep 2026
        // morning commute Home -> Work
        seed_drive(
            conn,
            &mut pos_id,
            &mut drive_id,
            &mut odo,
            &date,
            "07:40:00",
            28,
            51.5074,
            -0.1278,
            51.5155,
            -0.0922,
            1,
            2,
            1,
            2,
            62,
            8,
        )?;
        // evening commute Work -> Home
        seed_drive(
            conn,
            &mut pos_id,
            &mut drive_id,
            &mut odo,
            &date,
            "18:15:00",
            32,
            51.5155,
            -0.0922,
            51.5074,
            -0.1278,
            2,
            1,
            2,
            1,
            48,
            -6,
        )?;
        if day % 3 == 0 {
            seed_charge(
                conn,
                &mut pos_id,
                &mut charge_id,
                &mut tick_id,
                &date,
                "21:10:00",
                false,
                45,
                80,
                7,
                230,
                1,
                1,
            )?;
        }
    }

    seed_drive(
        conn,
        &mut pos_id,
        &mut drive_id,
        &mut odo,
        "2026-09-13",
        "09:00:00",
        95,
        51.5074,
        -0.1278,
        51.4700,
        -0.4543,
        1,
        3,
        1,
        3,
        88,
        45,
    )?;
    seed_charge(
        conn,
        &mut pos_id,
        &mut charge_id,
        &mut tick_id,
        "2026-09-13",
        "10:40:00",
        true,
        32,
        80,
        120,
        400,
        3,
        3,
    )?;
    seed_drive(
        conn,
        &mut pos_id,
        &mut drive_id,
        &mut odo,
        "2026-09-13",
        "11:20:00",
        90,
        51.4700,
        -0.4543,
        51.5074,
        -0.1278,
        3,
        1,
        3,
        1,
        80,
        -20,
    )?;

    conn.execute(
        "INSERT INTO states (state, start_date, end_date, car_id) VALUES
           ('asleep', '2026-09-06 00:00:00', '2026-09-06 07:35:00', 1),
           ('online', '2026-09-06 07:35:00', '2026-09-06 08:20:00', 1),
           ('asleep', '2026-09-19 23:00:00', NULL, 1)",
        [],
    )?;
    conn.execute(
        "INSERT INTO updates (start_date, end_date, version, car_id) VALUES
           ('2026-08-01 03:00:00', '2026-08-01 03:40:00', '2026.20.8 1fea4c2d', 1),
           ('2026-09-10 03:10:00', '2026-09-10 03:55:00', '2026.32.4 abcdef12', 1)",
        [],
    )?;

    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn seed_drive(
    conn: &Connection,
    pos_id: &mut i64,
    drive_id: &mut i64,
    odo: &mut f64,
    date: &str,
    start_tod: &str,
    minutes: i64,
    lat0: f64,
    lon0: f64,
    lat1: f64,
    lon1: f64,
    start_addr: i64,
    end_addr: i64,
    start_geo: i64,
    end_geo: i64,
    start_soc: i64,
    soc_delta: i64,
) -> Result<()> {
    let start = format!("{date} {start_tod}");
    let end_hms = add_minutes(start_tod, minutes);
    let end = format!("{date} {end_hms}");
    let steps = (minutes.max(4) as usize).min(40);
    let dist = haversine_km(lat0, lon0, lat1, lon1).max(1.2);
    let start_km = *odo;
    let start_range = start_soc as f64 * 4.8;
    let end_soc = (start_soc + soc_delta).clamp(5, 100);
    let end_range = end_soc as f64 * 4.8;
    let first_pos = *pos_id;

    conn.execute(
        "INSERT INTO drives (id, start_date, end_date, outside_temp_avg, speed_max, power_max, power_min,
            start_ideal_range_km, end_ideal_range_km, start_km, end_km, distance, duration_min, car_id,
            inside_temp_avg, start_address_id, end_address_id, start_rated_range_km, end_rated_range_km,
            start_geofence_id, end_geofence_id, ascent, descent)
         VALUES (?1, ?2, ?3, 14.0, 110, 180, -40, ?4, ?5, ?6, ?7, ?8, ?9, 1, 20.0, ?10, ?11, ?4, ?5, ?12, ?13, 20, 8)",
        params![
            *drive_id,
            start,
            end,
            start_range,
            end_range,
            start_km,
            start_km + dist,
            dist,
            minutes,
            start_addr,
            end_addr,
            start_geo,
            end_geo
        ],
    )?;

    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let lat = lat0 + (lat1 - lat0) * t;
        let lon = lon0 + (lon1 - lon0) * t;
        let km = start_km + dist * t;
        let soc = start_soc as f64 + (end_soc - start_soc) as f64 * t;
        let range = start_range + (end_range - start_range) * t;
        let ts = format!("{date} {}", add_minutes(start_tod, (minutes * i as i64) / steps as i64));
        conn.execute(
            "INSERT INTO positions (id, date, latitude, longitude, speed, power, odometer,
                ideal_battery_range_km, battery_level, outside_temp, elevation, car_id, drive_id,
                inside_temp, rated_battery_range_km, usable_battery_level, est_battery_range_km)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 14.0, 20, 1, ?10, 20.0, ?8, ?9, ?8)",
            params![
                *pos_id,
                ts,
                lat,
                lon,
                40 + ((i % 7) as i64) * 8,
                if i < steps / 2 { 20 } else { -5 },
                km,
                range,
                soc.round() as i64,
                *drive_id
            ],
        )?;
        *pos_id += 1;
    }
    let last_pos = *pos_id - 1;
    conn.execute(
        "UPDATE drives SET start_position_id=?1, end_position_id=?2 WHERE id=?3",
        params![first_pos, last_pos, *drive_id],
    )?;
    *odo = start_km + dist;
    *drive_id += 1;
    Ok(())
}

fn seed_charge(
    conn: &Connection,
    pos_id: &mut i64,
    charge_id: &mut i64,
    tick_id: &mut i64,
    date: &str,
    start_tod: &str,
    dc: bool,
    start_soc: i64,
    end_soc: i64,
    power_kw: i64,
    volts: i64,
    addr: i64,
    geo: i64,
) -> Result<()> {
    let minutes = if dc { 28 } else { 180 };
    let start = format!("{date} {start_tod}");
    let end = format!("{date} {}", add_minutes(start_tod, minutes));
    let lat = if dc { 51.4700 } else { 51.5074 };
    let lon = if dc { -0.4543 } else { -0.1278 };
    let energy = (end_soc - start_soc) as f64 * 0.95;
    conn.execute(
        "INSERT INTO positions (id, date, latitude, longitude, speed, power, odometer,
            ideal_battery_range_km, battery_level, outside_temp, car_id, rated_battery_range_km, usable_battery_level)
         VALUES (?1, ?2, ?3, ?4, 0, 0, 43000, ?5, ?6, 12.0, 1, ?5, ?6)",
        params![*pos_id, start, lat, lon, start_soc as f64 * 4.8, start_soc],
    )?;
    let position_id = *pos_id;
    *pos_id += 1;
    conn.execute(
        "INSERT INTO charging_processes (id, start_date, end_date, charge_energy_added, start_ideal_range_km,
            end_ideal_range_km, start_battery_level, end_battery_level, duration_min, outside_temp_avg,
            car_id, position_id, address_id, start_rated_range_km, end_rated_range_km, geofence_id,
            charge_energy_used, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 12.0, 1, ?10, ?11, ?5, ?6, ?12, ?13, ?14)",
        params![
            *charge_id,
            start,
            end,
            energy,
            start_soc as f64 * 4.8,
            end_soc as f64 * 4.8,
            start_soc,
            end_soc,
            minutes,
            position_id,
            addr,
            geo,
            energy / 0.92,
            energy * if dc { 0.40 } else { 0.28 }
        ],
    )?;
    let ticks = if dc { 14 } else { 10 };
    for i in 0..=ticks {
        let t = i as f64 / ticks as f64;
        let soc = start_soc as f64 + (end_soc - start_soc) as f64 * t;
        let added = energy * t;
        let ts = format!(
            "{date} {}",
            add_minutes(start_tod, (minutes * i as i64) / ticks as i64)
        );
        conn.execute(
            "INSERT INTO charges (id, date, battery_level, charge_energy_added, charger_actual_current,
                charger_phases, charger_pilot_current, charger_power, charger_voltage, fast_charger_present,
                conn_charge_cable, fast_charger_brand, fast_charger_type, ideal_battery_range_km,
                outside_temp, charging_process_id, rated_battery_range_km, usable_battery_level)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 12.0, ?15, ?14, ?3)",
            params![
                *tick_id,
                ts,
                soc.round() as i64,
                added,
                if dc { 300 } else { 32 },
                if dc { 1 } else { 3 },
                if dc { 300 } else { 32 },
                power_kw,
                volts,
                dc as i64,
                if dc { "Tesla Supercharger" } else { "IEC" },
                if dc { Some("Tesla") } else { None::<&str> },
                if dc { "Tesla" } else { "AC" },
                soc * 4.8,
                *charge_id
            ],
        )?;
        *tick_id += 1;
    }
    *charge_id += 1;
    Ok(())
}

fn add_minutes(hms: &str, minutes: i64) -> String {
    let parts: Vec<i64> = hms.split(':').filter_map(|s| s.parse().ok()).collect();
    let mut total = parts.first().copied().unwrap_or(0) * 60 + parts.get(1).copied().unwrap_or(0) + minutes;
    total = total.rem_euclid(24 * 60);
    format!("{:02}:{:02}:00", total / 60, total % 60)
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub const MOCK_VIN: &str = VIN;
