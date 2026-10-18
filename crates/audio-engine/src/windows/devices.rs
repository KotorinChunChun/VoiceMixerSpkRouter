use crate::error::{BackendError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceDirection {
    Capture,
    Render,
}

/// Logical endpoint selectors used by sample projects. They are resolved to
/// the current Windows endpoint when the graph runtime starts.
pub const DEFAULT_CONSOLE_ENDPOINT: &str = "default://console";
pub const DEFAULT_COMMUNICATIONS_ENDPOINT: &str = "default://communications";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AudioDeviceInfo {
    pub endpoint_id: String,
    pub friendly_name: String,
    pub direction: DeviceDirection,
    pub state: String,
    pub is_default: bool,
    #[serde(default)]
    pub is_default_communications: bool,
    pub current_sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub supported_sample_rates: Vec<u32>,
    pub user_visible: bool,
    pub user_sort_order: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DefaultDeviceEndpoints {
    pub capture: Option<String>,
    pub capture_communications: Option<String>,
    pub render: Option<String>,
    pub render_communications: Option<String>,
}

pub fn default_device_endpoints() -> DefaultDeviceEndpoints {
    DefaultDeviceEndpoints {
        capture: resolve_endpoint_id(DEFAULT_CONSOLE_ENDPOINT, DeviceDirection::Capture).ok(),
        capture_communications: resolve_endpoint_id(
            DEFAULT_COMMUNICATIONS_ENDPOINT,
            DeviceDirection::Capture,
        )
        .ok(),
        render: resolve_endpoint_id(DEFAULT_CONSOLE_ENDPOINT, DeviceDirection::Render).ok(),
        render_communications: resolve_endpoint_id(
            DEFAULT_COMMUNICATIONS_ENDPOINT,
            DeviceDirection::Render,
        )
        .ok(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointPeakReading {
    pub endpoint_id: String,
    pub peak: f32,
    pub error: Option<String>,
}

#[cfg(windows)]
pub fn read_endpoint_peaks(endpoint_ids: &[String]) -> Vec<EndpointPeakReading> {
    use wasapi::DeviceEnumerator;

    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new();
    let readings = endpoint_ids
        .iter()
        .map(|endpoint_id| {
            let result = enumerator
                .as_ref()
                .map_err(|error| error.to_string())
                .and_then(|enumerator| {
                    enumerator
                        .get_device(endpoint_id)
                        .map_err(|error| error.to_string())
                })
                .and_then(|device| {
                    device
                        .get_audiometerinformation()
                        .map_err(|error| error.to_string())
                })
                .and_then(|meter| meter.get_peak_value().map_err(|error| error.to_string()));
            match result {
                Ok(peak) => EndpointPeakReading {
                    endpoint_id: endpoint_id.clone(),
                    peak: peak.clamp(0.0, 1.0),
                    error: None,
                },
                Err(error) => EndpointPeakReading {
                    endpoint_id: endpoint_id.clone(),
                    peak: 0.0,
                    error: Some(error),
                },
            }
        })
        .collect();
    wasapi::deinitialize();
    readings
}

#[cfg(not(windows))]
pub fn read_endpoint_peaks(endpoint_ids: &[String]) -> Vec<EndpointPeakReading> {
    endpoint_ids
        .iter()
        .map(|endpoint_id| EndpointPeakReading {
            endpoint_id: endpoint_id.clone(),
            peak: 0.0,
            error: Some("Windows endpoint meters require Windows".into()),
        })
        .collect()
}

#[cfg(windows)]
pub fn list_devices(direction: DeviceDirection) -> Result<Vec<AudioDeviceInfo>> {
    use wasapi::{DeviceEnumerator, Direction, Role, SampleType, ShareMode, WaveFormat};
    let _ = wasapi::initialize_mta();
    let dir = match direction {
        DeviceDirection::Capture => Direction::Capture,
        DeviceDirection::Render => Direction::Render,
    };
    let en = DeviceEnumerator::new().map_err(|e| BackendError::Audio(e.to_string()))?;
    let default_id = en
        .get_default_device(&dir)
        .ok()
        .and_then(|d| d.get_id().ok());
    let communications_id = en
        .get_default_device_for_role(&dir, &Role::Communications)
        .ok()
        .and_then(|d| d.get_id().ok());
    let collection = en
        .get_device_collection(&dir)
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    let mut out = Vec::new();
    for i in 0..collection
        .get_nbr_devices()
        .map_err(|e| BackendError::Audio(e.to_string()))?
    {
        let device = collection
            .get_device_at_index(i)
            .map_err(|e| BackendError::Audio(e.to_string()))?;
        let endpoint_id = device
            .get_id()
            .map_err(|e| BackendError::Audio(e.to_string()))?;
        let friendly_name = device
            .get_friendlyname()
            .unwrap_or_else(|_| endpoint_id.clone());
        let state = device
            .get_state()
            .map(|x| format!("{x:?}"))
            .unwrap_or_else(|_| "Unknown".into());
        let mut current_sample_rate = None;
        let mut channels = None;
        let mut supported = Vec::new();
        if let Ok(client) = device.get_iaudioclient() {
            if let Ok(mix) = client.get_mixformat() {
                current_sample_rate = Some(mix.get_samplespersec());
                channels = Some(mix.get_nchannels());
            }
            for rate in [
                16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 192_000,
            ] {
                let fmt = WaveFormat::new(32, 32, &SampleType::Float, rate as usize, 2, None);
                if client.is_supported(&fmt, &ShareMode::Shared).is_ok() {
                    supported.push(rate);
                }
            }
        }
        out.push(AudioDeviceInfo {
            is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            is_default_communications: communications_id.as_deref() == Some(endpoint_id.as_str()),
            endpoint_id,
            friendly_name,
            direction,
            state,
            current_sample_rate,
            channels,
            supported_sample_rates: supported,
            user_visible: true,
            user_sort_order: i as i32,
        });
    }
    Ok(out)
}

pub fn resolve_endpoint_id(endpoint_id: &str, direction: DeviceDirection) -> Result<String> {
    match endpoint_id {
        DEFAULT_CONSOLE_ENDPOINT => resolve_default_endpoint(direction, false),
        DEFAULT_COMMUNICATIONS_ENDPOINT => resolve_default_endpoint(direction, true),
        _ => Ok(endpoint_id.into()),
    }
}

#[cfg(windows)]
pub fn set_default_audio_device(
    endpoint_id: &str,
    direction: DeviceDirection,
    communications: bool,
) -> Result<()> {
    if endpoint_id.starts_with("default://") {
        return Err(BackendError::InvalidOperation(
            "既定デバイスセレクターはWindows既定デバイスへ設定できません".into(),
        ));
    }
    let exists = list_devices(direction)?
        .into_iter()
        .any(|device| device.endpoint_id == endpoint_id);
    if !exists {
        return Err(BackendError::InvalidOperation(
            "指定されたEndpointは接続中のデバイス一覧にありません".into(),
        ));
    }
    crate::windows::default_device::set_default_endpoint(endpoint_id, communications)
}

#[cfg(not(windows))]
pub fn set_default_audio_device(
    _endpoint_id: &str,
    _direction: DeviceDirection,
    _communications: bool,
) -> Result<()> {
    Err(BackendError::PlatformUnsupported(
        "Windows既定デバイスの変更にはWindowsが必要です".into(),
    ))
}

#[cfg(windows)]
fn resolve_default_endpoint(direction: DeviceDirection, communications: bool) -> Result<String> {
    use wasapi::{DeviceEnumerator, Direction, Role};

    let _ = wasapi::initialize_mta();
    let dir = match direction {
        DeviceDirection::Capture => Direction::Capture,
        DeviceDirection::Render => Direction::Render,
    };
    let role = if communications {
        Role::Communications
    } else {
        Role::Console
    };
    DeviceEnumerator::new()
        .map_err(|error| BackendError::Audio(error.to_string()))?
        .get_default_device_for_role(&dir, &role)
        .and_then(|device| device.get_id())
        .map_err(|error| BackendError::Audio(error.to_string()))
}

#[cfg(not(windows))]
fn resolve_default_endpoint(_direction: DeviceDirection, _communications: bool) -> Result<String> {
    Err(BackendError::PlatformUnsupported(
        "Windows既定デバイスの解決にはWASAPIが必要です".into(),
    ))
}

#[cfg(not(windows))]
pub fn list_devices(_direction: DeviceDirection) -> Result<Vec<AudioDeviceInfo>> {
    Err(BackendError::PlatformUnsupported(
        "WASAPI device enumeration requires Windows".into(),
    ))
}
