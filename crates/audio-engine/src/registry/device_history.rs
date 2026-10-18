use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::{BackendError, Result},
    windows::devices::{AudioDeviceInfo, DeviceDirection},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct AppDeviceId(pub Uuid);

impl AppDeviceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AppDeviceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AppDeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceConnectionState {
    Connected,
    Disconnected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceUsage {
    Capture,
    Monitor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct KnownDevice {
    pub app_device_id: AppDeviceId,
    pub endpoint_id: String,
    pub friendly_name: String,
    pub direction: DeviceDirection,
    pub connection_state: DeviceConnectionState,
    pub capture_enabled: bool,
    pub monitor_enabled: bool,
    pub display_order: i32,
    pub last_detected_unix_ms: u64,
    #[serde(default)]
    pub last_usage: Option<String>,
    #[serde(default)]
    pub comment: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceHistory {
    #[serde(default)]
    pub devices: HashMap<AppDeviceId, KnownDevice>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceRefreshReport {
    pub added: Vec<AppDeviceId>,
    pub reconnected: Vec<AppDeviceId>,
    pub disconnected: Vec<AppDeviceId>,
}

impl DeviceHistory {
    pub fn refresh_direction(
        &mut self,
        direction: DeviceDirection,
        detected: &[AudioDeviceInfo],
        now_unix_ms: u64,
    ) -> DeviceRefreshReport {
        let mut report = DeviceRefreshReport::default();
        let detected_ids: std::collections::HashSet<_> = detected
            .iter()
            .filter(|device| device.direction == direction)
            .map(|device| device.endpoint_id.as_str())
            .collect();

        for device in self
            .devices
            .values_mut()
            .filter(|device| device.direction == direction)
        {
            if !detected_ids.contains(device.endpoint_id.as_str())
                && device.connection_state != DeviceConnectionState::Disconnected
            {
                device.connection_state = DeviceConnectionState::Disconnected;
                report.disconnected.push(device.app_device_id);
            }
        }

        for (index, resource) in detected
            .iter()
            .filter(|device| device.direction == direction)
            .enumerate()
        {
            if let Some(known) = self.devices.values_mut().find(|device| {
                device.direction == direction && device.endpoint_id == resource.endpoint_id
            }) {
                if known.connection_state == DeviceConnectionState::Disconnected {
                    report.reconnected.push(known.app_device_id);
                }
                known.friendly_name = resource.friendly_name.clone();
                known.connection_state = connection_state(&resource.state);
                known.last_detected_unix_ms = now_unix_ms;
            } else {
                let app_device_id = AppDeviceId::new();
                let known = KnownDevice {
                    app_device_id,
                    endpoint_id: resource.endpoint_id.clone(),
                    friendly_name: resource.friendly_name.clone(),
                    direction,
                    connection_state: connection_state(&resource.state),
                    capture_enabled: true,
                    monitor_enabled: direction == DeviceDirection::Render,
                    display_order: index as i32,
                    last_detected_unix_ms: now_unix_ms,
                    last_usage: None,
                    comment: String::new(),
                };
                self.devices.insert(app_device_id, known);
                report.added.push(app_device_id);
            }
        }
        report
    }

    pub fn set_usage_enabled(
        &mut self,
        id: AppDeviceId,
        usage: DeviceUsage,
        enabled: bool,
    ) -> Result<()> {
        let device = self.devices.get_mut(&id).ok_or_else(|| {
            BackendError::InvalidOperation(format!("known device {id} not found"))
        })?;
        match usage {
            DeviceUsage::Capture => device.capture_enabled = enabled,
            DeviceUsage::Monitor => device.monitor_enabled = enabled,
        }
        Ok(())
    }

    pub fn forget_disconnected(&mut self, id: AppDeviceId) -> Result<KnownDevice> {
        let device = self.devices.get(&id).ok_or_else(|| {
            BackendError::InvalidOperation(format!("known device {id} not found"))
        })?;
        if device.connection_state != DeviceConnectionState::Disconnected {
            return Err(BackendError::InvalidOperation(
                "connected devices cannot be forgotten".into(),
            ));
        }
        Ok(self.devices.remove(&id).expect("device existence checked"))
    }
}

fn connection_state(state: &str) -> DeviceConnectionState {
    let state = state.to_ascii_lowercase();
    if state.contains("active") || state.contains("connected") {
        DeviceConnectionState::Connected
    } else if state.contains("disabled")
        || state.contains("unplugged")
        || state.contains("notpresent")
        || state.contains("disconnected")
    {
        DeviceConnectionState::Disconnected
    } else {
        DeviceConnectionState::Unknown
    }
}
