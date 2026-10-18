use std::collections::HashMap;

use parking_lot::Mutex;

use crate::{
    audio::{graph::RegisteredNodeId, meter::MeterFrame},
    error::Result,
    windows::wasapi_capture::MeterCaptureHandle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeterDirection {
    Capture,
    RenderLoopback,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EndpointKey {
    endpoint_id: String,
    direction: MeterDirection,
}

#[derive(Debug, Clone)]
pub struct MeterMonitorTarget {
    pub registered_node_id: RegisteredNodeId,
    pub endpoint_id: String,
    pub direction: MeterDirection,
}

#[derive(Debug, Clone)]
pub struct MeterMonitorReading {
    pub registered_node_id: RegisteredNodeId,
    pub endpoint_id: String,
    pub frame: MeterFrame,
    pub state: String,
    pub active_method: Option<String>,
    pub error: Option<String>,
    pub error_kind: Option<String>,
}

struct MeterWorker {
    capture: MeterCaptureHandle,
}

struct MeterMonitorState {
    targets: HashMap<RegisteredNodeId, EndpointKey>,
    workers: HashMap<EndpointKey, MeterWorker>,
}

pub struct MeterMonitor {
    state: Mutex<MeterMonitorState>,
}

impl MeterMonitor {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MeterMonitorState {
                targets: HashMap::new(),
                workers: HashMap::new(),
            }),
        }
    }

    pub fn set_targets(&self, targets: Vec<MeterMonitorTarget>) -> Result<()> {
        let requested: HashMap<_, _> = targets
            .into_iter()
            .map(|target| {
                (
                    target.registered_node_id,
                    EndpointKey {
                        endpoint_id: target.endpoint_id,
                        direction: target.direction,
                    },
                )
            })
            .collect();

        let desired_keys: std::collections::HashSet<_> = requested.values().cloned().collect();
        let (to_stop, to_start) = {
            let mut state = self.state.lock();
            let old_workers = std::mem::take(&mut state.workers);
            let mut retained = HashMap::new();
            let mut to_stop = Vec::new();
            for (key, worker) in old_workers {
                if desired_keys.contains(&key) {
                    retained.insert(key, worker);
                } else {
                    to_stop.push(worker);
                }
            }
            let to_start = desired_keys
                .iter()
                .filter(|key| !retained.contains_key(*key))
                .cloned()
                .collect::<Vec<_>>();
            state.targets = requested;
            state.workers = retained;
            (to_stop, to_start)
        };

        drop(to_stop);
        for key in to_start {
            let capture = crate::windows::wasapi_capture::start_shared_meter(&key.endpoint_id)?;
            let mut state = self.state.lock();
            if state.targets.values().any(|target| target == &key) {
                state.workers.insert(key, MeterWorker { capture });
            }
        }
        Ok(())
    }

    pub fn readings(&self) -> Vec<MeterMonitorReading> {
        let state = self.state.lock();
        let mut readings: Vec<MeterMonitorReading> = state
            .targets
            .iter()
            .map(|(registered_node_id, key)| {
                let worker = state.workers.get(key);
                let (frame, snapshot) = worker
                    .map(|worker| (worker.capture.meter(), worker.capture.status()))
                    .unwrap_or((MeterFrame::default(), missing_worker_status()));
                let error = snapshot.last_error.clone();
                MeterMonitorReading {
                    registered_node_id: *registered_node_id,
                    endpoint_id: key.endpoint_id.clone(),
                    frame,
                    state: format!("{:?}", snapshot.state).to_lowercase(),
                    active_method: snapshot.active_method,
                    error_kind: error.as_deref().and_then(classify_meter_error),
                    error,
                }
            })
            .collect();
        readings.sort_by_key(|reading| reading.registered_node_id);
        readings
    }
}

fn missing_worker_status() -> crate::windows::worker_status::DeviceWorkerSnapshot {
    crate::windows::worker_status::DeviceWorkerSnapshot {
        state: crate::windows::worker_status::DeviceWorkerState::Reconnecting,
        attempts: 0,
        last_error: Some("共有メーターを開始できませんでした".into()),
        requested_method: Some("wasapi_shared_event".into()),
        active_method: None,
    }
}

fn classify_meter_error(error: &str) -> Option<String> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("device_in_use") || lower.contains("exclusive") || lower.contains("排他") {
        Some("exclusive".into())
    } else if lower.contains("unsupported") || lower.contains("format") {
        Some("unsupported_format".into())
    } else if lower.contains("invalidated") || lower.contains("device") {
        Some("device_unavailable".into())
    } else {
        Some("other".into())
    }
}

pub fn direction_for_device_node(
    node_type: crate::audio::graph::NodeType,
) -> Option<MeterDirection> {
    match node_type {
        crate::audio::graph::NodeType::Microphone => Some(MeterDirection::Capture),
        crate::audio::graph::NodeType::EndpointLoopback
        | crate::audio::graph::NodeType::Monitor
        | crate::audio::graph::NodeType::VirtualDevice => Some(MeterDirection::RenderLoopback),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_meter_error, direction_for_device_node, MeterDirection};
    use crate::audio::graph::NodeType;

    #[test]
    fn device_node_direction_uses_node_type_not_display_name() {
        assert_eq!(
            direction_for_device_node(NodeType::Microphone),
            Some(MeterDirection::Capture)
        );
        for node_type in [
            NodeType::EndpointLoopback,
            NodeType::Monitor,
            NodeType::VirtualDevice,
        ] {
            assert_eq!(
                direction_for_device_node(node_type),
                Some(MeterDirection::RenderLoopback)
            );
        }
        for node_type in [
            NodeType::TransformPlugin,
            NodeType::Recorder,
            NodeType::TestSignal,
        ] {
            assert_eq!(direction_for_device_node(node_type), None);
        }
    }

    #[test]
    fn meter_errors_classify_exclusive_and_device_failures() {
        assert_eq!(
            classify_meter_error("initialize_client: AUDCLNT_E_DEVICE_IN_USE"),
            Some("exclusive".into())
        );
        assert_eq!(
            classify_meter_error("format is unsupported"),
            Some("unsupported_format".into())
        );
        assert_eq!(
            classify_meter_error("device invalidated"),
            Some("device_unavailable".into())
        );
    }
}
