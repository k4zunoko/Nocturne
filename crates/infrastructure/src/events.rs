use std::sync::{Arc, Mutex};

use nocturne_core::{CoreEvent, CoreEventEnvelope, EventPublisherPort, PortError};

use crate::recover_lock;

#[derive(Debug, Clone, Default)]
pub struct LocalEventLog {
    entries: Arc<Mutex<Vec<CoreEventEnvelope<CoreEvent>>>>,
}

impl LocalEventLog {
    #[must_use]
    pub fn snapshot(&self) -> Vec<CoreEventEnvelope<CoreEvent>> {
        recover_lock(&self.entries).clone()
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

impl EventPublisherPort for LocalEventPublisher {
    fn publish(&mut self, event: CoreEventEnvelope<CoreEvent>) -> Result<(), PortError> {
        recover_lock(&self.log.entries).push(event);
        Ok(())
    }
}
