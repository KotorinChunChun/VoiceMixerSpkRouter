//! UI・CLI・音声処理で共有する操作と状態。保存形式は表示文言と分離する。
use super::config::{Mode, Settings};
use serde::Serialize;
use std::fmt;

#[derive(Debug)]
pub enum Action {
    Select(Mode),
    Set(Mode),
    ToggleOutput(usize),
    ToggleOverlay,
    Configure(Box<Settings>),
    Exit,
}

pub enum Transition {
    Continue(Settings),
    Exit,
}

impl Action {
    pub fn transition(self, current: &Settings) -> Transition {
        let mut next = current.clone();
        match self {
            Self::Select(mode) if current.mode_enabled(mode) => {
                next.mode = current.mode.toggle(mode)
            }
            Self::Set(mode) if current.mode_enabled(mode) => next.mode = mode,
            Self::Select(_) | Self::Set(_) => {}
            Self::ToggleOutput(index) => {
                if let Some(output) = next.outputs.get_mut(index) {
                    output.enabled = !output.enabled;
                }
            }
            Self::ToggleOverlay => next.overlay_mode = next.overlay_mode.next(),
            Self::Configure(value) => {
                next = *value;
                // 編集中の停止を、設定画面を開いた時点のモードで巻き戻さない。
                next.mode = current.mode;
            }
            Self::Exit => return Transition::Exit,
        }
        next.mode = next.available_mode(next.mode);
        Transition::Continue(next)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running,
    Waiting,
    Stopped,
    Error,
    Exited,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "準備中",
            Self::Running => "転送中",
            Self::Waiting => "待機中",
            Self::Error => "エラー",
            Self::Stopped | Self::Exited => "停止",
        }
    }

    pub fn is_ready(self) -> bool {
        matches!(self, Self::Running | Self::Stopped)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Stopped => "stopped",
            Self::Error => "error",
            Self::Exited => "exited",
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Status {
    pub mode: Mode,
    pub phase: Phase,
    pub message: String,
}

impl Status {
    pub fn new(mode: Mode, phase: Phase, message: impl Into<String>) -> Self {
        Self {
            mode,
            phase,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(action: Action, current: &Settings) -> Settings {
        match action.transition(current) {
            Transition::Continue(next) => next,
            Transition::Exit => panic!("設定変更を期待"),
        }
    }

    #[test]
    fn cli_is_idempotent_and_repeated_selection_stops() {
        let current = apply(Action::Set(Mode::Browser), &Settings::default());
        assert_eq!(apply(Action::Set(Mode::Browser), &current), current);
        assert_eq!(
            apply(Action::Select(Mode::Browser), &current).mode,
            Mode::Stopped
        );
        assert_eq!(
            apply(Action::Select(Mode::Device), &current).mode,
            Mode::Device
        );
    }

    #[test]
    fn saving_a_draft_keeps_stop_and_updates_other_settings() {
        let stopped = Settings::default();
        let mut draft = stopped.clone();
        draft.mode = Mode::Browser;
        draft.browser_port = 18767;
        let next = apply(Action::Configure(Box::new(draft)), &stopped);
        assert_eq!(next.mode, Mode::Stopped);
        assert_eq!(next.browser_port, 18767);
        assert_eq!(next.browser_token, stopped.browser_token);
    }

    #[test]
    fn outputs_toggle_independently_and_exit_does_not_modify_settings() {
        let current = Settings::default();
        let first = apply(Action::ToggleOutput(0), &current);
        assert!(first.outputs[0].enabled);
        assert!(!first.outputs[1].enabled);
        let both = apply(Action::ToggleOutput(1), &first);
        assert!(both.outputs.iter().all(|o| o.enabled));
        assert_eq!(apply(Action::ToggleOutput(7), &both), both);
        assert!(matches!(Action::Exit.transition(&both), Transition::Exit));
    }

    #[test]
    fn status_json_preserves_external_phase_names_and_diagnostic_success() {
        for (phase, name, ready) in [
            (Phase::Starting, "starting", false),
            (Phase::Running, "running", true),
            (Phase::Waiting, "waiting", false),
            (Phase::Stopped, "stopped", true),
            (Phase::Error, "error", false),
            (Phase::Exited, "exited", false),
        ] {
            let value = serde_json::to_value(Status::new(Mode::Stopped, phase, "状態")).unwrap();
            assert_eq!(
                value,
                serde_json::json!({"mode":"stopped","phase":name,"message":"状態"})
            );
            assert_eq!(phase.to_string(), name);
            assert_eq!(phase.is_ready(), ready);
        }
    }

    #[test]
    fn disabled_inputs_keep_settings_ignore_keys_and_stop_the_active_source() {
        let mut current = Settings::default();
        current.mode = Mode::Browser;
        let mut draft = current.clone();
        draft.input_enabled[2] = false;
        let next = apply(Action::Configure(Box::new(draft)), &current);
        assert_eq!(next.mode, Mode::Stopped);
        assert_eq!(next.browser_token, current.browser_token);
        assert_eq!(next.hotkeys, current.hotkeys);
        assert_eq!(apply(Action::Select(Mode::Browser), &next), next);
        assert_eq!(apply(Action::Set(Mode::Browser), &next), next);
        let mut restored = next.clone();
        restored.input_enabled[2] = true;
        assert_eq!(
            apply(Action::Select(Mode::Browser), &restored).mode,
            Mode::Browser
        );
    }
}
