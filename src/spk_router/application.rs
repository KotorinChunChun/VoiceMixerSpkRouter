//! 設定・UI・音声ワーカーの実行順序と終了処理を統括する。
use super::{
    catalog,
    cli::{self, Launch, Operation, Options},
    config::{self, Mode, Settings, APP_NAME, APP_VERSION},
    engine,
    state::{Action, Phase, Status},
    telemetry::Shared,
};
#[cfg(windows)]
use super::{
    instance::{Acquisition, Instance},
    state::Transition,
    ui,
};
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DIAGNOSTIC_DURATION: Duration = Duration::from_secs(10);
const DIAGNOSTIC_POLL: Duration = Duration::from_millis(100);
#[cfg(windows)]
const UI_POLL: Duration = Duration::from_millis(20);

struct Paths {
    settings: PathBuf,
    directory: PathBuf,
    status: PathBuf,
}

impl Paths {
    fn new(mut settings: PathBuf) -> Result<Self, String> {
        if !settings.is_absolute() {
            settings = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(settings);
        }
        let directory = settings
            .parent()
            .ok_or("設定保存先がありません")?
            .to_path_buf();
        let status = directory.join("spk-status.json");
        Ok(Self {
            settings,
            directory,
            status,
        })
    }

    fn lock_directory(&self) -> Result<File, String> {
        fs::create_dir_all(&self.directory).map_err(|e| e.to_string())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        options
            .open(self.directory.join("spk-instance.lock"))
            .map_err(|e| format!("既に起動中、または保存先に書き込めません: {e}"))
    }
}

