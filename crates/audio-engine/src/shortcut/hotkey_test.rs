use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    actions::model::KeyCode,
    audio::graph::NodeId,
    error::Result,
    shortcut::{
        model::{
            KeyCombination, ShortcutBinding, ShortcutId, ShortcutRegistrationState,
            ShortcutRuntimeEvent,
        },
        registrar::{platform_registrar, ShortcutRegistrar},
        ShortcutEngine,
    },
};

pub const TEST_HOTKEY_VK: u16 = 0x78; // F9

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HotkeyFireRecord {
    pub timestamp_utc: DateTime<Utc>,
    pub unix_ms: i64,
    pub shortcut_id: ShortcutId,
    pub node_id: NodeId,
    pub action_id: String,
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyTestExport {
    pub started_at_unix_ms: i64,
    pub combo: KeyCombination,
    pub combo_label: String,
    pub registration: ShortcutRegistrationState,
    pub events: Vec<HotkeyFireRecord>,
}

pub struct AlwaysOkRegistrar;

impl ShortcutRegistrar for AlwaysOkRegistrar {
    fn register(
        &mut self,
        _registration_id: i32,
        _combination: KeyCombination,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    fn unregister(&mut self, _registration_id: i32) {}

    fn poll_triggered(&mut self) -> Vec<i32> {
        Vec::new()
    }
}

pub fn default_test_combo() -> KeyCombination {
    KeyCombination {
        ctrl: true,
        shift: true,
        alt: false,
        win: false,
        key: KeyCode(TEST_HOTKEY_VK),
    }
}

pub fn combo_label(combo: &KeyCombination) -> String {
    let mut parts = Vec::new();
    if combo.ctrl {
        parts.push("Ctrl".to_string());
    }
    if combo.shift {
        parts.push("Shift".to_string());
    }
    if combo.alt {
        parts.push("Alt".to_string());
    }
    if combo.win {
        parts.push("Win".to_string());
    }
    parts.push(virtual_key_name(combo.key.0));
    parts.join("+")
}

pub struct HotkeyTestSession {
    engine: Arc<ShortcutEngine>,
    binding: ShortcutBinding,
    registration: ShortcutRegistrationState,
    started_at_unix_ms: i64,
    records: Arc<Mutex<Vec<HotkeyFireRecord>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl HotkeyTestSession {
    pub fn start_simulated() -> Result<Self> {
        Self::start_with_registrar(Box::new(AlwaysOkRegistrar), default_test_combo())
    }

    pub fn start_platform() -> Result<Self> {
        Self::start_with_registrar(platform_registrar(), default_test_combo())
    }

    pub fn start_with_registrar(
        registrar: Box<dyn ShortcutRegistrar>,
        combo: KeyCombination,
    ) -> Result<Self> {
        let engine = Arc::new(ShortcutEngine::new_with_registrar(
            Default::default(),
            crate::shortcut::model::default_reserved_combinations(),
            registrar,
        )?);
        let binding = engine.create(ShortcutBinding {
            id: ShortcutId::new(),
            enabled: true,
            key_combo: combo,
            node_id: NodeId::new(),
            action_id: "hotkey_test_mode".into(),
            action_parameter: None,
        })?;
        let registration = engine.registration_state(binding.id);
        let events_rx = engine.event_receiver();
        let triggers_rx = engine.trigger_receiver();
        let records = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let engine = engine.clone();
            let records = records.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("hotkey-test-drain".into())
                .spawn(move || loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok(shortcut_id) = triggers_rx.recv_timeout(Duration::from_millis(20)) {
                        if let Some(binding) = engine.get(shortcut_id) {
                            engine.report_triggered(&binding, Ok(()));
                        }
                    }
                    while let Ok(event) = events_rx.try_recv() {
                        if let ShortcutRuntimeEvent::ShortcutTriggered {
                            shortcut_id,
                            node_id,
                            action_id,
                            timestamp_utc,
                            success,
                            message,
                        } = event
                        {
                            records.lock().push(HotkeyFireRecord {
                                timestamp_utc,
                                unix_ms: timestamp_utc.timestamp_millis(),
                                shortcut_id,
                                node_id,
                                action_id,
                                success,
                                message,
                            });
                        }
                    }
                })
                .map_err(|error| crate::error::BackendError::InvalidOperation(error.to_string()))?
        };
        Ok(Self {
            engine,
            binding,
            registration,
            started_at_unix_ms: Utc::now().timestamp_millis(),
            records,
            stop,
            join: Some(join),
        })
    }

    pub fn simulate_fire(&self) {
        self.engine.report_triggered(&self.binding, Ok(()));
    }

    pub fn simulate_fire_failed(&self, message: impl Into<String>) {
        self.engine
            .report_triggered(&self.binding, Err(message.into()));
    }

    pub fn events(&self) -> Vec<HotkeyFireRecord> {
        self.records.lock().clone()
    }

    pub fn wait_for_events(&self, min: usize, timeout: Duration) -> Vec<HotkeyFireRecord> {
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events();
            if events.len() >= min || Instant::now() >= deadline {
                return events;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn export(&self) -> HotkeyTestExport {
        HotkeyTestExport {
            started_at_unix_ms: self.started_at_unix_ms,
            combo: self.binding.key_combo,
            combo_label: combo_label(&self.binding.key_combo),
            registration: self.registration.clone(),
            events: self.events(),
        }
    }
}

impl Drop for HotkeyTestSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn virtual_key_name(key: u16) -> String {
    match key {
        0x70..=0x7B => format!("F{}", key - 0x6F),
        other => format!("0x{other:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_fire_is_recorded_with_timestamp() {
        let session = HotkeyTestSession::start_simulated().unwrap();
        session.simulate_fire();
        let events = session.wait_for_events(1, Duration::from_secs(2));
        assert_eq!(events.len(), 1);
        assert!(events[0].success);
        assert!(events[0].unix_ms > 0);
        assert_eq!(events[0].action_id, "hotkey_test_mode");
        let export = session.export();
        assert_eq!(export.combo_label, "Ctrl+Shift+F9");
        assert_eq!(export.registration, ShortcutRegistrationState::Registered);
    }
}
