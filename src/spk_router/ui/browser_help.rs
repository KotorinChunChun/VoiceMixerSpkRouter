//! 配備したオフライン案内を既定ブラウザで開く。接続情報はURLへ含めない。
use super::native::wide;
use std::path::{Path, PathBuf};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HWND,
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    },
};

pub(super) const REPOSITORY_URL: &str = "https://github.com/KotorinChunChun/VoiceMixerSpkRouter";

fn guide_path() -> Result<PathBuf, String> {
    let deployed = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("browser-capture.html")));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/spk-browser-capture.html");
    deployed
        .into_iter()
        .chain([source])
        .find(|p| p.is_file())
        .ok_or_else(|| "ブラウザ音声キャプチャの案内が見つかりません。再配備してください。".into())
}

fn guide_url(path: &Path, setup: bool) -> String {
    let mut url = String::from("file:///");
    for byte in path.to_string_lossy().replace('\\', "/").bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url.push_str(if setup { "#setup" } else { "#install" });
    url
}

pub(super) fn open(owner: Option<HWND>, setup: bool) -> Result<(), String> {
    let path = guide_path()?.canonicalize().map_err(|e| e.to_string())?;
    let path = path.to_string_lossy();
    let path = Path::new(path.strip_prefix("\\\\?\\").unwrap_or(&path));
    open_url(owner, &guide_url(path, setup))
}

pub(super) fn open_url(owner: Option<HWND>, url: &str) -> Result<(), String> {
    let url = wide(url);
    let result = unsafe {
        ShellExecuteW(
            owner,
            PCWSTR(wide("open").as_ptr()),
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err("リンクを開けませんでした。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_links_encode_paths_and_select_separate_sections_without_secrets() {
        let path = Path::new("C:/案内 #1/help.html");
        let install = guide_url(path, false);
        assert!(install.starts_with("file:///C:/%E6%A1%88%E5%86%85%20%231/"));
        assert!(install.ends_with("help.html#install"));
        assert!(guide_url(path, true).ends_with("help.html#setup"));
        let guide = include_str!("../../../docs/spk-browser-capture.html");
        assert!(guide.contains("id=\"install\""));
        assert!(guide.contains("id=\"setup\""));
        assert!(guide.contains("18766"));
    }
}
