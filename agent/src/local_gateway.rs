//! Registered local route owners. A slow or failed client cannot stall its peer.
use crate::{
    config::{ClientRoutes, LocalConfig},
    local_protocol::{
        LOCAL_PROTOCOL, OutgoingEnvelope, RobotMessage, parse_robot_message, validate_identifier,
    },
};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_util::codec::{FramedRead, LinesCodec};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Interfaces {
    pub robot_adapter: bool,
    pub service_bridge: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    protocol: String,
    topic: String,
    payload: serde_json::Value,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    schema: String,
    client_id: String,
    publish_routes: Vec<String>,
    subscribe_routes: Vec<String>,
}
struct Client {
    output: mpsc::Sender<(OutgoingEnvelope, bool)>,
    policy: Option<ClientRoutes>,
    client_id: String,
    abort: Option<tokio::task::AbortHandle>,
}

pub fn matches(filter: &str, route: &str) -> bool {
    if filter == route {
        return true;
    }
    filter.strip_suffix('+').is_some_and(|prefix| {
        route.strip_prefix(prefix).is_some_and(|value| {
            !value.is_empty() && !value.contains('/') && validate_identifier(value, 128).is_ok()
        })
    })
}
fn overlaps(a: &str, b: &str) -> bool {
    a == b || matches(a, b) || matches(b, a)
}
fn validate_registration(
    reg: &Registration,
    config: &LocalConfig,
    clients: &HashMap<u64, Client>,
) -> Result<ClientRoutes> {
    anyhow::ensure!(
        reg.schema == "fleetly.local-registration.v1",
        "Unsupported registration schema"
    );
    validate_identifier(&reg.client_id, 64)?;
    let configured = config
        .clients
        .get(&reg.client_id)
        .context("Client is not configured")?;
    for (claims, allowed) in [
        (&reg.publish_routes, &configured.publish_routes),
        (&reg.subscribe_routes, &configured.subscribe_routes),
    ] {
        anyhow::ensure!(
            claims.len() <= 16 && claims.iter().collect::<HashSet<_>>().len() == claims.len(),
            "Invalid route claims"
        );
        anyhow::ensure!(
            claims.iter().all(|r| allowed.contains(r)),
            "Route is not allowlisted"
        );
    }
    anyhow::ensure!(
        !reg.publish_routes.is_empty() || !reg.subscribe_routes.is_empty(),
        "No routes claimed"
    );
    for client in clients.values().filter(|c| c.policy.is_some()) {
        let policy = client.policy.as_ref().unwrap();
        anyhow::ensure!(
            client.client_id != reg.client_id && policy.interface != configured.interface,
            "Client or interface already registered"
        );
        anyhow::ensure!(
            !policy
                .publish_routes
                .iter()
                .any(|a| reg.publish_routes.iter().any(|b| overlaps(a, b)))
                && !policy
                    .subscribe_routes
                    .iter()
                    .any(|a| reg.subscribe_routes.iter().any(|b| overlaps(a, b))),
            "Route ownership conflict"
        );
    }
    Ok(ClientRoutes {
        interface: configured.interface.clone(),
        publish_routes: reg.publish_routes.clone(),
        subscribe_routes: reg.subscribe_routes.clone(),
    })
}
fn error(code: &str) -> OutgoingEnvelope {
    OutgoingEnvelope::new(
        "agent/error",
        serde_json::json!({"schema":"fleetly.local-error.v1","code":code,"message":match code {
            "registration_rejected" => "Registration is missing, invalid, not allowlisted, or already owned.",
            "wrong_result_owner" => "This connection did not receive the command.",
            "route_not_owned" => "This connection does not own that route.",
            "event_outbox_unavailable" => "Issue was not accepted into durable storage; preserve it and retry after storage/capacity is restored.",
            _ => "Message is invalid or this client exceeded a queue or transport limit."
        }}),
    )
}

