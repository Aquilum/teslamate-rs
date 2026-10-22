//! When to call `vehicle_data`, and when to leave the car alone.
//!
//! `GET /api/1/vehicles/{id}` only reports online / asleep / offline and does
//! not reset the car's inactivity timer. `vehicle_data` is a live command:
//! Tesla restarts that timer on every call, so a parked car never reaches the
//! ~15 minutes of quiet it needs to sleep.
//!
//! TeslaMate (`TeslaMate.Vehicles.Vehicle`) therefore suspends logging once
//! the car has been idle. With the streaming API that is 3 minutes of idle,
//! then no `vehicle_data` until the stream shows real use or a confirmed
//! wakeup. Without it, the idle and suspend intervals come from `car_settings`
//! (`suspend_after_idle_min`, default 15; `suspend_min`, default 21). A brief
//! online with streaming `power` nil is a subsystem check, not a wakeup —
//! calling `vehicle_data` there turns a 2–3 minute blip into a full wake.

use crate::tesla;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleConfig {
    pub use_streaming: bool,
    pub req_not_unlocked: bool,
    /// Summary-only poll while asleep or suspended.
    pub asleep_interval: Duration,
    /// How long the car must sit idle before logging is suspended.
    pub idle_before_suspend: Duration,
    /// Polling mode: `vehicle_data` again if the car is still online this long
    /// after suspend. Streaming mode leaves this unused until the stream fails.
    pub suspend_for: Duration,
    /// `vehicle_data` cadence while idle but not yet suspended.
    pub idle_poll: Duration,
    pub user_present_poll: Duration,
    /// Sentry, Dog Mode, preconditioning.
    pub keeper_poll: Duration,
    /// Doors, trunk, update, power, unlocked.
    pub blocked_poll: Duration,
    /// Drive and charge cadence.
    pub active_poll: Duration,
}

/// `fast` compresses every interval to `active_poll_secs` (demo / `TESLAMATE_RS_POLL_SECS` <= 5).
pub fn config_for(
    suspend_min: i64,
    suspend_after_idle_min: i64,
    use_streaming: bool,
    req_not_unlocked: bool,
    active_poll_secs: u64,
    fast: bool,
) -> IdleConfig {
    let active = active_poll_secs.max(1);
    let scale = |secs: u64| Duration::from_secs(if fast { active } else { secs.max(1) });
    let multiplier = if use_streaming { 2 } else { 1 };
    let idle_before = if use_streaming {
        3 * 60
    } else {
        suspend_after_idle_min.max(1) as u64 * 60
    };
    let suspend_for = if use_streaming {
        30 * 60
    } else {
        suspend_min.max(1) as u64 * 60
    };
    IdleConfig {
        use_streaming,
        req_not_unlocked,
        asleep_interval: scale(30),
        idle_before_suspend: scale(idle_before),
        suspend_for: scale(suspend_for),
        idle_poll: scale(15 * multiplier),
        user_present_poll: scale(15),
        keeper_poll: scale(30 * multiplier),
        blocked_poll: scale(15 * multiplier),
        active_poll: Duration::from_secs(active),
    }
}

