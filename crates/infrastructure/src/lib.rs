use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfrastructureProfile {
    Local,
}

impl Display for InfrastructureProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local-subprocess-adapters"),
        }
    }
}
