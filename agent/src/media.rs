use std::{
    net::SocketAddr,
    process::Stdio,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    time::timeout,
};
use tracing::{info, warn};

use crate::{
    config::MediaConfig,
    contract::CameraState,
    local_protocol::{CameraStatusInput, OutgoingEnvelope, RobotMessage},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaControl {
    pub schema: String,
    pub action: MediaAction,
    pub publication_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub livekit_url: Option<String>,
    pub participant_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaAction {
    Start,
    Stop,
    Renew,
}

#[derive(Serialize)]
struct MediaGrant<'a> {
    schema: &'static str,
    publication_id: &'a str,
    livekit_url: &'a str,
    participant_token: &'a str,
    expires_at: u64,
}

struct ActiveMedia {
    publication_id: String,
    child: Child,
    control: BufWriter<ChildStdin>,
    expires_at: u64,
    livekit_url: String,
}

pub async fn run(
    config: MediaConfig,
    mut commands: mpsc::Receiver<MediaControl>,
    local_downlink: mpsc::Sender<OutgoingEnvelope>,
    mqtt_uplink: mpsc::Sender<RobotMessage>,
) -> Result<()> {
    let input_bind: SocketAddr = config
        .input_bind
        .parse()
        .context("validated media input bind became invalid")?;
    let mut active: Option<ActiveMedia> = None;
    publish_status(&mqtt_uplink, CameraState::Idle, None).await;

    loop {
        if let Some(media) = active.as_mut() {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        stop_active(&mut active).await;
                        return Ok(());
                    };
                    handle_command(
                        &config, input_bind, command, &mut active,
                        &local_downlink, &mqtt_uplink,
                    ).await;
                }
                result = media.child.wait() => {
                    let id = media.publication_id.clone();
                    match result {
                        Ok(status) if status.success() => {
                            info!(publication_id = %id, "media process ended");
                            publish_status(&mqtt_uplink, CameraState::Idle, None).await;
                        }
                        Ok(status) => {
                            warn!(publication_id = %id, %status, "media process failed");
                            publish_status(&mqtt_uplink, CameraState::Error, Some("media_process_failed")).await;
                        }
                        Err(error) => {
                            warn!(publication_id = %id, %error, "could not wait for media process");
                            publish_status(&mqtt_uplink, CameraState::Error, Some("media_process_wait_failed")).await;
                        }
                    }
                    active = None;
                    send_local_stop(&local_downlink, &id).await;
                }
            }
        } else {
            let Some(command) = commands.recv().await else {
                return Ok(());
            };
            handle_command(
                &config,
                input_bind,
                command,
                &mut active,
                &local_downlink,
                &mqtt_uplink,
            )
            .await;
        }
    }
}

pub fn validate_control(control: &MediaControl) -> Result<()> {
    if control.schema != "fleetly.media-control.v2" {
        bail!("media control schema must be fleetly.media-control.v2");
    }
    if control.publication_id.is_empty() || control.publication_id.len() > 128 {
        bail!("invalid publication_id");
    }
    if control.issued_at == 0 || control.expires_at == 0 || control.issued_at > control.expires_at {
        bail!("invalid media control timestamps");
    }
    match control.action {
        MediaAction::Start | MediaAction::Renew => {
            let url = control.livekit_url.as_deref().unwrap_or_default();
            let token = control.participant_token.as_deref().unwrap_or_default();
            if (!url.starts_with("wss://") && !url.starts_with("ws://"))
                || url
                    .split_once("://")
                    .is_none_or(|(_, host)| host.is_empty())
            {
                bail!("media start requires a valid LiveKit ws or wss URL");
            }
            if token.is_empty() || token.len() > 16 * 1024 {
                bail!("media start requires a valid participant token");
            }
            if control.expires_at <= unix_time_ms() {
                bail!("media start has expired");
            }
        }
        MediaAction::Stop => {
            if control.livekit_url.is_some() || control.participant_token.is_some() {
                bail!("media stop must not include publishing credentials");
            }
        }
    }
    Ok(())
}

