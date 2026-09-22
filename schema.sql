PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS car_settings (
    id INTEGER PRIMARY KEY,
    suspend_min INTEGER NOT NULL DEFAULT 21,
    suspend_after_idle_min INTEGER NOT NULL DEFAULT 15,
    req_not_unlocked INTEGER NOT NULL DEFAULT 0,
    free_supercharging INTEGER NOT NULL DEFAULT 0,
    use_streaming_api INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 1,
    lfp_battery INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS settings (
    id INTEGER PRIMARY KEY,
    inserted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    unit_of_length TEXT NOT NULL DEFAULT 'km',
    unit_of_temperature TEXT NOT NULL DEFAULT 'C',
    preferred_range TEXT NOT NULL DEFAULT 'rated',
    base_url TEXT,
    grafana_url TEXT,
    language TEXT NOT NULL DEFAULT 'en',
    unit_of_pressure TEXT NOT NULL DEFAULT 'bar',
    theme_mode TEXT NOT NULL DEFAULT 'system'
);

CREATE TABLE IF NOT EXISTS cars (
    id INTEGER PRIMARY KEY,
    eid INTEGER NOT NULL UNIQUE,
    vid INTEGER NOT NULL UNIQUE,
    model TEXT,
    efficiency REAL,
    inserted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    vin TEXT NOT NULL UNIQUE,
    name TEXT,
    trim_badging TEXT,
    settings_id INTEGER NOT NULL REFERENCES car_settings(id) ON DELETE CASCADE,
    exterior_color TEXT,
    spoiler_type TEXT,
    wheel_type TEXT,
    display_priority INTEGER NOT NULL DEFAULT 1,
    marketing_name TEXT
);

CREATE TABLE IF NOT EXISTS addresses (
    id INTEGER PRIMARY KEY,
    display_name TEXT,
    latitude REAL,
    longitude REAL,
    name TEXT,
    house_number TEXT,
    road TEXT,
    neighbourhood TEXT,
    city TEXT,
    county TEXT,
    postcode TEXT,
    state TEXT,
    state_district TEXT,
    country TEXT,
    raw TEXT,
    inserted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    osm_id INTEGER,
    osm_type TEXT
);

CREATE TABLE IF NOT EXISTS geofences (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    radius INTEGER NOT NULL DEFAULT 25,
    inserted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    cost_per_unit REAL,
    session_fee REAL,
    billing_type TEXT NOT NULL DEFAULT 'per_kwh'
);

CREATE TABLE IF NOT EXISTS positions (
    id INTEGER PRIMARY KEY,
    date TEXT NOT NULL,
    latitude REAL NOT NULL,
    longitude REAL NOT NULL,
    speed INTEGER,
    power INTEGER,
    odometer REAL,
    ideal_battery_range_km REAL,
    battery_level INTEGER,
    outside_temp REAL,
    elevation INTEGER,
    fan_status INTEGER,
    driver_temp_setting REAL,
    passenger_temp_setting REAL,
    is_climate_on INTEGER,
    is_rear_defroster_on INTEGER,
    is_front_defroster_on INTEGER,
    car_id INTEGER NOT NULL REFERENCES cars(id) ON DELETE CASCADE,
    drive_id INTEGER,
    inside_temp REAL,
    battery_heater INTEGER,
    battery_heater_on INTEGER,
    battery_heater_no_power INTEGER,
    est_battery_range_km REAL,
    rated_battery_range_km REAL,
    usable_battery_level INTEGER,
    tpms_pressure_fl REAL,
    tpms_pressure_fr REAL,
    tpms_pressure_rl REAL,
    tpms_pressure_rr REAL
);

CREATE TABLE IF NOT EXISTS drives (
    id INTEGER PRIMARY KEY,
    start_date TEXT NOT NULL,
    end_date TEXT,
    outside_temp_avg REAL,
    speed_max INTEGER,
    power_max INTEGER,
    power_min INTEGER,
    start_ideal_range_km REAL,
    end_ideal_range_km REAL,
    start_km REAL,
    end_km REAL,
    distance REAL,
    duration_min INTEGER,
    car_id INTEGER NOT NULL REFERENCES cars(id) ON DELETE CASCADE,
    inside_temp_avg REAL,
    start_address_id INTEGER REFERENCES addresses(id) ON DELETE SET NULL,
    end_address_id INTEGER REFERENCES addresses(id) ON DELETE SET NULL,
    start_rated_range_km REAL,
    end_rated_range_km REAL,
    start_position_id INTEGER REFERENCES positions(id) ON DELETE SET NULL,
    end_position_id INTEGER REFERENCES positions(id) ON DELETE SET NULL,
    start_geofence_id INTEGER REFERENCES geofences(id) ON DELETE SET NULL,
    end_geofence_id INTEGER REFERENCES geofences(id) ON DELETE SET NULL,
    ascent INTEGER,
    descent INTEGER
);

CREATE TABLE IF NOT EXISTS charging_processes (
    id INTEGER PRIMARY KEY,
    start_date TEXT NOT NULL,
    end_date TEXT,
    charge_energy_added REAL,
    start_ideal_range_km REAL,
    end_ideal_range_km REAL,
    start_battery_level INTEGER,
    end_battery_level INTEGER,
    duration_min INTEGER,
    outside_temp_avg REAL,
    car_id INTEGER NOT NULL REFERENCES cars(id) ON DELETE CASCADE,
    position_id INTEGER NOT NULL REFERENCES positions(id),
    address_id INTEGER REFERENCES addresses(id) ON DELETE SET NULL,
    start_rated_range_km REAL,
    end_rated_range_km REAL,
    geofence_id INTEGER REFERENCES geofences(id) ON DELETE SET NULL,
    charge_energy_used REAL,
    cost REAL
);

CREATE TABLE IF NOT EXISTS charges (
    id INTEGER PRIMARY KEY,
    date TEXT NOT NULL,
    battery_heater_on INTEGER,
    battery_level INTEGER,
    charge_energy_added REAL NOT NULL,
    charger_actual_current INTEGER,
    charger_phases INTEGER,
    charger_pilot_current INTEGER,
    charger_power INTEGER NOT NULL,
    charger_voltage INTEGER,
    fast_charger_present INTEGER,
    conn_charge_cable TEXT,
    fast_charger_brand TEXT,
    fast_charger_type TEXT,
    ideal_battery_range_km REAL NOT NULL,
    not_enough_power_to_heat INTEGER,
    outside_temp REAL,
    charging_process_id INTEGER NOT NULL REFERENCES charging_processes(id) ON DELETE CASCADE,
    battery_heater INTEGER,
    battery_heater_no_power INTEGER,
    rated_battery_range_km REAL,
    usable_battery_level INTEGER
);

CREATE TABLE IF NOT EXISTS states (
    id INTEGER PRIMARY KEY,
    state TEXT NOT NULL,
    start_date TEXT NOT NULL,
    end_date TEXT,
    car_id INTEGER NOT NULL REFERENCES cars(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS updates (
    id INTEGER PRIMARY KEY,
    start_date TEXT NOT NULL,
    end_date TEXT,
    version TEXT,
    car_id INTEGER NOT NULL REFERENCES cars(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS oauth_tokens (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS positions_car_id_date ON positions(car_id, date);
CREATE INDEX IF NOT EXISTS positions_drive_id_date ON positions(drive_id, date);
CREATE INDEX IF NOT EXISTS positions_date ON positions(date);
CREATE INDEX IF NOT EXISTS drives_car_id_start ON drives(car_id, start_date);
CREATE INDEX IF NOT EXISTS charges_process_date ON charges(charging_process_id, date);
CREATE INDEX IF NOT EXISTS charging_processes_car_start ON charging_processes(car_id, start_date);
CREATE INDEX IF NOT EXISTS states_car_start ON states(car_id, start_date);
CREATE INDEX IF NOT EXISTS updates_car_start ON updates(car_id, start_date);
CREATE INDEX IF NOT EXISTS charges_date ON charges(date);

CREATE TABLE IF NOT EXISTS charging_invoices (
    session_id TEXT PRIMARY KEY,
    vin TEXT NOT NULL,
    site_name TEXT,
    start_date TEXT NOT NULL,
    end_date TEXT,
    currency TEXT,
    total_due REAL NOT NULL,
    energy_kwh REAL,
    rate_per_kwh REAL,
    charging_process_id INTEGER UNIQUE REFERENCES charging_processes(id) ON DELETE SET NULL,
    fetched_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS charging_invoices_vin_start ON charging_invoices(vin, start_date);

CREATE TABLE IF NOT EXISTS position_hourly (
    car_id INTEGER NOT NULL,
    date TEXT NOT NULL,
    n INTEGER NOT NULL,
    battery_level REAL,
    usable_battery_level REAL,
    rated_battery_range_km REAL,
    ideal_battery_range_km REAL,
    odometer REAL,
    outside_temp REAL,
    PRIMARY KEY (car_id, date)
);

CREATE UNIQUE INDEX IF NOT EXISTS addresses_osm ON addresses(osm_id, osm_type) WHERE osm_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT NOT NULL UNIQUE,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    is_admin INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS webauthn_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id TEXT NOT NULL UNIQUE,
    passkey_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_seen TEXT
);
CREATE TABLE IF NOT EXISTS invites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash TEXT NOT NULL UNIQUE,
    created_by INTEGER NOT NULL REFERENCES users(id),
    used_by INTEGER REFERENCES users(id),
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS webauthn_challenges (
    id TEXT PRIMARY KEY,
    purpose TEXT NOT NULL,
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    username TEXT,
    user_uuid TEXT,
    invite_token_hash TEXT,
    state_json TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    actor TEXT,
    action TEXT NOT NULL,
    detail TEXT
);
CREATE INDEX IF NOT EXISTS audit_log_at ON audit_log(at);
