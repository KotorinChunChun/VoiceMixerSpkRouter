#[path = "engine/browser.rs"]
mod browser;
#[path = "engine/routing.rs"]
pub(crate) mod routing;
#[path = "engine/session.rs"]
mod session;
use crate::{
    config::Settings,
    state::{Phase, Status},
    telemetry::Shared,
};
use session::Resident;
use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
pub enum Command {
    Apply {
        next: Box<Settings>,
        path: PathBuf,
        result: Sender<Result<Status, String>>,
    },
    Exit,
}
pub(crate) struct TransactionFailure {
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub stage: &'static str,
    pub detail: String,
    pub recovery: Option<String>,
}

pub(crate) fn commit_audio<T, S>(
    backend: &mut T,
    current: &mut Settings,
    next: Settings,
    apply: impl FnOnce(&mut T, &Settings) -> Result<S, String>,
    save: impl FnOnce(&Settings) -> Result<(), String>,
    restore: impl FnOnce(&mut T, &Settings) -> Result<(), String>,
) -> Result<S, TransactionFailure> {
    let result = apply(backend, &next)
        .map_err(|error| ("audio_apply_failed", error))
        .and_then(|state| {
            if next != *current {
                save(&next).map_err(|error| ("save_failed", error))?;
            }
            Ok(state)
        });
    match result {
        Ok(state) => {
            *current = next;
            Ok(state)
        }
        Err((stage, detail)) => Err(TransactionFailure {
            stage,
            detail,
            recovery: restore(backend, current).err(),
        }),
    }
}