#[test]
fn public_registration_examples_match_real_default_route_policy() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = if root.join("contracts").exists() {
        root.join("contracts/local/v1/examples")
    } else {
        root.parent().unwrap().join("contracts/local/v1/examples")
    };
    for name in ["robot-adapter", "service-bridge"] {
        let input = std::fs::read_to_string(base.join(format!("register-{name}.json"))).unwrap();
        let envelope: Envelope = serde_json::from_str(&input).unwrap();
        let registration: Registration = serde_json::from_value(envelope.payload).unwrap();
        assert!(
            validate_registration(&registration, &LocalConfig::default(), &HashMap::new()).is_ok()
        );
    }
}
fn update(
    clients: &HashMap<u64, Client>,
    interfaces: &watch::Sender<Interfaces>,
    adapter: &watch::Sender<bool>,
) {
    let active = |name| {
        clients.values().any(|c| {
            c.policy.as_ref().is_some_and(|p| p.interface == name) && !c.output.is_closed()
        })
    };
    let state = Interfaces {
        robot_adapter: active("robot_adapter"),
        service_bridge: active("service_bridge"),
    };
    if *interfaces.borrow() != state {
        interfaces.send_replace(state);
    }
    adapter.send_replace(state.robot_adapter);
}

pub async fn serve(
    config: LocalConfig,
    uplink: mpsc::Sender<RobotMessage>,
    downlink: mpsc::Receiver<OutgoingEnvelope>,
    adapter: watch::Sender<bool>,
    interfaces: watch::Sender<Interfaces>,
    outbox: Option<crate::event_outbox::Shared>,
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind)
        .await
        .context("Cannot bind local Agent interface")?;
    tracing::info!(bind=%config.bind,"registered local interfaces listening");
    serve_listener(
        listener, config, uplink, downlink, adapter, interfaces, outbox,
    )
    .await
}

