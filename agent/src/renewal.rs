//! Automatic renewal keeps the installation key and process/local/media sessions.
use std::{fs, io::Write, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use rcgen::{CertificateParams, KeyPair, PublicKeyData};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, UnixTime, pem::PemObject},
    server::WebPkiClientVerifier,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{info, warn};
use uuid::Uuid;

use crate::installation::Connection;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    request_id: Uuid,
    previous_certificate_pem: String,
    certificate_pem: Option<String>,
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Credential path has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn validate_replacement(directory: &Path, previous: &str, replacement: &str) -> Result<()> {
    let cert = CertificateDer::from_pem_slice(replacement.as_bytes())?;
    let old = CertificateDer::from_pem_slice(previous.as_bytes())?;
    let (_, new_parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|_| anyhow::anyhow!("Invalid replacement certificate"))?;
    let (_, old_parsed) = x509_parser::parse_x509_certificate(old.as_ref())
        .map_err(|_| anyhow::anyhow!("Invalid previous certificate"))?;
    let key = KeyPair::from_pem(&fs::read_to_string(directory.join("private-key.pem"))?)?;
    if new_parsed.subject() != old_parsed.subject()
        || new_parsed.public_key().raw != key.subject_public_key_info()
        || new_parsed.validity().not_after <= old_parsed.validity().not_after
    {
        bail!(
            "Replacement certificate changed installation identity/key or did not extend validity"
        );
    }
    let ca = CertificateDer::from_pem_slice(&fs::read(directory.join("ca.pem"))?)?;
    let mut roots = RootCertStore::empty();
    roots.add(ca)?;
    WebPkiClientVerifier::builder_with_provider(
        std::sync::Arc::new(roots),
        std::sync::Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()?
    .verify_client_cert(&cert, &[], UnixTime::now())
    .context("Replacement certificate is not valid under the trusted installation CA")?;
    Ok(())
}

/// Recover a durable candidate before bootstrap, including an interrupted file write.
pub fn recover(directory: &Path) -> Result<bool> {
    let path = directory.join("renewal.json");
    if !path.exists() {
        return Ok(false);
    }
    let pending: Pending = serde_json::from_slice(&fs::read(path)?)?;
    if let Some(cert) = pending.certificate_pem {
        validate_replacement(directory, &pending.previous_certificate_pem, &cert)?;
        let path = directory.join("client-certificate.pem");
        if fs::read(&path).ok().as_deref() != Some(cert.as_bytes()) {
            atomic_write(&path, cert.as_bytes())?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn client(directory: &Path) -> Result<reqwest::Client> {
    let connection: Connection =
        serde_json::from_slice(&fs::read(directory.join("connection.json"))?)?;
    crate::installation::validate_hub(&connection.hub_url)?;
    let mut identity = fs::read(directory.join("private-key.pem"))?;
    identity.extend_from_slice(&fs::read(directory.join("client-certificate.pem"))?);
    let mut builder = reqwest::Client::builder();
    if let Some(path) = crate::installation::https_ca_path(directory, &connection) {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&fs::read(path)?)?);
    }
    Ok(builder
        .identity(reqwest::Identity::from_pem(&identity)?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?)
}

async fn post(client: &reqwest::Client, hub: &str, route: &str, body: Value) -> Result<Value> {
    let response = client
        .post(format!("{}{route}", hub.trim_end_matches('/')))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .context("Invalid certificate renewal response")?;
    if !status.is_success() {
        bail!(
            "Certificate renewal rejected ({status}): {}",
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Check installation authorization")
        );
    }
    Ok(value)
}

async fn step(directory: &Path, changed: &watch::Sender<u64>) -> Result<Duration> {
    let connection: Connection =
        serde_json::from_slice(&fs::read(directory.join("connection.json"))?)?;
    crate::installation::validate_hub(&connection.hub_url)?;
    let path = directory.join("renewal.json");
    let mut pending = if path.exists() {
        serde_json::from_slice::<Pending>(&fs::read(&path)?)?
    } else {
        let pending = Pending {
            request_id: Uuid::new_v4(),
            previous_certificate_pem: fs::read_to_string(directory.join("client-certificate.pem"))?,
            certificate_pem: None,
        };
        // Save intent before sending: a lost response retries the same operation.
        atomic_write(&path, &serde_json::to_vec(&pending)?)?;
        pending
    };
    if pending.certificate_pem.is_none() {
        let key = KeyPair::from_pem(&fs::read_to_string(directory.join("private-key.pem"))?)?;
        let csr = CertificateParams::new(Vec::<String>::new())?
            .serialize_request(&key)?
            .pem()?;
        let result = post(
            &client(directory)?,
            &connection.hub_url,
            "/v1/agent/certificates/renew",
            json!({"request_id":pending.request_id, "csr_pem":csr}),
        )
        .await?;
        if result["status"] == "not_due" {
            fs::remove_file(path)?;
            return Ok(Duration::from_secs(3600));
        }
        if result["status"] != "issued" {
            bail!("Unexpected certificate renewal status");
        }
        let cert = result["certificate_pem"]
            .as_str()
            .context("Missing renewal certificate")?;
        validate_replacement(directory, &pending.previous_certificate_pem, cert)?;
        pending.request_id = serde_json::from_value(result["request_id"].clone())?;
        pending.certificate_pem = Some(cert.to_owned());
        atomic_write(&path, &serde_json::to_vec(&pending)?)?;
    }
    if recover(directory)? {
        changed.send_modify(|version| *version = version.wrapping_add(1));
        info!(
            "Renewed installation certificate saved; MQTT will reconnect without restarting local clients or media"
        );
    }
    // Successful mTLS acknowledgement proves the new key/certificate works.
    let result = post(
        &client(directory)?,
        &connection.hub_url,
        "/v1/agent/certificates/acknowledge",
        json!({"request_id":pending.request_id}),
    )
    .await?;
    if result["status"] != "active" {
        bail!("Certificate activation was not acknowledged");
    }
    fs::remove_file(path)?;
    info!("Automatic certificate renewal activated");
    Ok(Duration::from_secs(3600))
}

pub async fn run(directory: &Path, changed: watch::Sender<u64>) -> Result<()> {
    let settings = crate::config::AgentSettings::load(directory)?.renewal;
    let mut retry = settings.initial_retry_seconds;
    loop {
        let delay = match step(directory, &changed).await {
            Ok(_) => {
                retry = settings.initial_retry_seconds;
                Duration::from_secs(settings.check_interval_seconds)
            }
            Err(error) => {
                warn!(%error, "Automatic renewal failed; retaining durable credentials and retrying. Expired installations require re-enrollment");
                let delay = Duration::from_secs(retry);
                retry = (retry * 2).min(settings.max_retry_seconds);
                delay
            }
        };
        let jitter = u64::from(Uuid::new_v4().as_bytes()[0]) % (delay.as_secs() / 10 + 1).min(31);
        tokio::time::sleep(delay + Duration::from_secs(jitter)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa};

    fn credentials() -> (tempfile::TempDir, String, String) {
        let directory = tempfile::tempdir().unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let signer = rcgen::Issuer::new(ca_params, &ca_key);
        let key = KeyPair::generate().unwrap();
        fs::write(
            directory.path().join("private-key.pem"),
            key.serialize_pem(),
        )
        .unwrap();
        fs::write(directory.path().join("ca.pem"), ca.pem()).unwrap();
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, "r-test");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_after = rcgen::date_time_ymd(2030, 1, 1);
        let old = params.signed_by(&key, &signer).unwrap().pem();
        params.not_after = rcgen::date_time_ymd(2031, 1, 1);
        let new = params.signed_by(&key, &signer).unwrap().pem();
        (directory, old, new)
    }

    #[test]
    fn journal_recovers_old_missing_and_interrupted_certificate_and_keeps_key() {
        let (directory, old, new) = credentials();
        let root = directory.path();
        let key = fs::read(root.join("private-key.pem")).unwrap();
        let pending = Pending {
            request_id: Uuid::new_v4(),
            previous_certificate_pem: old.clone(),
            certificate_pem: Some(new.clone()),
        };
        atomic_write(
            &root.join("renewal.json"),
            &serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        for value in [old, "partial write".to_owned()] {
            fs::write(root.join("client-certificate.pem"), value).unwrap();
            assert!(recover(root).unwrap());
            assert_eq!(
                fs::read_to_string(root.join("client-certificate.pem")).unwrap(),
                new
            );
            assert!(!recover(root).unwrap());
            assert_eq!(fs::read(root.join("private-key.pem")).unwrap(), key);
            crate::installation::validate_credentials(root).unwrap();
        }
        fs::remove_file(root.join("client-certificate.pem")).unwrap();
        assert!(recover(root).unwrap());
    }

    #[test]
    fn invalid_or_unchanged_replacement_never_overwrites_credentials() {
        let (directory, old, _) = credentials();
        assert!(validate_replacement(directory.path(), &old, &old).is_err());
        assert!(validate_replacement(directory.path(), &old, "invalid").is_err());
        let (_, _, unrelated) = credentials();
        assert!(validate_replacement(directory.path(), &old, &unrelated).is_err());
    }
}
