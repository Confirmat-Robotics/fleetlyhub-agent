use std::collections::HashSet;

use crate::contract::{AvailableCommand, CameraState, ExecutionState, Location};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const LOCAL_PROTOCOL: &str = "fleetly.local.v1";

#[derive(Debug)]
pub enum RobotMessage {
    ServicesStatus(ServicesStatusInput),
    ServiceControlResult {
        command_id: String,
        payload: CommandResultInput,
    },
    Telemetry(TelemetryInput),
    State(StateInput),
    Event {
        event_type: String,
        payload: EventInput,
    },
    CameraStatus(CameraStatusInput),
    CommandResult {
        command_id: String,
        payload: CommandResultInput,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryInput {
    pub schema: String,
    pub ts: u64,
    pub battery_pct: f64,
    pub temperature_c: Option<f64>,
    pub location: Location,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateInput {
    pub schema: String,
    pub ts: u64,
    pub mode: String,
    pub execution_state: ExecutionState,
    pub available_commands: Vec<AvailableCommand>,
    #[serde(default)]
    pub fault_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventInput {
    pub schema: String,
    pub event_id: String,
    pub ts: u64,
    pub severity: EventSeverity,
    pub source: String,
    #[serde(
        default,
        deserialize_with = "optional_active",
        skip_serializing_if = "Option::is_none"
    )]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_acknowledgement: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

fn optional_active<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraStatusInput {
    pub schema: String,
    pub ts: u64,
    pub status: CameraState,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResultInput {
    pub schema: String,
    pub ts: u64,
    pub result_seq: u64,
    pub status: CommandResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandResultStatus {
    Received,
    Accepted,
    Running,
    Succeeded,
    Rejected,
    Failed,
    Expired,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingEnvelope {
    protocol: String,
    topic: String,
    payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingEnvelope {
    pub protocol: &'static str,
    pub topic: String,
    pub payload: Value,
}

impl OutgoingEnvelope {
    pub fn new(topic: impl Into<String>, payload: Value) -> Self {
        Self {
            protocol: LOCAL_PROTOCOL,
            topic: topic.into(),
            payload,
        }
    }
}

pub(crate) fn parse_robot_message(line: &str) -> Result<RobotMessage> {
    let envelope: IncomingEnvelope =
        serde_json::from_str(line).context("local envelope is not valid JSON")?;
    anyhow::ensure!(
        envelope.protocol == LOCAL_PROTOCOL,
        "unsupported local protocol {:?}",
        envelope.protocol
    );

    match envelope.topic.as_str() {
        "services/status" => {
            let input: ServicesStatusInput =
                serde_json::from_value(envelope.payload).context("invalid service status")?;
            validate_services(&input)?;
            Ok(RobotMessage::ServicesStatus(input))
        }
        topic if topic.starts_with("services/control-results/") => {
            let command_id = &topic["services/control-results/".len()..];
            validate_identifier(command_id, 128)?;
            let payload: CommandResultInput = serde_json::from_value(envelope.payload)?;
            anyhow::ensure!(
                payload.schema == "robot.service-control-result.v1" && payload.result.is_none(),
                "invalid service result schema"
            );
            anyhow::ensure!(
                payload.ts > 0
                    && payload.ts <= 9_007_199_254_740_991
                    && payload.result_seq <= 9_007_199_254_740_991,
                "invalid service result numbers"
            );
            anyhow::ensure!(
                payload
                    .reason
                    .as_ref()
                    .is_none_or(|r| r.chars().count() <= 512),
                "invalid service result reason"
            );
            Ok(RobotMessage::ServiceControlResult {
                command_id: command_id.to_owned(),
                payload,
            })
        }
        "telemetry" => {
            let payload: TelemetryInput = serde_json::from_value(envelope.payload)
                .context("invalid local telemetry payload")?;
            validate_telemetry(&payload)?;
            Ok(RobotMessage::Telemetry(payload))
        }
        "state" => {
            let payload: StateInput =
                serde_json::from_value(envelope.payload).context("invalid local state payload")?;
            validate_state(&payload)?;
            Ok(RobotMessage::State(payload))
        }
        topic if topic.starts_with("events/") => {
            let event_type = topic.trim_start_matches("events/");
            validate_identifier(event_type, 64).context("invalid event route")?;
            let payload: EventInput =
                serde_json::from_value(envelope.payload).context("invalid local event payload")?;
            validate_event(&payload)?;
            Ok(RobotMessage::Event {
                event_type: event_type.to_owned(),
                payload,
            })
        }
        "camera/status" => {
            let payload: CameraStatusInput = serde_json::from_value(envelope.payload)
                .context("invalid local camera status payload")?;
            validate_camera_status(&payload)?;
            Ok(RobotMessage::CameraStatus(payload))
        }
        topic if topic.starts_with("command-results/") => {
            let command_id = topic.trim_start_matches("command-results/");
            validate_identifier(command_id, 128).context("invalid command result route")?;
            let payload: CommandResultInput = serde_json::from_value(envelope.payload)
                .context("invalid local command result payload")?;
            validate_command_result(&payload)?;
            Ok(RobotMessage::CommandResult {
                command_id: command_id.to_owned(),
                payload,
            })
        }
        _ => anyhow::bail!("local route {:?} is not supported", envelope.topic),
    }
}

fn validate_telemetry(payload: &TelemetryInput) -> Result<()> {
    anyhow::ensure!(
        payload.schema == "robot.telemetry.v1",
        "telemetry schema must be robot.telemetry.v1"
    );
    anyhow::ensure!(payload.ts > 0, "telemetry ts must be positive");
    anyhow::ensure!(
        (0.0..=100.0).contains(&payload.battery_pct),
        "battery_pct must be between 0 and 100"
    );
    if let Some(temperature) = payload.temperature_c {
        anyhow::ensure!(
            (-100.0..=200.0).contains(&temperature),
            "temperature_c is outside its contract range"
        );
    }
    validate_location(&payload.location)
}

fn validate_state(payload: &StateInput) -> Result<()> {
    anyhow::ensure!(
        payload.schema == "robot.state.v1",
        "state schema must be robot.state.v1"
    );
    anyhow::ensure!(payload.ts > 0, "state ts must be positive");
    anyhow::ensure!(
        !payload.mode.is_empty() && payload.mode.len() <= 64,
        "mode must contain 1 to 64 characters"
    );
    anyhow::ensure!(
        payload.available_commands.len() <= 3,
        "available_commands has too many values"
    );
    let mut commands = payload.available_commands.clone();
    commands.sort_by_key(|value| *value as u8);
    commands.dedup();
    anyhow::ensure!(
        commands.len() == payload.available_commands.len(),
        "available_commands contains duplicates"
    );
    anyhow::ensure!(
        payload.fault_codes.len() <= 32,
        "fault_codes has too many values"
    );
    for code in &payload.fault_codes {
        validate_identifier(code, 64).context("invalid fault code")?;
    }
    let unique_faults: HashSet<_> = payload.fault_codes.iter().collect();
    anyhow::ensure!(
        unique_faults.len() == payload.fault_codes.len(),
        "fault_codes contains duplicates"
    );
    Ok(())
}

fn validate_event(payload: &EventInput) -> Result<()> {
    anyhow::ensure!(
        serde_json::to_vec(payload)?.len() <= 32768,
        "event exceeds the Hub's 32 KiB event limit"
    );
    anyhow::ensure!(
        payload.schema == "robot.event.v1",
        "event schema must be robot.event.v1"
    );
    validate_identifier(&payload.event_id, 128).context("invalid event_id")?;
    anyhow::ensure!(payload.ts > 0, "event ts must be positive");
    anyhow::ensure!(
        !payload.source.is_empty() && payload.source.len() <= 128,
        "event source must contain 1 to 128 characters"
    );
    if let Some(message) = &payload.message {
        anyhow::ensure!(message.len() <= 1_024, "event message is too long");
    }
    if let Some(details) = &payload.details {
        anyhow::ensure!(details.len() <= 32, "event details has too many properties");
        validate_no_identity_fields(&Value::Object(details.clone()))?;
    }
    Ok(())
}

fn validate_camera_status(payload: &CameraStatusInput) -> Result<()> {
    anyhow::ensure!(
        payload.schema == "robot.camera-status.v1",
        "camera status schema must be robot.camera-status.v1"
    );
    anyhow::ensure!(payload.ts > 0, "camera status ts must be positive");
    match (payload.status, &payload.error_code) {
        (CameraState::Error, Some(code)) => {
            validate_identifier(code, 64).context("invalid camera error_code")
        }
        (CameraState::Error, None) => anyhow::bail!("camera error status requires error_code"),
        (_, Some(_)) => anyhow::bail!("camera error_code is only valid for error status"),
        (_, None) => Ok(()),
    }
}

fn validate_command_result(payload: &CommandResultInput) -> Result<()> {
    anyhow::ensure!(
        payload.schema == "robot.command-result.v1",
        "command result schema must be robot.command-result.v1"
    );
    anyhow::ensure!(payload.ts > 0, "command result ts must be positive");
    if let Some(reason) = &payload.reason {
        anyhow::ensure!(reason.len() <= 512, "command result reason is too long");
    }
    if let Some(result) = &payload.result {
        anyhow::ensure!(
            result.len() <= 32,
            "command result has too many result properties"
        );
        validate_no_identity_fields(&Value::Object(result.clone()))?;
    }
    Ok(())
}

fn validate_location(location: &Location) -> Result<()> {
    if location.known {
        anyhow::ensure!(
            location.name.is_some() || (location.lat.is_some() && location.lon.is_some()),
            "known location requires a name or coordinates"
        );
    } else {
        anyhow::ensure!(
            location.name.is_none() && location.lat.is_none() && location.lon.is_none(),
            "unknown location must omit name and coordinates"
        );
    }
    if let Some(name) = &location.name {
        anyhow::ensure!(
            !name.is_empty() && name.len() <= 128,
            "location name must contain 1 to 128 characters"
        );
    }
    match (location.lat, location.lon) {
        (Some(lat), Some(lon)) => {
            anyhow::ensure!(
                (-90.0..=90.0).contains(&lat),
                "location latitude is invalid"
            );
            anyhow::ensure!(
                (-180.0..=180.0).contains(&lon),
                "location longitude is invalid"
            );
        }
        (None, None) => {}
        _ => anyhow::bail!("location coordinates require both lat and lon"),
    }
    Ok(())
}

pub(crate) fn validate_identifier(value: &str, max_length: usize) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= max_length
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        });
    anyhow::ensure!(valid, "identifier has invalid syntax");
    Ok(())
}

pub(crate) fn validate_no_identity_fields(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                anyhow::ensure!(
                    !matches!(
                        key.as_str(),
                        "robot_id"
                            | "tenant_id"
                            | "organization_id"
                            | "org_id"
                            | "user_id"
                            | "issued_by_user_id"
                            | "email"
                            | "role"
                            | "topic"
                            | "session_id"
                            | "seq"
                    ),
                    "local payload contains forbidden identity or transport field {key}"
                );
                validate_no_identity_fields(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_no_identity_fields(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published_contract() -> std::path::PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let local = root.join("contracts/local/v1");
        if local.exists() {
            local
        } else {
            root.parent().unwrap().join("contracts/local/v1")
        }
    }

    #[test]
    fn published_domain_examples_are_accepted_by_the_real_parser() {
        let directory = published_contract();
        let mut checked = 0;
        for entry in std::fs::read_dir(directory.join("examples")).unwrap() {
            let path = entry.unwrap().path();
            let line = std::fs::read_to_string(&path).unwrap();
            let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
            let topic = envelope["topic"].as_str().unwrap();
            if topic == "telemetry"
                || topic == "state"
                || topic == "camera/status"
                || topic == "services/status"
                || topic.starts_with("events/")
                || topic.starts_with("command-results/")
                || topic.starts_with("services/control-results/")
            {
                assert!(parse_robot_message(&line).is_ok(), "{}", path.display());
                checked += 1;
            }
        }
        assert!(checked >= 14);
        let cases: Vec<serde_json::Value> = serde_json::from_str(
            &std::fs::read_to_string(directory.join("invalid-examples.json")).unwrap(),
        )
        .unwrap();
        for case in cases {
            if case["implementation_rejects"] == true {
                assert!(
                    parse_robot_message(&case["envelope"].to_string()).is_err(),
                    "{}",
                    case["name"]
                );
            }
        }
    }

    #[test]
    fn accepts_telemetry_projection_without_transport_fields() {
        let message = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"telemetry","payload":{"schema":"robot.telemetry.v1","ts":1790606898000,"battery_pct":72.0,"temperature_c":18.5,"location":{"known":false}}}"#,
        ).unwrap();
        assert!(matches!(message, RobotMessage::Telemetry(_)));
    }

    #[test]
    fn rejects_agent_owned_fields_from_robot() {
        let error = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"telemetry","payload":{"schema":"robot.telemetry.v1","ts":1790606898000,"session_id":"fake","seq":1,"battery_pct":72.0,"location":{"known":false}}}"#,
        ).unwrap_err();
        assert!(error.to_string().contains("invalid local telemetry"));
    }

    #[test]
    fn rejects_arbitrary_routes() {
        let error = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"fleetly/v2/robots/other/telemetry","payload":{}}"#,
        ).unwrap_err();
        assert!(error.to_string().contains("not supported"));
    }

    #[test]
    fn accepts_event_and_camera_status_projections() {
        let event = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"events/low_battery","payload":{"schema":"robot.event.v1","event_id":"battery-1","ts":1790606898000,"severity":"warning","source":"robot-controller","active":true}}"#,
        )
        .unwrap();
        assert!(matches!(event, RobotMessage::Event { .. }));

        let camera = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"camera/status","payload":{"schema":"robot.camera-status.v1","ts":1790606898000,"status":"idle"}}"#,
        )
        .unwrap();
        assert!(matches!(camera, RobotMessage::CameraStatus(_)));
    }

    #[test]
    fn event_active_is_optional_but_not_null() {
        let base = serde_json::json!({"protocol":"fleetly.local.v1","topic":"events/sensor",
            "payload":{"schema":"robot.event.v1","event_id":"sensor-1","ts":1790606898000_u64,
                "severity":"warning","source":"robot-controller"}});
        if let RobotMessage::Event { payload, .. } = parse_robot_message(&base.to_string()).unwrap()
        {
            assert!(payload.active.is_none());
            assert!(
                serde_json::to_value(payload)
                    .unwrap()
                    .get("active")
                    .is_none()
            );
        } else {
            panic!("Expected event");
        }
        for active in [
            serde_json::Value::Null,
            serde_json::json!("true"),
            serde_json::json!(1),
        ] {
            let mut invalid = base.clone();
            invalid["payload"]["active"] = active;
            assert!(parse_robot_message(&invalid.to_string()).is_err());
        }
    }

    #[test]
    fn rejects_camera_transport_fields_and_invalid_error_state() {
        for message in [
            r#"{"protocol":"fleetly.local.v1","topic":"camera/status","payload":{"schema":"robot.camera-status.v1","ts":1790606898000,"session_id":"fake","seq":1,"status":"idle"}}"#,
            r#"{"protocol":"fleetly.local.v1","topic":"camera/status","payload":{"schema":"robot.camera-status.v1","ts":1790606898000,"status":"error"}}"#,
        ] {
            assert!(parse_robot_message(message).is_err());
        }
    }

    #[test]
    fn remote_configuration_routes_are_not_implemented() {
        for route in ["config/desired", "config/reported"] {
            let message =
                format!(r#"{{"protocol":"fleetly.local.v1","topic":"{route}","payload":{{}}}}"#);
            let error = parse_robot_message(&message).unwrap_err();
            assert!(error.to_string().contains("not supported"));
        }
    }

    #[test]
    fn state_uses_pause_resume_and_never_start() {
        for (execution, commands) in [
            ("running", serde_json::json!(["stop", "pause"])),
            ("paused", serde_json::json!(["stop", "resume"])),
        ] {
            let message = serde_json::json!({"protocol":"fleetly.local.v1", "topic":"state", "payload":{
                "schema":"robot.state.v1", "ts":1790606898000_u64, "mode":"navigation",
                "execution_state":execution, "available_commands":commands, "fault_codes":[]}});
            assert!(parse_robot_message(&message.to_string()).is_ok());
            let mut invalid = message;
            invalid["payload"]["available_commands"] = serde_json::json!(["start"]);
            assert!(parse_robot_message(&invalid.to_string()).is_err());
        }
    }

    #[test]
    fn optional_command_result_fields_are_omitted() {
        let message = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"command-results/cmd-test","payload":{"schema":"robot.command-result.v1","ts":1790606898000,"result_seq":0,"status":"accepted"}}"#,
        )
        .unwrap();
        let RobotMessage::CommandResult { payload, .. } = message else {
            panic!("expected command result");
        };
        let value = serde_json::to_value(payload).unwrap();
        assert!(value.get("reason").is_none());
        assert!(value.get("result").is_none());
    }

    #[test]
    fn rejects_identity_hidden_in_flexible_result_data() {
        let error = parse_robot_message(
            r#"{"protocol":"fleetly.local.v1","topic":"command-results/cmd-test","payload":{"schema":"robot.command-result.v1","ts":1790606898000,"result_seq":1,"status":"succeeded","result":{"metadata":{"robot_id":"other"}}}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("forbidden identity"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesStatusInput {
    pub schema: String,
    pub ts: u64,
    pub services: Vec<ServiceStatus>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub service: String,
    pub status: String,
    pub substatus: String,
    pub available_actions: Vec<String>,
}
pub(crate) fn validate_services(input: &ServicesStatusInput) -> Result<()> {
    anyhow::ensure!(
        input.schema == "robot.services-status.v1" && input.ts > 0 && input.ts <= 9007199254740991,
        "Invalid service schema or timestamp"
    );
    anyhow::ensure!(input.services.len() <= 64, "Too many services");
    let mut names = HashSet::new();
    for service in &input.services {
        let name = &service.service;
        anyhow::ensure!(
            name.ends_with(".service")
                && name.len() <= 128
                && name.len() > 8
                && name
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| b.is_ascii_alphanumeric()
                        || (i > 0 && matches!(b, b'.' | b'_' | b'@' | b':' | b'-'))),
            "Invalid full service unit name"
        );
        anyhow::ensure!(names.insert(name), "Duplicate service unit");
        for (value, max) in [(&service.status, 32), (&service.substatus, 64)] {
            anyhow::ensure!(
                !value.is_empty()
                    && value.len() <= max
                    && value
                        .bytes()
                        .enumerate()
                        .all(|(i, b)| b.is_ascii_lowercase() || (i > 0 && b == b'-')),
                "Invalid systemd state"
            );
        }
        anyhow::ensure!(
            service.available_actions.len() <= 3
                && service
                    .available_actions
                    .iter()
                    .collect::<HashSet<_>>()
                    .len()
                    == service.available_actions.len()
                && service
                    .available_actions
                    .iter()
                    .all(|a| matches!(a.as_str(), "start" | "stop" | "restart")),
            "Invalid service actions"
        );
    }
    anyhow::ensure!(
        serde_json::to_vec(input)?.len() <= 16000,
        "Service payload too large"
    );
    Ok(())
}
pub(crate) fn validate_policy(policy: &crate::config::ClientRoutes) -> Result<()> {
    let pub_allowed: &[&str] = if policy.interface == "robot_adapter" {
        &[
            "telemetry",
            "state",
            "events/+",
            "camera/status",
            "command-results/+",
        ]
    } else {
        &["services/status", "services/control-results/+"]
    };
    let sub_allowed: &[&str] = if policy.interface == "robot_adapter" {
        &[
            "commands/pause",
            "commands/stop",
            "commands/resume",
            "supervision",
            "media/start",
            "media/stop",
        ]
    } else {
        &["services/control"]
    };
    for (routes, allowed) in [
        (&policy.publish_routes, pub_allowed),
        (&policy.subscribe_routes, sub_allowed),
    ] {
        anyhow::ensure!(
            routes.len() <= 16
                && routes.iter().collect::<HashSet<_>>().len() == routes.len()
                && routes.iter().all(|r| allowed.contains(&r.as_str())),
            "Invalid per-client route allowlist"
        );
    }
    Ok(())
}
