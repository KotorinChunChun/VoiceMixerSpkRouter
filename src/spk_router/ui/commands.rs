//! IPCの数値互換性と、常駐UI内の操作・編集制限を管理する。
use crate::{config::Mode, state::Action};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingCommand {
    ShowSettings,
    Exit,
    SetMode(Mode),
}
impl ExistingCommand {
    pub(super) fn command(self) -> Command {
        match self {
            Self::ShowSettings => Command::Show,
            Self::Exit => Command::Exit,
            Self::SetMode(mode) => match mode {
                Mode::Device => Command::SetDevice,
                Mode::Application => Command::SetApplication,
                Mode::Browser => Command::SetBrowser,
                Mode::Stopped => Command::SetStopped,
            },
        }
    }
}

#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Command {
    Device = 1,
    Application = 2,
    Browser = 3,
    Stopped = 4,
    Output1 = 5,
    Output2 = 6,
    Show = 100,
    Exit = 101,
    Menu = 102,
    Restore = 103,
    ToggleOverlay = 104,
    MenuExit = 205,
    BasicPage = 506,
    KeyPage = 507,
    SetDevice = 201,
    SetApplication = 202,
    SetBrowser = 203,
    SetStopped = 204,
    Save = 501,
    Cancel = 502,
    Refresh = 503,
    AppChanged = 504,
    KeyChanged = 505,
    InputChanged = 508,
    BrowserInstallHelp = 509,
    BrowserSetupHelp = 510,
    OpenRepository = 511,
}
impl Command {
    pub(super) fn is_shortcut(self) -> bool {
        matches!(
            self,
            Self::Device
                | Self::Application
                | Self::Browser
                | Self::Stopped
                | Self::Show
                | Self::ToggleOverlay
        )
    }
    pub(super) fn from_id(id: usize) -> Option<Self> {
        Some(match id {
            1 => Self::Device,
            2 => Self::Application,
            3 => Self::Browser,
            4 => Self::Stopped,
            5 => Self::Output1,
            6 => Self::Output2,
            100 => Self::Show,
            101 => Self::Exit,
            102 => Self::Menu,
            103 => Self::Restore,
            104 => Self::ToggleOverlay,
            205 => Self::MenuExit,
            506 => Self::BasicPage,
            507 => Self::KeyPage,
            201 => Self::SetDevice,
            202 => Self::SetApplication,
            203 => Self::SetBrowser,
            204 => Self::SetStopped,
            501 => Self::Save,
            502 => Self::Cancel,
            503 => Self::Refresh,
            504 => Self::AppChanged,
            505 => Self::KeyChanged,
            508 => Self::InputChanged,
            509 => Self::BrowserInstallHelp,
            510 => Self::BrowserSetupHelp,
            511 => Self::OpenRepository,
            _ => return None,
        })
    }
    pub(super) fn action(self, editing: bool) -> Option<Action> {
        match self {
            Self::Device if !editing => Some(Action::Select(Mode::Device)),
            Self::Application if !editing => Some(Action::Select(Mode::Application)),
            Self::Browser if !editing => Some(Action::Select(Mode::Browser)),
            Self::Stopped if !editing => Some(Action::Select(Mode::Stopped)),
            Self::Output1 if !editing => Some(Action::ToggleOutput(0)),
            Self::Output2 if !editing => Some(Action::ToggleOutput(1)),
            Self::SetDevice if !editing => Some(Action::Set(Mode::Device)),
            Self::SetApplication if !editing => Some(Action::Set(Mode::Application)),
            Self::SetBrowser if !editing => Some(Action::Set(Mode::Browser)),
            Self::SetStopped if !editing => Some(Action::Set(Mode::Stopped)),
            Self::MenuExit if !editing => Some(Action::Exit),
            Self::Exit => Some(Action::Exit),
            Self::ToggleOverlay if !editing => Some(Action::ToggleOverlay),
            _ => None,
        }
    }
}
static COMMANDS: Mutex<Vec<(Command, bool)>> = Mutex::new(Vec::new());
pub(super) fn queue(command: Command) {
    COMMANDS.lock().unwrap().push((command, false));
}
pub(super) fn queue_shortcut(command: Command) {
    if command.is_shortcut() {
        COMMANDS.lock().unwrap().push((command, true));
    }
}
pub(super) fn drain() -> Vec<(Command, bool)> {
    std::mem::take(&mut *COMMANDS.lock().unwrap())
}
pub(super) fn clear() {
    COMMANDS.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::{Command, ExistingCommand};
    use crate::{config::Mode, state::Action};
    #[test]
    fn only_four_inputs_and_two_independent_outputs_are_commands() {
        for (command, mode) in [
            (Command::Device, Mode::Device),
            (Command::Application, Mode::Application),
            (Command::Browser, Mode::Browser),
            (Command::Stopped, Mode::Stopped),
        ] {
            assert!(matches!(command.action(false), Some(Action::Select(value)) if value == mode));
            assert_eq!(Command::from_id(command as usize), Some(command));
        }
        assert!(matches!(
            Command::Output1.action(false),
            Some(Action::ToggleOutput(0))
        ));
        assert!(matches!(
            Command::Output2.action(false),
            Some(Action::ToggleOutput(1))
        ));
        assert!(Command::from_id(7).is_none());
        assert!(Command::Show.action(false).is_none());
    }
    #[test]
    fn editing_blocks_all_changes_except_maintenance_exit() {
        for command in [
            Command::Device,
            Command::Application,
            Command::Browser,
            Command::Output1,
            Command::Output2,
            Command::SetDevice,
            Command::SetApplication,
            Command::SetBrowser,
        ] {
            assert!(command.action(true).is_none());
        }
        assert!(Command::Stopped.action(true).is_none());
        assert!(Command::SetStopped.action(true).is_none());
        assert!(Command::MenuExit.action(true).is_none());
        assert!(Command::ToggleOverlay.action(true).is_none());
        assert!(matches!(Command::Exit.action(true), Some(Action::Exit)));
    }
    #[test]
    fn ipc_mode_commands_set_without_toggling() {
        for mode in Mode::ALL {
            let command = ExistingCommand::SetMode(mode).command();
            assert!(matches!(command.action(false), Some(Action::Set(value)) if value == mode));
            assert_eq!(Command::from_id(command as usize), Some(command));
        }
        assert_eq!(ExistingCommand::ShowSettings.command() as usize, 100);
        assert_eq!(ExistingCommand::Exit.command() as usize, 101);
        assert_eq!(
            ExistingCommand::SetMode(Mode::Device).command() as usize,
            201
        );
        assert_eq!(
            ExistingCommand::SetMode(Mode::Application).command() as usize,
            202
        );
        assert_eq!(
            ExistingCommand::SetMode(Mode::Browser).command() as usize,
            203
        );
        assert_eq!(
            ExistingCommand::SetMode(Mode::Stopped).command() as usize,
            204
        );
    }
}
