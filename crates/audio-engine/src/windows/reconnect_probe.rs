use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::audio::buffer::AudioRingBuffer;
use crate::windows::{
    devices::{AudioDeviceInfo, DeviceDirection},
    wasapi_capture::start_microphone_capture,
    wasapi_render::start_render,
};

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_POLL_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeAction {
    Help,
    List { direction: Option<DeviceDirection> },
    Watch(WatchOptions),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    pub device_name: Option<String>,
    pub endpoint_id: Option<String>,
    pub direction: DeviceDirection,
    pub timeout_ms: u64,
    pub poll_ms: u64,
    pub auto_reconnect_devices: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeEventKind {
    Watching,
    Disconnected,
    Reconnected,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeEvent {
    pub kind: ProbeEventKind,
    pub unix_ms: u64,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReconnectResult {
    pub attempted: bool,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectProbeReport {
    pub ok: bool,
    pub endpoint_id: String,
    pub friendly_name: String,
    pub direction: DeviceDirection,
    pub auto_reconnect_devices: bool,
    pub auto_reconnect: AutoReconnectResult,
    pub recovery_ms: Option<u64>,
    pub events: Vec<ProbeEvent>,
}

#[derive(Debug, Clone)]
pub struct PresenceWatch {
    started_ms: u64,
    present: bool,
    disconnected_at_ms: Option<u64>,
    pub events: Vec<ProbeEvent>,
    pub finished: bool,
    pub ok: bool,
}

impl PresenceWatch {
    pub fn start(now_ms: u64, present: bool) -> Result<Self, String> {
        if !present {
            return Err("指定Endpointが列挙にありません".into());
        }
        Ok(Self {
            started_ms: now_ms,
            present: true,
            disconnected_at_ms: None,
            events: vec![ProbeEvent {
                kind: ProbeEventKind::Watching,
                unix_ms: now_ms,
                present: true,
                recovery_ms: None,
            }],
            finished: false,
            ok: false,
        })
    }

    pub fn on_sample(&mut self, present: bool, now_ms: u64, timeout_ms: u64) {
        if self.finished {
            return;
        }
        if now_ms.saturating_sub(self.started_ms) >= timeout_ms {
            self.events.push(ProbeEvent {
                kind: ProbeEventKind::Timeout,
                unix_ms: now_ms,
                present,
                recovery_ms: self
                    .disconnected_at_ms
                    .map(|disconnected| now_ms.saturating_sub(disconnected)),
            });
            self.finished = true;
            self.ok = false;
            return;
        }
        if self.present && !present {
            self.present = false;
            self.disconnected_at_ms = Some(now_ms);
            self.events.push(ProbeEvent {
                kind: ProbeEventKind::Disconnected,
                unix_ms: now_ms,
                present: false,
                recovery_ms: None,
            });
            return;
        }
        if !self.present && present {
            let recovery_ms = now_ms.saturating_sub(self.disconnected_at_ms.unwrap_or(now_ms));
            self.present = true;
            self.events.push(ProbeEvent {
                kind: ProbeEventKind::Reconnected,
                unix_ms: now_ms,
                present: true,
                recovery_ms: Some(recovery_ms),
            });
            self.finished = true;
            self.ok = true;
        }
    }

    pub fn recovery_ms(&self) -> Option<u64> {
        self.events.iter().rev().find_map(|event| event.recovery_ms)
    }
}

pub fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<ProbeAction, String> {
    let mut arguments = arguments.into_iter();
    let mut list = false;
    let mut device_name = None;
    let mut endpoint_id = None;
    let mut direction = None;
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut poll_ms = DEFAULT_POLL_MS;
    let mut auto_reconnect_devices = true;
    let mut saw_auto_reconnect_flag = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(ProbeAction::Help),
            "--list" => list = true,
            "--device" => device_name = Some(next_value(&mut arguments, "--device")?),
            "--endpoint-id" => endpoint_id = Some(next_value(&mut arguments, "--endpoint-id")?),
            "--direction" => {
                direction = Some(parse_direction(&next_value(
                    &mut arguments,
                    "--direction",
                )?)?)
            }
            "--timeout-ms" => {
                timeout_ms = parse_u64(next_value(&mut arguments, "--timeout-ms")?, "--timeout-ms")?
            }
            "--poll-ms" => {
                poll_ms = parse_u64(next_value(&mut arguments, "--poll-ms")?, "--poll-ms")?
            }
            "--auto-reconnect" => {
                auto_reconnect_devices = true;
                saw_auto_reconnect_flag = true;
            }
            "--no-auto-reconnect" => {
                auto_reconnect_devices = false;
                saw_auto_reconnect_flag = true;
            }
            other => return Err(format!("不明な引数です: {other}")),
        }
    }

    if list {
        if device_name.is_some() || endpoint_id.is_some() || saw_auto_reconnect_flag {
            return Err("--listは監視用引数と同時に指定できません".into());
        }
        return Ok(ProbeAction::List { direction });
    }
    if device_name.is_some() == endpoint_id.is_some() {
        return Err("--deviceまたは--endpoint-idのどちらか一方を指定してください".into());
    }
    if timeout_ms == 0 || poll_ms == 0 {
        return Err("--timeout-msと--poll-msは1以上にしてください".into());
    }
    Ok(ProbeAction::Watch(WatchOptions {
        device_name,
        endpoint_id,
        direction: direction.unwrap_or(DeviceDirection::Capture),
        timeout_ms,
        poll_ms,
        auto_reconnect_devices,
    }))
}

pub fn select_device<'a>(
    devices: &'a [AudioDeviceInfo],
    endpoint_id: Option<&str>,
    device_name: Option<&str>,
) -> Result<&'a AudioDeviceInfo, String> {
    if let Some(endpoint_id) = endpoint_id {
        return devices
            .iter()
            .find(|device| device.endpoint_id.eq_ignore_ascii_case(endpoint_id))
            .ok_or_else(|| format!("Endpointが見つかりません: {endpoint_id}"));
    }
    let device_name = device_name.ok_or_else(|| "デバイス名がありません".to_string())?;
    let needle = device_name.to_ascii_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|device| device.friendly_name.to_ascii_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [device] => Ok(*device),
        [] => Err(format!("デバイスが見つかりません: {device_name}")),
        _ => Err(format!(
            "同名のデバイスが複数あります。--endpoint-idを指定してください: {device_name}"
        )),
    }
}

