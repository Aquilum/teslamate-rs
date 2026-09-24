use anyhow::Result;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct MockCar {
    pub lat: f64,
    pub lon: f64,
    pub speed_mph: i64,
    pub power_kw: i64,
    pub soc: i64,
    pub odo_mi: f64,
    pub shift: &'static str,
    pub charging_state: &'static str,
    pub charger_power: i64,
    pub energy_added: f64,
    pub state: &'static str,
}

impl Default for MockCar {
    fn default() -> Self {
        Self {
            lat: 51.5080,
            lon: -0.1280,
            speed_mph: 38,
            power_kw: 22,
            soc: 67,
            odo_mi: 26840.0,
            shift: "D",
            charging_state: "Disconnected",
            charger_power: 0,
            energy_added: 0.0,
            state: "online",
        }
    }
}

impl MockCar {
    fn tick(&mut self) {
        if self.shift == "D" {
            self.lat += 0.0008;
            self.lon += 0.0004;
            self.odo_mi += 0.04;
            self.soc = (self.soc - 1).clamp(20, 100);
            self.speed_mph = 32 + ((self.odo_mi * 10.0) as i64 % 25);
            self.power_kw = 15 + ((self.odo_mi * 3.0) as i64 % 40);
        } else if self.charging_state == "Charging" {
            self.energy_added += 0.4;
            self.soc = (self.soc + 1).clamp(0, 90);
            self.speed_mph = 0;
            self.power_kw = 0;
        }
    }

    fn snapshot(&self) -> Value {
        json!({
            "id": 9001,
            "vehicle_id": 8001,
            "vin": crate::seed::MOCK_VIN,
            "display_name": "Mock S",
            "state": self.state,
            "charge_state": {
                "battery_level": self.soc,
                "usable_battery_level": self.soc,
                "battery_range": self.soc as f64 * 3.0,
                "est_battery_range": self.soc as f64 * 2.7,
                "ideal_battery_range": self.soc as f64 * 3.1,
                "charging_state": self.charging_state,
                "charger_power": self.charger_power,
                "charger_voltage": if self.charger_power > 0 { 400 } else { 0 },
                "charger_actual_current": if self.charger_power > 0 { 300 } else { 0 },
                "charger_pilot_current": if self.charger_power > 0 { 300 } else { 0 },
                "charger_phases": 1,
                "charge_energy_added": self.energy_added,
                "charge_limit_soc": 80,
                "time_to_full_charge": if self.charger_power > 0 { 0.4 } else { 0.0 },
                "minutes_to_full_charge": if self.charger_power > 0 { 24 } else { 0 },
                "charge_port_door_open": self.charger_power > 0,
                "scheduled_charging_pending": false,
                "fast_charger_present": self.charger_power > 20,
                "fast_charger_brand": if self.charger_power > 20 { "Tesla" } else { "" },
                "fast_charger_type": if self.charger_power > 20 { "Tesla" } else { "" },
                "conn_charge_cable": if self.charger_power > 0 { "Tesla Supercharger" } else { "<invalid>" },
                "battery_heater": false,
                "battery_heater_on": false,
                "outside_temp": 14.0
            },
            "drive_state": {
                "latitude": self.lat,
                "longitude": self.lon,
                "speed": self.speed_mph,
                "power": self.power_kw,
                "shift_state": self.shift,
                "heading": 85,
                "native_elevation": 18,
                "active_route": if self.shift == "D" {
                    json!({"destination": "Work", "miles_to_arrival": 3.2, "minutes_to_arrival": 12, "energy_at_arrival": 58})
                } else {
                    Value::Null
                }
            },
            "climate_state": {
                "inside_temp": 20.5,
                "outside_temp": 14.0,
                "driver_temp_setting": 21.0,
                "passenger_temp_setting": 21.0,
                "is_climate_on": true,
                "is_preconditioning": false,
                "climate_keeper_mode": "off",
                "is_rear_defroster_on": false,
                "is_front_defroster_on": false,
                "seat_heater_left": 0,
                "seat_heater_right": 1,
                "seat_heater_rear_left": 0,
                "seat_heater_rear_right": 0,
                "steering_wheel_heater": false,
                "cabin_overheat_protection": "On"
            },
            "vehicle_state": {
                "odometer": self.odo_mi,
                "car_version": "2026.32.4 abcdef12",
                "locked": true,
                "sentry_mode": self.shift != "D",
                "is_user_present": self.shift == "D",
                "valet_mode": false,
                "df": 0, "dr": 0, "pf": 0, "pr": 0, "ft": 0, "rt": 0,
                "fd_window": 0, "fp_window": 0, "rd_window": 0, "rp_window": 0,
                "dashcam_state": "Unavailable",
                "center_display_state": if self.shift == "D" { 3 } else { 0 },
                "software_update": {"status": "", "version": ""},
                "tpms_pressure_fl": 42.0,
                "tpms_pressure_fr": 42.0,
                "tpms_pressure_rl": 40.5,
                "tpms_pressure_rr": 40.5,
                "tpms_soft_warning_fl": false,
                "tpms_soft_warning_fr": false,
                "tpms_soft_warning_rl": false,
                "tpms_soft_warning_rr": false
            },
            "vehicle_config": {
                "car_type": "models",
                "exterior_color": "Red",
                "wheel_type": "Base19"
            }
        })
    }
}

