fn main() {
    println!("cargo:rerun-if-changed=assets/SpkRouter.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set("ProductName", "VoiceMixerSpkRouter")
            .set("FileDescription", "VoiceMixerSpkRouter")
            .set("OriginalFilename", "VoiceMixerSpkRouter.exe")
            .set_icon("assets/SpkRouter.ico")
            .compile()
            .expect("Windows製品リソースの生成に失敗しました");
    }
}
