//! 外部TTS/STT CLIを音声threadと独立して実行する。
use crate::{config::persistence::ConfigStore, BackendError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechSettings {
    pub tts_executable: PathBuf,
    pub tts_settings: PathBuf,
    pub engine: String,
    pub engine_url: String,
    pub voice_name: String,
    pub voice_version: String,
    pub speed: f64,
    pub python_executable: PathBuf,
    pub stt_script: PathBuf,
    pub model: String,
    pub language: String,
    pub device: String,
}

impl Default for SpeechSettings {
    fn default() -> Self {
        Self {
            tts_executable: PathBuf::new(),
            tts_settings: PathBuf::new(),
            engine: "voisona".into(),
            engine_url: "http://127.0.0.1:50021".into(),
            voice_name: String::new(),
            voice_version: String::new(),
            speed: 1.0,
            python_executable: PathBuf::new(),
            stt_script: PathBuf::new(),
            model: "small".into(),
            language: "ja".into(),
            device: "cpu".into(),
        }
    }
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::InvalidOperation(message.into())
}

impl SpeechSettings {
    fn validate(&self) -> Result<()> {
        if !["voisona", "voicevox"].contains(&self.engine.as_str())
            || !["cpu", "cuda"].contains(&self.device.as_str())
            || !self.speed.is_finite()
            || !(0.5..=2.0).contains(&self.speed)
        {
            return Err(invalid("エンジン、デバイス、速度の設定が不正です"));
        }
        for value in [
            &self.engine_url,
            &self.voice_name,
            &self.voice_version,
            &self.model,
            &self.language,
        ] {
            if value.len() > 4096 || value.contains('\0') {
                return Err(invalid("設定値が長すぎるか不正です"));
            }
        }
        for path in [
            &self.tts_executable,
            &self.tts_settings,
            &self.python_executable,
            &self.stt_script,
        ] {
            if !path.as_os_str().is_empty()
                && (!path.is_absolute() || path.as_os_str().len() > 32760)
            {
                return Err(invalid("CLIと設定ファイルは絶対パスで指定してください"));
            }
        }
        if self.model.trim().is_empty() || self.language.trim().is_empty() {
            return Err(invalid("モデルと言語を指定してください"));
        }
        Ok(())
    }
}

pub fn load_settings(store: &ConfigStore) -> Result<SpeechSettings> {
    let path = store.base_dir().join("speech-cli-settings.json");
    if !path.exists() {
        return Ok(SpeechSettings::default());
    }
    if fs::metadata(&path)?.len() > 65536 {
        return Err(invalid("音声CLI設定が大きすぎます"));
    }
    let settings: SpeechSettings = serde_json::from_slice(&fs::read(path)?)?;
    settings.validate()?;
    Ok(settings)
}

pub fn save_settings(store: &ConfigStore, settings: SpeechSettings) -> Result<()> {
    settings.validate()?;
    let base = store.base_dir();
    fs::create_dir_all(&base)?;
    crate::config::transaction::commit_single(
        &base,
        &base.join("speech-cli-settings.json"),
        serde_json::to_vec_pretty(&settings)?,
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeechJob {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub path: Option<PathBuf>,
    pub text: Option<String>,
    pub error: Option<String>,
}

struct ActiveJob {
    value: Arc<Mutex<SpeechJob>>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct SpeechSession {
    job: Mutex<Option<ActiveJob>>,
}

impl Drop for SpeechSession {
    fn drop(&mut self) {
        if let Some(job) = self.job.get_mut() {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

impl SpeechSession {
    pub fn poll(&self) -> Option<SpeechJob> {
        self.job.lock().as_ref().map(|job| job.value.lock().clone())
    }
    pub fn cancel(&self, id: &str) -> Result<()> {
        let current = self.job.lock();
        let job = current
            .as_ref()
            .ok_or_else(|| invalid("実行中の処理がありません"))?;
        if job.value.lock().id != id {
            return Err(invalid("処理IDが一致しません"));
        }
        job.cancel.store(true, Ordering::Release);
        Ok(())
    }

    pub fn start(
        &self,
        store: &ConfigStore,
        kind: &str,
        text: String,
        audio: Option<PathBuf>,
    ) -> Result<SpeechJob> {
        let mut current = self.job.lock();
        if current
            .as_ref()
            .is_some_and(|j| j.value.lock().state == "running")
        {
            return Err(invalid("音声CLIは既に処理中です"));
        }
        let settings = load_settings(store)?;
        let id = uuid::Uuid::new_v4().to_string();
        let directory = store.base_dir().join("speech-output").join(&id);
        let output = directory.join(if kind == "tts" {
            "speech.wav"
        } else {
            "transcript.txt"
        });
        let (command, stdin, timeout) =
            command_for(&settings, kind, &text, audio.as_deref(), &output)?;
        fs::create_dir_all(&directory)?;
        let initial = SpeechJob {
            id,
            kind: kind.into(),
            state: "running".into(),
            path: None,
            text: None,
            error: None,
        };
        let value = Arc::new(Mutex::new(initial.clone()));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_value = value.clone();
        let worker_cancel = cancel.clone();
        let is_tts = kind == "tts";
        thread::Builder::new()
            .name("speech-cli".into())
            .spawn(move || {
                let outcome = run_command(command, &stdin, timeout, &worker_cancel)
                    .and_then(|_| read_result(&output, is_tts));
                let mut job = worker_value.lock();
                match outcome {
                    Ok(result) if !worker_cancel.load(Ordering::Acquire) => {
                        job.state = "completed".into();
                        job.path = Some(output);
                        job.text = result;
                    }
                    outcome => {
                        job.state = if worker_cancel.load(Ordering::Acquire) {
                            "cancelled"
                        } else {
                            "failed"
                        }
                        .into();
                        job.error = Some(
                            outcome
                                .err()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "取消しました".into()),
                        );
                        // 今回生成する一意パスだけを後始末する。既存成果物には触れない。
                        let _ = fs::remove_file(&output);
                        let _ = fs::remove_dir(&directory);
                    }
                }
            })?;
        *current = Some(ActiveJob { value, cancel });
        Ok(initial)
    }
}

fn existing_file(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() || !path.is_file() {
        return Err(invalid(format!("{label}の絶対パスを確認してください")));
    }
    Ok(())
}

fn command_for(
    settings: &SpeechSettings,
    kind: &str,
    text: &str,
    audio: Option<&Path>,
    output: &Path,
) -> Result<(Command, Vec<u8>, Duration)> {
    settings.validate()?;
    let mut command;
    let stdin;
    let timeout;
    match kind {
        "tts" => {
            existing_file(&settings.tts_executable, "ttscli.exe")?;
            let text = text.trim();
            if !(1..=500).contains(&text.chars().count()) {
                return Err(invalid("原稿は1～500文字で入力してください"));
            }
            command = Command::new(&settings.tts_executable);
            command
                .args(["synthesize", "--text-file", "-"])
                .arg("--output")
                .arg(output)
                .args(["--speed", &settings.speed.to_string(), "--timeout", "180"]);
            if settings.engine == "voicevox" {
                command.args(["--engine", "voicevox", "--engine-url", &settings.engine_url]);
            }
            if !settings.tts_settings.as_os_str().is_empty() {
                existing_file(&settings.tts_settings, "TTS接続設定")?;
                command.arg("--settings").arg(&settings.tts_settings);
            }
            if !settings.voice_name.trim().is_empty() {
                command.args(["--voice-name", &settings.voice_name]);
            }
            if !settings.voice_version.trim().is_empty() {
                command.args(["--voice-version", &settings.voice_version]);
            }
            stdin = text.as_bytes().to_vec();
            timeout = Duration::from_secs(200);
        }
        "stt" => {
            existing_file(&settings.python_executable, "Python実行ファイル")?;
            existing_file(&settings.stt_script, "transcribe.py")?;
            let audio = audio.ok_or_else(|| invalid("文字起こしする音声がありません"))?;
            existing_file(audio, "音声ファイル")?;
            command = Command::new(&settings.python_executable);
            command
                .arg(&settings.stt_script)
                .arg(audio)
                .arg("-o")
                .arg(output)
                .args([
                    "--model",
                    &settings.model,
                    "--language",
                    &settings.language,
                    "--device",
                    &settings.device,
                ])
                .env("PYTHONIOENCODING", "utf-8");
            stdin = Vec::new();
            timeout = Duration::from_secs(1800);
        }
        _ => return Err(invalid("不明な音声CLI処理です")),
    }
    command.current_dir(output.parent().ok_or_else(|| invalid("保存先が不正です"))?);
    Ok((command, stdin, timeout))
}

// 診断を全量蓄積せず、pipeは終端まで消費して子プロセスの停止を防ぐ。
fn drain(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        retained.extend_from_slice(&buffer[..count]);
        if retained.len() > 16384 {
            retained.drain(..retained.len() - 16384);
        }
    }
    Ok(retained)
}

fn run_command(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| invalid(format!("CLIを起動できません: {e}")))?;
    let stderr = child.stderr.take().expect("piped stderr");
    let reader = thread::spawn(move || drain(stderr));
    if let Some(mut pipe) = child.stdin.take() {
        if let Err(error) = pipe.write_all(input) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    }
    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::Acquire) || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid(if cancel.load(Ordering::Acquire) {
                "取消しました"
            } else {
                "CLIの処理がタイムアウトしました"
            }));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
    };
    let diagnostic = reader
        .join()
        .map_err(|_| invalid("CLI診断の取得に失敗しました"))??;
    if !status.success() {
        return Err(invalid(format!(
            "CLIが失敗しました（{status}）: {}",
            String::from_utf8_lossy(&diagnostic)
        )));
    }
    Ok(())
}

fn read_result(path: &Path, is_tts: bool) -> Result<Option<String>> {
    let bytes = fs::metadata(path)?.len();
    if is_tts {
        if !(44..=128 * 1024 * 1024).contains(&bytes) {
            return Err(invalid("合成WAVのサイズが不正です"));
        }
        let reader = hound::WavReader::open(path)
            .map_err(|e| invalid(format!("合成WAVを読めません: {e}")))?;
        if reader.duration() == 0 {
            return Err(invalid("合成音声が空です"));
        }
        Ok(None)
    } else {
        if bytes > 2 * 1024 * 1024 {
            return Err(invalid("文字起こし結果が2MBを超えています"));
        }
        Ok(Some(fs::read_to_string(path)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("config.json"));
        assert_eq!(load_settings(&store).unwrap().model, "small");
        let mut settings = SpeechSettings::default();
        settings.voice_name = "日本語 / 話者".into();
        save_settings(&store, settings.clone()).unwrap();
        assert_eq!(
            load_settings(&store).unwrap().voice_name,
            settings.voice_name
        );
        settings.device = "shell".into();
        assert!(save_settings(&store, settings).is_err());
    }
    #[test]
    fn arguments_preserve_paths_and_text_is_not_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("dummy cli.exe");
        fs::write(&executable, b"").unwrap();
        let settings = SpeechSettings {
            tts_executable: executable.clone(),
            python_executable: executable.clone(),
            stt_script: executable.clone(),
            ..Default::default()
        };
        let text = "日本語 \" & $(test)";
        let (cmd, input, _) =
            command_for(&settings, "tts", text, None, &dir.path().join("output.wav")).unwrap();
        assert_eq!(input, text.as_bytes());
        assert!(!cmd.get_args().any(|a| a.to_string_lossy().contains(text)));
        assert!(command_for(
            &settings,
            "tts",
            &"あ".repeat(501),
            None,
            &dir.path().join("o.wav")
        )
        .is_err());
        let (cmd, _, _) = command_for(
            &settings,
            "stt",
            "",
            Some(&executable),
            &dir.path().join("o.txt"),
        )
        .unwrap();
        assert_eq!(cmd.get_args().nth(1).unwrap(), executable.as_os_str());
        assert!(!cmd.get_args().any(|a| a == "--force"));
    }
    #[test]
    fn results_reject_invalid_audio_and_keep_japanese_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        fs::write(&path, "日本語の認識結果\n").unwrap();
        assert_eq!(
            read_result(&path, false).unwrap().unwrap(),
            "日本語の認識結果\n"
        );
        assert!(read_result(&path, true).is_err());
        assert_eq!(drain(&vec![b'x'; 100000][..]).unwrap().len(), 16384);
    }
    #[test]
    #[cfg(windows)]
    fn process_failure_timeout_and_cancel() {
        let cancel = AtomicBool::new(false);
        let mut success = Command::new("pwsh");
        success.args([
            "-NoProfile",
            "-Command",
            "$value = [Console]::In.ReadToEnd(); if ($value -ne 'hello') { exit 2 }; exit 0",
        ]);
        run_command(success, b"hello", Duration::from_secs(30), &cancel).unwrap();
        let mut command = Command::new("pwsh");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Error.Write('失敗'); exit 7",
        ]);
        assert!(run_command(command, b"", Duration::from_secs(30), &cancel)
            .unwrap_err()
            .to_string()
            .contains("7"));
        let mut command = Command::new("pwsh");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
        assert!(
            run_command(command, b"", Duration::from_millis(100), &cancel)
                .unwrap_err()
                .to_string()
                .contains("タイムアウト")
        );
        cancel.store(true, Ordering::Release);
        let mut command = Command::new("pwsh");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
        assert!(run_command(command, b"", Duration::from_secs(30), &cancel)
            .unwrap_err()
            .to_string()
            .contains("取消"));
    }
}