async fn handle_command(
    config: &MediaConfig,
    input_bind: SocketAddr,
    control: MediaControl,
    active: &mut Option<ActiveMedia>,
    local_downlink: &mpsc::Sender<OutgoingEnvelope>,
    mqtt_uplink: &mpsc::Sender<RobotMessage>,
) {
    if let Err(error) = validate_control(&control) {
        warn!(%error, "ignored invalid media control message");
        return;
    }

    match control.action {
        MediaAction::Start => {
            // MQTT QoS 1 can redeliver start. Reusing the same publication must
            // never restart LiveKit or reopen the physical camera.
            if active
                .as_ref()
                .is_some_and(|media| media.publication_id == control.publication_id)
            {
                return;
            }
            stop_active(active).await;
            if !config.enabled {
                warn!("media start ignored because media integration is disabled");
                publish_status(mqtt_uplink, CameraState::Unavailable, None).await;
                return;
            }
            match start_media(config, input_bind, &control).await {
                Ok(media) => {
                    info!(publication_id = %control.publication_id, %input_bind, "media publication started");
                    *active = Some(media);
                    publish_status(mqtt_uplink, CameraState::Publishing, None).await;
                    send_local_start(local_downlink, input_bind, &control).await;
                }
                Err(error) => {
                    warn!(publication_id = %control.publication_id, %error, "failed to start media process");
                    publish_status(mqtt_uplink, CameraState::Error, Some("media_start_failed"))
                        .await;
                }
            }
        }
        MediaAction::Renew => {
            if let Some(media) = active.as_mut() {
                if media.publication_id != control.publication_id
                    || media.livekit_url != control.livekit_url.as_deref().unwrap_or_default()
                    || media.expires_at <= unix_time_ms()
                    || control.expires_at <= media.expires_at
                {
                    return;
                }
                let grant = MediaGrant {
                    schema: "fleetly.media-grant.v2",
                    publication_id: &control.publication_id,
                    livekit_url: control.livekit_url.as_deref().unwrap_or_default(),
                    participant_token: control.participant_token.as_deref().unwrap_or_default(),
                    expires_at: control.expires_at,
                };
                let delivery = async {
                    media
                        .control
                        .write_all(&serde_json::to_vec(&grant)?)
                        .await?;
                    media.control.write_all(b"\n").await?;
                    media.control.flush().await
                };
                match timeout(Duration::from_secs(2), delivery).await {
                    Ok(Ok(())) => media.expires_at = control.expires_at,
                    _ => {
                        warn!(publication_id = %control.publication_id, "media renewal could not reach publisher")
                    }
                }
            }
        }
        MediaAction::Stop => {
            let matches_active = active
                .as_ref()
                .is_some_and(|media| media.publication_id == control.publication_id);
            if matches_active {
                stop_active(active).await;
                info!(publication_id = %control.publication_id, "media publication stopped");
                publish_status(mqtt_uplink, CameraState::Idle, None).await;
                send_local_stop(local_downlink, &control.publication_id).await;
            }
        }
    }
}

