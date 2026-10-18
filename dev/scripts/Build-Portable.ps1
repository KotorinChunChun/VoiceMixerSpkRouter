#Requires -Version 7.0
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Push-Location -LiteralPath $projectRoot
try {
    $version = (([IO.File]::ReadAllText((Join-Path $projectRoot 'Cargo.toml')) | Select-String 'version = "([0-9.]+)"').Matches[0].Groups[1].Value)
    $target = 'x86_64-pc-windows-msvc'
    $buildRoot = Join-Path $projectRoot 'target/portable'
    $executable = Join-Path $buildRoot "$target/release/voicemixer-spk.exe"
    if (-not $SkipBuild) {
        # 外部CRTを要求しない配布専用ビルド。通常のDebug/Releaseと分離する。
        $savedFlags = [Environment]::GetEnvironmentVariable('RUSTFLAGS','Process')
        $savedEncoded = [Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS','Process')
        $savedTarget = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR','Process')
        try {
            $env:RUSTFLAGS = '-C target-feature=+crt-static'
            # 空のENCODED_RUSTFLAGSでも通常のRUSTFLAGSより優先されるため、同じ指定を明示する。
            $env:CARGO_ENCODED_RUSTFLAGS = "-C$([char]31)target-feature=+crt-static"
            $env:CARGO_TARGET_DIR = $buildRoot
            & cargo build --locked --release --target $target --bin voicemixer-spk
            if ($LASTEXITCODE -ne 0) { throw '配布EXEのビルドに失敗しました。' }
        } finally {
            [Environment]::SetEnvironmentVariable('RUSTFLAGS',$savedFlags,'Process')
            [Environment]::SetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS',$savedEncoded,'Process')
            [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR',$savedTarget,'Process')
        }
        & npm --prefix browser-extension ci
        if ($LASTEXITCODE -ne 0) { throw '拡張の依存導入に失敗しました。' }
        & npm --prefix browser-extension run build
        if ($LASTEXITCODE -ne 0) { throw '拡張のビルドに失敗しました。' }
    }
    $extension = Join-Path $projectRoot 'browser-extension/dist'
    foreach ($required in @($executable,(Join-Path $extension 'manifest.json'))) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "成果物がありません: $required" }
    }
    $output = Join-Path $projectRoot 'release/Publish'
    $stage = Join-Path $output ('staging/' + [guid]::NewGuid().ToString('N'))
    $package = Join-Path $stage 'VoiceMixerSpkRouter'
    [void][IO.Directory]::CreateDirectory($package)
    Copy-Item -LiteralPath $executable -Destination (Join-Path $package 'voicemixer-spk.exe')
    Copy-Item -LiteralPath (Join-Path $projectRoot 'docs/portable-start.html') -Destination (Join-Path $package 'はじめに.html')
    Copy-Item -LiteralPath (Join-Path $projectRoot 'docs/spk-browser-capture.html') -Destination (Join-Path $package 'browser-capture.html')
    $extensionTarget = Join-Path $package 'browser-extension'
    [void][IO.Directory]::CreateDirectory($extensionTarget)
    Get-ChildItem -LiteralPath $extension -Force | Copy-Item -Destination $extensionTarget -Recurse -Force
    $commit = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'ソースのコミットを確認できません。' }
    [IO.File]::WriteAllText((Join-Path $package 'VERSION.txt'),"VoiceMixerSpkRouter v$version`nWindows x64 / static CRT`nSource: $commit`n")
    [IO.File]::WriteAllText((Join-Path $package 'README.txt'),"ZIPをすべて展開し、voicemixer-spk.exeをダブルクリックしてください。`r`n詳しい手順は はじめに.html を開いてください。`r`nRust・Node.js・PowerShell 7は不要です。`r`n")
    $files = @(Get-ChildItem -LiteralPath $package -File -Recurse)
    foreach ($file in $files) {
        $relative = [IO.Path]::GetRelativePath($package,$file.FullName).Replace('\','/')
        if ($relative -match '(?i)(spk-settings|spk-status|spk-router-project)\.json|\.(key|pem|pdb|log)$|(^|/)(private|temp|node_modules)/') { throw "設定または不要物が混入しました: $relative" }
    }
    $hashes = foreach ($file in $files | Sort-Object FullName) {
        $relative = [IO.Path]::GetRelativePath($package,$file.FullName).Replace('\','/')
        (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant() + '  ' + $relative
    }
    [IO.File]::WriteAllLines((Join-Path $package 'SHA256SUMS.txt'),[string[]]$hashes)
    $zip = Join-Path $output "VoiceMixerSpkRouter-v$version-windows-x64.zip"
    Compress-Archive -LiteralPath $package -DestinationPath $zip -CompressionLevel Optimal -Force
    $zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText(($zip+'.sha256'),($zipHash+'  '+[IO.Path]::GetFileName($zip)+"`n"))
    [pscustomobject]@{Version=$version;Zip=$zip;SHA256=$zipHash;Package=$package;Files=$files.Count+1;SourceCommit=$commit}
} finally { Pop-Location }
