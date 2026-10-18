use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};

use crate::error::{BackendError, Result};

use super::{
    model::{
        default_reserved_combinations, KeyCombination, ShortcutBinding, ShortcutConflict,
        ShortcutId, ShortcutRegistrationState, ShortcutRuntimeEvent, ShortcutValidation,
    },
    registrar::{platform_registrar, ShortcutRegistrar},
};

const CONTROL_CAPACITY: usize = 256;
const TRIGGER_CAPACITY: usize = 256;

enum RegistrarCommand {
    Register {
        shortcut_id: ShortcutId,
        combination: KeyCombination,
        response: Sender<std::result::Result<(), String>>,
    },
    Unregister {
        shortcut_id: ShortcutId,
        response: Sender<()>,
    },
}

pub struct ShortcutEngine {
    bindings: RwLock<HashMap<ShortcutId, ShortcutBinding>>,
    registration_states: RwLock<HashMap<ShortcutId, ShortcutRegistrationState>>,
    reserved: Vec<KeyCombination>,
    control_tx: Sender<RegistrarCommand>,
    triggers_rx: Receiver<ShortcutId>,
    events_tx: Sender<ShortcutRuntimeEvent>,
    events_rx: Receiver<ShortcutRuntimeEvent>,
    handle: Mutex<Option<RegistrarHandle>>,
}

impl ShortcutEngine {
    pub fn new(bindings: HashMap<ShortcutId, ShortcutBinding>) -> Result<Self> {
        Self::new_with_registrar(
            bindings,
            default_reserved_combinations(),
            platform_registrar(),
        )
    }

    pub fn new_with_registrar(
        bindings: HashMap<ShortcutId, ShortcutBinding>,
        reserved: Vec<KeyCombination>,
        registrar: Box<dyn ShortcutRegistrar>,
    ) -> Result<Self> {
        let (control_tx, control_rx) = crossbeam_channel::bounded(CONTROL_CAPACITY);
        let (triggers_tx, triggers_rx) = crossbeam_channel::bounded(TRIGGER_CAPACITY);
        let (events_tx, events_rx) = crossbeam_channel::bounded(1024);
        let stop = Arc::new(AtomicBool::new(false));
        let join = spawn_registrar_thread(registrar, control_rx, triggers_tx, stop.clone())?;
        let engine = Self {
            bindings: RwLock::new(HashMap::new()),
            registration_states: RwLock::new(HashMap::new()),
            reserved,
            control_tx,
            triggers_rx,
            events_tx,
            events_rx,
            handle: Mutex::new(Some(RegistrarHandle {
                stop,
                join: Some(join),
            })),
        };
        engine.replace_bindings(bindings)?;
        Ok(engine)
    }

    pub fn list(&self) -> Vec<ShortcutBinding> {
        let mut bindings: Vec<_> = self.bindings.read().values().cloned().collect();
        bindings.sort_by_key(|binding| binding.id);
        bindings
    }

    pub fn snapshot(&self) -> HashMap<ShortcutId, ShortcutBinding> {
        self.bindings.read().clone()
    }

    pub fn get(&self, id: ShortcutId) -> Option<ShortcutBinding> {
        self.bindings.read().get(&id).cloned()
    }

    pub fn registration_state(&self, id: ShortcutId) -> ShortcutRegistrationState {
        self.registration_states
            .read()
            .get(&id)
            .cloned()
            .unwrap_or(ShortcutRegistrationState::Unregistered)
    }

    pub fn validate(&self, candidate: &ShortcutBinding) -> ShortcutValidation {
        if let Err(message) = candidate.key_combo.validate() {
            return ShortcutValidation {
                conflict: ShortcutConflict::ReservedByVoiceMixer,
                message: Some(message),
            };
        }
        if self.reserved.contains(&candidate.key_combo) {
            return ShortcutValidation {
                conflict: ShortcutConflict::ReservedByVoiceMixer,
                message: Some("shortcut is reserved by VoiceMixer".into()),
            };
        }
        if candidate.enabled
            && self.bindings.read().values().any(|binding| {
                binding.id != candidate.id
                    && binding.enabled
                    && binding.key_combo == candidate.key_combo
            })
        {
            return ShortcutValidation {
                conflict: ShortcutConflict::DuplicateInVoiceMixer,
                message: Some("shortcut duplicates another VoiceMixer binding".into()),
            };
        }
        if matches!(
            self.registration_state(candidate.id),
            ShortcutRegistrationState::Failed { .. }
        ) && self.bindings.read().contains_key(&candidate.id)
        {
            return ShortcutValidation {
                conflict: ShortcutConflict::RegistrationFailed,
                message: Some("shortcut registration failed".into()),
            };
        }
        ShortcutValidation::none()
    }

