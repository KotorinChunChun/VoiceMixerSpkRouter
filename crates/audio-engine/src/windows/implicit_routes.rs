use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{BackendError, Result};

/// Frontend-defined relationship between a virtual driver's render endpoint
/// (the endpoint this mixer writes to) and capture endpoint (the endpoint a
/// conferencing/recording application selects as its microphone).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct VirtualDevicePair {
    pub id: String,
    pub display_name: String,
    pub render_endpoint_id: String,
    pub capture_endpoint_id: String,
    pub enabled: bool,
}

impl VirtualDevicePair {
    pub fn new(
        display_name: impl Into<String>,
        render_endpoint_id: impl Into<String>,
        capture_endpoint_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            display_name: display_name.into(),
            render_endpoint_id: render_endpoint_id.into(),
            capture_endpoint_id: capture_endpoint_id.into(),
            enabled: true,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(BackendError::InvalidOperation(
                "virtual device pair id must not be empty".into(),
            ));
        }
        if self.render_endpoint_id.trim().is_empty() || self.capture_endpoint_id.trim().is_empty() {
            return Err(BackendError::InvalidOperation(
                "virtual device pair requires both render_endpoint_id and capture_endpoint_id"
                    .into(),
            ));
        }
        if self.render_endpoint_id == self.capture_endpoint_id {
            return Err(BackendError::InvalidOperation(
                "virtual device pair render and capture endpoint IDs must differ".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DetectionState {
    Detected,
    NotDetected,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImplicitRouteKind {
    VirtualDevicePair,
    OsListenToDevice,
    ExternalApplication,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImplicitRoute {
    pub kind: ImplicitRouteKind,
    pub from: String,
    pub to: String,
    pub detection: DetectionState,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListenDetection {
    pub capture_endpoint_id: String,
    pub state: DetectionState,
    pub playback_endpoint_id: Option<String>,
    pub note: String,
}

/// Builds driver-internal routes from pairings explicitly supplied by the
/// frontend. The backend deliberately does not infer vendor-specific pairs.
pub fn routes_from_virtual_pairs(pairs: &[VirtualDevicePair]) -> Vec<ImplicitRoute> {
    pairs
        .iter()
        .filter(|p| p.enabled)
        .map(|p| ImplicitRoute {
            kind: ImplicitRouteKind::VirtualDevicePair,
            from: p.render_endpoint_id.clone(),
            to: p.capture_endpoint_id.clone(),
            detection: DetectionState::Detected,
            note: format!(
                "frontend configured virtual-device pair: {}",
                p.display_name
            ),
        })
        .collect()
}

/// Enumerates OS implicit routes that can be inspected without changing any
/// Windows setting. "Listen to this device" is intentionally best-effort:
/// Microsoft does not publish a dedicated PKEY contract for this UI setting,
/// so absence/driver incompatibility is reported as Unknown instead of being
/// treated as a required engine capability.
pub fn detect_all_listen_routes() -> Vec<ImplicitRoute> {
    #[cfg(not(windows))]
    {
        Vec::new()
    }

    #[cfg(windows)]
    {
        use crate::windows::devices::{list_devices, DeviceDirection};
        let Ok(devices) = list_devices(DeviceDirection::Capture) else {
            return Vec::new();
        };
        devices
            .into_iter()
            .filter_map(|device| {
                let detection = detect_listen_to_device(&device.endpoint_id);
                match detection.state {
                    DetectionState::Detected => Some(ImplicitRoute {
                        kind: ImplicitRouteKind::OsListenToDevice,
                        from: device.endpoint_id,
                        to: detection
                            .playback_endpoint_id
                            .unwrap_or_else(|| "<windows-default-playback>".into()),
                        detection: DetectionState::Detected,
                        note: detection.note,
                    }),
                    _ => None,
                }
            })
            .collect()
    }
}

pub fn list_implicit_routes(pairs: &[VirtualDevicePair]) -> Vec<ImplicitRoute> {
    let mut routes = routes_from_virtual_pairs(pairs);
    routes.extend(detect_all_listen_routes());
    routes
}

/// Detects the legacy Windows "Listen to this device" route by reading the
/// capture endpoint's MMDevice property store (IMMDevice::OpenPropertyStore).
///
/// Observed property-store keys used by the Windows Sound control panel:
///   {24DBB0FC-9311-4B3D-9CF0-18FF155639D4}, 1 => listen enabled
///   {24DBB0FC-9311-4B3D-9CF0-18FF155639D4}, 0 => playback target endpoint
///
/// These keys are not a documented public PKEY API contract. Therefore any
/// unexpected value/error becomes Unknown and never prevents audio startup.
pub fn detect_listen_to_device(capture_endpoint_id: &str) -> ListenDetection {
    #[cfg(not(windows))]
    {
        ListenDetection {
            capture_endpoint_id: capture_endpoint_id.to_string(),
            state: DetectionState::Unknown,
            playback_endpoint_id: None,
            note: "Listen-to-device inspection requires Windows".into(),
        }
    }

    #[cfg(windows)]
    {
        match read_listen_properties(capture_endpoint_id) {
            Ok((false, _)) => ListenDetection {
                capture_endpoint_id: capture_endpoint_id.to_string(),
                state: DetectionState::NotDetected,
                playback_endpoint_id: None,
                note: "Windows endpoint property store reports Listen to this device disabled".into(),
            },
            Ok((true, target)) => ListenDetection {
                capture_endpoint_id: capture_endpoint_id.to_string(),
                state: DetectionState::Detected,
                playback_endpoint_id: target,
                note: "Detected via IMMDevice/IPropertyStore; property key is Windows implementation detail and is treated as best-effort".into(),
            },
            Err(err) => ListenDetection {
                capture_endpoint_id: capture_endpoint_id.to_string(),
                state: DetectionState::Unknown,
                playback_endpoint_id: None,
                note: format!("Unable to inspect Listen to this device through MMDevice property store: {err}"),
            },
        }
    }
}

#[cfg(windows)]
fn read_listen_properties(endpoint_id: &str) -> Result<(bool, Option<String>)> {
    use windows::{
        core::{GUID, HSTRING},
        Win32::{
            Foundation::PROPERTYKEY,
            Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator},
            System::Com::StructuredStorage::{
                PropVariantClear, PropVariantToStringAlloc, PROPVARIANT,
            },
            System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ},
        },
    };

    const LISTEN_PROPERTY_GUID: u128 = 0x24dbb0fc_9311_4b3d_9cf0_18ff155639d4;
    let enabled_key = PROPERTYKEY {
        fmtid: GUID::from_u128(LISTEN_PROPERTY_GUID),
        pid: 1,
    };
    let target_key = PROPERTYKEY {
        fmtid: GUID::from_u128(LISTEN_PROPERTY_GUID),
        pid: 0,
    };

    // CoreAudio operations already initialize COM on their worker threads. This
    // helper may also be invoked directly from a command thread, so initialize
    // MTA defensively; S_FALSE/RPC_E_CHANGED_MODE are handled by the Windows
    // wrappers used below rather than becoming an engine requirement.
    let _ = wasapi::initialize_mta();

    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| BackendError::Audio(e.to_string()))?
    };
    let endpoint_id = HSTRING::from(endpoint_id);
    let device = unsafe { enumerator.GetDevice(&endpoint_id) }
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }
        .map_err(|e| BackendError::Audio(e.to_string()))?;

    fn prop_to_string(mut prop: PROPVARIANT) -> Result<String> {
        let converted = unsafe { PropVariantToStringAlloc(&prop) }
            .map_err(|e| BackendError::Audio(e.to_string()));
        let value = match converted {
            Ok(ptr) => {
                let text =
                    unsafe { ptr.to_string() }.map_err(|e| BackendError::Audio(e.to_string()));
                unsafe { CoTaskMemFree(Some(ptr.0.cast())) };
                text
            }
            Err(err) => Err(err),
        };
        unsafe { PropVariantClear(&mut prop) }.map_err(|e| BackendError::Audio(e.to_string()))?;
        value
    }

    let enabled =
        unsafe { store.GetValue(&enabled_key) }.map_err(|e| BackendError::Audio(e.to_string()))?;
    let enabled_text = prop_to_string(enabled)?;
    let enabled = matches!(
        enabled_text.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    );
    if !enabled {
        return Ok((false, None));
    }

    let target = unsafe { store.GetValue(&target_key) }
        .ok()
        .and_then(|prop| prop_to_string(prop).ok())
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty() && x != "0");

    Ok((true, target))
}
