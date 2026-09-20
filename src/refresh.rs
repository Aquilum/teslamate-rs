use crate::db::Db;
use crate::invoices;
use crate::logger;
use crate::tesla::{self, Tesla};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::Value;

/// TeslaMate-style read sweep of the Owner API. Never sends `wake_up`.
/// `vehicle_data` and `nearby_charging_sites` run only when Tesla already reports online.
pub async fn run(db: &Db) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let Some(tokens) = logger::load_tokens(db)? else {
        lines.push("oauth: no tokens".into());
        return Ok(lines);
    };
    let mut tesla = Tesla::new(tokens)?;
    tesla.force_refresh().await?;
    logger::store_tokens(db, tesla.tokens())?;
    let left = tesla.tokens().expires_at - chrono::Utc::now().timestamp();
    lines.push(format!("oauth: refreshed expires_in={left}s"));

    match tesla.users_me().await {
        Ok(u) => lines.push(format!(
            "GET /api/1/users/me: ok email_set={}",
            tesla::str_field(&u, &["email"]).is_some()
        )),
        Err(e) => lines.push(format!("GET /api/1/users/me: fail {e}")),
    }

    let products = match tesla.products().await {
        Ok(p) => {
            lines.push(format!("GET /api/1/products: ok n={}", p.len()));
            p
        }
        Err(e) => {
            lines.push(format!("GET /api/1/products: fail {e}"));
            Vec::new()
        }
    };

    let vehicles_list = match tesla.vehicles().await {
        Ok(v) => {
            lines.push(format!("GET /api/1/vehicles: ok n={}", v.len()));
            v
        }
        Err(e) => {
            lines.push(format!("GET /api/1/vehicles: fail {e}"));
            Vec::new()
        }
    };

    // TeslaMate lists cars from products (`vehicle_id` present). Owner API
    // `/api/1/vehicles` often 412s; keep going from the products list.
    let vehicles: Vec<Value> = if !vehicles_list.is_empty() {
        vehicles_list
    } else {
        products
            .iter()
            .filter(|p| tesla::str_field(p, &["vin"]).is_some())
            .cloned()
            .collect()
    };

    for p in vehicles {
        let name = tesla::str_field(&p, &["display_name"]).unwrap_or("?");
        let id = tesla::i64_field(&p, &["id"]).unwrap_or(0);
        if id == 0 {
            lines.push(format!("vehicle {name}: skip (no id)"));
            continue;
        }
        let summary = match tesla.vehicle(id).await {
            Ok(v) => v,
            Err(e) => {
                lines.push(format!("GET /api/1/vehicles/{{id}}: fail {e}"));
                continue;
            }
        };
        let state = tesla::str_field(&summary, &["state"]).unwrap_or("unknown");
        lines.push(format!("GET /api/1/vehicles/{{id}}: ok {name} state={state}"));

        if state != "online" {
            lines.push(format!(
                "GET mobile_enabled: skipped ({name} is {state})"
            ));
            lines.push(format!(
                "GET vehicle_data: skipped ({name} is {state}; would wake the car)"
            ));
            lines.push(format!(
                "GET nearby_charging_sites: skipped ({name} is {state})"
            ));
            continue;
        }

        match tesla.mobile_enabled(id).await {
            Ok(on) => lines.push(format!("GET mobile_enabled: ok {on}")),
            Err(e) => lines.push(format!("GET mobile_enabled: fail {e}")),
        }

        match tesla.vehicle_data(id).await {
            Ok(data) => {
                lines.push(format!(
                    "GET vehicle_data: ok soc={} charging={} shift={}",
                    tesla::i64_field(&data, &["charge_state", "battery_level"])
                        .map(|n| n.to_string())
                        .unwrap_or("?".into()),
                    tesla::str_field(&data, &["charge_state", "charging_state"]).unwrap_or("?"),
                    tesla::str_field(&data, &["drive_state", "shift_state"]).unwrap_or("-")
                ));
                if let Some(car_id) = car_id_for(db, &summary, id) {
                    match logger::ingest_online(db, car_id, &data) {
                        Ok(()) => lines.push("snapshot: stored".into()),
                        Err(e) => lines.push(format!("snapshot: fail {e}")),
                    }
                } else {
                    lines.push("snapshot: skipped (no matching car row)".into());
                }
            }
            Err(e) => lines.push(format!("GET vehicle_data: fail {e}")),
        }

        match tesla.nearby_charging_sites(id).await {
            Ok(sites) => lines.push(summarize_nearby(&sites)),
            Err(e) => lines.push(format!("GET nearby_charging_sites: fail {e}")),
        }
    }

    match invoices::sync_once_opts(db, false).await {
        Ok(s) => lines.push(format!(
            "charging history: pages={} unique={} matched={} cost_updated={}",
            s.pages, s.fetched, s.matched, s.cost_updated
        )),
        Err(e) => lines.push(format!("charging history: fail {e}")),
    }

    Ok(lines)
}

fn car_id_for(db: &Db, summary: &Value, api_id: i64) -> Option<i64> {
    let conn = db.lock();
    if let Some(vin) = tesla::str_field(summary, &["vin"]) {
        if let Ok(id) = conn.query_row("SELECT id FROM cars WHERE vin=?1", [vin], |r| r.get(0)) {
            return Some(id);
        }
    }
    conn.query_row(
        "SELECT id FROM cars WHERE vid=?1 OR eid=?1",
        [api_id],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn summarize_nearby(v: &Value) -> String {
    let sc = v
        .get("superchargers")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let dc = v
        .get("destination_charging")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!("GET nearby_charging_sites: ok superchargers={sc} destination={dc}")
}