/// 音声反映と保存の両方が成功したときだけ設定を確定する。
fn apply_transaction(
    resident: &mut Option<Resident>,
    current: &mut Settings,
    next: Settings,
    path: &Path,
    directory: &Path,
) -> Result<Status, String> {
    next.validate()?;
    // キー・表示・用途名だけなら、故障中のデバイスの復旧にも依存しない。
    if !current.requires_audio_update(&next) {
        if next != *current {
            next.save(path)?;
        }
        *current = next;
        return Ok(match resident.as_mut() {
            Some(active) => active
                .tick(current)
                .unwrap_or_else(|error| Status::new(current.mode, Phase::Error, error)),
            None => Status::new(current.mode, Phase::Starting, "準備中"),
        });
    }
    let created = resident.is_none();
    if created {
        *resident = Some(Resident::new(current, directory)?);
    }
    let active = resident.as_mut().unwrap();
    commit_audio(
        active,
        current,
        next,
        |active, next| {
            active.configure(next)?;
            active.tick(next)
        },
        |next| next.save(path),
        |active, previous| active.configure(previous),
    )
    .map_err(|failure| match failure.recovery {
        Some(restore) => format!("{} / 旧音声設定の復旧にも失敗: {restore}", failure.detail),
        None => failure.detail,
    })
}
pub fn spawn(
    settings: Settings,
    directory: PathBuf,
) -> (Sender<Command>, Receiver<Status>, Shared, JoinHandle<()>) {
    let (commands, requests) = mpsc::channel();
    let (events, statuses) = mpsc::channel();
    let meters: Shared = Arc::new(Mutex::new(None));
    let shared = meters.clone();
    let worker = thread::spawn(move || {
        let mut settings = settings;
        let mut resident: Option<Resident> = None;
        let mut validate_at = Instant::now();
        let mut failed = false;
        let mut last = None;
        let mut publish = |status: Status| {
            if last.as_ref() != Some(&status) {
                let _ = events.send(status.clone());
                last = Some(status);
            }
        };
        loop {
            match requests.recv_timeout(COMMAND_POLL_INTERVAL) {
                Ok(Command::Exit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Command::Apply { next, path, result }) => {
                    let changed = settings.requires_audio_update(&next);
                    let applied =
                        apply_transaction(&mut resident, &mut settings, *next, &path, &directory);
                    if changed {
                        *shared.lock().unwrap() = None;
                    }
                    if let Ok(status) = &applied {
                        failed = false;
                        publish(status.clone());
                    } else if resident
                        .as_ref()
                        .is_some_and(|active| !active.is_configured())
                    {
                        failed = true;
                        publish(Status::new(
                            settings.mode,
                            Phase::Error,
                            applied.as_ref().unwrap_err().clone(),
                        ));
                    }
                    // 返信後に状態通知を消費するUIも、旧設定の復旧状態を取得できる。
                    let _ = result.send(applied);
                    validate_at = Instant::now() + RETRY_INTERVAL;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= validate_at {
                let applied = (|| {
                    if resident.is_none() {
                        resident = Some(Resident::new(&settings, &directory)?);
                    }
                    resident.as_mut().unwrap().configure(&settings)
                })();
                validate_at = Instant::now() + RETRY_INTERVAL;
                match applied {
                    Ok(()) => failed = false,
                    Err(error) => {
                        failed = true;
                        publish(Status::new(settings.mode, Phase::Error, error));
                    }
                }
            }
            if let Some(active) = resident.as_mut() {
                if failed {
                    active.discard_events();
                    continue;
                }
                match active.tick(&settings) {
                    Ok(status) => publish(status),
                    Err(error) => {
                        active.silence();
                        *shared.lock().unwrap() = None;
                        failed = true;
                        validate_at = Instant::now() + RETRY_INTERVAL;
                        publish(Status::new(settings.mode, Phase::Error, error));
                    }
                }
                if !failed {
                    if let Some(snapshot) = active.meter_snapshot(settings.mode) {
                        *shared.lock().unwrap() = Some(snapshot);
                    }
                }
            }
        }
        drop(resident);
    });
    (commands, statuses, meters, worker)
}
#[cfg(all(test, feature = "edge-websocket"))]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Settings) {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut settings = Settings::default();
        settings.browser_port = port.local_addr().unwrap().port();
        settings.mode = crate::config::Mode::Browser;
        (dir, settings)
    }
    #[test]
    fn audio_apply_and_save_failures_restore_committed_settings_and_release_candidate_port() {
        let (dir, mut current) = setup();
        let path = dir.path().join("settings.json");
        current.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut resident = Some(Resident::new(&current, dir.path()).unwrap());
        resident.as_mut().unwrap().configure(&current).unwrap();
        let original = current.clone();
        apply_transaction(
            &mut resident,
            &mut current,
            original.clone(),
            &path,
            dir.path(),
        )
        .unwrap();
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut bad = current.clone();
        bad.browser_port = occupied.local_addr().unwrap().port();
        assert!(apply_transaction(&mut resident, &mut current, bad, &path, dir.path()).is_err());
        assert_eq!(current, original);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            resident.as_mut().unwrap().tick(&current).unwrap().phase,
            Phase::Waiting
        );
        let mut changed = current.clone();
        changed.mode = crate::config::Mode::Stopped;
        assert!(
            apply_transaction(&mut resident, &mut current, changed, dir.path(), dir.path())
                .is_err()
        );
        assert_eq!(current, original);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            resident.as_mut().unwrap().tick(&current).unwrap().phase,
            Phase::Waiting
        );
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    #[test]
    fn key_and_display_save_does_not_create_or_restart_an_audio_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut current = Settings::default();
        current.mode = crate::config::Mode::Device;
        current.device_id = "unavailable-device".into();
        let mut next = current.clone();
        next.hotkeys[0].ctrl = true;
        next.output_labels[1] = "音声入力用".into();
        next.overlay_mode = crate::config::OverlayMode::Brief;
        let mut resident = None;
        apply_transaction(&mut resident, &mut current, next.clone(), &path, dir.path()).unwrap();
        assert!(resident.is_none());
        assert_eq!(current, next);
        assert_eq!(Settings::load(&path).unwrap(), next);
    }
}