pub fn probe_timeout(purpose: ProbePurpose, fast_secs: Option<u64>) -> Duration {
    if let Some(secs) = fast_secs {
        return Duration::from_secs(secs.max(1));
    }
    match purpose {
        ProbePurpose::Wake => Duration::from_secs(30),
        ProbePurpose::Suspend => Duration::from_secs(5),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No observation yet this process.
    Startup,
    /// Last summary was asleep or offline. `vehicle_data` would wake it.
    Asleep,
    /// Logging. `idle_since` is the last time the car was busy, or when this
    /// idle stretch started.
    Awake { idle_since: Instant },
    /// `vehicle_data` suppressed. `resume_at` is set in polling mode, and in
    /// streaming mode only after the stream stops producing samples.
    Suspended {
        idle_since: Instant,
        resume_at: Option<Instant>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbePurpose {
    /// Summary flipped to online while we believed the car was asleep.
    Wake,
    /// Already suspended; watch the stream for a drive or charge.
    Suspend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SummaryOnly { phase: Phase, wait: Duration },
    Probe { purpose: ProbePurpose },
    VehicleData,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamSample {
    pub speed: Option<f64>,
    pub power: Option<f64>,
    pub shift: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamReading {
    Sample(StreamSample),
    Error,
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    VehicleData,
    Wait { phase: Phase, wait: Duration },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Driving,
    Charging,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepBlock {
    UserPresent,
    Preconditioning,
    DogMode,
    Sentry,
    Update,
    Doors,
    Trunk,
    Unlocked,
    Power,
}

impl SleepBlock {
    fn resets_idle(self) -> bool {
        !matches!(self, Self::Unlocked)
    }

    fn poll_after(self, cfg: &IdleConfig) -> Duration {
        match self {
            Self::UserPresent => cfg.user_present_poll,
            Self::Preconditioning | Self::DogMode | Self::Sentry => cfg.keeper_poll,
            Self::Update | Self::Doors | Self::Trunk | Self::Unlocked | Self::Power => {
                cfg.blocked_poll
            }
        }
    }
}

impl std::fmt::Display for SleepBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UserPresent => "user present",
            Self::Preconditioning => "preconditioning",
            Self::DogMode => "dog mode",
            Self::Sentry => "sentry mode",
            Self::Update => "software update",
            Self::Doors => "doors open",
            Self::Trunk => "trunk open",
            Self::Unlocked => "unlocked",
            Self::Power => "power use",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    pub phase: Phase,
    pub wait: Duration,
    pub activity: Activity,
    /// Idle already exceeded the suspend threshold, but the car cannot sleep.
    pub block: Option<SleepBlock>,
}

pub fn plan(phase: &Phase, summary_state: &str, now: Instant, cfg: &IdleConfig) -> Action {
    if summary_state == "asleep" || summary_state == "offline" {
        return Action::SummaryOnly {
            phase: Phase::Asleep,
            wait: cfg.asleep_interval,
        };
    }
    if summary_state != "online" {
        return Action::SummaryOnly {
            phase: *phase,
            wait: cfg.asleep_interval,
        };
    }
    match *phase {
        Phase::Suspended {
            resume_at: Some(t), ..
        } if now >= t => Action::VehicleData,
        Phase::Suspended { .. } if cfg.use_streaming => Action::Probe {
            purpose: ProbePurpose::Suspend,
        },
        Phase::Suspended { .. } => Action::SummaryOnly {
            phase: *phase,
            wait: cfg.asleep_interval,
        },
        Phase::Asleep | Phase::Startup if cfg.use_streaming => Action::Probe {
            purpose: ProbePurpose::Wake,
        },
        Phase::Awake { .. } | Phase::Asleep | Phase::Startup => Action::VehicleData,
    }
}

pub fn after_probe(
    purpose: ProbePurpose,
    reading: &StreamReading,
    phase: &Phase,
    now: Instant,
    cfg: &IdleConfig,
) -> ProbeOutcome {
    match purpose {
        ProbePurpose::Wake => match reading {
            StreamReading::Sample(sample) if !wake_is_real(sample) => ProbeOutcome::Wait {
                phase: Phase::Asleep,
                wait: cfg.asleep_interval,
            },
            _ => ProbeOutcome::VehicleData,
        },
        ProbePurpose::Suspend => {
            let (idle_since, resume_at) = match *phase {
                Phase::Suspended {
                    idle_since,
                    resume_at,
                } => (idle_since, resume_at),
                _ => (now, None),
            };
            match reading {
                StreamReading::Sample(sample) if suspend_is_active(sample) => {
                    ProbeOutcome::VehicleData
                }
                StreamReading::Sample(_) => ProbeOutcome::Wait {
                    phase: Phase::Suspended {
                        idle_since,
                        resume_at: None,
                    },
                    wait: cfg.asleep_interval,
                },
                StreamReading::Error | StreamReading::Timeout => ProbeOutcome::Wait {
                    phase: Phase::Suspended {
                        idle_since,
                        resume_at: resume_at.or(Some(now + cfg.suspend_for)),
                    },
                    wait: cfg.asleep_interval,
                },
            }
        }
    }
}

pub fn apply_vehicle_data(data: &Value, phase: &Phase, now: Instant, cfg: &IdleConfig) -> Applied {
    let idle_since = match *phase {
        Phase::Awake { idle_since } | Phase::Suspended { idle_since, .. } => idle_since,
        Phase::Startup | Phase::Asleep => now,
    };
    let activity = activity(data);
    match activity {
        Activity::Driving | Activity::Charging => Applied {
            phase: Phase::Awake { idle_since: now },
            wait: cfg.active_poll,
            activity,
            block: None,
        },
        Activity::Idle => match can_fall_asleep(data, cfg) {
            Err(block) => {
                let elapsed = now.saturating_duration_since(idle_since) >= cfg.idle_before_suspend;
                Applied {
                    phase: Phase::Awake {
                        idle_since: if block.resets_idle() { now } else { idle_since },
                    },
                    wait: block.poll_after(cfg),
                    activity,
                    block: if elapsed { Some(block) } else { None },
                }
            }
            Ok(()) => {
                if now.saturating_duration_since(idle_since) >= cfg.idle_before_suspend {
                    Applied {
                        phase: Phase::Suspended {
                            idle_since,
                            resume_at: if cfg.use_streaming {
                                None
                            } else {
                                Some(now + cfg.suspend_for)
                            },
                        },
                        wait: cfg.asleep_interval,
                        activity,
                        block: None,
                    }
                } else {
                    Applied {
                        phase: Phase::Awake { idle_since },
                        wait: cfg.idle_poll,
                        activity,
                        block: None,
                    }
                }
            }
        },
    }
}

pub fn activity(data: &Value) -> Activity {
    if is_charging(data) {
        Activity::Charging
    } else if is_driving(data) {
        Activity::Driving
    } else {
        Activity::Idle
    }
}

fn is_charging(data: &Value) -> bool {
    matches!(
        tesla::str_field(data, &["charge_state", "charging_state"]),
        Some("Charging" | "Starting")
    )
}

fn is_driving(data: &Value) -> bool {
    match tesla::str_field(data, &["drive_state", "shift_state"]) {
        Some("D" | "R" | "N") => true,
        _ => tesla::f64_field(data, &["drive_state", "speed"]).unwrap_or(0.0) > 0.0,
    }
}

/// TeslaMate `can_fall_asleep/2`. `Ok` means nothing we can see would keep the
/// car awake, so logging may be suspended.
pub fn can_fall_asleep(data: &Value, cfg: &IdleConfig) -> Result<(), SleepBlock> {
    if tesla::bool_field(data, &["vehicle_state", "is_user_present"]) == Some(true) {
        return Err(SleepBlock::UserPresent);
    }
    if tesla::bool_field(data, &["climate_state", "is_preconditioning"]) == Some(true) {
        return Err(SleepBlock::Preconditioning);
    }
    if tesla::str_field(data, &["climate_state", "climate_keeper_mode"]) == Some("dog") {
        return Err(SleepBlock::DogMode);
    }
    if tesla::bool_field(data, &["vehicle_state", "sentry_mode"]) == Some(true) {
        return Err(SleepBlock::Sentry);
    }
    match tesla::str_field(data, &["vehicle_state", "software_update", "status"]) {
        Some("installing") => return Err(SleepBlock::Update),
        Some("downloading") => {
            let perc =
                tesla::f64_field(data, &["vehicle_state", "software_update", "download_perc"])
                    .unwrap_or(0.0);
            if perc < 100.0 {
                return Err(SleepBlock::Update);
            }
        }
        _ => {}
    }
    if ["df", "pf", "dr", "pr"]
        .into_iter()
        .any(|door| closure_open(data, door))
    {
        return Err(SleepBlock::Doors);
    }
    if ["ft", "rt"].into_iter().any(|lid| closure_open(data, lid)) {
        return Err(SleepBlock::Trunk);
    }
    if cfg.req_not_unlocked && tesla::bool_field(data, &["vehicle_state", "locked"]) == Some(false)
    {
        return Err(SleepBlock::Unlocked);
    }
    if tesla::f64_field(data, &["drive_state", "power"]).unwrap_or(0.0) > 0.0 {
        return Err(SleepBlock::Power);
    }
    Ok(())
}

fn closure_open(data: &Value, key: &str) -> bool {
    let Some(cell) = data.get("vehicle_state").and_then(|s| s.get(key)) else {
        return false;
    };
    cell.as_bool() == Some(true)
        || cell.as_f64().unwrap_or(0.0) > 0.0
        || cell.as_i64().unwrap_or(0) > 0
}

/// Columns match the streaming subscribe list: speed, odometer, soc, elevation,
/// est_heading, est_lat, est_lng, power, shift_state, ...
pub fn parse_stream_csv(csv: &str) -> Option<StreamSample> {
    let cols: Vec<&str> = csv.split(',').collect();
    if cols.len() < 8 {
        return None;
    }
    let shift = cols.get(8).map(|s| s.trim()).filter(|s| !s.is_empty());
    Some(StreamSample {
        speed: parse_f64(cols[0]),
        power: parse_f64(cols[7]),
        shift: shift.map(str::to_string),
    })
}

fn parse_f64(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

fn wake_is_real(sample: &StreamSample) -> bool {
    sample.power.is_some() || shift_or_speed(sample)
}

fn suspend_is_active(sample: &StreamSample) -> bool {
    shift_or_speed(sample) || matches!(sample.power, Some(p) if p != 0.0)
}

fn shift_or_speed(sample: &StreamSample) -> bool {
    matches!(sample.shift.as_deref(), Some("D" | "R" | "N")) || sample.speed.unwrap_or(0.0) > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn streaming() -> IdleConfig {
        config_for(21, 15, true, false, 30, false)
    }

    fn polling() -> IdleConfig {
        config_for(21, 15, false, false, 30, false)
    }

    fn parked() -> Value {
        json!({
            "drive_state": {"shift_state": null, "power": 0, "speed": null},
            "charge_state": {"charging_state": "Disconnected"},
            "climate_state": {"is_preconditioning": false, "climate_keeper_mode": "off"},
            "vehicle_state": {
                "is_user_present": false,
                "sentry_mode": false,
                "locked": true,
                "df": 0, "pf": 0, "dr": 0, "pr": 0, "ft": 0, "rt": 0,
                "software_update": {"status": "", "download_perc": 0}
            }
        })
    }

    #[test]
    fn streaming_and_polling_timeouts_match_teslamate() {
        let s = streaming();
        assert_eq!(s.idle_before_suspend, Duration::from_secs(3 * 60));
        assert_eq!(s.suspend_for, Duration::from_secs(30 * 60));
        assert_eq!(s.idle_poll, Duration::from_secs(30));
        let p = polling();
        assert_eq!(p.idle_before_suspend, Duration::from_secs(15 * 60));
        assert_eq!(p.suspend_for, Duration::from_secs(21 * 60));
        assert_eq!(p.idle_poll, Duration::from_secs(15));
        assert_eq!(p.asleep_interval, Duration::from_secs(30));
    }

    #[test]
    fn parked_car_stops_requesting_vehicle_data() {
        let cfg = streaming();
        let t0 = Instant::now();
        let data = parked();

        assert!(matches!(
            plan(&Phase::Startup, "online", t0, &cfg),
            Action::Probe {
                purpose: ProbePurpose::Wake
            }
        ));
        let awake = StreamSample {
            speed: Some(0.0),
            power: Some(0.0),
            shift: None,
        };
        assert_eq!(
            after_probe(
                ProbePurpose::Wake,
                &StreamReading::Sample(awake),
                &Phase::Startup,
                t0,
                &cfg
            ),
            ProbeOutcome::VehicleData
        );

        let first = apply_vehicle_data(&data, &Phase::Startup, t0, &cfg);
        assert!(matches!(first.phase, Phase::Awake { idle_since } if idle_since == t0));
        assert_eq!(first.activity, Activity::Idle);

        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(plan(&first.phase, "online", t1, &cfg), Action::VehicleData);
        let mid = apply_vehicle_data(&data, &first.phase, t1, &cfg);
        assert!(matches!(mid.phase, Phase::Awake { idle_since } if idle_since == t0));

        let t2 = t0 + Duration::from_secs(180);
        let suspended = apply_vehicle_data(&data, &mid.phase, t2, &cfg);
        assert!(matches!(
            suspended.phase,
            Phase::Suspended {
                resume_at: None,
                ..
            }
        ));

        let t3 = t2 + Duration::from_secs(30);
        assert!(matches!(
            plan(&suspended.phase, "online", t3, &cfg),
            Action::Probe {
                purpose: ProbePurpose::Suspend
            }
        ));
        let quiet = StreamSample {
            speed: Some(0.0),
            power: Some(0.0),
            shift: Some("P".into()),
        };
        match after_probe(
            ProbePurpose::Suspend,
            &StreamReading::Sample(quiet),
            &suspended.phase,
            t3,
            &cfg,
        ) {
            ProbeOutcome::Wait { phase, .. } => assert!(matches!(
                phase,
                Phase::Suspended {
                    resume_at: None,
                    ..
                }
            )),
            ProbeOutcome::VehicleData => panic!("quiet stream must not call vehicle_data"),
        }

        assert!(matches!(
            plan(&suspended.phase, "asleep", t3, &cfg),
            Action::SummaryOnly {
                phase: Phase::Asleep,
                ..
            }
        ));
    }

    #[test]
    fn subsystem_check_does_not_call_vehicle_data() {
        let cfg = streaming();
        let now = Instant::now();
        let nil_power = StreamSample {
            speed: None,
            power: None,
            shift: None,
        };
        assert_eq!(
            after_probe(
                ProbePurpose::Wake,
                &StreamReading::Sample(nil_power),
                &Phase::Asleep,
                now,
                &cfg
            ),
            ProbeOutcome::Wait {
                phase: Phase::Asleep,
                wait: cfg.asleep_interval,
            }
        );
        assert_eq!(
            after_probe(
                ProbePurpose::Wake,
                &StreamReading::Timeout,
                &Phase::Asleep,
                now,
                &cfg
            ),
            ProbeOutcome::VehicleData
        );
    }

    #[test]
    fn polling_mode_waits_out_suspend_then_checks_once() {
        let cfg = polling();
        let t0 = Instant::now();
        assert_eq!(
            plan(&Phase::Asleep, "online", t0, &cfg),
            Action::VehicleData,
            "polling mode has no stream to separate a subsystem check"
        );
        let applied = apply_vehicle_data(&parked(), &Phase::Asleep, t0, &cfg);
        let Phase::Awake { idle_since } = applied.phase else {
            panic!("expected awake, got {:?}", applied.phase);
        };
        let t1 = idle_since + cfg.idle_before_suspend;
        let suspended = apply_vehicle_data(&parked(), &applied.phase, t1, &cfg);
        let Phase::Suspended {
            resume_at: Some(until),
            ..
        } = suspended.phase
        else {
            panic!("expected polling suspend, got {:?}", suspended.phase);
        };
        assert_eq!(until, t1 + cfg.suspend_for);
        assert!(matches!(
            plan(
                &suspended.phase,
                "online",
                t1 + Duration::from_secs(60),
                &cfg
            ),
            Action::SummaryOnly { .. }
        ));
        assert_eq!(
            plan(&suspended.phase, "online", until, &cfg),
            Action::VehicleData
        );
        let again = apply_vehicle_data(&parked(), &suspended.phase, until, &cfg);
        assert!(matches!(again.phase, Phase::Suspended { .. }));
    }

    #[test]
    fn failed_stream_arms_a_polling_fallback() {
        let cfg = streaming();
        let t0 = Instant::now();
        let phase = Phase::Suspended {
            idle_since: t0,
            resume_at: None,
        };
        match after_probe(
            ProbePurpose::Suspend,
            &StreamReading::Error,
            &phase,
            t0,
            &cfg,
        ) {
            ProbeOutcome::Wait {
                phase:
                    Phase::Suspended {
                        resume_at: Some(until),
                        ..
                    },
                ..
            } => assert_eq!(until, t0 + cfg.suspend_for),
            other => panic!("expected armed suspend, got {other:?}"),
        }
    }

    #[test]
    fn suspend_stream_resumes_for_drive_and_charge() {
        let cfg = streaming();
        let now = Instant::now();
        let phase = Phase::Suspended {
            idle_since: now,
            resume_at: None,
        };
        let drive = StreamSample {
            speed: Some(10.0),
            power: Some(20.0),
            shift: Some("D".into()),
        };
        assert_eq!(
            after_probe(
                ProbePurpose::Suspend,
                &StreamReading::Sample(drive),
                &phase,
                now,
                &cfg
            ),
            ProbeOutcome::VehicleData
        );
        let charging = StreamSample {
            speed: Some(0.0),
            power: Some(-7.0),
            shift: Some("P".into()),
        };
        assert_eq!(
            after_probe(
                ProbePurpose::Suspend,
                &StreamReading::Sample(charging),
                &phase,
                now,
                &cfg
            ),
            ProbeOutcome::VehicleData
        );
    }

    #[test]
    fn blockers_keep_logging_and_sentry_resets_idle() {
        let cfg = streaming();
        let idle_since = Instant::now();
        let t0 = idle_since + Duration::from_secs(600);
        let mut data = parked();
        data["vehicle_state"]["sentry_mode"] = json!(true);
        let awake = Phase::Awake { idle_since };
        let applied = apply_vehicle_data(&data, &awake, t0, &cfg);
        assert_eq!(applied.block, Some(SleepBlock::Sentry));
        assert!(matches!(applied.phase, Phase::Awake { idle_since } if idle_since == t0));

        data["vehicle_state"]["sentry_mode"] = json!(false);
        data["vehicle_state"]["df"] = json!(1);
        let applied = apply_vehicle_data(&data, &awake, t0, &cfg);
        assert_eq!(applied.block, Some(SleepBlock::Doors));

        data["vehicle_state"]["df"] = json!(0);
        data["climate_state"]["climate_keeper_mode"] = json!("dog");
        assert_eq!(can_fall_asleep(&data, &cfg), Err(SleepBlock::DogMode));

        data["climate_state"]["climate_keeper_mode"] = json!("off");
        data["drive_state"]["shift_state"] = json!("D");
        let driving = apply_vehicle_data(&data, &awake, t0, &cfg);
        assert_eq!(driving.activity, Activity::Driving);
        assert!(driving.block.is_none());
    }

    #[test]
    fn unlocked_requirement_does_not_reset_the_idle_clock() {
        let cfg = config_for(21, 15, false, true, 30, false);
        let idle_since = Instant::now();
        let t0 = idle_since + Duration::from_secs(20 * 60);
        let mut data = parked();
        data["vehicle_state"]["locked"] = json!(false);
        let applied = apply_vehicle_data(&data, &Phase::Awake { idle_since }, t0, &cfg);
        assert_eq!(applied.block, Some(SleepBlock::Unlocked));
        assert!(matches!(applied.phase, Phase::Awake { idle_since: since } if since == idle_since));
    }

    #[test]
    fn parses_nil_power_separately_from_zero() {
        let nil = parse_stream_csv("0,1,2,3,4,5,6,,P").unwrap();
        assert_eq!(nil.power, None);
        assert_eq!(nil.shift.as_deref(), Some("P"));
        assert!(!wake_is_real(&nil));
        let zero = parse_stream_csv("0,1,2,3,4,5,6,0,P").unwrap();
        assert_eq!(zero.power, Some(0.0));
        assert!(wake_is_real(&zero));
        assert!(!suspend_is_active(&zero));
    }
}