pub fn endpoint_is_present(devices: &[AudioDeviceInfo], endpoint_id: &str) -> bool {
    devices
        .iter()
        .any(|device| device.endpoint_id.eq_ignore_ascii_case(endpoint_id))
}

pub fn run_probe(
    options: &WatchOptions,
    selected: &AudioDeviceInfo,
    mut list: impl FnMut() -> Result<Vec<AudioDeviceInfo>, String>,
    opener: impl FnOnce(&str) -> Result<(), String>,
    mut now_ms: impl FnMut() -> u64,
    mut sleep_ms: impl FnMut(u64),
) -> Result<ReconnectProbeReport, String> {
    let mut watch = PresenceWatch::start(
        now_ms(),
        endpoint_is_present(std::slice::from_ref(selected), &selected.endpoint_id),
    )?;
    while !watch.finished {
        sleep_ms(options.poll_ms);
        let present = match list() {
            Ok(devices) => endpoint_is_present(&devices, &selected.endpoint_id),
            Err(_) => false,
        };
        watch.on_sample(present, now_ms(), options.timeout_ms);
    }

    let auto_reconnect = if watch.ok && options.auto_reconnect_devices {
        match opener(&selected.endpoint_id) {
            Ok(()) => AutoReconnectResult {
                attempted: true,
                ok: true,
                message: "再接続後にデバイスを開けました".into(),
            },
            Err(message) => AutoReconnectResult {
                attempted: true,
                ok: false,
                message,
            },
        }
    } else if watch.ok {
        AutoReconnectResult {
            attempted: false,
            ok: true,
            message: "auto_reconnect_devicesが無効のためオープンを試していません".into(),
        }
    } else {
        AutoReconnectResult {
            attempted: false,
            ok: false,
            message: "再接続が完了していないためオープンを試していません".into(),
        }
    };

    Ok(ReconnectProbeReport {
        ok: watch.ok && auto_reconnect.ok,
        endpoint_id: selected.endpoint_id.clone(),
        friendly_name: selected.friendly_name.clone(),
        direction: options.direction,
        auto_reconnect_devices: options.auto_reconnect_devices,
        auto_reconnect,
        recovery_ms: watch.recovery_ms(),
        events: watch.events,
    })
}

pub fn try_open_endpoint(endpoint_id: &str, direction: DeviceDirection) -> Result<(), String> {
    match direction {
        DeviceDirection::Capture => {
            let ring = AudioRingBuffer::new(48_000 * 2);
            let handle =
                start_microphone_capture(endpoint_id, ring).map_err(|error| error.to_string())?;
            handle
                .wait_until_running(Duration::from_secs(3))
                .map_err(|error| error.to_string())?;
            handle.stop();
            Ok(())
        }
        DeviceDirection::Render => {
            let ring = AudioRingBuffer::new(48_000 * 2);
            let handle = start_render(endpoint_id, ring).map_err(|error| error.to_string())?;
            handle
                .wait_until_running(Duration::from_secs(3))
                .map_err(|error| error.to_string())?;
            handle.stop();
            Ok(())
        }
    }
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn next_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{option}の値を指定してください"))
}