async fn start_media(
    config: &MediaConfig,
    input_bind: SocketAddr,
    control: &MediaControl,
) -> Result<ActiveMedia> {
    let mut child = Command::new(&config.executable)
        .arg("--input-bind")
        .arg(input_bind.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to execute {}", config.executable.display()))?;

    let grant = MediaGrant {
        schema: "fleetly.media-grant.v2",
        publication_id: &control.publication_id,
        livekit_url: control.livekit_url.as_deref().unwrap_or_default(),
        participant_token: control.participant_token.as_deref().unwrap_or_default(),
        expires_at: control.expires_at,
    };
    let stdin = child
        .stdin
        .take()
        .context("fleetly-media stdin was not available")?;
    let mut writer = BufWriter::new(stdin);
    writer
        .write_all(&serde_json::to_vec(&grant)?)
        .await
        .context("failed to send grant to fleetly-media")?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(ActiveMedia {
        publication_id: control.publication_id.clone(),
        child,
        control: writer,
        expires_at: control.expires_at,
        livekit_url: control.livekit_url.clone().unwrap_or_default(),
    })
}

async fn stop_active(active: &mut Option<ActiveMedia>) {
    if let Some(mut media) = active.take() {
        let graceful = async {
            media.control.write_all(b"stop\n").await?;
            media.control.flush().await?;
            media.control.shutdown().await
        }
        .await;
        if let Err(error) = graceful {
            warn!(%error, "failed to request graceful media stop");
        }

        if timeout(Duration::from_secs(3), media.child.wait())
            .await
            .is_err()
        {
            warn!(publication_id = %media.publication_id, "media process did not stop gracefully; terminating it");
            if let Err(error) = media.child.kill().await {
                warn!(%error, "failed to terminate media process");
            }
            let _ = media.child.wait().await;
        }
    }
}

async fn send_local_start(
    downlink: &mpsc::Sender<OutgoingEnvelope>,
    input_bind: SocketAddr,
    control: &MediaControl,
) {
    let payload = serde_json::json!({
        "schema": "fleetly.local-media-control.v2",
        "action": "start",
        "publication_id": control.publication_id,
        "expires_at": control.expires_at,
        "input": {
            "transport": "tcp",
            "host": input_bind.ip().to_string(),
            "port": input_bind.port(),
            "protocol": "fleetly.media.v1",
            "codec": "h264"
        }
    });
    let _ = downlink
        .send(OutgoingEnvelope::new("media/start", payload))
        .await;
}

async fn send_local_stop(downlink: &mpsc::Sender<OutgoingEnvelope>, publication_id: &str) {
    let payload = serde_json::json!({
        "schema": "fleetly.local-media-control.v2",
        "action": "stop",
        "publication_id": publication_id
    });
    let _ = downlink
        .send(OutgoingEnvelope::new("media/stop", payload))
        .await;
}

async fn publish_status(
    uplink: &mpsc::Sender<RobotMessage>,
    status: CameraState,
    error_code: Option<&str>,
) {
    let _ = uplink
        .send(RobotMessage::CameraStatus(CameraStatusInput {
            schema: "robot.camera-status.v1".to_owned(),
            ts: unix_time_ms(),
            status,
            error_code: error_code.map(str::to_owned),
        }))
        .await;
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real child process lets this regression verify that duplicate MQTT
    // control does not kill/restart the publisher or acquire camera resources.
    #[test]
    fn media_child_fixture() {
        if std::env::var_os("FLEETLY_MEDIA_TEST_CHILD").is_some() {
            let mut line = String::new();
            loop {
                line.clear();
                if std::io::stdin().read_line(&mut line).unwrap() == 0 || line.trim() == "stop" {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn duplicate_start_and_unrelated_stop_leave_publisher_running() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "media::tests::media_child_fixture",
                "--nocapture",
            ])
            .env("FLEETLY_MEDIA_TEST_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdin = child.stdin.take().unwrap();
        let mut active = Some(ActiveMedia {
            publication_id: "pub-original".into(),
            child,
            control: BufWriter::new(stdin),
            expires_at: unix_time_ms() + 30_000,
            livekit_url: "wss://fixture.invalid".into(),
        });
        let config = MediaConfig {
            enabled: true,
            executable: "must-not-be-executed".into(),
            input_bind: "127.0.0.1:7002".into(),
        };
        let (local, mut local_messages) = mpsc::channel(8);
        let (uplink, mut statuses) = mpsc::channel(8);
        let mut control = MediaControl {
            schema: "fleetly.media-control.v2".into(),
            action: MediaAction::Start,
            publication_id: "pub-original".into(),
            issued_at: unix_time_ms(),
            expires_at: unix_time_ms() + 60000,
            livekit_url: Some("wss://fixture.invalid".into()),
            participant_token: Some("fixture".into()),
        };
        handle_command(
            &config,
            "127.0.0.1:7002".parse().unwrap(),
            control.clone(),
            &mut active,
            &local,
            &uplink,
        )
        .await;
        assert_eq!(active.as_ref().unwrap().child.id(), pid);
        assert!(statuses.try_recv().is_err());
        control.action = MediaAction::Renew;
        handle_command(
            &config,
            "127.0.0.1:7002".parse().unwrap(),
            control.clone(),
            &mut active,
            &local,
            &uplink,
        )
        .await;
        assert_eq!(active.as_ref().unwrap().child.id(), pid);
        assert_eq!(active.as_ref().unwrap().expires_at, control.expires_at);
        assert!(local_messages.try_recv().is_err());
        let expiry = control.expires_at;
        control.expires_at -= 1000; // delayed older renewal must not shorten the lease
        handle_command(
            &config,
            "127.0.0.1:7002".parse().unwrap(),
            control.clone(),
            &mut active,
            &local,
            &uplink,
        )
        .await;
        assert_eq!(active.as_ref().unwrap().expires_at, expiry);
        control.action = MediaAction::Stop;
        control.publication_id = "another-publication".into();
        control.livekit_url = None;
        control.participant_token = None;
        handle_command(
            &config,
            "127.0.0.1:7002".parse().unwrap(),
            control.clone(),
            &mut active,
            &local,
            &uplink,
        )
        .await;
        assert_eq!(active.as_ref().unwrap().child.id(), pid);
        control.publication_id = "pub-original".into();
        handle_command(
            &config,
            "127.0.0.1:7002".parse().unwrap(),
            control,
            &mut active,
            &local,
            &uplink,
        )
        .await;
        assert!(active.is_none());
        assert!(matches!(
            statuses.recv().await,
            Some(RobotMessage::CameraStatus(_))
        ));
        assert_eq!(local_messages.recv().await.unwrap().topic, "media/stop");
    }

    #[test]
    fn start_requires_ephemeral_credentials() {
        let control = MediaControl {
            schema: "fleetly.media-control.v2".into(),
            action: MediaAction::Start,
            publication_id: "pub-test".into(),
            issued_at: unix_time_ms(),
            expires_at: unix_time_ms() + 60_000,
            livekit_url: Some("wss://example.livekit.cloud".into()),
            participant_token: Some("short-lived-token".into()),
        };
        assert!(validate_control(&control).is_ok());
    }

    #[test]
    fn stop_rejects_credentials() {
        let control = MediaControl {
            schema: "fleetly.media-control.v2".into(),
            action: MediaAction::Stop,
            publication_id: "pub-test".into(),
            issued_at: 1,
            expires_at: 2,
            livekit_url: Some("wss://example.livekit.cloud".into()),
            participant_token: None,
        };
        assert!(validate_control(&control).is_err());
    }
}
