use std::{sync::Arc, time::Duration};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::{BackendError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceWorkerState {
    Starting,
    Running,
    Reconnecting,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeviceWorkerSnapshot {
    pub state: DeviceWorkerState,
    pub attempts: u64,
    pub last_error: Option<String>,
    #[serde(default)]
    pub requested_method: Option<String>,
    #[serde(default)]
    pub active_method: Option<String>,
}

#[derive(Clone)]
pub(crate) struct DeviceWorkerStatus {
    inner: Arc<Mutex<DeviceWorkerSnapshot>>,
}

impl DeviceWorkerStatus {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DeviceWorkerSnapshot {
                state: DeviceWorkerState::Starting,
                attempts: 0,
                last_error: None,
                requested_method: None,
                active_method: None,
            })),
        }
    }

    pub(crate) fn capture(requested_method: impl Into<String>) -> Self {
        let out = Self::new();
        out.inner.lock().requested_method = Some(requested_method.into());
        out
    }

    pub(crate) fn running(&self) {
        let mut status = self.inner.lock();
        status.state = DeviceWorkerState::Running;
        status.last_error = None;
    }

    pub(crate) fn running_with_method(&self, method: impl Into<String>) {
        let mut status = self.inner.lock();
        status.state = DeviceWorkerState::Running;
        status.last_error = None;
        status.active_method = Some(method.into());
    }

    pub(crate) fn reconnecting(&self, error: impl Into<String>) {
        let mut status = self.inner.lock();
        status.state = DeviceWorkerState::Reconnecting;
        status.attempts = status.attempts.saturating_add(1);
        status.last_error = Some(error.into());
        status.active_method = None;
    }

    pub(crate) fn stopped(&self) {
        self.inner.lock().state = DeviceWorkerState::Stopped;
    }

    pub(crate) fn snapshot(&self) -> DeviceWorkerSnapshot {
        self.inner.lock().clone()
    }

    pub(crate) fn wait_until_running(&self, timeout: Duration) -> Result<DeviceWorkerSnapshot> {
        let started = std::time::Instant::now();
        loop {
            let snapshot = self.snapshot();
            match snapshot.state {
                DeviceWorkerState::Running => return Ok(snapshot),
                DeviceWorkerState::Stopped => {
                    return Err(BackendError::Audio(snapshot.last_error.unwrap_or_else(
                        || "WASAPI worker stopped before becoming ready".into(),
                    )));
                }
                DeviceWorkerState::Starting | DeviceWorkerState::Reconnecting => {}
            }
            if started.elapsed() >= timeout {
                return Err(BackendError::Audio(format!(
                    "WASAPI worker did not become ready within {} ms{}",
                    timeout.as_millis(),
                    snapshot
                        .last_error
                        .as_deref()
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
