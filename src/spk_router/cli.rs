//! 副作用のない引数解析。キーのトグルとCLIのモード指定を混同しない。
use super::config::{Mode, APP_NAME, APP_VERSION};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    Restore,
    Startup,
    Settings,
    Stop,
    Mode(Mode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Launch(Launch),
    Initialize,
    Status,
    Diagnose(Mode),
    Version,
    List,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub config_path: PathBuf,
    pub startup: bool,
    pub launch_origin: String,
    pub operation: Operation,
}

impl Options {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        default_path: PathBuf,
    ) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut path = default_path;
        let mut startup = false;
        let mut launch_origin = "standalone".to_owned();
        let mut action = None;
        let mut initial = None;
        let mut init = false;
        let mut status = false;
        let mut check = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--startup" => startup = true,
                "--launch-origin" => {
                    launch_origin = args.next().ok_or("--launch-originの値が必要です")?;
                    if !matches!(
                        launch_origin.as_str(),
                        "standalone" | "host_manual" | "direct_task" | "via_kotorintoys"
                    ) {
                        return Err("起動経路が不正です".into());
                    }
                }
                "--config" => path = PathBuf::from(args.next().ok_or("--configの値が必要です")?),
                "--mode" => {
                    initial = Some(Mode::parse(&args.next().ok_or("--modeの値が必要です")?)?)
                }
                "--init" => init = true,
                "--stop" => action = Some(Launch::Stop),
                "--settings" => action = Some(Launch::Settings),
                "--status" => status = true,
                "--check" => {
                    check = Some(Mode::parse(&args.next().ok_or("--checkの値が必要です")?)?)
                }
                "--version" | "--list" | "--help" => {
                    // 既存CLIと同様に、その時点で情報表示へ切り替える。
                    return Ok(Self {
                        config_path: path,
                        startup,
                        launch_origin,
                        operation: match arg.as_str() {
                            "--version" => Operation::Version,
                            "--list" => Operation::List,
                            _ => Operation::Help,
                        },
                    });
                }
                _ => return Err(format!("不明な引数: {arg}")),
            }
        }
        if [
            init,
            status,
            check.is_some(),
            initial.is_some(),
            action.is_some(),
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
            > 1
        {
            return Err("初期化・状態確認・診断・起動操作は一度に1つ指定してください".into());
        }
        let operation = if init {
            Operation::Initialize
        } else if status {
            Operation::Status
        } else if let Some(mode) = check {
            Operation::Diagnose(mode)
        } else {
            Operation::Launch(action.or(initial.map(Launch::Mode)).unwrap_or(if startup {
                Launch::Startup
            } else {
                Launch::Restore
            }))
        };
        Ok(Self {
            config_path: path,
            startup,
            launch_origin,
            operation,
        })
    }
}

pub fn help() -> String {
    format!("{APP_NAME} v{APP_VERSION}\n--config FILE\n--startup: 設定画面を開かず起動\n--launch-origin standalone|host_manual|direct_task|via_kotorintoys\n--init: 初期設定作成\n--mode device|application|browser|stopped: 取得元切替\n--settings: 設定画面\n--stop: 常駐終了\n--status: 状態JSON\n--list: 再生デバイス一覧\n--check device|application|browser|stopped: 保存設定で10秒診断\n既定キー（設定画面で変更可能）\nShift+F1/F2/F3: 取得元切替（再押下で停止）\nShift+F4: 停止")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(|s| (*s).to_owned()), "default.json".into())
    }

    #[test]
    fn command_line_distinguishes_stop_from_stopped_and_keeps_aliases() {
        for (args, expected) in [
            (vec![], Operation::Launch(Launch::Restore)),
            (vec!["--stop"], Operation::Launch(Launch::Stop)),
            (
                vec!["--mode", "stop"],
                Operation::Launch(Launch::Mode(Mode::Stopped)),
            ),
            (
                vec!["--mode", "app"],
                Operation::Launch(Launch::Mode(Mode::Application)),
            ),
            (
                vec!["--check", "browser"],
                Operation::Diagnose(Mode::Browser),
            ),
            (vec!["--init"], Operation::Initialize),
            (vec!["--status"], Operation::Status),
        ] {
            assert_eq!(parse(&args).unwrap().operation, expected);
        }
    }

    #[test]
    fn invalid_or_conflicting_operations_fail_before_any_side_effect() {
        for args in [
            vec!["--config"],
            vec!["--mode"],
            vec!["--check"],
            vec!["--mode", "invalid"],
            vec!["--unknown"],
            vec!["--init", "--status"],
            vec!["--mode", "device", "--settings"],
            vec!["--stop", "--check", "stopped"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn repeated_options_and_immediate_information_keep_their_precedence() {
        assert_eq!(
            parse(&["--mode", "device", "--mode", "browser"])
                .unwrap()
                .operation,
            Operation::Launch(Launch::Mode(Mode::Browser))
        );
        assert_eq!(
            parse(&["--stop", "--settings"]).unwrap().operation,
            Operation::Launch(Launch::Settings)
        );
        for (flag, operation) in [
            ("--help", Operation::Help),
            ("--version", Operation::Version),
            ("--list", Operation::List),
        ] {
            assert_eq!(
                parse(&["--init", "--status", flag, "--invalid"])
                    .unwrap()
                    .operation,
                operation
            );
        }
    }

    #[test]
    fn config_path_preserves_spaces_and_last_value() {
        let options =
            parse(&["--config", "old.json", "--config", "設定 with spaces.json"]).unwrap();
        assert_eq!(options.config_path, PathBuf::from("設定 with spaces.json"));
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;
    #[test]
    fn host_start_is_silent_and_origin_is_validated() {
        let parse = |values: &[&str]| {
            Options::parse(values.iter().map(|s| s.to_string()), "settings.json".into())
        };
        let options = parse(&["--startup", "--launch-origin", "via_kotorintoys"]).unwrap();
        assert!(options.startup);
        assert_eq!(options.launch_origin, "via_kotorintoys");
        assert!(parse(&["--launch-origin"]).is_err());
        assert!(parse(&["--launch-origin", "unknown"]).is_err());
        assert!(!parse(&[]).unwrap().startup);
    }
}
