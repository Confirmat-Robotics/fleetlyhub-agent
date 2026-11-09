use std::{
    collections::HashMap,
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rumqttc::{
    AsyncClient, Event, LastWill, MqttOptions, Outgoing, Packet, QoS, TlsConfiguration, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::RuntimeConfig,
    contract::{CameraStatus, Presence, PresenceStatus, RobotState, Telemetry, Topics},
    local_protocol::{
        CameraStatusInput, CommandResultStatus, OutgoingEnvelope, RobotMessage, StateInput,
        TelemetryInput, validate_identifier, validate_no_identity_fields,
    },
    media::{MediaControl, validate_control},
};

pub async fn run(
    config: RuntimeConfig,
    mut uplink: mpsc::Receiver<RobotMessage>,
    downlink: mpsc::Sender<OutgoingEnvelope>,
    media_control: mpsc::Sender<MediaControl>,
    local_connected: watch::Receiver<bool>,
    mut interfaces: watch::Receiver<crate::local_gateway::Interfaces>,
    certificate_changes: watch::Receiver<u64>,
) -> Result<()> {
    let session_id = format!("ses-{}", Uuid::new_v4().simple());
    let topics = Topics::new(&config.robot_id);
    let offline = encode(&Presence::new(&session_id, PresenceStatus::Offline))?;

    let mut options = MqttOptions::new(
        format!("fleetly-agent-{}", &session_id[4..]),
        &config.mqtt.host,
        config.mqtt.port,
    );
    options.set_keep_alive(Duration::from_secs(config.mqtt.keep_alive_seconds));
    options.set_clean_session(true);
    options.set_last_will(LastWill::new(
        topics.presence.clone(),
        offline,
        QoS::AtLeastOnce,
        true,
    ));
    options.set_transport(tls_transport(&config.mqtt)?);

    let (client, event_loop) = AsyncClient::new(options, 64);
    let (events_tx, mut broker_events) = mpsc::channel(128);
    let mqtt_config = config.mqtt.clone();
    tokio::spawn(poll_broker_with_renewal(
        event_loop,
        events_tx,
        certificate_changes,
        mqtt_config,
    ));
    let mut ticker = interval(Duration::from_millis(config.telemetry_interval_ms));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut telemetry_seq = 0_u64;
    let mut state_seq = 0_u64;
    let mut camera_seq = 0_u64;
    let mut pending_telemetry: Option<TelemetryInput> = None;
    let mut pending_state: Option<StateInput> = None;
    let mut connected = false;
    let mut interface_seq = 0_u64;
    let mut service_seq = 0_u64;
    let mut interface_ticker = interval(Duration::from_secs(10));
    interface_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut delivered_commands = HashMap::<String, Value>::new();
    let mut delivered_services = HashMap::<String, Value>::new();
    let mut event_retry = interval(Duration::from_secs(2));
    event_retry.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = interfaces.changed() => {
                if changed.is_err() { return Ok(()); }
                let current = *interfaces.borrow_and_update();
                if !current.robot_adapter { pending_state=None;pending_telemetry=None; }
                if connected {
                    publish_interfaces(&client,&topics,&session_id,&mut interface_seq,current).await?;
                }
            }
            event = broker_events.recv() => {
                let Some(event) = event else { return Ok(()); };
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        connected = true;
                        subscribe_downlink(&client, &topics, config.event_outbox.is_some()).await?;
                        publish_json(
                            &client,
                            &topics.presence,
                            true,
                            &Presence::new(&session_id, PresenceStatus::Online),
                        ).await?;
                        if let Some(state) = pending_state.take() {
                            publish_state(&client, &topics, &session_id, &mut state_seq, state).await?;
                        }
                        let state = *interfaces.borrow();
                        publish_interfaces(&client, &topics, &session_id, &mut interface_seq, state).await?;
                        info!(robot_id = %config.robot_id, session_id, "connected to MQTT broker");
                        if let Some(outbox) = &config.event_outbox {
                            outbox.lock().map_err(|_|anyhow::anyhow!("Event outbox lock unavailable"))?.reconnect();
                        }
                        publish_due_event(&client, config.event_outbox.as_ref()).await?;
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        if publish.topic == format!("{}/event-receipts", topics.root) {
                            if let Some(outbox) = &config.event_outbox {
                                outbox.lock().map_err(|_|anyhow::anyhow!("Event outbox lock unavailable"))?
                                    .receipt(&publish.payload, publish.retain)?;
                                publish_due_event(&client, config.event_outbox.as_ref()).await?;
                            }
                            continue;
                        }
                        if publish.topic == format!("{}/services/control", topics.root) {
                            let bridge_connected = interfaces.borrow().service_bridge;
                            if !publish.retain {
                                route_service_control(publish.payload.as_ref(), &downlink, &mut delivered_services, bridge_connected).await;
                            }
                            continue;
                        }
                        if publish.retain && publish.topic.contains("/commands/") {
                            warn!(topic = %publish.topic, "ignored retained robot command");
                            continue;
                        }
                        route_downlink(
                            &topics,
                            publish.topic.as_str(),
                            publish.payload.as_ref(),
                            &downlink,
                            &media_control,
                            &mut delivered_commands,
                            &local_connected,
                        ).await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if connected {
                            warn!(%error, "MQTT connection lost; reconnecting");
                        } else {
                            warn!(%error, "MQTT connection failed; retrying");
                        }
                        connected = false;
                    }
                }
            }
            _ = ticker.tick() => {
                if connected && let Some(telemetry) = pending_telemetry.take() {
                    publish_telemetry(
                        &client,
                        &topics,
                        &session_id,
                        &mut telemetry_seq,
                        telemetry,
                    ).await?;
                }
            }
            message = uplink.recv() => {
                let Some(message) = message else { return Ok(()); };
                match message {
                    RobotMessage::ServiceControlResult { command_id, payload } => {
                        if connected && delivered_services.contains_key(&command_id) {
                            publish_json(&client, &format!("{}/services/control-results/{command_id}",topics.root), false, &payload).await?;
                        }
                    }
                    RobotMessage::ServicesStatus(input) => {
                        // Never cache/replay a bridge snapshot across disconnection.
                        if connected && interfaces.borrow().service_bridge {
                            let mut payload = serde_json::to_value(input)?;
                            payload["session_id"] = serde_json::json!(session_id);
                            payload["seq"] = serde_json::json!(service_seq);
                            publish_value(&client, &format!("{}/services/status",topics.root), true, payload).await?;
                            service_seq = service_seq.saturating_add(1);
                        }
                    }
                    RobotMessage::Telemetry(telemetry) => pending_telemetry = Some(telemetry),
                    RobotMessage::State(state) => {
                        if connected {
                            publish_state(&client, &topics, &session_id, &mut state_seq, state).await?;
                        } else {
                            pending_state = Some(state);
                        }
                    }
                    RobotMessage::Event { event_type, payload } => {
                        if config.event_outbox.is_some() {
                            // Already persisted by the registered local gateway. This is a wakeup,
                            // not a second enqueue (a fast receipt may already have removed it).
                            if connected { publish_due_event(&client, config.event_outbox.as_ref()).await?; }
                        } else {
                            publish_json(&client, &topics.event(&event_type), false, &payload).await?;
                        }
                    }
                    RobotMessage::CameraStatus(status) => {
                        publish_camera_status(
                            &client,
                            &topics,
                            &session_id,
                            &mut camera_seq,
                            status,
                        ).await?;
                    }
                    RobotMessage::CommandResult { command_id, payload } => {
                        if delivered_commands.contains_key(&command_id) {
                            publish_json(&client, &topics.command_result(&command_id), false, &payload).await?;
                            if matches!(
                                payload.status,
                                CommandResultStatus::Succeeded
                                    | CommandResultStatus::Rejected
                                    | CommandResultStatus::Failed
                                    | CommandResultStatus::Expired
                            ) {
                                delivered_commands.remove(&command_id);
                            }
                        } else {
                            let error = OutgoingEnvelope::new(
                                "agent/error",
                                serde_json::json!({
                                    "schema": "fleetly.local-error.v1",
                                    "code": "unknown_command_id",
                                    "message": "command result does not match a command delivered by this Agent",
                                }),
                            );
                            let _ = downlink.send(error).await;
                        }
                    }
                }
            }
            _ = interface_ticker.tick() => {
                if connected { let state = *interfaces.borrow(); publish_interfaces(&client,&topics,&session_id,&mut interface_seq,state).await?; }
            }
            _ = event_retry.tick(), if connected => {
                publish_due_event(&client, config.event_outbox.as_ref()).await?;
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for Ctrl-C")?;
                info!("shutdown requested");
                graceful_disconnect(&client, &mut broker_events, &topics, &session_id).await;
                return Ok(());
            }
        }
    }
}

