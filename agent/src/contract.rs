use serde::{Deserialize, Serialize};

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Presence<'a> {
    pub schema: &'static str,
    pub session_id: &'a str,
    pub status: PresenceStatus,
    pub agent_version: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceStatus {
    Online,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry<'a> {
    pub schema: &'static str,
    pub ts: u64,
    pub session_id: &'a str,
    pub seq: u64,
    pub battery_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RobotState<'a> {
    pub schema: &'static str,
    pub ts: u64,
    pub session_id: &'a str,
    pub seq: u64,
    pub mode: String,
    pub execution_state: ExecutionState,
    pub available_commands: Vec<AvailableCommand>,
    pub fault_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraStatus<'a> {
    pub schema: &'static str,
    pub ts: u64,
    pub session_id: &'a str,
    pub seq: u64,
    pub status: CameraState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionState {
    Idle,
    Ready,
    Running,
    Paused,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AvailableCommand {
    Pause,
    Stop,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraState {
    Unavailable,
    Idle,
    Publishing,
    Error,
}

impl<'a> Presence<'a> {
    pub fn new(session_id: &'a str, status: PresenceStatus) -> Self {
        Self {
            schema: "robot.presence.v1",
            session_id,
            status,
            agent_version: AGENT_VERSION,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Topics {
    pub root: String,
    pub telemetry: String,
    pub state: String,
    pub presence: String,
    pub camera_status: String,
}

impl Topics {
    pub fn new(robot_id: &str) -> Self {
        let root = format!("fleetly/v2/robots/{robot_id}");
        Self {
            root: root.clone(),
            telemetry: format!("{root}/telemetry"),
            state: format!("{root}/state"),
            presence: format!("{root}/presence"),
            camera_status: format!("{root}/camera/status"),
        }
    }

    pub fn event(&self, event_type: &str) -> String {
        format!("{}/events/{event_type}", self.root)
    }

    pub fn command_result(&self, command_id: &str) -> String {
        format!("{}/command-results/{command_id}", self.root)
    }

    pub fn commands_filter(&self) -> String {
        format!("{}/commands/#", self.root)
    }

    pub fn supervision(&self) -> String {
        format!("{}/supervision", self.root)
    }

    pub fn media_control(&self) -> String {
        format!("{}/media/control", self.root)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn presence_has_no_payload_identity() {
        let value =
            serde_json::to_value(Presence::new("ses-test", PresenceStatus::Online)).unwrap();
        assert_eq!(
            value,
            json!({
                "schema": "robot.presence.v1",
                "session_id": "ses-test",
                "status": "online",
                "agent_version": AGENT_VERSION
            })
        );
        assert!(value.get("robot_id").is_none());
    }

    #[test]
    fn topics_derive_identity_from_route() {
        let topics = Topics::new("robot1");
        assert_eq!(topics.telemetry, "fleetly/v2/robots/robot1/telemetry");
        assert_eq!(topics.state, "fleetly/v2/robots/robot1/state");
        assert_eq!(topics.presence, "fleetly/v2/robots/robot1/presence");
    }

    #[test]
    fn telemetry_and_state_do_not_repeat_robot_identity() {
        let telemetry = Telemetry {
            schema: "robot.telemetry.v1",
            ts: 1_790_606_898_000,
            session_id: "ses-test",
            seq: 7,
            battery_pct: 72.0,
            temperature_c: Some(18.5),
            location: Location {
                known: true,
                name: Some("Test Field".into()),
                lat: Some(51.21),
                lon: Some(5.52),
            },
        };
        let state = RobotState {
            schema: "robot.state.v1",
            ts: 1_790_606_898_000,
            session_id: "ses-test",
            seq: 3,
            mode: "driving".into(),
            execution_state: ExecutionState::Running,
            available_commands: Vec::new(),
            fault_codes: Vec::new(),
        };

        let telemetry = serde_json::to_value(telemetry).unwrap();
        let state = serde_json::to_value(state).unwrap();
        assert_eq!(telemetry["schema"], "robot.telemetry.v1");
        assert_eq!(telemetry["ts"], 1_790_606_898_000_u64);
        assert_eq!(state["schema"], "robot.state.v1");
        assert_eq!(state["available_commands"], json!([]));
        assert!(telemetry.get("robot_id").is_none());
        assert!(state.get("robot_id").is_none());
    }
}
