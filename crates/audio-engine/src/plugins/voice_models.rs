//! 人声抽出モデルの明示導入。Graph・Tauriに依存しない管理境界。
use crate::error::{BackendError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

pub const PLUGIN_ID: &str = "voicemixer.voice-extraction";
const METHODS: [&str; 3] = ["deepfilternet3", "htdemucs", "htdemucs_ft"];

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VoiceModelJob {
    pub id: String,
    pub method: String,
    pub state: String,
    pub progress: Option<Value>,
    pub message: Option<String>,
}

struct JobState {
    report: VoiceModelJob,
    cancel: PathBuf,
    pid: Option<u32>,
}

#[derive(Clone)]
pub struct VoiceModelManager {
    package: PathBuf,
    root: PathBuf,
    jobs: Arc<Mutex<BTreeMap<String, JobState>>>,
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::InvalidOperation(message.into())
}
fn terminal(state: &str) -> bool {
    matches!(state, "installed" | "failed" | "cancelled")
}
fn hidden(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

impl VoiceModelManager {
    pub fn new(package_dir: PathBuf, model_root: PathBuf) -> Result<Self> {
        let package = package_dir.canonicalize()?;
        let manifest: Value = serde_json::from_slice(&fs::read(package.join("plugin.json"))?)?;
        if manifest["plugin_id"] != PLUGIN_ID || !package.join("model_manager.py").is_file() {
            return Err(invalid("人声抽出の専用Plugin packageではありません"));
        }
        if !model_root.is_absolute()
            || model_root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(invalid(
                "モデル専用フォルダーは..を含まない絶対パスで指定してください",
            ));
        }
        let root = if model_root.exists() {
            model_root.canonicalize()?
        } else {
            model_root
        };
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let dev_root = package.join(".models");
        let is_dev = name == ".models"
            && (root == dev_root || dev_root.canonicalize().ok().as_ref() == Some(&root));
        if (!is_dev && name != "voice-extraction") || root == package {
            return Err(invalid("保存先は専用voice-extractionフォルダー、またはPlugin配下の.modelsを指定してください"));
        }
        Ok(Self {
            package,
            root,
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn plugin_configuration(&self) -> Value {
        json!({"plugin_id":PLUGIN_ID,"search_path":self.package,"parameters":{"model_root":self.root},"runtime_setup_script":self.package.join("setup-runtime.ps1")})
    }

    fn python(&self) -> Result<PathBuf> {
        let path = self.package.join(".venv/Scripts/python.exe");
        if !path.is_file() {
            return Err(invalid(
                "人声抽出の実行環境が未導入です。実行環境の導入を実行してください",
            ));
        }
        Ok(path)
    }

    pub fn list(&self, rvc_roots: &[PathBuf]) -> Result<Value> {
        // 未導入でもカタログを表示できる。Python起動・ネット通信は行わない。
        let mut catalog: Value =
            serde_json::from_slice(&fs::read(self.package.join("models.json"))?)?;
        for model in catalog["models"]
            .as_array_mut()
            .ok_or_else(|| invalid("モデルカタログが不正です"))?
        {
            let method = model["id"]
                .as_str()
                .ok_or_else(|| invalid("モデルIDがありません"))?
                .to_owned();
            if !METHODS.contains(&method.as_str()) {
                return Err(invalid("未対応のモデルIDです"));
            }
            let folder = self.root.join(&method);
            model["path"] = json!(folder);
            match fs::read(folder.join("installed.json")) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(marker) if marker["method"] == method && marker["schema_version"] == 1 => {
                        let valid = marker["files"].as_array().is_some_and(|files| {
                            !files.is_empty()
                                && files.iter().all(|file| {
                                    let Some(relative) = file["path"].as_str() else {
                                        return false;
                                    };
                                    let path = Path::new(relative);
                                    !path.is_absolute()
                                        && !path
                                            .components()
                                            .any(|p| matches!(p, Component::ParentDir))
                                        && folder.join(path).is_file()
                                })
                        });
                        model["state"] = json!(if valid { "installed" } else { "failed" });
                        model["installation"] = marker;
                    }
                    _ => model["state"] = json!("failed"),
                },
                Err(_) => {
                    model["state"] = json!(if folder.exists() {
                        "failed"
                    } else {
                        "not_installed"
                    })
                }
            }
        }
        let runtime_path = self.package.join("runtime-install.json");
        let runtime = fs::read(runtime_path).ok().and_then(|data| {
            serde_json::from_slice::<Value>(data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&data))
                .ok()
        });
        catalog["runtime"]["state"] = json!(if self.python().is_ok()
            && runtime.as_ref().is_some_and(|r| r["state"] == "installed")
        {
            "installed"
        } else {
            "not_installed"
        });
        catalog["runtime"]["installation"] = json!(runtime);
        catalog["runtime"]["setup_command"] = json!(format!(
            "pwsh -File \"{}\" -AcceptDownload",
            self.package.join("setup-runtime.ps1").display()
        ));
        catalog["configuration"] = self.plugin_configuration();
        catalog["jobs"] = json!(self
            .jobs
            .lock()
            .unwrap()
            .values()
            .map(|s| s.report.clone())
            .collect::<Vec<_>>());
        let mut external = Vec::new();
        // RVCは明示されたフォルダー直下の読み取り表示のみ。削除・取得しない。
        for root in rvc_roots {
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && path
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|ext| {
                                matches!(
                                    ext.to_ascii_lowercase().as_str(),
                                    "pth" | "onnx" | "index"
                                )
                            })
                    {
                        external.push(json!({"path":path,"name":entry.file_name().to_string_lossy(),"bytes":entry.metadata().ok().map(|m|m.len()),"read_only":true}));
                    }
                }
            }
        }
        catalog["external_rvc"] = json!(external);
        Ok(catalog)
    }

    pub fn query(&self, id: &str) -> Result<VoiceModelJob> {
        self.jobs
            .lock()
            .unwrap()
            .get(id)
            .map(|j| j.report.clone())
            .ok_or_else(|| invalid("導入ジョブがありません"))
    }

    pub fn install(&self, method: &str) -> Result<VoiceModelJob> {
        if !METHODS.contains(&method) {
            return Err(invalid("未対応の抽出方式です"));
        }
        let mut command = Command::new(self.python()?);
        command
            .arg(self.package.join("model_manager.py"))
            .arg("--root")
            .arg(&self.root)
            .args(["install", method, "--accept-download"]);
        self.launch(method, command, true)
    }

    pub fn setup_runtime(&self) -> Result<VoiceModelJob> {
        let mut command = Command::new("pwsh");
        command
            .args(["-NoProfile", "-File"])
            .arg(self.package.join("setup-runtime.ps1"))
            .args(["-AcceptDownload", "-TorchVariant", "cpu"]);
        self.launch("runtime", command, false)
    }

    fn launch(
        &self,
        method: &str,
        mut command: Command,
        cancellable_file: bool,
    ) -> Result<VoiceModelJob> {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.values().any(|job| !terminal(&job.report.state)) {
            return Err(invalid("別のモデル操作が実行中です"));
        }
        fs::create_dir_all(self.root.join(".jobs"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = self.root.join(".jobs").join(format!("{id}.cancel"));
        if cancellable_file {
            command.arg("--cancel-file").arg(&cancel);
        }
        let log_path = self.root.join(".jobs").join(format!("{id}.stderr.log"));
        command
            .current_dir(&self.package)
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&log_path)?)
            .stdin(Stdio::null());
        hidden(&mut command);
        let mut child = command.spawn()?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| invalid("ジョブ出力がありません"))?;
        let report = VoiceModelJob {
            id: id.clone(),
            method: method.into(),
            state: if cancellable_file {
                "downloading"
            } else {
                "setting_up"
            }
            .into(),
            progress: None,
            message: None,
        };
        jobs.insert(
            id.clone(),
            JobState {
                report: report.clone(),
                cancel,
                pid: Some(child.id()),
            },
        );
        drop(jobs);
        let states = self.jobs.clone();
        thread::spawn(move || {
            for line in BufReader::new(output)
                .lines()
                .map_while(std::result::Result::ok)
            {
                let mut jobs = states.lock().unwrap();
                let Some(job) = jobs.get_mut(&id) else {
                    break;
                };
                if job.report.state == "cancelled" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if let Some(state) = value["state"].as_str() {
                        job.report.state = state.into();
                    }
                    job.report.message = value["message"].as_str().map(str::to_owned);
                    job.report.progress = Some(value);
                } else if !line.trim().is_empty() {
                    job.report.message = Some(line.chars().take(1000).collect());
                }
            }
            let succeeded = child.wait().is_ok_and(|status| status.success());
            let mut jobs = states.lock().unwrap();
            if let Some(job) = jobs.get_mut(&id) {
                job.pid = None;
                if job.report.state == "cancelling" && !succeeded {
                    job.report.state = "cancelled".into();
                }
                if job.report.state != "cancelled" {
                    job.report.state = if succeeded { "installed" } else { "failed" }.into();
                    if !succeeded && job.report.message.is_none() {
                        let tail = fs::read_to_string(&log_path).unwrap_or_default();
                        job.report.message = Some(
                            tail.chars()
                                .rev()
                                .take(2000)
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect(),
                        );
                    }
                }
                let _ = fs::remove_file(&job.cancel);
            }
        });
        Ok(report)
    }

    pub fn cancel(&self, id: &str) -> Result<VoiceModelJob> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| invalid("導入ジョブがありません"))?;
        if terminal(&job.report.state) {
            return Ok(job.report.clone());
        }
        if job.report.method == "runtime" {
            #[cfg(windows)]
            if let Some(pid) = job.pid {
                let mut kill = Command::new("taskkill");
                kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
                hidden(&mut kill);
                if !kill.output()?.status.success() {
                    return Err(invalid("実行環境導入の停止に失敗しました"));
                }
            }
            #[cfg(not(windows))]
            return Err(invalid("実行環境の取消はWindowsのみ対応です"));
        } else {
            fs::write(&job.cancel, b"cancel")?;
        }
        job.report.state = "cancelling".into();
        job.report.message = Some("取消処理中です。処理終了まで新規導入は開始しません。".into());
        // 子プロセスがモデル有効化直後ならinstalledを優先する。取消フラグだけで成功を隠さない。
        Ok(job.report.clone())
    }

    pub fn remove(&self, method: &str, in_use: bool) -> Result<Value> {
        if !METHODS.contains(&method) {
            return Err(invalid("未対応の抽出方式です"));
        }
        if in_use {
            return Err(invalid(
                "使用中モデルは削除できません。対象ノードを停止してください",
            ));
        }
        let jobs = self.jobs.lock().unwrap();
        if jobs.values().any(|j| !terminal(&j.report.state)) {
            return Err(invalid("導入中はモデルを削除できません"));
        }
        let mut command = Command::new(self.python()?);
        command
            .arg(self.package.join("model_manager.py"))
            .arg("--root")
            .arg(&self.root)
            .args(["remove", method, "--confirm-remove"]);
        hidden(&mut command);
        let output = command.output()?;
        if !output.status.success() {
            return Err(invalid(
                String::from_utf8_lossy(&output.stdout).into_owned(),
            ));
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_works_without_runtime_and_lists_rvc_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("package");
        fs::create_dir(&package).unwrap();
        fs::write(
            package.join("plugin.json"),
            r#"{"plugin_id":"voicemixer.voice-extraction"}"#,
        )
        .unwrap();
        fs::write(package.join("model_manager.py"), "").unwrap();
        fs::write(
            package.join("models.json"),
            r#"{"models":[{"id":"deepfilternet3"}],"runtime":{}}"#,
        )
        .unwrap();
        let rvc = directory.path().join("rvc");
        fs::create_dir(&rvc).unwrap();
        fs::write(rvc.join("voice.pth"), b"example").unwrap();
        let manager =
            VoiceModelManager::new(package, directory.path().join("voice-extraction")).unwrap();
        let catalog = manager.list(&[rvc.clone()]).unwrap();
        assert_eq!(catalog["runtime"]["state"], "not_installed");
        assert_eq!(catalog["models"][0]["state"], "not_installed");
        assert_eq!(catalog["external_rvc"][0]["read_only"], true);
        assert_eq!(fs::read(rvc.join("voice.pth")).unwrap(), b"example");
        assert!(manager.install("deepfilternet3").is_err());
    }
    #[test]
    fn rejects_broad_paths_and_unknown_methods_before_starting_process() {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/voice-extraction")
            .canonicalize()
            .unwrap();
        assert!(
            VoiceModelManager::new(package.clone(), package.parent().unwrap().to_path_buf())
                .is_err()
        );
        let manager = VoiceModelManager::new(package.clone(), package.join(".models")).unwrap();
        assert!(manager.install("../bad").is_err());
        assert!(manager.remove("htdemucs", true).is_err());
        assert!(manager.query("missing").is_err());
        assert_eq!(manager.plugin_configuration()["plugin_id"], PLUGIN_ID);
    }
}
