use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use nocturne_core::{CoreEvent, CoreEventEnvelope, EventPublisherPort, PortError};
use tokio::sync::broadcast;

use crate::recover_lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCursorError {
    CursorNotFound,
}

#[derive(Debug, Clone)]
pub struct LocalEventLog {
    entries: Arc<Mutex<VecDeque<CoreEventEnvelope<CoreEvent>>>>,
    max_entries: usize,
}

impl LocalEventLog {
    const DEFAULT_MAX_ENTRIES: usize = 256;

    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(max_entries))),
            max_entries,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<CoreEventEnvelope<CoreEvent>> {
        recover_lock(&self.entries).iter().cloned().collect()
    }

    pub fn push(&self, event: CoreEventEnvelope<CoreEvent>) {
        let mut entries = recover_lock(&self.entries);
        if entries.len() >= self.max_entries {
            entries.pop_front();
        }
        entries.push_back(event);
    }

    #[must_use]
    pub fn latest_event_id(&self) -> Option<String> {
        recover_lock(&self.entries)
            .back()
            .map(|event| event.event_id.clone())
    }

    #[must_use]
    pub fn events_after(
        &self,
        event_id: Option<&str>,
    ) -> Result<Vec<CoreEventEnvelope<CoreEvent>>, EventCursorError> {
        let Some(event_id) = event_id else {
            return Ok(Vec::new());
        };

        let entries = recover_lock(&self.entries);
        let Some(index) = entries.iter().position(|event| event.event_id == event_id) else {
            return Err(EventCursorError::CursorNotFound);
        };

        Ok(entries.iter().skip(index + 1).cloned().collect())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        recover_lock(&self.entries).len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        recover_lock(&self.entries).is_empty()
    }
}

impl Default for LocalEventLog {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_ENTRIES)
    }
}

#[derive(Debug, Clone)]
pub struct LocalEventPublisher {
    log: LocalEventLog,
}

impl LocalEventPublisher {
    #[must_use]
    pub fn new(log: LocalEventLog) -> Self {
        Self { log }
    }
}

#[derive(Debug, Clone)]
pub struct BroadcastEventPublisher {
    log: LocalEventLog,
    tx: broadcast::Sender<CoreEventEnvelope<CoreEvent>>,
}

impl BroadcastEventPublisher {
    #[must_use]
    pub fn new(log: LocalEventLog, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { log, tx }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEventEnvelope<CoreEvent>> {
        self.tx.subscribe()
    }

    pub fn subscribe_after(
        &self,
        event_id: Option<&str>,
    ) -> Result<
        (
            broadcast::Receiver<CoreEventEnvelope<CoreEvent>>,
            Vec<CoreEventEnvelope<CoreEvent>>,
        ),
        EventCursorError,
    > {
        let receiver = self.tx.subscribe();
        let replay = self.log.events_after(event_id)?;
        Ok((receiver, replay))
    }

    #[must_use]
    pub fn log(&self) -> &LocalEventLog {
        &self.log
    }
}

impl EventPublisherPort for LocalEventPublisher {
    fn publish(&mut self, event: CoreEventEnvelope<CoreEvent>) -> Result<(), PortError> {
        self.log.push(event);
        Ok(())
    }
}

impl EventPublisherPort for BroadcastEventPublisher {
    fn publish(&mut self, event: CoreEventEnvelope<CoreEvent>) -> Result<(), PortError> {
        self.log.push(event.clone());
        let _ = self.tx.send(event);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nocturne_core::{CoreEventKind, SystemErrorEvent, SystemErrorSeverity};

    fn system_error_event(event_id: &str) -> CoreEventEnvelope<CoreEvent> {
        CoreEventEnvelope::new(
            event_id.to_owned(),
            CoreEventKind::SystemError,
            String::from("2026-04-23T12:34:56Z"),
            CoreEvent::SystemError(SystemErrorEvent {
                code: String::from("test_error"),
                message: String::from("test message"),
                severity: SystemErrorSeverity::Warning,
            }),
        )
    }

    #[test]
    fn events_after_without_cursor_returns_empty_replay() {
        let log = LocalEventLog::default();
        assert_eq!(log.events_after(None).unwrap(), Vec::new());
    }

    #[test]
    fn events_after_returns_error_when_cursor_is_missing() {
        let log = LocalEventLog::default();
        log.push(system_error_event("evt_0001"));

        assert_eq!(
            log.events_after(Some("evt_missing")),
            Err(EventCursorError::CursorNotFound)
        );
    }
}
