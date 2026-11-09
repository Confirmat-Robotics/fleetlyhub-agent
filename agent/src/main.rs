mod config;
mod contract;
mod enrollment;
mod event_outbox;
mod installation;
mod local_gateway;
mod local_protocol;
mod media;
mod mqtt;
mod renewal;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    // Enrollment HTTP and MQTT share the same explicitly selected TLS provider.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Cannot initialize the TLS crypto provider"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fleetlyhub_agent=info,fleetly_agent=info,rumqttc=warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    if let Some(config::AgentCommand::Enroll { hub, ca, replace }) = &cli.command {
        let (hub, ca) = cli.enrollment_defaults(hub.clone(), ca.clone())?;
        return enrollment::run(&hub, ca.as_deref(), &cli.state_dir, *replace).await;
    }
    info!(state_dir = %cli.state_dir.display(), "authenticating enrolled installation with Hub");
    renewal::recover(&cli.state_dir)?;
    let config = installation::bootstrap(&cli.state_dir).await?;

    info!(robot_id = %config.robot_id, robot_name = %config.robot_name,
        mqtt_host = %config.mqtt.host, mqtt_port = config.mqtt.port, "starting Fleetly Hub Agent");
    let (uplink_tx, uplink_rx) = mpsc::channel(128);
    let (downlink_tx, downlink_rx) = mpsc::channel(128);
    let (media_tx, media_rx) = mpsc::channel(16);
    let (local_connected_tx, local_connected_rx) = watch::channel(false);
    let (interfaces_tx, interfaces_rx) = watch::channel(local_gateway::Interfaces::default());
    let local_config = config.local.clone();
    let event_outbox = config.event_outbox.clone();
    let media_config = config.media.clone();
    let media_uplink = uplink_tx.clone();
    let media_downlink = downlink_tx.clone();
    let (certificate_tx, certificate_rx) = watch::channel(0u64);

    tokio::select! {
        result = mqtt::run(config, uplink_rx, downlink_tx, media_tx, local_connected_rx, interfaces_rx, certificate_rx) => result,
        result = renewal::run(&cli.state_dir, certificate_tx) => result,
        result = local_gateway::serve(local_config, uplink_tx, downlink_rx, local_connected_tx, interfaces_tx, event_outbox) => {
            warn!("local Robot-Agent server stopped");
            result
        }
        result = media::run(media_config, media_rx, media_downlink, media_uplink) => {
            warn!("media supervisor stopped");
            result
        }
    }
}