pub fn run(options: Options) -> Result<(), String> {
    match options.operation {
        Operation::Version => {
            println!("{APP_NAME} v{APP_VERSION}");
            return Ok(());
        }
        Operation::Help => {
            println!("{}", cli::help());
            return Ok(());
        }
        Operation::List => {
            println!(
                "{}",
                serde_json::to_string_pretty(&catalog::device_choices("", "")?)
                    .map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        _ => {}
    }
    let paths = Paths::new(options.config_path)?;
    match options.operation {
        Operation::Initialize => {
            if paths.settings.exists() {
                return Err("既存設定は上書きしません".into());
            }
            return Settings::default().save(&paths.settings);
        }
        Operation::Status => {
            println!(
                "{}",
                fs::read_to_string(&paths.status).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        _ => {}
    }
    // IPCは設定へのアクセスより先に行う。別の--configからの操作でも常駐は一つ。
    #[cfg(windows)]
    let _instance = match Instance::acquire(match options.operation {
        Operation::Launch(launch) => Some(launch),
        _ => None,
    })? {
        Acquisition::Forwarded => return Ok(()),
        Acquisition::Owner(instance) => instance,
    };
    let _directory_lock = paths.lock_directory()?;
    let first = !paths.settings.exists();
    let mut settings = if first {
        let settings = Settings::default();
        settings.save(&paths.settings)?;
        settings
    } else {
        Settings::load(&paths.settings)?
    };
    match options.operation {
        Operation::Diagnose(mode) => {
            settings.mode = settings.available_mode(mode);
            return diagnose(settings, paths.directory);
        }
        Operation::Launch(Launch::Mode(mode)) => {
            settings.mode = settings.available_mode(mode);
            settings.save(&paths.settings)?;
        }
        Operation::Launch(_) => {
            if let Some(mode) = settings.startup_mode {
                settings.mode = settings.available_mode(mode);
            }
        }
        _ => {}
    }
    settings.mode = settings.available_mode(settings.mode);
    #[cfg(windows)]
    {
        resident(
            settings,
            paths,
            (!options.startup && first) || options.operation == Operation::Launch(Launch::Settings),
            options.launch_origin,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (settings, first);
        Err("Windows専用です".into())
    }
}

/// 途中のエラーでもExitを送り、ワーカー終了まで排他ガードを保持する。
struct AudioWorker {
    commands: Sender<engine::Command>,
    statuses: Receiver<Status>,
    handle: Option<JoinHandle<()>>,
    meters: Shared,
}

impl AudioWorker {
    fn start(settings: Settings, directory: PathBuf) -> Self {
        let (commands, statuses, meters, handle) = engine::spawn(settings, directory);
        Self {
            commands,
            statuses,
            handle: Some(handle),
            meters,
        }
    }

    #[cfg(windows)]
    fn apply(
        &self,
        next: &Settings,
        path: &std::path::Path,
    ) -> Result<Receiver<Result<Status, String>>, String> {
        let (result, receive) = std::sync::mpsc::channel();
        self.commands
            .send(engine::Command::Apply {
                next: Box::new(next.clone()),
                path: path.into(),
                result,
            })
            .map_err(|_| "音声ワーカーが終了しました".to_owned())?;
        Ok(receive)
    }

    fn shutdown(&mut self) -> Result<(), String> {
        if let Some(handle) = self.handle.take() {
            let _ = self.commands.send(engine::Command::Exit);
            handle
                .join()
                .map_err(|_| "音声ワーカーが異常終了しました")?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn diagnose(settings: Settings, directory: PathBuf) -> Result<(), String> {
    let mut worker = AudioWorker::start(settings, directory);
    let started = Instant::now();
    let mut latest = None;
    while started.elapsed() < DIAGNOSTIC_DURATION {
        if let Ok(value) = worker.statuses.recv_timeout(DIAGNOSTIC_POLL) {
            println!(
                "{}",
                serde_json::to_string(&value).map_err(|e| e.to_string())?
            );
            latest = Some(value);
        }
    }
    worker.shutdown()?;
    diagnostic_result(latest)
}

fn diagnostic_result(latest: Option<Status>) -> Result<(), String> {
    match latest {
        Some(value) if value.phase.is_ready() => Ok(()),
        Some(value) => Err(format!("診断未完了（{}）: {}", value.phase, value.message)),
        None => Err("音声ワーカーから応答がありません".into()),
    }
}

#[cfg(windows)]
fn resident(
    mut settings: Settings,
    paths: Paths,
    open_settings: bool,
    launch_origin: String,
) -> Result<(), String> {
    struct Pending {
        next: Settings,
        editor: bool,
        prepared: Option<ui::PreparedHotkeys>,
        result: Receiver<Result<Status, String>>,
        rollback_error: Option<String>,
    }
    let mut ui = ui::Ui::new(&settings)?;
    if open_settings {
        ui.show_settings();
    }
    let mut worker = AudioWorker::start(settings.clone(), paths.directory.clone());
    let mut status = Status::new(settings.mode, Phase::Starting, "");
    let mut failure = None;
    let mut pending: Option<Pending> = None;
    let mut queued = None;
    let mut written_status = None;
    let management = crate::management::Endpoint::start(launch_origin)?;
    'resident: loop {
        if management.poll(&mut ui) {
            break;
        }
        let mut actions = ui.poll();
        if pending.is_none() {
            if let Some(action) = queued.take() {
                actions.insert(0, action);
            }
        }
        for action in actions {
            if matches!(action, Action::Exit) {
                break 'resident;
            }
            if pending.is_some() {
                queued = Some(action);
                continue;
            }
            let editor = matches!(&action, Action::Configure(_));
            let next = match action.transition(&settings) {
                Transition::Continue(next) => next,
                Transition::Exit => break 'resident,
            };
            if next == settings && !editor {
                continue;
            }
            let prepared = match next.validate().and_then(|_| ui.prepare_hotkeys(&next)) {
                Ok(keys) => keys,
                Err(error) => {
                    ui.settings_error(&error);
                    continue;
                }
            };
            let result = worker.apply(&next, &paths.settings)?;
            ui.set_applying(true);
            pending = Some(Pending {
                next,
                editor,
                prepared: Some(prepared),
                result,
                rollback_error: None,
            });
        }
        let ready = pending.as_ref().map(|p| p.result.try_recv());
        if let Some(Ok(result)) = ready {
            let mut applied = pending.take().unwrap();
            match result {
                Ok(value) => {
                    if let Some(error) = applied.rollback_error.take() {
                        ui.set_applying(false);
                        if applied.editor {
                            ui.settings_error(&error);
                        }
                        status = value;
                    } else if let Err(error) = ui.commit_hotkeys(applied.prepared.take().unwrap()) {
                        applied.next = settings.clone();
                        applied.result = worker.apply(&settings, &paths.settings)?;
                        applied.rollback_error = Some(error);
                        pending = Some(applied);
                    } else {
                        settings = applied.next;
                        ui.clear_operation_error();
                        status = value;
                        ui.set_applying(false);
                        if applied.editor {
                            ui.settings_saved(&settings);
                        }
                    }
                }
                Err(error) => {
                    ui.set_applying(false);
                    if applied.rollback_error.is_some() {
                        failure = Some(format!("キー設定の復旧に失敗: {error}"));
                        break;
                    }
                    ui.settings_error(&error);
                }
            }
        } else if matches!(
            ready,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected))
        ) {
            failure = Some("音声設定の適用応答が途絶えました".into());
            break;
        }
        for value in worker.statuses.try_iter() {
            if pending.is_none() && value.mode == settings.mode {
                status = value;
            }
        }
        if written_status.as_ref() != Some(&status) {
            if let Err(error) = config::write_json(&paths.status, &status) {
                ui.settings_error(&error);
            } else {
                written_status = Some(status.clone());
            }
        }
        let meter = worker.meters.lock().unwrap().clone();
        ui.update_meters(meter.as_ref());
        ui.update(&settings, status.phase, &status.message);
        if worker.is_finished() {
            failure = Some("音声ワーカーが終了しました".into());
            break;
        }
        thread::sleep(UI_POLL);
    }
    worker.shutdown()?;
    config::write_json(
        &paths.status,
        &Status::new(Mode::Stopped, Phase::Exited, "終了"),
    )?;
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_waiting_error_and_absent_status_are_failures() {
        assert!(diagnostic_result(None).is_err());
        for phase in [Phase::Waiting, Phase::Starting, Phase::Error, Phase::Exited] {
            assert!(diagnostic_result(Some(Status::new(Mode::Browser, phase, "未完了"))).is_err());
        }
        for phase in [Phase::Running, Phase::Stopped] {
            assert!(diagnostic_result(Some(Status::new(Mode::Browser, phase, "完了"))).is_ok());
        }
    }

    #[test]
    fn worker_drop_sends_exit_and_joins_before_returning() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc,
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = stopped.clone();
        let (commands, requests) = mpsc::channel();
        let (_, statuses) = mpsc::channel();
        let handle = thread::spawn(move || {
            assert!(matches!(requests.recv().unwrap(), engine::Command::Exit));
            observed.store(true, Ordering::SeqCst);
        });
        drop(AudioWorker {
            commands,
            statuses,
            handle: Some(handle),
            meters: Default::default(),
        });
        assert!(stopped.load(Ordering::SeqCst));
    }
}