fn parse_u64(value: String, option: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{option}には0以上の整数を指定してください: {value}"))
}

fn parse_direction(value: &str) -> Result<DeviceDirection, String> {
    match value.to_ascii_lowercase().as_str() {
        "capture" | "input" | "mic" => Ok(DeviceDirection::Capture),
        "render" | "output" | "playback" => Ok(DeviceDirection::Render),
        other => Err(format!(
            "--directionはcaptureまたはrenderを指定してください: {other}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(endpoint_id: &str, name: &str) -> AudioDeviceInfo {
        AudioDeviceInfo {
            endpoint_id: endpoint_id.into(),
            friendly_name: name.into(),
            direction: DeviceDirection::Capture,
            state: "Active".into(),
            is_default: true,
            is_default_communications: false,
            current_sample_rate: Some(48_000),
            channels: Some(2),
            supported_sample_rates: vec![48_000],
            user_visible: true,
            user_sort_order: 0,
        }
    }

    fn watch_options(auto_reconnect: bool) -> WatchOptions {
        WatchOptions {
            device_name: None,
            endpoint_id: Some("ep-1".into()),
            direction: DeviceDirection::Capture,
            timeout_ms: 5_000,
            poll_ms: 1,
            auto_reconnect_devices: auto_reconnect,
        }
    }

    #[test]
    fn parses_watch_options() {
        let action = parse_args([
            "--device".into(),
            "USB Mic".into(),
            "--direction".into(),
            "render".into(),
            "--timeout-ms".into(),
            "30000".into(),
            "--no-auto-reconnect".into(),
        ])
        .unwrap();
        assert_eq!(
            action,
            ProbeAction::Watch(WatchOptions {
                device_name: Some("USB Mic".into()),
                endpoint_id: None,
                direction: DeviceDirection::Render,
                timeout_ms: 30_000,
                poll_ms: DEFAULT_POLL_MS,
                auto_reconnect_devices: false,
            })
        );
    }

    #[test]
    fn rejects_list_with_watch_args() {
        let error = parse_args(["--list".into(), "--device".into(), "mic".into()]).unwrap_err();
        assert!(error.contains("--list"));
    }

    #[test]
    fn presence_watch_records_recovery() {
        let mut watch = PresenceWatch::start(1_000, true).unwrap();
        watch.on_sample(false, 1_400, 5_000);
        watch.on_sample(true, 1_900, 5_000);
        assert!(watch.ok);
        assert_eq!(watch.recovery_ms(), Some(500));
        assert_eq!(watch.events[1].kind, ProbeEventKind::Disconnected);
        assert_eq!(watch.events[2].kind, ProbeEventKind::Reconnected);
    }

    #[test]
    fn presence_watch_times_out_without_unplug() {
        let mut watch = PresenceWatch::start(1_000, true).unwrap();
        watch.on_sample(true, 6_000, 5_000);
        assert!(watch.finished);
        assert!(!watch.ok);
        assert_eq!(watch.events.last().unwrap().kind, ProbeEventKind::Timeout);
    }

    #[test]
    fn run_probe_records_auto_reconnect_attempt() {
        let selected = device("ep-1", "USB Mic");
        let snapshots = [vec![], vec![device("ep-1", "USB Mic")]];
        let mut index = 0usize;
        let mut now = 1_000u64;
        let report = run_probe(
            &watch_options(true),
            &selected,
            || {
                let snapshot = snapshots[index.min(snapshots.len() - 1)].clone();
                index += 1;
                Ok(snapshot)
            },
            |_| Ok(()),
            || {
                let value = now;
                now += 200;
                value
            },
            |_| {},
        )
        .unwrap();
        assert!(report.ok);
        assert_eq!(report.recovery_ms, Some(200));
        assert!(report.auto_reconnect.attempted);
        assert!(report.auto_reconnect.ok);
    }

    #[test]
    fn run_probe_skips_open_when_auto_reconnect_disabled() {
        let selected = device("ep-1", "USB Mic");
        let snapshots = [vec![], vec![device("ep-1", "USB Mic")]];
        let mut index = 0usize;
        let mut now = 1_000u64;
        let report = run_probe(
            &watch_options(false),
            &selected,
            || {
                let snapshot = snapshots[index.min(snapshots.len() - 1)].clone();
                index += 1;
                Ok(snapshot)
            },
            |_| panic!("opener must not run"),
            || {
                let value = now;
                now += 200;
                value
            },
            |_| {},
        )
        .unwrap();
        assert!(report.ok);
        assert!(!report.auto_reconnect.attempted);
    }
}