    pub fn create(&self, binding: ShortcutBinding) -> Result<ShortcutBinding> {
        if self.bindings.read().contains_key(&binding.id) {
            return Err(BackendError::InvalidOperation(
                "shortcut id already exists".into(),
            ));
        }
        ensure_internal_validation(self.validate(&binding))?;
        self.bindings.write().insert(binding.id, binding.clone());
        self.apply_registration(&binding);
        self.emit_changed();
        Ok(binding)
    }

    pub fn update(&self, binding: ShortcutBinding) -> Result<ShortcutBinding> {
        let previous = self
            .bindings
            .read()
            .get(&binding.id)
            .cloned()
            .ok_or_else(|| BackendError::InvalidOperation("shortcut binding not found".into()))?;
        ensure_internal_validation(self.validate(&binding))?;
        if binding.enabled {
            let state = self.register(binding.id, binding.key_combo);
            self.set_registration_state(binding.id, state.clone());
            if matches!(state, ShortcutRegistrationState::Failed { .. }) && previous.enabled {
                let rollback = self.register(previous.id, previous.key_combo);
                self.set_registration_state(previous.id, rollback);
                return Err(BackendError::InvalidOperation(
                    "SHORTCUT_REGISTRATION_FAILED: previous binding was restored".into(),
                ));
            }
        } else {
            self.unregister(binding.id);
            self.set_registration_state(binding.id, ShortcutRegistrationState::Unregistered);
        }
        self.bindings.write().insert(binding.id, binding.clone());
        self.emit_changed();
        Ok(binding)
    }

    pub fn delete(&self, id: ShortcutId) -> Result<ShortcutBinding> {
        let binding =
            self.bindings.write().remove(&id).ok_or_else(|| {
                BackendError::InvalidOperation("shortcut binding not found".into())
            })?;
        // Remove from the dispatch lookup before asking the OS to unregister;
        // an already queued WM_HOTKEY can no longer execute the binding.
        self.unregister(id);
        self.set_registration_state(id, ShortcutRegistrationState::Unregistered);
        self.registration_states.write().remove(&id);
        self.emit_changed();
        Ok(binding)
    }

    pub fn set_enabled(&self, id: ShortcutId, enabled: bool) -> Result<ShortcutBinding> {
        let mut binding =
            self.bindings.read().get(&id).cloned().ok_or_else(|| {
                BackendError::InvalidOperation("shortcut binding not found".into())
            })?;
        binding.enabled = enabled;
        ensure_internal_validation(self.validate(&binding))?;
        if enabled {
            let state = self.register(id, binding.key_combo);
            self.set_registration_state(id, state);
        } else {
            self.unregister(id);
            self.set_registration_state(id, ShortcutRegistrationState::Unregistered);
        }
        self.bindings.write().insert(id, binding.clone());
        self.emit_changed();
        Ok(binding)
    }

    pub fn replace_bindings(&self, bindings: HashMap<ShortcutId, ShortcutBinding>) -> Result<()> {
        let previous_ids: Vec<_> = self.bindings.read().keys().copied().collect();
        self.bindings.write().clear();
        for id in previous_ids {
            self.unregister(id);
        }
        self.registration_states.write().clear();
        *self.bindings.write() = bindings;
        let bindings = self.list();
        for binding in &bindings {
            if !binding.enabled {
                self.set_registration_state(binding.id, ShortcutRegistrationState::Unregistered);
                continue;
            }
            let validation = self.validate(binding);
            if validation.conflict != ShortcutConflict::None {
                self.set_registration_state(
                    binding.id,
                    ShortcutRegistrationState::Failed {
                        message: format!("SHORTCUT_CONFLICT: {:?}", validation.conflict),
                    },
                );
                continue;
            }
            self.apply_registration(binding);
        }
        self.emit_changed();
        Ok(())
    }

    pub fn trigger_receiver(&self) -> Receiver<ShortcutId> {
        self.triggers_rx.clone()
    }

    pub fn event_receiver(&self) -> Receiver<ShortcutRuntimeEvent> {
        self.events_rx.clone()
    }

    pub fn report_triggered(
        &self,
        binding: &ShortcutBinding,
        outcome: std::result::Result<(), String>,
    ) {
        let (success, message) = match outcome {
            Ok(()) => (true, None),
            Err(message) => (false, Some(message)),
        };
        let _ = self
            .events_tx
            .try_send(ShortcutRuntimeEvent::ShortcutTriggered {
                shortcut_id: binding.id,
                node_id: binding.node_id,
                action_id: binding.action_id.clone(),
                timestamp_utc: chrono::Utc::now(),
                success,
                message,
            });
    }

