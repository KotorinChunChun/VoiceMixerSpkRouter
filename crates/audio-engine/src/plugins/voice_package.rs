//! モデルを含めず、人声抽出の実行コードと導入UI用カタログだけを同梱する。
use std::path::Path;
pub fn install(root: &Path) -> std::io::Result<()> {
    let folder = root.join("plugins/voice-extraction");
    std::fs::create_dir_all(&folder)?;
    let files: [(&str, &[u8]); 11] = [
        (
            "plugin.json",
            include_bytes!("../../../../plugins/voice-extraction/plugin.json"),
        ),
        (
            "voice_extraction.py",
            include_bytes!("../../../../plugins/voice-extraction/voice_extraction.py"),
        ),
        (
            "shared_audio.py",
            include_bytes!("../../../../plugins/voice-extraction/shared_audio.py"),
        ),
        (
            "engines.py",
            include_bytes!("../../../../plugins/voice-extraction/engines.py"),
        ),
        (
            "model_manager.py",
            include_bytes!("../../../../plugins/voice-extraction/model_manager.py"),
        ),
        (
            "models.json",
            include_bytes!("../../../../plugins/voice-extraction/models.json"),
        ),
        (
            "model-sha256.json",
            include_bytes!("../../../../plugins/voice-extraction/model-sha256.json"),
        ),
        (
            "setup-runtime.ps1",
            include_bytes!("../../../../plugins/voice-extraction/setup-runtime.ps1"),
        ),
        (
            "requirements.txt",
            include_bytes!("../../../../plugins/voice-extraction/requirements.txt"),
        ),
        (
            "constraints.txt",
            include_bytes!("../../../../plugins/voice-extraction/constraints.txt"),
        ),
        (
            ".gitignore",
            include_bytes!("../../../../plugins/voice-extraction/.gitignore"),
        ),
    ];
    for (name, bytes) in files {
        let path = folder.join(name);
        // 利用者が配置した同名packageのコードを起動時に上書きしない。
        if !path.exists() {
            std::fs::write(path, bytes)?;
        }
    }
    Ok(())
}
