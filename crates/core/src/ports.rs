use std::error::Error;
use std::fmt::{self, Display, Formatter};

use nocturne_domain::QueueItem;

use crate::{CoreEvent, CoreEventEnvelope, CoreId, CoreTimestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Command,
    Event,
    Snapshot,
    QueueItem,
    SearchJob,
}

pub trait ClockPort {
    fn now(&self) -> CoreTimestamp;
}

pub trait IdGeneratorPort {
    fn next_id(&mut self, kind: IdKind) -> CoreId;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortError {
    code: String,
    message: String,
}

impl PortError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for PortError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl Error for PortError {}

pub trait EventPublisherPort {
    fn publish(&mut self, event: CoreEventEnvelope<CoreEvent>) -> Result<(), PortError>;
}

pub trait PlaybackPort {
    fn start(&mut self, item: &QueueItem, position_ms: u64) -> Result<(), PortError>;

    fn set_volume(&mut self, gain: f32) -> Result<(), PortError>;

    fn pause(&mut self) -> Result<(), PortError>;

    fn resume(&mut self) -> Result<(), PortError>;

    fn stop(&mut self) -> Result<(), PortError>;

    fn seek(&mut self, position_ms: u64) -> Result<(), PortError>;
}

pub trait SearchPort {
    fn start_search(&mut self, job_id: &str, query: &str) -> Result<(), PortError>;
}