    fn apply_registration(&self, binding: &ShortcutBinding) {
        let state = if binding.enabled {
            self.register(binding.id, binding.key_combo)
        } else {
            ShortcutRegistrationState::Unregistered
        };
        self.set_registration_state(binding.id, state);
    }

    fn register(
        &self,
        shortcut_id: ShortcutId,
        combination: KeyCombination,
    ) -> ShortcutRegistrationState {
        let (response_tx, response_rx) = crossbeam_channel::bounded(1);
        if self
            .control_tx
            .try_send(RegistrarCommand::Register {
                shortcut_id,
                combination,
                response: response_tx,
            })
            .is_err()
        {
            return ShortcutRegistrationState::Failed {
                message: "SHORTCUT_REGISTRATION_FAILED: registrar queue is full".into(),
            };
        }
        match response_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => ShortcutRegistrationState::Registered,
            Ok(Err(message)) => ShortcutRegistrationState::Failed { message },
            Err(_) => ShortcutRegistrationState::Failed {
                message: "SHORTCUT_REGISTRATION_FAILED: registrar timed out".into(),
            },
        }
    }

    fn unregister(&self, shortcut_id: ShortcutId) {
        let (response_tx, response_rx) = crossbeam_channel::bounded(1);
        if self
            .control_tx
            .try_send(RegistrarCommand::Unregister {
                shortcut_id,
                response: response_tx,
            })
            .is_ok()
        {
            let _ = response_rx.recv_timeout(Duration::from_secs(2));
        }
    }

    fn set_registration_state(&self, id: ShortcutId, state: ShortcutRegistrationState) {
        self.registration_states.write().insert(id, state.clone());
        let _ = self
            .events_tx
            .try_send(ShortcutRuntimeEvent::RegistrationStateChanged {
                shortcut_id: id,
                state,
            });
    }

    fn emit_changed(&self) {
        let _ = self
            .events_tx
            .try_send(ShortcutRuntimeEvent::ShortcutsChanged {
                bindings: self.list(),
            });
    }
}

fn ensure_internal_validation(validation: ShortcutValidation) -> Result<()> {
    if validation.conflict == ShortcutConflict::None
        || validation.conflict == ShortcutConflict::RegistrationFailed
    {
        Ok(())
    } else {
        Err(BackendError::InvalidOperation(format!(
            "SHORTCUT_CONFLICT: {:?}",
            validation.conflict
        )))
    }
}

fn spawn_registrar_thread(
    mut registrar: Box<dyn ShortcutRegistrar>,
    commands: Receiver<RegistrarCommand>,
    triggers: Sender<ShortcutId>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("shortcut-windows-control".into())
        .spawn(move || {
            let mut ids = HashMap::<ShortcutId, i32>::new();
            let mut reverse = HashMap::<i32, ShortcutId>::new();
            let mut next_id = 1_i32;
            // Prime the Windows message queue on its owning thread before the
            // first RegisterHotKey request is accepted.
            let _ = registrar.poll_triggered();
            while !stop.load(Ordering::Acquire) {
                if let Ok(command) = commands.recv_timeout(Duration::from_millis(10)) {
                    match command {
                        RegistrarCommand::Register {
                            shortcut_id,
                            combination,
                            response,
                        } => {
                            let registration_id = *ids.entry(shortcut_id).or_insert_with(|| {
                                let current = next_id;
                                next_id = next_id.saturating_add(1);
                                current
                            });
                            registrar.unregister(registration_id);
                            reverse.remove(&registration_id);
                            let result = registrar.register(registration_id, combination);
                            if result.is_ok() {
                                reverse.insert(registration_id, shortcut_id);
                            }
                            let _ = response.try_send(result);
                        }
                        RegistrarCommand::Unregister {
                            shortcut_id,
                            response,
                        } => {
                            if let Some(registration_id) = ids.remove(&shortcut_id) {
                                reverse.remove(&registration_id);
                                registrar.unregister(registration_id);
                            }
                            let _ = response.try_send(());
                        }
                    }
                }
                for registration_id in registrar.poll_triggered() {
                    if let Some(shortcut_id) = reverse.get(&registration_id) {
                        let _ = triggers.try_send(*shortcut_id);
                    }
                }
            }
            for registration_id in ids.into_values() {
                registrar.unregister(registration_id);
            }
        })
        .map_err(Into::into)
}

struct RegistrarHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for RegistrarHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ShortcutEngine {
    fn drop(&mut self) {
        self.handle.get_mut().take();
    }
}
