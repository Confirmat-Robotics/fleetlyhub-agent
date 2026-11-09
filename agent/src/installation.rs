use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AgentSettings, MqttConfig, RuntimeConfig, validate_robot_id};

/// Assignment and MQTT endpoint are fetched using the enrolled certificate.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub hub_url: String,
    /// Absent in legacy installations, which use ca.pem for both transports.
    #[serde(default)]
    pub https_trust: Option<HttpsTrust>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpsTrust {
    Public,
    CustomCa,
}

pub(crate) fn https_ca_path(
    directory: &Path,
    connection: &Connection,
) -> Option<std::path::PathBuf> {
    match connection.https_trust {
        Some(HttpsTrust::Public) => None,
        Some(HttpsTrust::CustomCa) => Some(directory.join("https-ca.pem")),
        None => Some(directory.join("ca.pem")),
    }
}

#[derive(Deserialize)]
struct Bootstrap {
    schema: String,
    robot: Robot,
    installation_id: uuid::Uuid,
    credential_generation: u64,
    mqtt: Endpoint,
    #[serde(default)]
    event_receipts: bool,
}

#[derive(Deserialize)]
struct Robot {
    id: String,
    record_id: uuid::Uuid,
    name: String,
}

#[derive(Deserialize)]
struct Endpoint {
    host: String,
    port: u16,
}

pub fn validate_hub(hub: &str) -> Result<()> {
    let url = reqwest::Url::parse(hub)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Hub address must be an HTTPS origin without credentials or a path");
    }
    Ok(())
}

pub fn validate_credentials(directory: &Path) -> Result<()> {
    let cert = fs::read(directory.join("client-certificate.pem"))?;
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let der = CertificateDer::from_pem_slice(&cert)?;
    let (_, parsed) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|_| anyhow::anyhow!("Invalid installation certificate"))?;
    if !parsed.validity().is_valid() {
        bail!(
            "Installation certificate expired or is not yet valid. Check the clock; expired installations require explicit re-enrollment"
        );
    }
    let mut identity = fs::read(directory.join("private-key.pem"))?;
    identity.extend_from_slice(&cert);
    reqwest::Identity::from_pem(&identity)
        .context("Invalid installation certificate/private key")?;
    reqwest::Certificate::from_pem(&fs::read(directory.join("ca.pem"))?)
        .context("Invalid Hub CA")?;
    Ok(())
}

pub async fn bootstrap(directory: &Path) -> Result<RuntimeConfig> {
    if directory.join("pending.json").exists() {
        bail!("Enrollment is incomplete; resume the enroll command first");
    }
    let path = directory.join("connection.json");
    let connection: Connection = serde_json::from_slice(&fs::read(&path)
        .context("No enrolled installation found. Run fleetlyhub-agent enroll first (or supply --hub and --ca for a development Hub)")?)?;
    validate_hub(&connection.hub_url)?;
    let settings = AgentSettings::load(directory)?;
    validate_credentials(directory)?;
    let client = crate::renewal::client(directory)?;
    let response = bootstrap_response(
        &client,
        &format!(
            "{}/v1/agent/session/bootstrap",
            connection.hub_url.trim_end_matches('/')
        ),
        Duration::from_secs(5),
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        let result: serde_json::Value = response.json().await.unwrap_or_default();
        bail!(
            "Hub bootstrap rejected ({status}): {}",
            result
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(
                    "Check that the installation certificate is valid and enrollment is complete"
                )
        );
    }
    let assignment: Bootstrap = response
        .json()
        .await
        .context("Invalid Hub bootstrap response")?;
    if assignment.schema != "fleetly.agent-bootstrap.v1"
        || assignment.credential_generation == 0
        || assignment.robot.name.trim().is_empty()
        || assignment.mqtt.host.trim().is_empty()
        || assignment.mqtt.port == 0
    {
        bail!("Invalid Hub assignment");
    }
    validate_robot_id(&assignment.robot.id)?;
    tracing::info!(installation_id = %assignment.installation_id, record_id = %assignment.robot.record_id,
        generation = assignment.credential_generation, "Hub authorized this installation");
    Ok(RuntimeConfig {
        event_outbox: if assignment.event_receipts {
            Some(std::sync::Arc::new(std::sync::Mutex::new(
                crate::event_outbox::EventOutbox::open(
                    &directory.join("event-outbox.json"),
                    crate::event_outbox::Owner {
                        hub: connection.hub_url,
                        installation_id: assignment.installation_id.to_string(),
                        robot_id: assignment.robot.id.clone(),
                    },
                )?,
            )))
        } else {
            tracing::warn!(
                "Hub does not support durable issue receipts; upgrade the Hub before relying on issue replay"
            );
            None
        },
        robot_id: assignment.robot.id,
        robot_name: assignment.robot.name,
        mqtt: MqttConfig {
            host: assignment.mqtt.host,
            port: assignment.mqtt.port,
            ca_certificate: directory.join("ca.pem"),
            client_certificate: directory.join("client-certificate.pem"),
            private_key: directory.join("private-key.pem"),
            keep_alive_seconds: 15,
        },
        telemetry_interval_ms: settings.telemetry_interval_ms,
        local: settings.local,
        media: settings.media,
    })
}

async fn bootstrap_response(
    client: &reqwest::Client,
    url: &str,
    retry: Duration,
) -> Result<reqwest::Response> {
    loop {
        match client
            .post(url)
            .json(&serde_json::json!({"agent_version":env!("CARGO_PKG_VERSION")}))
            .send()
            .await
        {
            Ok(response)
                if response.status().is_server_error()
                    || matches!(response.status().as_u16(), 408 | 429) => {}
            Ok(response) => return Ok(response), // Authentication/revocation errors fail immediately.
            Err(error) if error.is_connect() || error.is_timeout() => {}
            Err(error) => {
                return Err(error).context("Unable to authenticate this installation with the Hub");
            }
        }
        tracing::warn!("Hub temporarily unavailable; retrying installation bootstrap");
        tokio::select! {
            _ = tokio::time::sleep(retry) => {}
            _ = tokio::signal::ctrl_c() => bail!("Agent startup canceled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn https_trust_is_independent_of_mqtt_with_legacy_compatibility() {
        let directory = Path::new("/credentials");
        let legacy: Connection =
            serde_json::from_str(r#"{"hub_url":"https://hub.example.test"}"#).unwrap();
        assert_eq!(
            https_ca_path(directory, &legacy),
            Some(directory.join("ca.pem"))
        );
        let mut connection = legacy;
        connection.https_trust = Some(HttpsTrust::CustomCa);
        assert_eq!(
            https_ca_path(directory, &connection),
            Some(directory.join("https-ca.pem"))
        );
        connection.https_trust = Some(HttpsTrust::Public);
        assert!(https_ca_path(directory, &connection).is_none());
    }

    #[tokio::test]
    async fn startup_retries_a_hub_restart_but_not_revoked_access() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bootstrap", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [503, 200, 403] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let received = socket.read(&mut request).await.unwrap();
                assert!(received > 0);
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::new();
        assert_eq!(
            bootstrap_response(&client, &url, Duration::from_millis(1))
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            bootstrap_response(&client, &url, Duration::from_millis(1))
                .await
                .unwrap()
                .status(),
            403
        );
        server.await.unwrap();
    }
}