async fn serve_listener(
    listener: TcpListener,
    config: LocalConfig,
    uplink: mpsc::Sender<RobotMessage>,
    mut downlink: mpsc::Receiver<OutgoingEnvelope>,
    adapter: watch::Sender<bool>,
    interfaces: watch::Sender<Interfaces>,
    outbox: Option<crate::event_outbox::Shared>,
) -> Result<()> {
    let (incoming, mut messages) =
        mpsc::channel::<(u64, String)>(config.queue_capacity * config.max_clients);
    let mut tasks = JoinSet::new();
    let mut clients: HashMap<u64, Client> = HashMap::new();
    let mut serial = 0u64;
    let mut supervision: Option<OutgoingEnvelope> = None;
    // Results must originate from the very connection that received a command.
    let mut delivered: HashMap<String, u64> = HashMap::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream,_) = accepted?;
                if tasks.len() >= config.max_clients { drop(stream); continue; }
                serial += 1; let id = serial;
                let (output,mut receive) = mpsc::channel::<(OutgoingEnvelope,bool)>(config.queue_capacity);
                clients.insert(id,Client{output,policy:None,client_id:String::new(),abort:None});
                let input = incoming.clone(); let max_line = config.max_line_bytes; let deadline = config.registration_timeout_ms;
                let task = tasks.spawn(async move {
                    let (reader,mut writer) = stream.into_split();
                    let mut lines = FramedRead::new(reader,LinesCodec::new_with_max_length(max_line));
                    // First line must register; a silent connection has a bounded lifetime.
                    if let Ok(Some(Ok(line))) = timeout(Duration::from_millis(deadline),lines.next()).await {
                        if input.send_timeout((id,line),Duration::from_millis(100)).await.is_err() { return id; }
                    } else { return id; }
                    let mut second = std::time::Instant::now(); let mut rate = 0;
                    loop { tokio::select! {
                        line = lines.next() => {
                            let Some(Ok(line)) = line else { break; };
                            if second.elapsed() >= Duration::from_secs(1) { second=std::time::Instant::now();rate=0; }
                            rate += 1; if rate > 100 { break; }
                            if input.send_timeout((id,line),Duration::from_millis(100)).await.is_err() { break; }
                        }
                        item = receive.recv() => {
                            let Some((envelope,close)) = item else { break; };
                            let mut data = serde_json::to_vec(&envelope).unwrap(); data.push(b'\n');
                            if !matches!(timeout(Duration::from_secs(1),writer.write_all(&data)).await,Ok(Ok(()))) || close { break; }
                        }
                    }}
                    id
                });
                clients.get_mut(&id).unwrap().abort=Some(task);
            }
            finished = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(id)) = finished {
                    clients.remove(&id); delivered.retain(|_,owner|*owner!=id);
                    update(&clients,&interfaces,&adapter);
                }
            }
            input = messages.recv() => {
                let Some((id,line)) = input else { return Ok(()); };
                let Some(client) = clients.get(&id) else { continue; };
                let decoded = serde_json::from_str::<Envelope>(&line);
                let Ok(envelope) = decoded else { let _=client.output.try_send((error("invalid_message"),true)); continue; };
                if envelope.protocol != LOCAL_PROTOCOL { let _=client.output.try_send((error("invalid_message"),true));continue; }
                if client.policy.is_none() {
                    let policy = if envelope.topic == "agent/register" {
                        serde_json::from_value::<Registration>(envelope.payload).ok().and_then(|reg|validate_registration(&reg,&config,&clients).ok().map(|p|(reg.client_id,p)))
                    } else { None };
                    if let Some((name,policy)) = policy {
                        let c=clients.get_mut(&id).unwrap(); c.client_id=name.clone();c.policy=Some(policy);
                        let _=c.output.try_send((OutgoingEnvelope::new("agent/registered",serde_json::json!({"schema":"fleetly.local-registration-result.v1","client_id":name,"accepted":true})),false));
                        if c.policy.as_ref().unwrap().subscribe_routes.contains(&"supervision".to_owned())
                            && let Some(ref s) = supervision { let _=c.output.try_send((s.clone(),false)); }
                        update(&clients,&interfaces,&adapter);
                    } else { let _=client.output.try_send((error("registration_rejected"),true)); }
                    continue;
                }
                if !client.policy.as_ref().unwrap().publish_routes.iter().any(|r|matches(r,&envelope.topic)) {
                    let _=client.output.try_send((error("route_not_owned"),false));continue;
                }
                if let Some(command) = envelope.topic.strip_prefix("command-results/")
                    && delivered.get(command) != Some(&id) { let _=client.output.try_send((error("wrong_result_owner"),false));continue; }
                if let Some(command) = envelope.topic.strip_prefix("services/control-results/")
                    && delivered.get(&format!("service:{command}")) != Some(&id) { let _=client.output.try_send((error("wrong_result_owner"),false));continue; }
                match parse_robot_message(&line) {
                    Ok(message) => {
                        if let RobotMessage::Event { event_type, payload } = &message
                            && let Some(outbox) = &outbox {
                            let result = outbox.lock().map_err(|_|anyhow::anyhow!("Event outbox lock unavailable"))
                                .and_then(|mut queue|queue.enqueue(event_type,payload));
                            if let Err(failure) = result {
                                tracing::error!(%failure,"Robot issue was not accepted into durable storage");
                                let _=client.output.try_send((error("event_outbox_unavailable"),true));
                                continue;
                            }
                            // Notification only: even a full uplink cannot lose the persisted issue.
                            let _=uplink.try_send(message);
                            continue;
                        }
                        let terminal = if let RobotMessage::CommandResult {command_id,payload} = &message {
                            if matches!(payload.status,crate::local_protocol::CommandResultStatus::Succeeded | crate::local_protocol::CommandResultStatus::Rejected | crate::local_protocol::CommandResultStatus::Failed | crate::local_protocol::CommandResultStatus::Expired) { Some(command_id.clone()) } else { None }
                        } else if let RobotMessage::ServiceControlResult {command_id,payload} = &message {
                            if matches!(payload.status,crate::local_protocol::CommandResultStatus::Succeeded | crate::local_protocol::CommandResultStatus::Rejected | crate::local_protocol::CommandResultStatus::Failed | crate::local_protocol::CommandResultStatus::Expired) { Some(format!("service:{command_id}")) } else { None }
                        } else { None };
                        if uplink.try_send(message).is_err() { let _=client.output.try_send((error("queue_full"),true)); }
                        else if let Some(command)=terminal { delivered.remove(&command); }
                    }
                    Err(_) => { let _=client.output.try_send((error("invalid_message"),false)); }
                }
            }
        message = downlink.recv() => {
                let Some(message) = message else { return Ok(()); };
                if message.topic == "supervision" { supervision=Some(message.clone()); }
            let mut slow = Vec::new();
            for (&id,client) in &clients {
                    if client.policy.as_ref().is_some_and(|p|p.subscribe_routes.iter().any(|r|matches(r,&message.topic))) {
                        if client.output.try_send((message.clone(),false)).is_err() {
                            // Dropping only this output closes that client; peers remain available.
                        tracing::warn!(client=%client.client_id,"local client output queue unavailable");
                        slow.push(id);
                        } else if message.topic.starts_with("commands/")
                            && let Some(command) = message.payload.get("command_id").and_then(|v|v.as_str())
                                && delivered.len() < 1024 { delivered.insert(command.to_owned(),id); }
                        else if message.topic == "services/control"
                            && let Some(command) = message.payload.get("command_id").and_then(|v|v.as_str())
                                && delivered.len() < 1024 { delivered.insert(format!("service:{command}"),id); }
                    }
                }
            for id in slow { if let Some(c)=clients.remove(&id) && let Some(task)=c.abort {task.abort();} delivered.retain(|_,owner|*owner!=id); }
            update(&clients,&interfaces,&adapter);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_route_ownership() {
        assert!(matches("events/+", "events/low_battery"));
        assert!(!matches("events/+", "events/a/b"));
        assert!(!matches("services/status", "telemetry"));
    }
    #[test]
    fn registration_requires_configured_routes_and_unique_owners() {
        let config = LocalConfig::default();
        let clients = HashMap::new();
        let mut reg = Registration {
            schema: "fleetly.local-registration.v1".into(),
            client_id: "service-bridge".into(),
            publish_routes: vec!["services/status".into()],
            subscribe_routes: vec![],
        };
        assert!(validate_registration(&reg, &config, &clients).is_ok());
        reg.publish_routes.push("telemetry".into());
        assert!(validate_registration(&reg, &config, &clients).is_err());
        reg.publish_routes.pop();
        let (output, _) = mpsc::channel(2);
        let clients = [(
            1,
            Client {
                output,
                client_id: reg.client_id.clone(),
                abort: None,
                policy: Some(validate_registration(&reg, &config, &clients).unwrap()),
            },
        )]
        .into();
        assert!(validate_registration(&reg, &config, &clients).is_err());
    }

    async fn register(
        address: std::net::SocketAddr,
        client: &str,
        routes: &[&str],
    ) -> (
        tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
        serde_json::Value,
    ) {
        use tokio::io::AsyncBufReadExt;
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let registration = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"agent/register","payload":{"schema":"fleetly.local-registration.v1","client_id":client,"publish_routes":routes,"subscribe_routes":[]}});
        write
            .write_all(format!("{registration}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        (reader, write, serde_json::from_str(&line).unwrap())
    }

    #[tokio::test]
    async fn issue_is_durable_even_when_mqtt_uplink_is_full_and_gateway_stops() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("event-outbox.json");
        let owner = crate::event_outbox::Owner {
            hub: "https://hub.test".into(),
            installation_id: "i-test".into(),
            robot_id: "r-test".into(),
        };
        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::event_outbox::EventOutbox::open(&path, owner.clone()).unwrap(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (uplink, _observed) = mpsc::channel(1);
        let (_downlink, commands) = mpsc::channel(1);
        let line=serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"events/test","payload":{"schema":"robot.event.v1","event_id":"durable","ts":1793290000000_u64,"severity":"warning","source":"test"}}).to_string();
        uplink
            .try_send(parse_robot_message(&line).unwrap())
            .unwrap();
        let (adapter, _) = watch::channel(false);
        let (interfaces, _) = watch::channel(Interfaces::default());
        let server = tokio::spawn(serve_listener(
            listener,
            LocalConfig::default(),
            uplink,
            commands,
            adapter,
            interfaces,
            Some(queue),
        ));
        let (_reader, mut writer, registration) =
            register(address, "robot-adapter", &["events/+"]).await;
        assert_eq!(registration["topic"], "agent/registered");
        writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            while !path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.abort();
        let pending = crate::event_outbox::EventOutbox::open(&path, owner)
            .unwrap()
            .due(std::time::Instant::now())
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pending.payload).unwrap()["event_id"],
            "durable"
        );
    }

    #[tokio::test]
    async fn concurrent_registered_interfaces_and_isolated_failures() {
        use tokio::io::AsyncBufReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (uplink, mut observed) = mpsc::channel(32);
        let (_downlink, commands) = mpsc::channel(32);
        let (adapter, adapter_state) = watch::channel(false);
        let (interfaces, mut status) = watch::channel(Interfaces::default());
        let server = tokio::spawn(serve_listener(
            listener,
            LocalConfig::default(),
            uplink,
            commands,
            adapter,
            interfaces,
            None,
        ));
        let (gps_read, mut gps_write, gps) =
            register(address, "robot-adapter", &["telemetry"]).await;
        assert_eq!(gps["topic"], "agent/registered");
        let (mut bridge_read, mut bridge_write, bridge) =
            register(address, "service-bridge", &["services/status"]).await;
        assert_eq!(bridge["topic"], "agent/registered");
        assert!(status.borrow().robot_adapter && status.borrow().service_bridge);
        let (_, _, duplicate) = register(address, "service-bridge", &["services/status"]).await;
        assert_eq!(duplicate["payload"]["code"], "registration_rejected");
        bridge_write
            .write_all(
                b"{\"protocol\":\"fleetly.local.v1\",\"topic\":\"telemetry\",\"payload\":{}}\n",
            )
            .await
            .unwrap();
        let mut line = String::new();
        timeout(Duration::from_secs(2), bridge_read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["payload"]["code"],
            "route_not_owned"
        );
        bridge_write.write_all(b"{\"protocol\":\"fleetly.local.v1\",\"topic\":\"services/status\",\"payload\":{\"schema\":\"robot.services-status.v1\",\"ts\":1791025200000,\"services\":[]}}\n").await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), observed.recv())
                .await
                .unwrap()
                .unwrap(),
            RobotMessage::ServicesStatus(_)
        ));
        gps_write.write_all(b"{\"protocol\":\"fleetly.local.v1\",\"topic\":\"telemetry\",\"payload\":{\"schema\":\"robot.telemetry.v1\",\"ts\":1791025200000,\"battery_pct\":72,\"location\":{\"known\":false}}}\n").await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), observed.recv())
                .await
                .unwrap()
                .unwrap(),
            RobotMessage::Telemetry(_)
        ));
        drop(gps_read);
        drop(gps_write);
        timeout(Duration::from_secs(2), async {
            while status.borrow().robot_adapter {
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(!*adapter_state.borrow());
        assert!(status.borrow().service_bridge);
        bridge_write.write_all(b"{\"protocol\":\"fleetly.local.v1\",\"topic\":\"services/status\",\"payload\":{\"schema\":\"robot.services-status.v1\",\"ts\":1791025200000,\"services\":[]}}\n").await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), observed.recv())
                .await
                .unwrap()
                .unwrap(),
            RobotMessage::ServicesStatus(_)
        ));
        let (_gps_read, mut gps_write, new) =
            register(address, "robot-adapter", &["telemetry"]).await;
        assert_eq!(new["topic"], "agent/registered");
        drop(bridge_read);
        drop(bridge_write);
        timeout(Duration::from_secs(2), async {
            while status.borrow().service_bridge {
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(status.borrow().robot_adapter);
        gps_write.write_all(b"{\"protocol\":\"fleetly.local.v1\",\"topic\":\"telemetry\",\"payload\":{\"schema\":\"robot.telemetry.v1\",\"ts\":1791025200000,\"battery_pct\":72,\"location\":{\"known\":false}}}\n").await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), observed.recv())
                .await
                .unwrap()
                .unwrap(),
            RobotMessage::Telemetry(_)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn service_result_requires_command_delivery_to_current_owner() {
        use tokio::io::AsyncBufReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (uplink, mut messages) = mpsc::channel(8);
        let (downlink, commands) = mpsc::channel(8);
        let (adapter, _) = watch::channel(false);
        let (interfaces, status) = watch::channel(Interfaces::default());
        let server = tokio::spawn(serve_listener(
            listener,
            LocalConfig::default(),
            uplink,
            commands,
            adapter,
            interfaces,
            None,
        ));
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let registration = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"agent/register","payload":{"schema":"fleetly.local-registration.v1","client_id":"service-bridge","publish_routes":["services/status","services/control-results/+"],"subscribe_routes":["services/control"]}});
        write
            .write_all(format!("{registration}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["topic"],
            "agent/registered"
        );
        assert!(!status.borrow().robot_adapter && status.borrow().service_bridge);
        let result = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"services/control-results/service-1","payload":{"schema":"robot.service-control-result.v1","ts":1,"result_seq":1,"status":"succeeded"}});
        write
            .write_all(format!("{result}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["payload"]["code"],
            "wrong_result_owner"
        );
        assert!(messages.try_recv().is_err());
        downlink
            .send(OutgoingEnvelope::new(
                "services/control",
                serde_json::json!({"command_id":"service-1"}),
            ))
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["topic"],
            "services/control"
        );
        write
            .write_all(format!("{result}\n").as_bytes())
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), messages.recv())
                .await
                .unwrap()
                .unwrap(),
            RobotMessage::ServiceControlResult { .. }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn task_control_roundtrip_on_registered_tcp_interface() {
        use tokio::io::AsyncBufReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (uplink, mut messages) = mpsc::channel(16);
        let (downlink, commands) = mpsc::channel(16);
        let (adapter, _) = watch::channel(false);
        let (interfaces, _) = watch::channel(Interfaces::default());
        let server = tokio::spawn(serve_listener(
            listener,
            LocalConfig::default(),
            uplink,
            commands,
            adapter,
            interfaces,
            None,
        ));
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let registration = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"agent/register","payload":{
            "schema":"fleetly.local-registration.v1","client_id":"robot-adapter",
            "publish_routes":["state","command-results/+"],
            "subscribe_routes":["commands/stop","commands/pause","commands/resume"]}});
        write
            .write_all(format!("{registration}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["topic"],
            "agent/registered"
        );
        for (action, execution, available) in [
            ("pause", "paused", serde_json::json!(["stop", "resume"])),
            ("resume", "running", serde_json::json!(["stop", "pause"])),
            ("stop", "idle", serde_json::json!([])),
        ] {
            let id = format!("cmd-{action}");
            downlink
                .send(OutgoingEnvelope::new(
                    format!("commands/{action}"),
                    serde_json::json!({"command_id":id}),
                ))
                .await
                .unwrap();
            line.clear();
            timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["topic"],
                format!("commands/{action}")
            );
            let state = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":"state","payload":{
                "schema":"robot.state.v1","ts":1,"mode":"navigation","execution_state":execution,
                "available_commands":available,"fault_codes":[]}});
            write
                .write_all(format!("{state}\n").as_bytes())
                .await
                .unwrap();
            let received = timeout(Duration::from_secs(2), messages.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(received, RobotMessage::State(_)));
            let result = serde_json::json!({"protocol":LOCAL_PROTOCOL,"topic":format!("command-results/{id}"),
                "payload":{"schema":"robot.command-result.v1","ts":1,"result_seq":1,"status":"succeeded"}});
            write
                .write_all(format!("{result}\n").as_bytes())
                .await
                .unwrap();
            let received = timeout(Duration::from_secs(2), messages.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(received, RobotMessage::CommandResult {command_id, ..} if command_id == id)
            );
        }
        server.abort();
    }
}
