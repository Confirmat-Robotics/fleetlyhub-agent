use std::{fs, io::IsTerminal, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer},
    pkcs8::DecodePrivateKey,
};
use rcgen::{CertificateParams, DnType, KeyPair, PublicKeyData};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct Pending {
    hub: String,
    device_code: String,
    user_code: String,
    verification_url: String,
}

#[derive(Debug)]
struct HubError {
    status: reqwest::StatusCode,
    code: String,
    message: String,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Hub rejected enrollment ({}): {}",
            self.status, self.message
        )
    }
}
impl std::error::Error for HubError {}

fn can_refresh(error: &anyhow::Error, directory: &Path) -> bool {
    error.downcast_ref::<HubError>().is_some_and(|error| {
        error.status == reqwest::StatusCode::GONE && error.code == "enrollment_expired"
    }) && !directory.join("connection.json").exists()
        && !directory.join("client-certificate.pem").exists()
}

fn protect_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(windows)]
    {
        let account = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN")?,
            std::env::var("USERNAME")?
        );
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("{account}:(OI)(CI)F"),
            ])
            .output()?;
        if !status.status.success() {
            bail!("Cannot restrict enrollment directory permissions");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn save(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

async fn post(client: &reqwest::Client, hub: &str, route: &str, body: Value) -> Result<Value> {
    let response = client
        .post(format!("{hub}{route}"))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let result: Value = response
        .json()
        .await
        .context("Hub returned an invalid response")?;
    if !status.is_success() {
        return Err(HubError {
            status,
            code: result
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            message: result
                .get("error")
                .or_else(|| result.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Check Hub configuration")
                .to_owned(),
        }
        .into());
    }
    Ok(result)
}

async fn start_request(client: &reqwest::Client, hub: &str, directory: &Path) -> Result<Pending> {
    let key_path = directory.join("private-key.pem");
    let key = if key_path.exists() {
        KeyPair::from_pem(&fs::read_to_string(&key_path)?)?
    } else {
        let key = KeyPair::generate()?;
        save(&key_path, key.serialize_pem())?;
        key
    };
    let mut parameters = CertificateParams::new(Vec::<String>::new())?;
    parameters
        .distinguished_name
        .push(DnType::CommonName, "pending-agent");
    let csr = parameters.serialize_request(&key)?.pem()?;
    let result = post(
        client,
        hub,
        "/v1/enrollments",
        json!({"csr_pem": csr, "agent_version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    let pending = Pending {
        hub: hub.to_owned(),
        device_code: field(&result, "device_code")?.to_owned(),
        user_code: field(&result, "user_code")?.to_owned(),
        verification_url: field(&result, "verification_url")?.to_owned(),
    };
    save(
        &directory.join("pending.json"),
        serde_json::to_vec(&pending)?,
    )?;
    Ok(pending)
}

fn show_code(pending: &Pending) {
    let bold = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
    let code = if bold {
        format!("\x1b[1m{}\x1b[0m", pending.user_code)
    } else {
        pending.user_code.clone()
    };
    println!(
        "\nComplete enrollment in Fleetly Hub:\n\n{code}\n\nOpen {}\nSelect your robot, enter this code and approve enrollment.\n\nWaiting for approval… Your private key stays on this robot.",
        pending.verification_url
    );
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing Hub response field: {name}"))
}

fn retire_credentials(directory: &Path) -> Result<Option<std::path::PathBuf>> {
    let names = [
        "private-key.pem",
        "client-certificate.pem",
        "ca.pem",
        "https-ca.pem",
        "connection.json",
        "pending.json",
        "renewal.json",
        "event-outbox.json",
    ];
    if !names.iter().any(|name| directory.join(name).exists()) {
        return Ok(None);
    }
    let archive = directory
        .join("retired")
        .join(uuid::Uuid::new_v4().to_string());
    protect_directory(&archive)?;
    let mut moved = Vec::new();
    for name in names {
        if directory.join(name).exists() {
            if let Err(error) = fs::rename(directory.join(name), archive.join(name)) {
                for previous in moved.iter().rev() {
                    fs::rename(archive.join(previous), directory.join(previous))
                        .context("Cannot restore archived credentials after replacement failed")?;
                }
                return Err(error).context("Cannot archive previous installation credentials");
            }
            moved.push(name);
        }
    }
    Ok(Some(archive))
}

pub async fn run(hub: &str, ca: Option<&Path>, directory: &Path, replace: bool) -> Result<()> {
    crate::installation::validate_hub(hub)?;
    let hub = hub.trim_end_matches('/');
    let trust = ca
        .map(fs::read)
        .transpose()
        .context("Cannot read trusted Hub HTTPS CA")?;
    let mut builder = reqwest::Client::builder();
    if let Some(trust) = &trust {
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(trust)?);
    }
    let client = builder
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    protect_directory(directory)?;
    if replace && let Some(archive) = retire_credentials(directory)? {
        println!(
            "Previous local credentials archived in {}. A new installation key will be generated.",
            archive.display()
        );
        println!(
            "Replacement does not revoke the old certificate remotely. Use Set up Agent again on the Fleetly website first."
        );
    }
    let pending_path = directory.join("pending.json");
    let key_path = directory.join("private-key.pem");
    if directory.join("client-certificate.pem").exists() && !pending_path.exists() {
        bail!(
            "Existing local enrollment credentials found on this computer.\nTo enroll again, rerun this command with --replace.\nFirst reset the enrollment on the Fleetly Hub website, if you haven't already."
        );
    }
    let mut pending: Pending = if pending_path.exists() {
        let pending: Pending = serde_json::from_slice(&fs::read(&pending_path)?)?;
        if pending.hub != hub {
            bail!("Pending enrollment belongs to another Hub");
        }
        match post(
            &client,
            hub,
            "/v1/enrollments/poll",
            json!({"device_code": pending.device_code}),
        )
        .await
        {
            Ok(_) => pending,
            Err(error) if can_refresh(&error, directory) => {
                println!(
                    "The previous request expired. Requesting a new code with the same local key..."
                );
                start_request(&client, hub, directory).await?
            }
            Err(error) => return Err(error),
        }
    } else {
        start_request(&client, hub, directory).await?
    };
    show_code(&pending);
    let key = SigningKey::from_pkcs8_pem(&fs::read_to_string(&key_path)?)?;
    let pair = KeyPair::from_pem(&fs::read_to_string(&key_path)?)?;
    println!(
        "Installation fingerprint: SHA256:{:x}",
        Sha256::digest(pair.subject_public_key_info())
    );
    loop {
        let result = match post(
            &client,
            hub,
            "/v1/enrollments/poll",
            json!({"device_code": pending.device_code}),
        )
        .await
        {
            Ok(result) => result,
            Err(error) if can_refresh(&error, directory) => {
                println!(
                    "Enrollment code expired. Requesting a new code with the same local key..."
                );
                pending = start_request(&client, hub, directory).await?;
                show_code(&pending);
                continue;
            }
            Err(error) => return Err(error),
        };
        match field(&result, "status")? {
            "pending" => tokio::time::sleep(Duration::from_secs(5)).await,
            "issued" => {
                save(
                    &directory.join("client-certificate.pem"),
                    field(&result, "certificate_pem")?,
                )?;
                save(&directory.join("ca.pem"), field(&result, "ca_pem")?)?;
                if let Some(trust) = &trust {
                    save(&directory.join("https-ca.pem"), trust)?;
                }
                crate::installation::validate_credentials(directory)?;
                save(
                    &directory.join("connection.json"),
                    serde_json::to_vec_pretty(&crate::installation::Connection {
                        hub_url: hub.to_owned(),
                        https_trust: Some(if trust.is_some() {
                            crate::installation::HttpsTrust::CustomCa
                        } else {
                            crate::installation::HttpsTrust::Public
                        }),
                    })?,
                )?;
                let message = format!(
                    "fleetly-enrollment-complete:{}:{}",
                    field(&result, "request_id")?,
                    field(&result, "completion_nonce")?
                );
                let signature: Signature = key.sign(message.as_bytes());
                post(&client, hub, "/v1/enrollments/complete", json!({"device_code": pending.device_code, "signature": STANDARD.encode(signature.to_der().as_bytes())})).await?;
                fs::remove_file(&pending_path)?;
                println!(
                    "Enrollment complete. Start fleetlyhub-agent --state-dir {}",
                    directory.display()
                );
                return Ok(());
            }
            "complete" if directory.join("connection.json").exists() => {
                crate::installation::validate_credentials(directory)?;
                fs::remove_file(&pending_path)?;
                println!(
                    "Enrollment complete. Start fleetlyhub-agent --state-dir {}",
                    directory.display()
                );
                return Ok(());
            }
            _ => bail!("Unexpected enrollment status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reenrollment_without_replace_explains_local_credentials_and_preserves_them() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["client-certificate.pem", "private-key.pem", "settings.json"] {
            fs::write(directory.path().join(name), "existing local data").unwrap();
        }
        let error = run("https://127.0.0.1:1", None, directory.path(), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Existing local enrollment credentials found on this computer"));
        assert!(error.contains("rerun this command with --replace"));
        assert!(error.contains("if you haven't already"));
        assert!(!error.contains("Start fleetlyhub-agent"));
        for name in ["client-certificate.pem", "private-key.pem", "settings.json"] {
            assert_eq!(
                fs::read_to_string(directory.path().join(name)).unwrap(),
                "existing local data"
            );
        }
        assert!(!directory.path().join("pending.json").exists());
        assert!(!directory.path().join("retired").exists());
    }

    #[test]
    fn replacement_rotates_credentials_but_preserves_settings() {
        let directory =
            std::env::temp_dir().join(format!("fleetly-replace-test-{}", uuid::Uuid::new_v4()));
        protect_directory(&directory).unwrap();
        let key = KeyPair::generate().unwrap();
        save(&directory.join("private-key.pem"), key.serialize_pem()).unwrap();
        for name in [
            "client-certificate.pem",
            "connection.json",
            "ca.pem",
            "https-ca.pem",
            "pending.json",
            "renewal.json",
            "settings.json",
            "event-outbox.json",
        ] {
            save(&directory.join(name), "test").unwrap();
        }
        let archive = retire_credentials(&directory).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(archive.join("private-key.pem")).unwrap(),
            key.serialize_pem()
        );
        assert!(directory.join("settings.json").exists());
        assert!(!directory.join("private-key.pem").exists());
        assert!(!directory.join("pending.json").exists());
        assert!(!directory.join("https-ca.pem").exists());
        assert!(archive.join("https-ca.pem").exists());
        assert!(archive.join("renewal.json").exists());
        assert!(!directory.join("renewal.json").exists());
        assert!(archive.join("event-outbox.json").exists());
        assert!(!directory.join("event-outbox.json").exists());
        let fresh = KeyPair::generate().unwrap();
        assert_ne!(
            key.subject_public_key_info(),
            fresh.subject_public_key_info()
        );
        assert!(retire_credentials(&directory).unwrap().is_none());
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn only_expired_unapproved_requests_can_be_refreshed() {
        let directory =
            std::env::temp_dir().join(format!("fleetly-renew-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let error = |code: &str| -> anyhow::Error {
            HubError {
                status: reqwest::StatusCode::GONE,
                code: code.to_owned(),
                message: "test".to_owned(),
            }
            .into()
        };
        assert!(can_refresh(&error("enrollment_expired"), &directory));
        assert!(!can_refresh(
            &error("enrollment_delivery_expired"),
            &directory
        ));
        assert!(!can_refresh(&error("installation_revoked"), &directory));
        fs::write(directory.join("client-certificate.pem"), "test").unwrap();
        assert!(!can_refresh(&error("enrollment_expired"), &directory));
        fs::remove_file(directory.join("client-certificate.pem")).unwrap();
        fs::write(directory.join("connection.json"), "test").unwrap();
        assert!(!can_refresh(&error("enrollment_expired"), &directory));
        fs::remove_file(directory.join("connection.json")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
