//! Small shared primitives used by the Chrome, CDP, and terminal boundaries.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

macro_rules! numeric_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

numeric_id!(NodeId);
numeric_id!(LinkId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    CliUsage,
    Url,
    Dns,
    Connection,
    Tls,
    HttpPolicy,
    NavigationLimit,
    DocumentParsing,
    Rendering,
    JavaScript,
    Storage,
    Terminal,
    SecurityPolicy,
    Cancelled,
    ResourceLimit,
    CorruptedInput,
    Internal,
}

impl ErrorKind {
    pub const fn automation_exit_code(self) -> u8 {
        match self {
            Self::CliUsage => 2,
            Self::Url => 10,
            Self::Dns => 11,
            Self::Connection => 12,
            Self::Tls => 13,
            Self::HttpPolicy => 14,
            Self::NavigationLimit => 15,
            Self::DocumentParsing => 20,
            Self::Rendering => 21,
            Self::JavaScript => 22,
            Self::Storage => 30,
            Self::Terminal => 40,
            Self::SecurityPolicy => 50,
            Self::Cancelled | Self::ResourceLimit | Self::CorruptedInput | Self::Internal => 70,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{public_message}")]
pub struct TermGlideError {
    kind: ErrorKind,
    public_message: String,
    recovery: Option<String>,
}

impl TermGlideError {
    pub fn new(kind: ErrorKind, public_message: impl Into<String>) -> Self {
        Self {
            kind,
            public_message: public_message.into(),
            recovery: None,
        }
    }

    pub fn with_recovery(mut self, recovery: impl Into<String>) -> Self {
        self.recovery = Some(recovery.into());
        self
    }

    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn public_message(&self) -> &str {
        &self.public_message
    }

    pub fn recovery(&self) -> Option<&str> {
        self.recovery.as_deref()
    }

    pub const fn automation_exit_code(&self) -> u8 {
        self.kind.automation_exit_code()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_bytes: usize,
    pub max_elements: usize,
    pub max_depth: usize,
    pub max_queue_items: usize,
}

impl ResourceLimits {
    pub const BROWSER_DEFAULT: Self = Self {
        max_bytes: 64 * 1024 * 1024,
        max_elements: 1_000_000,
        max_depth: 1_024,
        max_queue_items: 4_096,
    };
}

pub trait Clock: Send + Sync {
    fn monotonic_now(&self) -> Duration;
    fn wall_now(&self) -> SystemTime;
}

#[derive(Debug)]
pub struct SystemClock {
    monotonic_origin: Instant,
    wall_origin: SystemTime,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            monotonic_origin: Instant::now(),
            wall_origin: SystemTime::now(),
        }
    }
}

impl Clock for SystemClock {
    fn monotonic_now(&self) -> Duration {
        self.monotonic_origin.elapsed()
    }

    fn wall_now(&self) -> SystemTime {
        self.wall_origin + self.monotonic_origin.elapsed()
    }
}

#[derive(Debug, Clone)]
pub struct VirtualClock {
    monotonic: Arc<Mutex<Duration>>,
    wall_origin: SystemTime,
}

impl VirtualClock {
    pub fn new(wall_origin: SystemTime) -> Self {
        Self {
            monotonic: Arc::new(Mutex::new(Duration::ZERO)),
            wall_origin,
        }
    }

    pub fn advance(&self, duration: Duration) {
        let mut current = lock_unpoisoned(&self.monotonic);
        *current = current.saturating_add(duration);
    }

    pub fn set(&self, duration: Duration) {
        *lock_unpoisoned(&self.monotonic) = duration;
    }
}

impl Clock for VirtualClock {
    fn monotonic_now(&self) -> Duration {
        *lock_unpoisoned(&self.monotonic)
    }

    fn wall_now(&self) -> SystemTime {
        self.wall_origin + self.monotonic_now()
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Clone)]
pub struct Cancellation {
    token: CancellationToken,
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    pub fn child(&self) -> Self {
        Self {
            token: self.token.child_token(),
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{Cancellation, Clock, ErrorKind, VirtualClock};

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(ErrorKind::CliUsage.automation_exit_code(), 2);
        assert_eq!(ErrorKind::Url.automation_exit_code(), 10);
        assert_eq!(ErrorKind::Internal.automation_exit_code(), 70);
    }

    #[test]
    fn virtual_clock_is_deterministic() {
        let clock = VirtualClock::new(UNIX_EPOCH);
        clock.advance(Duration::from_millis(40));
        clock.advance(Duration::from_millis(2));
        assert_eq!(clock.monotonic_now(), Duration::from_millis(42));
        assert_eq!(clock.wall_now(), UNIX_EPOCH + Duration::from_millis(42));
    }

    #[test]
    fn parent_cancellation_reaches_child() {
        let parent = Cancellation::new();
        let child = parent.child();
        parent.cancel();
        assert!(child.is_cancelled());
    }
}
