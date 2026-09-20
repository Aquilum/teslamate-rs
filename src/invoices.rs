use crate::db::Db;
use crate::logger;
use crate::tesla::{self, Tesla};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

const MATCH_SECS: i64 = 25 * 60;
const MAX_PAGES: i64 = 80;

#[derive(Debug, Clone, PartialEq)]
pub struct Invoice {
    pub session_id: String,
    pub vin: String,
    pub site_name: String,
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    pub currency: String,
    pub total_due: f64,
    pub energy_kwh: Option<f64>,
    pub rate_per_kwh: Option<f64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SyncStats {
    pub fetched: usize,
    pub stored: usize,
    pub matched: usize,
    pub cost_updated: usize,
    pub pages: usize,
}

pub fn interval_secs() -> u64 {
    std::env::var("TESLAMATE_RS_INVOICE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6 * 3600)
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("TESLAMATE_RS_NO_INVOICES").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

pub async fn run(db: Db) {
    tokio::time::sleep(Duration::from_secs(8)).await;
    loop {
        if enabled() {
            match sync_once(&db).await {
                Ok(stats) => {
                    if stats.fetched > 0 || stats.matched > 0 {
                        info!(
                            "charging invoices fetched={} stored={} matched={} cost_updated={}",
                            stats.fetched, stats.stored, stats.matched, stats.cost_updated
                        );
                    }
                }
                Err(e) => warn!("charging invoices: {e:#}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(interval_secs().max(300))).await;
    }
}

pub async fn sync_once(db: &Db) -> Result<SyncStats> {
    sync_once_opts(db, false).await
}

/// `full` always hits Tesla OAuth, lists products (no vehicle_data), and walks
/// every charging-history page Tesla returns.
pub async fn sync_once_opts(db: &Db, full: bool) -> Result<SyncStats> {
    let Some(tokens) = logger::load_tokens(db)? else {
        return Ok(SyncStats::default());
    };
    let mut tesla = Tesla::new(tokens)?;
    if full {
        tesla.force_refresh().await?;
    } else {
        tesla.ensure_fresh().await?;
    }
    logger::store_tokens(db, tesla.tokens())?;

    if full {
        let products = tesla.products().await?;
        info!("tesla products={}", products.len());
        for p in &products {
            let name = tesla::str_field(p, &["display_name"]).unwrap_or("?");
            let state = tesla::str_field(p, &["state"]).unwrap_or("?");
            info!("tesla product {name} state={state}");
        }
    }

    let cars: Vec<(i64, String)> = {
        let conn = db.lock();
        let mut stmt = conn.prepare("SELECT id, vin FROM cars")?;
        let cars = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        cars
    };

    let mut stats = SyncStats::default();
    for (_car_id, vin) in cars {
        let mut page = 1i64;
        let mut seen_ids = HashSet::new();
        loop {
            let raw = tesla.charging_history_page(&vin, page).await?;
            let (items, has_more, total) = parse_history_page(&raw);
            stats.pages += 1;
            let items: Vec<_> = items
                .into_iter()
                .filter(|inv| seen_ids.insert(inv.session_id.clone()))
                .collect();
            info!(
                page,
                items = items.len(),
                has_more,
                total = total.unwrap_or(-1),
                "charging history page"
            );
            if items.is_empty() {
                break;
            }
            stats.fetched += items.len();
            let conn = db.lock();
            for inv in items {
                if upsert_invoice(&conn, &inv)? {
                    stats.stored += 1;
                }
                if let Some(pid) = match_process(&conn, &inv)? {
                    conn.execute(
                        "UPDATE charging_invoices SET charging_process_id=?1 WHERE session_id=?2",
                        params![pid, inv.session_id],
                    )?;
                    let n = conn.execute(
                        "UPDATE charging_processes SET cost=?1 WHERE id=?2",
                        params![inv.total_due, pid],
                    )?;
                    stats.matched += 1;
                    stats.cost_updated += n;
                }
            }
            drop(conn);
            let more_by_total = total.map(|t| seen_ids.len() < t as usize).unwrap_or(false);
            if page >= MAX_PAGES {
                break;
            }
            if has_more || more_by_total {
                page += 1;
                continue;
            }
            if full && page == 1 {
                page += 1;
                continue;
            }
            break;
        }
    }
    Ok(stats)
}

pub fn parse_history_page(v: &Value) -> (Vec<Invoice>, bool, Option<i64>) {
    let history = v
        .pointer("/data/me/charging/historyV2")
        .or_else(|| v.pointer("/me/charging/historyV2"))
        .or_else(|| v.get("data"));
    let rows = history
        .and_then(|h| h.get("data"))
        .and_then(|d| d.as_array())
        .or_else(|| history.and_then(|h| h.as_array()))
        .cloned()
        .unwrap_or_default();
    let has_more = history
        .and_then(|h| h.get("hasMoreData"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let total = history
        .and_then(|h| h.get("totalResults"))
        .and_then(|x| x.as_i64());
    let items = rows.iter().filter_map(parse_item).collect();
    (items, has_more, total)
}

pub fn parse_item(v: &Value) -> Option<Invoice> {
    let session_id = json_id(v.get("chargeSessionId"))
        .or_else(|| json_id(v.get("sessionId")))?;
    let vin = tesla::str_field(v, &["vin"])?.to_string();
    let start = parse_ts(tesla::str_field(v, &["chargeStartDateTime"])?)?;
    let end = tesla::str_field(v, &["chargeStopDateTime"]).and_then(parse_ts);
    let site_name = tesla::str_field(v, &["siteLocationName"])
        .unwrap_or("")
        .to_string();
    let fees = v.get("fees").and_then(|f| f.as_array());
    let (total_due, energy_kwh, rate_per_kwh, currency) = fee_totals(fees);
    Some(Invoice {
        session_id,
        vin,
        site_name,
        start,
        end,
        currency,
        total_due,
        energy_kwh,
        rate_per_kwh,
    })
}

fn fee_totals(fees: Option<&Vec<Value>>) -> (f64, Option<f64>, Option<f64>, String) {
    let Some(fees) = fees else {
        return (0.0, None, None, String::new());
    };
    let mut total = 0.0;
    let mut energy = 0.0;
    let mut energy_n = 0;
    let mut rate = None;
    let mut currency = String::new();
    for f in fees {
        let due = tesla::f64_field(f, &["netDue"])
            .or_else(|| tesla::f64_field(f, &["totalDue"]))
            .or_else(|| tesla::f64_field(f, &["amountDue"]))
            .unwrap_or(0.0);
        let pricing = tesla::str_field(f, &["pricingType"]).unwrap_or("");
        let fee_type = tesla::str_field(f, &["feeType"]).unwrap_or("");
        let signed = if pricing.eq_ignore_ascii_case("CREDIT")
            || fee_type.to_ascii_uppercase().contains("CREDIT")
        {
            -due.abs()
        } else {
            due
        };
        total += signed;
        if currency.is_empty() {
            currency = tesla::str_field(f, &["currencyCode"])
                .unwrap_or("")
                .to_string();
        }
        let uom = tesla::str_field(f, &["uom"]).unwrap_or("");
        if uom.eq_ignore_ascii_case("kwh") || uom.eq_ignore_ascii_case("kWh") {
            if let Some(kwh) = tesla::f64_field(f, &["usageBase"]) {
                energy += kwh;
                energy_n += 1;
            }
            if rate.is_none() {
                rate = tesla::f64_field(f, &["rateBase"]);
            }
        }
    }
    (
        (total * 100.0).round() / 100.0,
        if energy_n > 0 { Some(energy) } else { None },
        rate,
        currency,
    )
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
                .ok()
                .map(|n| n.and_utc())
        })
}

fn json_id(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn utc_naive(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn upsert_invoice(conn: &rusqlite::Connection, inv: &Invoice) -> Result<bool> {
    let n = conn.execute(
        "INSERT INTO charging_invoices (
            session_id, vin, site_name, start_date, end_date, currency,
            total_due, energy_kwh, rate_per_kwh, fetched_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,datetime('now'))
         ON CONFLICT(session_id) DO UPDATE SET
           site_name=excluded.site_name,
           start_date=excluded.start_date,
           end_date=excluded.end_date,
           currency=excluded.currency,
           total_due=excluded.total_due,
           energy_kwh=excluded.energy_kwh,
           rate_per_kwh=excluded.rate_per_kwh,
           fetched_at=excluded.fetched_at",
        params![
            inv.session_id,
            inv.vin,
            inv.site_name,
            utc_naive(inv.start),
            inv.end.map(utc_naive),
            inv.currency,
            inv.total_due,
            inv.energy_kwh,
            inv.rate_per_kwh,
        ],
    )?;
    Ok(n > 0)
}

fn match_process(conn: &rusqlite::Connection, inv: &Invoice) -> Result<Option<i64>> {
    let linked: Option<i64> = conn
        .query_row(
            "SELECT charging_process_id FROM charging_invoices
             WHERE session_id=?1 AND charging_process_id IS NOT NULL",
            [&inv.session_id],
            |r| r.get(0),
        )
        .optional()?;
    if linked.is_some() {
        return Ok(linked);
    }
    let start = utc_naive(inv.start);
    let mut stmt = conn.prepare(
        "SELECT cp.id, cp.start_date, cp.charge_energy_added
         FROM charging_processes cp
         JOIN cars c ON c.id = cp.car_id
         WHERE c.vin = ?1
           AND cp.id NOT IN (
             SELECT charging_process_id FROM charging_invoices
             WHERE charging_process_id IS NOT NULL
           )
           AND abs(strftime('%s', cp.start_date) - strftime('%s', ?2)) < ?3",
    )?;
    let rows: Vec<(i64, String, Option<f64>)> = stmt
        .query_map(params![inv.vin, start, MATCH_SECS], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(best_match(inv, &rows))
}

fn best_match(inv: &Invoice, rows: &[(i64, String, Option<f64>)]) -> Option<i64> {
    let inv_secs = inv.start.timestamp();
    let mut best: Option<(i64, i64, f64)> = None;
    for (id, start, kwh) in rows {
        let Ok(secs) = chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M:%S%.f"))
            .map(|n| n.and_utc().timestamp())
        else {
            continue;
        };
        let dt = (secs - inv_secs).abs();
        if dt >= MATCH_SECS {
            continue;
        }
        let energy_pen = match (inv.energy_kwh, kwh) {
            (Some(a), Some(b)) => (a - b).abs(),
            _ => 0.0,
        };
        let better = match best {
            None => true,
            Some((_, bdt, bpen)) => dt < bdt || (dt == bdt && energy_pen < bpen),
        };
        if better {
            best = Some((*id, dt, energy_pen));
        }
    }
    best.map(|(id, _, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    fn sample_item() -> Value {
        json!({
            "chargeSessionId": "sc-1",
            "sessionId": 99,
            "vin": "5YJSA7E2XNF000001",
            "siteLocationName": "Trumpington, UK",
            "chargeStartDateTime": "2026-07-10T13:55:00Z",
            "chargeStopDateTime": "2026-07-10T14:20:00Z",
            "fees": [{
                "feeType": "CHARGING",
                "pricingType": "PAYMENT",
                "currencyCode": "GBP",
                "usageBase": 50.3,
                "rateBase": 0.69,
                "uom": "kwh",
                "totalDue": 34.71,
                "netDue": 34.71
            }, {
                "feeType": "PARKING",
                "pricingType": "PAYMENT",
                "currencyCode": "GBP",
                "uom": "min",
                "totalDue": 1.00,
                "netDue": 1.00
            }]
        })
    }

    #[test]
    fn parses_fees_including_idle() {
        let inv = parse_item(&sample_item()).unwrap();
        assert_eq!(inv.session_id, "sc-1");
        assert_eq!(inv.total_due, 35.71);
        assert_eq!(inv.energy_kwh, Some(50.3));
        assert_eq!(inv.currency, "GBP");
        assert_eq!(inv.site_name, "Trumpington, UK");
    }

    #[test]
    fn credits_reduce_total() {
        let mut v = sample_item();
        v["fees"].as_array_mut().unwrap().push(json!({
            "feeType": "CREDIT",
            "pricingType": "CREDIT",
            "currencyCode": "GBP",
            "totalDue": 5.0,
            "netDue": 5.0
        }));
        let inv = parse_item(&v).unwrap();
        assert_eq!(inv.total_due, 30.71);
    }

    #[test]
    fn graphql_wrapper_and_has_more() {
        let raw = json!({
            "data": {"me": {"charging": {"historyV2": {
                "hasMoreData": true,
                "data": [sample_item()]
            }}}}
        });
        let (items, more, total) = parse_history_page(&raw);
        assert!(more);
        assert_eq!(total, None);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn matches_nearby_process_and_sets_cost() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        conn.execute(
            "INSERT INTO car_settings (id) VALUES (1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cars (id, eid, vid, inserted_at, updated_at, vin, settings_id)
             VALUES (2, 1, 1, '2026-01-01', '2026-01-01', '5YJSA7E2XNF000001', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO positions (id, date, latitude, longitude, car_id)
             VALUES (1, '2026-07-10 13:55:07', 52.168, 0.109, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO charging_processes (
                id, start_date, end_date, charge_energy_added, duration_min,
                car_id, position_id)
             VALUES (10, '2026-07-10 13:55:07', '2026-07-10 14:20:00', 50.3, 25, 2, 1)",
            [],
        )
        .unwrap();
        let inv = parse_item(&sample_item()).unwrap();
        upsert_invoice(&conn, &inv).unwrap();
        let pid = match_process(&conn, &inv).unwrap().unwrap();
        assert_eq!(pid, 10);
        conn.execute(
            "UPDATE charging_processes SET cost=?1 WHERE id=?2",
            params![inv.total_due, pid],
        )
        .unwrap();
        let cost: f64 = conn
            .query_row("SELECT cost FROM charging_processes WHERE id=10", [], |r| r.get(0))
            .unwrap();
        assert!((cost - 35.71).abs() < 0.001);
    }

    #[test]
    fn ignores_unrelated_home_session() {
        let inv = parse_item(&sample_item()).unwrap();
        let rows = vec![(99, "2026-07-10 08:00:00".into(), Some(11.0))];
        assert!(best_match(&inv, &rows).is_none());
    }
}
