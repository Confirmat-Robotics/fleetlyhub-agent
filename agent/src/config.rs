use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

pub const DEFAULT_HUB: &str = "https://api.fleetlyhub.eu:8443";

#[derive(Debug, Parser)]
#[command(name = "fleetlyhub-agent", version, about)]
pub struct Cli {
    /// Installation credential directory, never a robot identity configuration.
    #[arg(long, global = true, default_value = default_state_dir())]
    pub state_dir: PathBuf,
    /// Packaged enrollment defaults. Explicit --hub and --ca take precedence.
    #[arg(long, global = true)]
    pub deployment_config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<AgentCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Generate local credentials and request administrator approval.
    Enroll {
        #[arg(long)]
        hub: Option<String>,
        #[arg(long)]
        ca: Option<PathBuf>,
        /// Archive local credentials and generate a new installation key.
        /// First revoke the previous installation on the Fleetly Hub website.
        #[arg(long)]
        replace: bool,
    },
}

fn default_state_dir() -> &'static str {
    option_env!("FLEETLY_DEFAULT_STATE_DIR").unwrap_or(".local/enrolled")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    pub hub: Option<String>,
    /// None uses normal public HTTPS trust. This is independent of the MQTT CA.
    pub ca: Option<PathBuf>,
}

impl DeploymentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(
            &fs::read(path)
                .with_context(|| format!("Cannot read deployment defaults {}", path.display()))?,
        )
        .context("Invalid deployment defaults")?;
        if config.ca.as_ref().is_some_and(|ca| !ca.is_absolute()) {
            bail!("The deployment CA path must be absolute");
        }
        Ok(config)
    }
}

impl Cli {
    pub fn enrollment_defaults(
        &self,
        hub: Option<String>,
        ca: Option<PathBuf>,
    ) -> Result<(String, Option<PathBuf>)> {
        // A Hub override must not inherit another deployment's private CA.
        if let Some(hub) = hub {
            return Ok((hub, ca));
        }
        let path = self
            .deployment_config
            .clone()
            .or_else(|| option_env!("FLEETLY_DEPLOYMENT_CONFIG").map(PathBuf::from));
        let defaults = match path {
            Some(path) => DeploymentConfig::load(&path)?,
            None => DeploymentConfig::default(),
        };
        match defaults.hub {
            Some(hub) => Ok((hub, ca.or(defaults.ca))),
            // Old packages may retain null deployment defaults on upgrade.
            // Do not carry a private deployment CA into the public fallback.
            None => Ok((DEFAULT_HUB.to_owned(), ca)),
        }
    }
}

/// Resolved only from a certificate-authenticated Hub bootstrap, never a file.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub robot_id: String,
    pub robot_name: String,
    pub mqtt: MqttConfig,
    pub telemetry_interval_ms: u64,
    pub local: LocalConfig,
    pub media: MediaConfig,
    pub event_outbox: Option<crate::event_outbox::Shared>,
}

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub ca_certificate: PathBuf,
    pub client_certificate: PathBuf,
    pub private_key: PathBuf,
    pub keep_alive_seconds: u64,
}

/// Optional local process settings contain no Hub-assigned identity or endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSettings {
    pub telemetry_interval_ms: u64,
    pub local: LocalConfig,
    pub media: MediaConfig,
    pub renewal: RenewalSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenewalSettings {
    pub check_interval_seconds: u64,
    pub initial_retry_seconds: u64,
    pub max_retry_seconds: u64,
}