async fn publish_due_event(
    client: &AsyncClient,
    outbox: Option<&crate::event_outbox::Shared>,
) -> Result<()> {
    let event = match outbox {
        Some(queue) => queue
            .lock()
            .map_err(|_| anyhow::anyhow!("Event outbox lock unavailable"))?
            .due(std::time::Instant::now()),
        None => None,
    };
    if let Some(event) = event {
        client
            .publish(event.topic, QoS::AtLeastOnce, false, event.payload)
            .await?;
    }
    Ok(())
}

// The transport must keep polling while a publisher waits for space in the
// MQTT request queue. Polling and publishing in one task can deadlock after a
// disconnect or a burst larger than that queue. Outgoing traffic needs no
// application event, so it never fills the bounded downlink queue.
#[cfg(test)]
async fn poll_broker(
    event_loop: rumqttc::EventLoop,
    events: mpsc::Sender<std::result::Result<Event, rumqttc::ConnectionError>>,
) {
    poll_transport(event_loop, events, None).await;
}

fn tls_transport(config: &crate::config::MqttConfig) -> Result<Transport> {
    Ok(Transport::Tls(TlsConfiguration::Simple {
        ca: read_tls_file(&config.ca_certificate)?,
        client_auth: Some((
            read_tls_file(&config.client_certificate)?,
            read_tls_file(&config.private_key)?,
        )),
        alpn: None,
    }))
}

