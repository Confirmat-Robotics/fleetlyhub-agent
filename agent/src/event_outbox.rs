//! Robot issues survive MQTT session replacement and process restarts.
//! A broker PUBACK is deliberately NOT a Hub/database receipt.
use std::{
    collections::VecDeque,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{contract::Topics, local_protocol::EventInput};

const MAX_MESSAGES: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const RETRY: Duration = Duration::from_secs(2);
pub type Shared = Arc<Mutex<EventOutbox>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub hub: String,
    pub installation_id: String,
    pub robot_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingEvent {
    pub topic: String,
    pub payload: String,
    pub sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    owner: Owner,
    events: VecDeque<PendingEvent>,
}

pub struct EventOutbox {
    path: PathBuf,
    saved: Saved,
    last_attempt: Option<(String, Instant)>,
}

impl std::fmt::Debug for EventOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventOutbox")
            .field("queued", &self.saved.events.len())
            .finish_non_exhaustive()
    }
}

pub fn digest(topic: &str, payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(topic.as_bytes());
    hash.update([0]);
    hash.update(payload);
    format!("{:x}", hash.finalize())
}

impl EventOutbox {
    pub fn open(path: &Path, owner: Owner) -> Result<Self> {
        let saved = if path.exists() {
            ensure!(
                fs::metadata(path)?.len() <= (MAX_BYTES * 2 + 1024 * 1024) as u64,
                "Event outbox file exceeds its limit"
            );
            let saved: Saved = serde_json::from_slice(&fs::read(path)?)
                .context("Cannot read event outbox; preserve it for recovery")?;
            ensure!(
                saved.version == 1 && saved.owner == owner,
                "Event outbox belongs to another Hub/installation; never replay it to another robot"
            );
            ensure!(
                saved.events.len() <= MAX_MESSAGES
                    && saved
                        .events
                        .iter()
                        .map(|e| e.topic.len() + e.payload.len())
                        .sum::<usize>()
                        <= MAX_BYTES,
                "Event outbox exceeds its capacity"
            );
            for event in &saved.events {
                ensure!(
                    event
                        .topic
                        .starts_with(&format!("{}/events/", Topics::new(&owner.robot_id).root))
                        && event.sha256 == digest(&event.topic, event.payload.as_bytes()),
                    "Invalid event outbox entry; preserve it for recovery"
                );
            }
            saved
        } else {
            Saved {
                version: 1,
                owner,
                events: VecDeque::new(),
            }
        };
        Ok(Self {
            path: path.to_owned(),
            saved,
            last_attempt: None,
        })
    }