impl Default for RenewalSettings {
    fn default() -> Self {
        Self {
            check_interval_seconds: 3600,
            initial_retry_seconds: 60,
            max_retry_seconds: 3600,
        }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            telemetry_interval_ms: 1000,
            local: LocalConfig::default(),
            media: MediaConfig::default(),
            renewal: RenewalSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub bind: String,
    pub max_line_bytes: usize,
    pub max_clients: usize,
    pub queue_capacity: usize,
    pub registration_timeout_ms: u64,
    pub clients: std::collections::HashMap<String, ClientRoutes>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRoutes {
    pub interface: String,
    pub publish_routes: Vec<String>,
    pub subscribe_routes: Vec<String>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7001".to_owned(),
            max_line_bytes: 65_536,
            max_clients: 8,
            queue_capacity: 32,
            registration_timeout_ms: 5000,
            clients: [
                (
                    "robot-adapter".to_owned(),
                    ClientRoutes {
                        interface: "robot_adapter".to_owned(),
                        publish_routes: [
                            "telemetry",
                            "state",
                            "events/+",
                            "camera/status",
                            "command-results/+",
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                        subscribe_routes: [
                            "commands/pause",
                            "commands/stop",
                            "commands/resume",
                            "supervision",
                            "media/start",
                            "media/stop",
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                    },
                ),
                (
                    "service-bridge".to_owned(),
                    ClientRoutes {
                        interface: "service_bridge".to_owned(),
                        publish_routes: ["services/status", "services/control-results/+"]
                            .map(str::to_owned)
                            .to_vec(),
                        subscribe_routes: vec!["services/control".to_owned()],
                    },
                ),
            ]
            .into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    pub enabled: bool,
    pub executable: PathBuf,
    pub input_bind: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: option_env!("FLEETLY_MEDIA_EXECUTABLE")
                .unwrap_or("fleetly-media")
                .into(),
            input_bind: "127.0.0.1:7002".to_owned(),
        }
    }
}

impl AgentSettings {
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("settings.json");
        let settings: Self = if path.is_file() {
            serde_json::from_slice(&fs::read(&path)?).context(
                "Invalid local settings.json; robot identity and MQTT settings belong to the Hub",
            )?
        } else {
            Self::default()
        };
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        if !(1..=86400).contains(&self.renewal.check_interval_seconds)
            || !(1..=3600).contains(&self.renewal.initial_retry_seconds)
            || !(self.renewal.initial_retry_seconds..=3600)
                .contains(&self.renewal.max_retry_seconds)
        {
            bail!("Invalid renewal check/retry interval");
        }
        if self.telemetry_interval_ms < 100 {
            bail!("telemetry_interval_ms must be at least 100");
        }
        let bind = self
            .local
            .bind
            .parse::<std::net::SocketAddr>()
            .context("Invalid local.bind socket address")?;
        if !bind.ip().is_loopback() {
            bail!("local.bind must use a loopback address");
        }
        if !(1024..=65_536).contains(&self.local.max_line_bytes) {
            bail!("local.max_line_bytes must be between 1024 and 65536");
        }
        if !(2..=32).contains(&self.local.max_clients)
            || !(1..=256).contains(&self.local.queue_capacity)
            || !(100..=30000).contains(&self.local.registration_timeout_ms)
        {
            bail!("Invalid local client, queue or registration timeout limit");
        }
        for (id, policy) in &self.local.clients {
            crate::local_protocol::validate_identifier(id, 64)?;
            if !matches!(
                policy.interface.as_str(),
                "robot_adapter" | "service_bridge"
            ) {
                bail!("Unknown local interface");
            }
            crate::local_protocol::validate_policy(policy)?;
        }
        let media_bind = self
            .media
            .input_bind
            .parse::<std::net::SocketAddr>()
            .context("Invalid media.input_bind socket address")?;
        if !media_bind.ip().is_loopback() || bind == media_bind {
            bail!("media.input_bind must use a different loopback port");
        }
        if self.media.executable.as_os_str().is_empty() {
            bail!("media.executable must not be empty");
        }
        Ok(())
    }
}

pub fn validate_robot_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        })
    {
        bail!("Hub robot ID must match ^[a-z0-9][a-z0-9-]{{0,63}}$");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_uses_defaults_and_does_not_inherit_trust_for_another_hub() {
        let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory).unwrap();
        let defaults = directory.join("deployment.json");
        let ca = directory.join("hub-ca.pem");
        fs::write(
            &defaults,
            serde_json::to_vec(&serde_json::json!({
                "hub": "https://hub.example.test", "ca": ca,
            }))
            .unwrap(),
        )
        .unwrap();
        let cli = Cli::parse_from([
            "fleetly-agent",
            "--deployment-config",
            defaults.to_str().unwrap(),
            "enroll",
        ]);
        assert!(matches!(
            cli.command,
            Some(AgentCommand::Enroll {
                hub: None,
                ca: None,
                ..
            })
        ));
        let (hub, trust) = cli.enrollment_defaults(None, None).unwrap();
        assert_eq!(hub, "https://hub.example.test");
        assert_eq!(trust, Some(ca.clone()));
        let (_, trust) = cli
            .enrollment_defaults(Some("https://other.example.test".into()), None)
            .unwrap();
        assert!(trust.is_none());
        fs::write(&defaults, r#"{"hub":null,"ca":null}"#).unwrap();
        assert_eq!(
            cli.enrollment_defaults(None, None).unwrap(),
            (DEFAULT_HUB.into(), None)
        );
        fs::write(
            &defaults,
            serde_json::to_vec(&serde_json::json!({"hub": null, "ca": ca})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cli.enrollment_defaults(None, None).unwrap(),
            (DEFAULT_HUB.into(), None)
        );
        fs::write(
            &defaults,
            r#"{"hub":"https://hub.example.test","ca":"relative.pem"}"#,
        )
        .unwrap();
        assert!(DeploymentConfig::load(&defaults).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn media_is_enabled_without_local_configuration() {
        let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory).unwrap();
        assert!(AgentSettings::load(&directory).unwrap().media.enabled);
        fs::write(directory.join("settings.json"), r#"{"media":{}}"#).unwrap();
        assert!(AgentSettings::load(&directory).unwrap().media.enabled);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_locally_configured_identity_and_mqtt_endpoint() {
        for value in [
            r#"{"robot_id":"robot1"}"#,
            r#"{"mqtt":{"host":"other-host"}}"#,
            r#"{"robot_name":"other-robot"}"#,
        ] {
            assert!(serde_json::from_str::<AgentSettings>(value).is_err());
        }
    }

    #[test]
    fn rejects_network_exposed_local_interfaces() {
        let mut settings = AgentSettings::default();
        settings.local.bind = "0.0.0.0:7001".to_owned();
        assert!(settings.validate().is_err());
        settings.local.bind = "127.0.0.1:7001".to_owned();
        settings.media.input_bind = "0.0.0.0:7002".to_owned();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_identity_lookalikes() {
        for value in ["", "Robot1", "robot_1", "-robot1", "robot/1"] {
            assert!(validate_robot_id(value).is_err());
        }
        assert!(validate_robot_id("r-123").is_ok());
    }
}