#[derive(Clone)]
struct App {
    car: Arc<Mutex<MockCar>>,
}

pub async fn serve(bind: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/oauth2/v3/token", post(token))
        .route("/api/1/products", get(products))
        .route("/api/1/vehicles", get(products))
        .route("/api/1/vehicles/{id}", get(vehicle))
        .route("/api/1/vehicles/{id}/vehicle_data", get(vehicle_data))
        .route("/api/1/vehicles/{id}/nearby_charging_sites", get(nearby))
        .route("/api/1/vehicles/{id}/mobile_enabled", get(mobile_enabled))
        .route("/api/1/users/me", get(users_me))
        .route("/graphql", post(charging_history))
        .route("/mobile-app/charging/history", get(charging_history_get))
        .with_state(App {
            car: Arc::new(Mutex::new(MockCar::default())),
        });
    tracing::info!("mock Tesla Owner API on http://{bind}");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn token() -> Json<Value> {
    Json(json!({
        "access_token": "mock-access-token",
        "refresh_token": "mock-refresh-token",
        "expires_in": 28800,
        "token_type": "Bearer"
    }))
}

async fn products(State(app): State<App>) -> Json<Value> {
    let car = app.car.lock().await;
    Json(json!({
        "response": [{
            "id": 9001,
            "vehicle_id": 8001,
            "vin": crate::seed::MOCK_VIN,
            "display_name": "Mock S",
            "state": car.state
        }]
    }))
}

async fn vehicle(State(app): State<App>) -> Json<Value> {
    let car = app.car.lock().await;
    Json(json!({
        "response": {
            "id": 9001,
            "vehicle_id": 8001,
            "vin": crate::seed::MOCK_VIN,
            "display_name": "Mock S",
            "state": car.state
        }
    }))
}

async fn vehicle_data(State(app): State<App>) -> Json<Value> {
    let mut car = app.car.lock().await;
    car.tick();
    Json(json!({ "response": car.snapshot() }))
}

fn mock_invoice() -> Value {
    json!({
        "chargeSessionId": "mock-sc-1",
        "sessionId": 1,
        "vin": crate::seed::MOCK_VIN,
        "siteLocationName": "Heathrow, UK",
        "chargeStartDateTime": "2026-06-01T08:00:00Z",
        "chargeStopDateTime": "2026-06-01T08:28:00Z",
        "fees": [{
            "feeType": "CHARGING",
            "pricingType": "PAYMENT",
            "currencyCode": "GBP",
            "usageBase": 26.6,
            "rateBase": 0.40,
            "uom": "kwh",
            "totalDue": 10.64,
            "netDue": 10.64
        }]
    })
}

async fn charging_history() -> Json<Value> {
    Json(json!({
        "data": {"me": {"charging": {"historyV2": {
            "hasMoreData": false,
            "pageNumber": 1,
            "data": [mock_invoice()]
        }}}}
    }))
}

async fn charging_history_get() -> Json<Value> {
    Json(json!({ "data": [mock_invoice()] }))
}

async fn nearby() -> Json<Value> {
    Json(json!({
        "response": {
            "superchargers": [{"name": "Heathrow", "type": "supercharger"}],
            "destination_charging": []
        }
    }))
}

async fn users_me() -> Json<Value> {
    Json(json!({
        "response": { "email": "mock@example.com", "full_name": "Mock User" }
    }))
}

async fn mobile_enabled() -> Json<Value> {
    Json(json!({ "response": true }))
}