    fn save(&self, events: &VecDeque<PendingEvent>) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("Event outbox has no directory")?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(
            file.as_file_mut(),
            &Saved {
                version: 1,
                owner: self.saved.owner.clone(),
                events: events.clone(),
            },
        )?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .map_err(|error| error.error)
            .context("Cannot atomically persist event outbox")?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }

    /// Persist before the local gateway accepts the message into its uplink.
    pub fn enqueue(&mut self, event_type: &str, payload: &EventInput) -> Result<()> {
        let topic = Topics::new(&self.saved.owner.robot_id).event(event_type);
        let payload = serde_json::to_string(payload)?;
        let sha256 = digest(&topic, payload.as_bytes());
        if self.saved.events.iter().any(|event| event.sha256 == sha256) {
            return Ok(());
        }
        ensure!(
            self.saved.events.len() < MAX_MESSAGES
                && self
                    .saved
                    .events
                    .iter()
                    .map(|e| e.topic.len() + e.payload.len())
                    .sum::<usize>()
                    + topic.len()
                    + payload.len()
                    <= MAX_BYTES,
            "Event outbox is full; message not accepted"
        );
        let mut next = self.saved.events.clone();
        next.push_back(PendingEvent {
            topic,
            payload,
            sha256,
        });
        self.save(&next)?;
        self.saved.events = next;
        Ok(())
    }

    pub fn due(&mut self, now: Instant) -> Option<PendingEvent> {
        let front = self.saved.events.front()?;
        if self.last_attempt.as_ref().is_some_and(|(sha, when)| {
            sha == &front.sha256 && now.saturating_duration_since(*when) < RETRY
        }) {
            return None;
        }
        self.last_attempt = Some((front.sha256.clone(), now));
        Some(front.clone())
    }

    pub fn reconnect(&mut self) {
        self.last_attempt = None;
    }

    pub fn receipt(&mut self, bytes: &[u8], retained: bool) -> Result<bool> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Receipt {
            schema: String,
            sha256: String,
        }
        if retained || bytes.len() > 256 {
            return Ok(false);
        }
        let Ok(receipt) = serde_json::from_slice::<Receipt>(bytes) else {
            return Ok(false);
        };
        if receipt.schema != "fleetly.event-receipt.v1"
            || !self
                .saved
                .events
                .front()
                .is_some_and(|e| e.sha256 == receipt.sha256)
        {
            return Ok(false);
        }
        let mut next = self.saved.events.clone();
        next.pop_front();
        self.save(&next)?;
        self.saved.events = next;
        self.last_attempt = None;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> Owner {
        Owner {
            hub: "https://hub.test".into(),
            installation_id: "installation".into(),
            robot_id: "r-test".into(),
        }
    }
    fn event(id: &str) -> EventInput {
        serde_json::from_value(serde_json::json!({"schema":"robot.event.v1","event_id":id,"ts":1793290000000_u64,"severity":"warning","source":"test"})).unwrap()
    }
    fn ack(event: &PendingEvent) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"schema":"fleetly.event-receipt.v1","sha256":event.sha256}),
        )
        .unwrap()
    }

    #[test]
    fn restart_and_new_mqtt_session_keep_original_order_and_bytes_until_hub_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.json");
        let mut outbox = EventOutbox::open(&path, owner()).unwrap();
        outbox.enqueue("warning", &event("one")).unwrap();
        outbox.enqueue("warning", &event("two")).unwrap();
        let first = outbox.due(Instant::now()).unwrap();
        assert!(outbox.due(Instant::now()).is_none());
        drop(outbox); // Includes loss of all MQTT in-flight state.
        let mut outbox = EventOutbox::open(&path, owner()).unwrap();
        assert_eq!(outbox.due(Instant::now()).unwrap().payload, first.payload);
        outbox.reconnect();
        assert_eq!(outbox.due(Instant::now()).unwrap().sha256, first.sha256);
        assert!(!outbox.receipt(&ack(&first), true).unwrap());
        let wrong = PendingEvent {
            sha256: "wrong".into(),
            ..first.clone()
        };
        assert!(!outbox.receipt(&ack(&wrong), false).unwrap());
        assert!(outbox.receipt(&ack(&first), false).unwrap());
        let second = outbox.due(Instant::now()).unwrap();
        assert_ne!(second.sha256, first.sha256);
        assert!(!outbox.receipt(&ack(&first), false).unwrap());
        assert!(outbox.receipt(&ack(&second), false).unwrap());
        assert!(
            EventOutbox::open(&path, owner())
                .unwrap()
                .due(Instant::now())
                .is_none()
        );
    }

    #[test]
    fn duplicate_capacity_corruption_and_assignment_change_do_not_erase_pending_issues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.json");
        let mut outbox = EventOutbox::open(&path, owner()).unwrap();
        outbox.enqueue("warning", &event("one")).unwrap();
        outbox.enqueue("warning", &event("one")).unwrap();
        assert_eq!(outbox.saved.events.len(), 1);
        let first = outbox.saved.events[0].clone();
        outbox.saved.events.resize(MAX_MESSAGES, first);
        assert!(outbox.enqueue("warning", &event("overflow")).is_err());
        assert_eq!(
            EventOutbox::open(&path, owner())
                .unwrap()
                .saved
                .events
                .len(),
            1
        );
        let mut other = owner();
        other.installation_id = "replacement".into();
        assert!(EventOutbox::open(&path, other).is_err());
        fs::write(&path, b"broken").unwrap();
        assert!(EventOutbox::open(&path, owner()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken");
    }

    #[test]
    fn retry_without_receipt_does_not_advance_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut outbox = EventOutbox::open(&dir.path().join("events.json"), owner()).unwrap();
        outbox.enqueue("warning", &event("one")).unwrap();
        let now = Instant::now();
        let first = outbox.due(now).unwrap();
        assert!(outbox.due(now + Duration::from_secs(1)).is_none());
        assert_eq!(outbox.due(now + RETRY).unwrap().sha256, first.sha256);
    }
}