async fn poll_broker_with_renewal(
    event_loop: rumqttc::EventLoop,
    events: mpsc::Sender<std::result::Result<Event, rumqttc::ConnectionError>>,
    changes: watch::Receiver<u64>,
    config: crate::config::MqttConfig,
) {
    poll_transport(event_loop, events, Some((changes, config))).await;
}

async fn poll_transport(
    mut event_loop: rumqttc::EventLoop,
    events: mpsc::Sender<std::result::Result<Event, rumqttc::ConnectionError>>,
    mut renewal: Option<(watch::Receiver<u64>, crate::config::MqttConfig)>,
) {
    let mut retry_certificate: Option<crate::config::MqttConfig> = None;
    loop {
        let event = tokio::select! {
            _ = events.closed() => return,
            changed = async {
                match renewal.as_mut() {
                    Some((changes,_)) => changes.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() { renewal = None; continue; }
                let config = &renewal.as_ref().expect("renewal receiver exists").1;
                match tls_transport(config) {
                    Ok(transport) => {
                        event_loop.mqtt_options.set_transport(transport);
                        event_loop.clean(); // Keeps queued/in-flight requests and process client id.
                        retry_certificate = None;
                    }
                    Err(error) => {
                        warn!(%error, "Cannot load renewed MQTT certificate; retrying without ending local/media sessions");
                        retry_certificate = Some(config.clone());
                    }
                }
                continue;
            }
            _ = sleep(Duration::from_secs(2)), if retry_certificate.is_some() => {
                if let Ok(transport) = tls_transport(retry_certificate.as_ref().expect("certificate retry is pending")) {
                    event_loop.mqtt_options.set_transport(transport);
                    event_loop.clean();
                    retry_certificate = None;
                }
                continue;
            }
            event = event_loop.poll() => event,
        };
        let failed = event.is_err();
        if matches!(
            &event,
            Ok(Event::Incoming(Packet::ConnAck(_) | Packet::Publish(_)))
                | Ok(Event::Outgoing(Outgoing::Disconnect))
                | Err(_)
        ) && events.send(event).await.is_err()
        {
            return;
        }
        if failed {
            sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn subscribe_downlink(
    client: &AsyncClient,
    topics: &Topics,
    event_receipts: bool,
) -> Result<()> {
    client
        .subscribe(
            format!("{}/services/control", topics.root),
            QoS::AtLeastOnce,
        )
        .await?;
    client
        .subscribe(topics.commands_filter(), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(topics.supervision(), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(topics.media_control(), QoS::AtLeastOnce)
        .await?;
    if event_receipts {
        client
            .subscribe(format!("{}/event-receipts", topics.root), QoS::AtLeastOnce)
            .await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceControlPayload {
    schema: String,
    command_id: String,
    issued_at: u64,
    expires_at: u64,
    service: String,
    action: String,
}

#[cfg(test)]
mod service_control_tests {
    use super::*;
    fn command() -> Value {
        let now = unix_time_ms();
        serde_json::json!({"schema":"robot.service-control.v1","command_id":"test-1","issued_at":now,"expires_at":now+30000,"service":"navigation.service","action":"restart"})
    }
    #[tokio::test]
    async fn bridge_controls_are_bounded_strict_and_independent_of_robot_adapter() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut delivered = HashMap::new();
        let c = command();
        route_service_control(&serde_json::to_vec(&c).unwrap(), &tx, &mut delivered, false).await;
        assert!(rx.try_recv().is_err());
        route_service_control(&serde_json::to_vec(&c).unwrap(), &tx, &mut delivered, true).await;
        assert_eq!(rx.recv().await.unwrap().topic, "services/control");
        let mut conflict = c.clone();
        conflict["action"] = serde_json::json!("stop");
        route_service_control(
            &serde_json::to_vec(&conflict).unwrap(),
            &tx,
            &mut delivered,
            true,
        )
        .await;
        assert!(rx.try_recv().is_err());
        for (field, value) in [
            ("expires_at", serde_json::json!(1)),
            ("service", serde_json::json!("../bad.service")),
            ("issued_by_userid", serde_json::json!("forged")),
            ("action", serde_json::json!("Start")),
        ] {
            let mut invalid = command();
            invalid[field] = value;
            route_service_control(
                &serde_json::to_vec(&invalid).unwrap(),
                &tx,
                &mut delivered,
                true,
            )
            .await;
            assert!(rx.try_recv().is_err());
        }
        // An identical redelivery is permitted for the bridge's durable idempotency journal.
        route_service_control(&serde_json::to_vec(&c).unwrap(), &tx, &mut delivered, true).await;
        assert!(rx.try_recv().is_ok());
        assert_eq!(delivered.len(), 1);
    }
    #[test]
    fn service_results_have_their_own_schema_and_no_identity_fields() {
        let valid = serde_json::json!({"protocol":"fleetly.local.v1","topic":"services/control-results/test-1","payload":{"schema":"robot.service-control-result.v1","ts":unix_time_ms(),"result_seq":1,"status":"succeeded"}});
        assert!(matches!(
            crate::local_protocol::parse_robot_message(&valid.to_string()).unwrap(),
            RobotMessage::ServiceControlResult { .. }
        ));
        for (field, value) in [
            ("schema", serde_json::json!("robot.command-result.v1")),
            ("result", serde_json::json!({"status":"forged"})),
            ("user_id", serde_json::json!("forged")),
        ] {
            let mut invalid = valid.clone();
            invalid["payload"][field] = value;
            assert!(crate::local_protocol::parse_robot_message(&invalid.to_string()).is_err());
        }
    }
}

async fn route_service_control(
    bytes: &[u8],
    downlink: &mpsc::Sender<OutgoingEnvelope>,
    delivered: &mut HashMap<String, Value>,
    bridge_connected: bool,
) {
    if bytes.len() > 16384 || !bridge_connected {
        return;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    let Ok(command) = serde_json::from_value::<ServiceControlPayload>(payload.clone()) else {
        return;
    };
    let now = unix_time_ms();
    let valid_unit = command
        .service
        .strip_suffix(".service")
        .is_some_and(|name| {
            !name.is_empty()
                && name.len() <= 120
                && name.as_bytes()[0].is_ascii_alphanumeric()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.@:-".contains(&c))
        });
    if command.schema != "robot.service-control.v1"
        || validate_identifier(&command.command_id, 128).is_err()
        || !valid_unit
        || !matches!(command.action.as_str(), "start" | "stop" | "restart")
        || command.issued_at == 0
        || command.issued_at > now.saturating_add(5000)
        || command.expires_at <= now
        || command.expires_at < command.issued_at
        || command.expires_at - command.issued_at > 30_000
    {
        return;
    }
    delivered.retain(|_, v| {
        v["expires_at"]
            .as_u64()
            .is_some_and(|t| t.saturating_add(60_000) > now)
    });
    if delivered
        .get(&command.command_id)
        .is_some_and(|old| old != &payload)
        || (!delivered.contains_key(&command.command_id) && delivered.len() >= 1024)
    {
        return;
    }
    if downlink
        .try_send(OutgoingEnvelope::new("services/control", payload.clone()))
        .is_ok()
    {
        delivered.insert(command.command_id, payload);
    }
}

async fn route_downlink(
    topics: &Topics,
    mqtt_topic: &str,
    bytes: &[u8],
    downlink: &mpsc::Sender<OutgoingEnvelope>,
    media_control: &mpsc::Sender<MediaControl>,
    delivered_commands: &mut HashMap<String, Value>,
    local_connected: &watch::Receiver<bool>,
) {
    let Some(local_topic) = mqtt_topic.strip_prefix(&format!("{}/", topics.root)) else {
        warn!(%mqtt_topic, "ignored MQTT message outside robot root");
        return;
    };
    let expected_schema = if local_topic.starts_with("commands/") {
        "robot.command.v1"
    } else if local_topic == "supervision" {
        "robot.supervision.v1"
    } else if local_topic == "media/control" {
        "fleetly.media-control.v2"
    } else {
        warn!(%mqtt_topic, "ignored MQTT message on unsupported downlink route");
        return;
    };

    if bytes.len() > 16_384 {
        warn!(%mqtt_topic, size = bytes.len(), "ignored oversized MQTT downlink payload");
        return;
    }
    let payload: Value = match serde_json::from_slice(bytes) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(%mqtt_topic, %error, "ignored malformed MQTT downlink payload");
            return;
        }
    };
    if payload.get("schema").and_then(Value::as_str) != Some(expected_schema) {
        warn!(%mqtt_topic, %expected_schema, "ignored MQTT downlink payload with wrong schema");
        return;
    }

    if local_topic == "media/control" {
        let control: MediaControl = match serde_json::from_value(payload) {
            Ok(control) => control,
            Err(error) => {
                warn!(%mqtt_topic, %error, "ignored invalid media control payload");
                return;
            }
        };
        if let Err(error) = validate_control(&control) {
            warn!(%mqtt_topic, %error, "ignored invalid media control payload");
            return;
        }
        if media_control.send(control).await.is_err() {
            warn!(%mqtt_topic, "media supervisor is unavailable");
        }
        return;
    }

    let mut validated_command_id = None;
    if local_topic.starts_with("commands/") {
        let command_type = local_topic.trim_start_matches("commands/");
        if !matches!(command_type, "pause" | "stop" | "resume") {
            warn!(%mqtt_topic, %command_type, "ignored unsupported command type");
            return;
        }
        let command: CommandPayload = match serde_json::from_value(payload.clone()) {
            Ok(command) => command,
            Err(error) => {
                warn!(%mqtt_topic, %error, "ignored invalid command payload");
                return;
            }
        };
        if command.expires_at < unix_time_ms() || command.issued_at > command.expires_at {
            warn!(command_id = %command.command_id, "ignored expired or invalid command");
            return;
        }
        if validate_identifier(&command.command_id, 128).is_err() {
            warn!(command_id = %command.command_id, "ignored command with invalid command_id");
            return;
        }
        if command.schema != "robot.command.v1"
            || !command.parameters.is_object()
            || command
                .parameters
                .as_object()
                .is_some_and(|value| value.len() > 32)
        {
            warn!(command_id = %command.command_id, "ignored command with invalid schema or parameters");
            return;
        }
        if let Err(error) = validate_no_identity_fields(&command.parameters) {
            warn!(command_id = %command.command_id, %error, "ignored command with forbidden metadata");
            return;
        }
        if !*local_connected.borrow() {
            warn!(command_id = %command.command_id, "ignored command because local robot software is disconnected");
            return;
        }
        if let Some(previous) = delivered_commands.get(&command.command_id)
            && previous != &serde_json::json!({"topic":local_topic,"payload":payload})
        {
            warn!(command_id = %command.command_id, "ignored conflicting duplicate command");
            return;
        }
        if !delivered_commands.contains_key(&command.command_id)
            && delivered_commands.len() >= 1_024
        {
            warn!(command_id = %command.command_id, "ignored command because pending command capacity is exhausted");
            return;
        }
        validated_command_id = Some(command.command_id);
    } else {
        let supervision: SupervisionPayload = match serde_json::from_value(payload.clone()) {
            Ok(supervision) => supervision,
            Err(error) => {
                warn!(%mqtt_topic, %error, "ignored invalid supervision payload");
                return;
            }
        };
        if supervision.schema != "robot.supervision.v1"
            || supervision.revision == 0
            || supervision.updated_at == 0
            || supervision.expires_at == 0
            || supervision.updated_at > supervision.expires_at
            || (!supervision.active && supervision.active_session_count != 0)
        {
            warn!(%mqtt_topic, "ignored supervision payload outside contract bounds");
            return;
        }
    }

    if downlink
        .send(OutgoingEnvelope::new(local_topic, payload.clone()))
        .await
        .is_err()
    {
        warn!(%mqtt_topic, "local Robot-Agent server is unavailable");
    } else if let Some(command_id) = validated_command_id {
        delivered_commands.insert(
            command_id,
            serde_json::json!({"topic":local_topic,"payload":payload}),
        );
    }
}

async fn publish_telemetry(
    client: &AsyncClient,
    topics: &Topics,
    session_id: &str,
    sequence: &mut u64,
    input: TelemetryInput,
) -> Result<()> {
    let payload = Telemetry {
        schema: "robot.telemetry.v1",
        ts: input.ts,
        session_id,
        seq: *sequence,
        battery_pct: input.battery_pct,
        temperature_c: input.temperature_c,
        location: input.location,
    };
    publish_json(client, &topics.telemetry, false, &payload).await?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

async fn publish_state(
    client: &AsyncClient,
    topics: &Topics,
    session_id: &str,
    sequence: &mut u64,
    input: StateInput,
) -> Result<()> {
    let payload = RobotState {
        schema: "robot.state.v1",
        ts: input.ts,
        session_id,
        seq: *sequence,
        mode: input.mode,
        execution_state: input.execution_state,
        available_commands: input.available_commands,
        fault_codes: input.fault_codes,
    };
    publish_json(client, &topics.state, true, &payload).await?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

async fn publish_camera_status(
    client: &AsyncClient,
    topics: &Topics,
    session_id: &str,
    sequence: &mut u64,
    input: CameraStatusInput,
) -> Result<()> {
    let payload = CameraStatus {
        schema: "robot.camera-status.v1",
        ts: input.ts,
        session_id,
        seq: *sequence,
        status: input.status,
        error_code: input.error_code,
    };
    publish_json(client, &topics.camera_status, true, &payload).await?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

async fn publish_interfaces(
    client: &AsyncClient,
    topics: &Topics,
    session: &str,
    sequence: &mut u64,
    interfaces: crate::local_gateway::Interfaces,
) -> Result<()> {
    let status = |active| if active { "connected" } else { "disconnected" };
    publish_value(client,&format!("{}/interfaces/status",topics.root),true,serde_json::json!({
        "schema":"robot.interfaces-status.v1","ts":unix_time_ms(),"session_id":session,"seq":*sequence,
        "interfaces":[{"interface":"robot_adapter","status":status(interfaces.robot_adapter)},{"interface":"service_bridge","status":status(interfaces.service_bridge)}]
    })).await?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

async fn publish_json<T: Serialize>(
    client: &AsyncClient,
    topic: &str,
    retain: bool,
    payload: &T,
) -> Result<()> {
    publish_value(
        client,
        topic,
        retain,
        serde_json::to_value(payload).context("failed to encode MQTT payload")?,
    )
    .await
}

async fn publish_value(
    client: &AsyncClient,
    topic: &str,
    retain: bool,
    payload: Value,
) -> Result<()> {
    client
        .publish(topic, QoS::AtLeastOnce, retain, encode(&payload)?)
        .await
        .with_context(|| format!("failed to queue MQTT publication to {topic}"))
}

async fn graceful_disconnect(
    client: &AsyncClient,
    events: &mut mpsc::Receiver<std::result::Result<Event, rumqttc::ConnectionError>>,
    topics: &Topics,
    session_id: &str,
) {
    if let Err(error) = publish_json(
        client,
        &topics.presence,
        true,
        &Presence::new(session_id, PresenceStatus::Offline),
    )
    .await
    {
        warn!(%error, "failed to queue offline presence");
        return;
    }
    if let Err(error) = client.disconnect().await {
        warn!(%error, "failed to queue MQTT disconnect");
        return;
    }

    let drain = async {
        loop {
            match events.recv().await {
                Some(Ok(Event::Outgoing(Outgoing::Disconnect))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    warn!(%error, "MQTT stopped while publishing offline presence");
                    return;
                }
            }
        }
    };
    if timeout(Duration::from_secs(3), drain).await.is_err() {
        warn!("timed out while publishing offline presence");
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("failed to encode MQTT payload")
}

fn read_tls_file(path: &std::path::Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read TLS file {}", path.display()))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandPayload {
    schema: String,
    command_id: String,
    issued_at: u64,
    expires_at: u64,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupervisionPayload {
    schema: String,
    revision: u64,
    active: bool,
    active_session_count: u64,
    updated_at: u64,
    expires_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_broker_session_replays_issue_until_database_receipt_not_broker_ack() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn packet(socket: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
            let header = socket.read_u8().await.unwrap();
            let mut length = 0;
            let mut shift = 0;
            loop {
                let byte = socket.read_u8().await.unwrap();
                length |= ((byte & 127) as usize) << shift;
                if byte & 128 == 0 {
                    break;
                }
                shift += 7;
            }
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            (header, bytes)
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = tokio::spawn(async move {
            let mut offered = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert_eq!(packet(&mut socket).await.0 >> 4, 1);
                socket.write_all(&[0x20, 2, 0, 0]).await.unwrap(); // New session: rumqttc discards its old pending queue.
                let (header, bytes) = packet(&mut socket).await;
                assert_eq!(header >> 4, 3);
                let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                let offset = 2 + length;
                let topic = String::from_utf8(bytes[2..offset].to_vec()).unwrap();
                let payload = bytes[offset + 2..].to_vec();
                socket
                    .write_all(&[0x40, 2, bytes[offset], bytes[offset + 1]])
                    .await
                    .unwrap();
                offered.push((topic.clone(), payload.clone()));
                if attempt == 0 {
                    continue;
                } // Broker accepted it, but no Hub receipt was delivered.
                let receipt=serde_json::to_vec(&serde_json::json!({"schema":"fleetly.event-receipt.v1","sha256":crate::event_outbox::digest(&topic,&payload)})).unwrap();
                let topic = b"fleetly/v2/robots/r-test/event-receipts";
                let mut body = Vec::new();
                body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
                body.extend_from_slice(topic);
                body.extend(receipt);
                let mut publish = vec![0x30];
                let mut size = body.len();
                loop {
                    let mut byte = (size % 128) as u8;
                    size /= 128;
                    if size > 0 {
                        byte |= 128;
                    }
                    publish.push(byte);
                    if size == 0 {
                        break;
                    }
                }
                publish.extend(body);
                socket.write_all(&publish).await.unwrap();
                assert_eq!(packet(&mut socket).await.0 >> 4, 14);
            }
            offered
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.json");
        let owner = crate::event_outbox::Owner {
            hub: "https://hub.test".into(),
            installation_id: "i-test".into(),
            robot_id: "r-test".into(),
        };
        let mut saved = crate::event_outbox::EventOutbox::open(&path, owner.clone()).unwrap();
        let payload=serde_json::from_value(serde_json::json!({"schema":"robot.event.v1","event_id":"lost-receipt","ts":1793290000000_u64,"severity":"warning","source":"test"})).unwrap();
        saved.enqueue("test", &payload).unwrap();
        let outbox = std::sync::Arc::new(std::sync::Mutex::new(saved));
        let (client, event_loop) =
            AsyncClient::new(MqttOptions::new("receipt-test", "127.0.0.1", port), 64);
        let (send, mut events) = mpsc::channel(32);
        let transport = tokio::spawn(poll_broker(event_loop, send));
        timeout(Duration::from_secs(8), async {
            loop {
                match events.recv().await.unwrap() {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        outbox.lock().unwrap().reconnect();
                        publish_due_event(&client, Some(&outbox)).await.unwrap();
                    }
                    Ok(Event::Incoming(Packet::Publish(receipt))) => {
                        assert!(
                            outbox
                                .lock()
                                .unwrap()
                                .receipt(&receipt.payload, receipt.retain)
                                .unwrap()
                        );
                        break;
                    }
                    Err(_) => {
                        assert!(path.exists());
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("Stored issue must replay after a new MQTT session and wait for its Hub receipt");
        client.disconnect().await.unwrap();
        while !matches!(
            events.recv().await,
            Some(Ok(Event::Outgoing(Outgoing::Disconnect)))
        ) {}
        let offered = broker.await.unwrap();
        assert_eq!(offered.len(), 2);
        assert_eq!(offered[0], offered[1]);
        assert!(
            crate::event_outbox::EventOutbox::open(&path, owner)
                .unwrap()
                .due(std::time::Instant::now())
                .is_none()
        );
        drop(events);
        transport.await.unwrap();
    }

    #[tokio::test]
    async fn publisher_burst_larger_than_request_queue_does_not_block_transport() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let broker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = 0;
            loop {
                let header = socket.read_u8().await.unwrap();
                let mut size = 0;
                let mut shift = 0;
                loop {
                    let byte = socket.read_u8().await.unwrap();
                    size |= ((byte & 127) as usize) << shift;
                    if byte & 128 == 0 {
                        break;
                    }
                    shift += 7;
                }
                let mut packet = vec![0; size];
                socket.read_exact(&mut packet).await.unwrap();
                match header >> 4 {
                    1 => socket.write_all(&[0x20, 2, 0, 0]).await.unwrap(),
                    3 => {
                        let topic_length = u16::from_be_bytes([packet[0], packet[1]]) as usize;
                        let offset = 2 + topic_length;
                        socket
                            .write_all(&[0x40, 2, packet[offset], packet[offset + 1]])
                            .await
                            .unwrap();
                        received += 1;
                    }
                    14 => return received,
                    _ => panic!("Unexpected test MQTT packet"),
                }
            }
        });
        let (client, event_loop) =
            AsyncClient::new(MqttOptions::new("burst-test", "127.0.0.1", port), 64);
        let (sender, mut events) = mpsc::channel(128);
        let transport = tokio::spawn(poll_broker(event_loop, sender));
        timeout(Duration::from_secs(5), async {
            for _ in 0..200 {
                client
                    .publish("test/burst", QoS::AtLeastOnce, false, "sample")
                    .await
                    .unwrap();
            }
            client.disconnect().await.unwrap();
            while let Some(event) = events.recv().await {
                if matches!(event, Ok(Event::Outgoing(Outgoing::Disconnect))) {
                    break;
                }
            }
        })
        .await
        .expect("Transport must drain requests while publisher awaits queue space");
        assert_eq!(broker.await.unwrap(), 200);
        drop(events);
        transport.await.unwrap();
    }

    #[test]
    fn timestamps_are_epoch_milliseconds() {
        assert!(unix_time_ms() > 1_000_000_000_000);
    }

    #[test]
    fn command_payload_keeps_server_owned_fields() {
        let command: CommandPayload = serde_json::from_value(serde_json::json!({
            "schema": "robot.command.v1",
            "command_id": "cmd-test",
            "issued_at": 1,
            "expires_at": 2,
            "parameters": {}
        }))
        .unwrap();
        assert_eq!(command.schema, "robot.command.v1");
        assert!(command.parameters.is_object());
    }

    #[tokio::test]
    async fn command_is_not_recorded_when_local_robot_is_disconnected() {
        let topics = Topics::new("robot1");
        let (downlink, mut receiver) = mpsc::channel(2);
        let (media, _media_receiver) = mpsc::channel(2);
        let (_status, connected) = watch::channel(false);
        let mut delivered = HashMap::new();
        let now = unix_time_ms();
        let payload = serde_json::json!({
            "schema": "robot.command.v1",
            "command_id": "cmd-disconnected",
            "issued_at": now,
            "expires_at": now + 60_000,
            "parameters": {}
        });

        route_downlink(
            &topics,
            &format!("{}/commands/stop", topics.root),
            &serde_json::to_vec(&payload).unwrap(),
            &downlink,
            &media,
            &mut delivered,
            &connected,
        )
        .await;

        assert!(delivered.is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn conflicting_command_retry_is_not_forwarded() {
        let topics = Topics::new("robot1");
        let (downlink, mut receiver) = mpsc::channel(2);
        let (media, _media_receiver) = mpsc::channel(2);
        let (_status, connected) = watch::channel(true);
        let mut delivered = HashMap::new();
        let now = unix_time_ms();
        let first = serde_json::json!({
            "schema": "robot.command.v1",
            "command_id": "cmd-conflict",
            "issued_at": now,
            "expires_at": now + 60_000,
            "parameters": {"speed": 1}
        });
        let conflicting = serde_json::json!({
            "schema": "robot.command.v1",
            "command_id": "cmd-conflict",
            "issued_at": now,
            "expires_at": now + 60_000,
            "parameters": {"speed": 2}
        });

        for payload in [&first, &conflicting] {
            route_downlink(
                &topics,
                &format!("{}/commands/pause", topics.root),
                &serde_json::to_vec(payload).unwrap(),
                &downlink,
                &media,
                &mut delivered,
                &connected,
            )
            .await;
        }

        let forwarded = receiver.recv().await.unwrap();
        assert_eq!(forwarded.payload, first);
        assert!(receiver.try_recv().is_err());
        assert_eq!(delivered.len(), 1);
    }

    #[tokio::test]
    async fn pause_resume_routes_reject_start_and_conflicting_action_replay() {
        let topics = Topics::new("robot1");
        let (downlink, mut receiver) = mpsc::channel(8);
        let (media, _media_receiver) = mpsc::channel(2);
        let (_status, connected) = watch::channel(true);
        let mut delivered = HashMap::new();
        let now = unix_time_ms();
        for (action, id, expected) in [
            ("start", "old-start", false),
            ("pause", "pause-1", true),
            ("resume", "pause-1", false), // Same UUID, different intention.
            ("resume", "resume-1", true),
            ("stop", "stop-1", true),
        ] {
            let payload = serde_json::json!({"schema":"robot.command.v1", "command_id":id,
                "issued_at":now, "expires_at":now+30000, "parameters":{}});
            route_downlink(
                &topics,
                &format!("{}/commands/{action}", topics.root),
                &serde_json::to_vec(&payload).unwrap(),
                &downlink,
                &media,
                &mut delivered,
                &connected,
            )
            .await;
            if expected {
                let forwarded = receiver.try_recv().unwrap();
                assert_eq!(forwarded.topic, format!("commands/{action}"));
                assert_eq!(forwarded.payload, payload);
            } else {
                assert!(receiver.try_recv().is_err());
            }
        }
        assert_eq!(delivered.len(), 3);
    }
}
