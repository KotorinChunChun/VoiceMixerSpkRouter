#Requires -Version 7.0
param(
    [string]$Directory = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'release/SpkRouter'),
    [switch]$SkipBuild,
    [switch]$Startup,
    [switch]$Start
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'SpkRouter.Common.ps1')
$projectRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$Directory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Directory)
$guide = Join-Path $projectRoot 'docs/spk-router.md'
$browserGuide = Join-Path $projectRoot 'docs/spk-browser-capture.html'
$extension = Join-Path $projectRoot 'browser-extension/dist'
if (-not (Test-Path -LiteralPath $guide -PathType Leaf)) {
    throw "利用方法の資料がありません: $guide"
}
if (-not (Test-Path -LiteralPath $browserGuide -PathType Leaf) -or -not (Test-Path -LiteralPath (Join-Path $extension 'manifest.json') -PathType Leaf)) {
    throw 'ブラウザ音声キャプチャの案内または拡張がありません。browser-extensionで npm run build を実行してください。'
}
if (-not $SkipBuild) {
    Invoke-SpkRouterCargo -ProjectRoot $projectRoot -Operation ReleaseBuild
}
$targetDirectory = Invoke-SpkRouterCargo -ProjectRoot $projectRoot -Operation TargetDirectory
$build = Join-Path $targetDirectory 'release/voicemixer-spk.exe'
if (-not (Test-Path -LiteralPath $build -PathType Leaf)) {
    throw "ビルド成果物がありません: $build"
}

[IO.Directory]::CreateDirectory($Directory) | Out-Null
$executable = Join-Path $Directory 'voicemixer-spk.exe'
$configPath = Join-Path $Directory 'spk-settings.json'
if ([string]::Equals([IO.Path]::GetFullPath($build), $executable, [StringComparison]::OrdinalIgnoreCase)) {
    throw '配備先にはCargoのビルド出力先と異なるフォルダーを指定してください。'
}
try {
    # 起動中の実行ファイルはWindowsのファイルロックで拒否する。強制停止しない。
    Copy-Item -LiteralPath $build -Destination $executable -Force
} catch {
    throw "実行ファイルを配置できませんでした。起動中ならトレイから終了してください。配備先: $executable`n$($_.Exception.Message)"
}

# 既存設定は保持し、初回だけEXE自身に生成させる。
if (-not (Test-Path -LiteralPath $configPath)) {
    $process = Start-Process -FilePath $executable -ArgumentList @('--config', ('"' + $configPath + '"'), '--init') -WorkingDirectory $Directory -WindowStyle Hidden -Wait -PassThru
    try {
        if ($process.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
            throw "初期設定の生成に失敗しました。終了コード: $($process.ExitCode)"
        }
    } finally {
        $process.Dispose()
    }
}
Copy-Item -LiteralPath $guide -Destination (Join-Path $Directory '使い方.md') -Force
Copy-Item -LiteralPath $browserGuide -Destination (Join-Path $Directory 'browser-capture.html') -Force
$extensionTarget = Join-Path $Directory 'browser-extension'
[void][IO.Directory]::CreateDirectory($extensionTarget)
Get-ChildItem -LiteralPath $extension -Force | Copy-Item -Destination $extensionTarget -Recurse -Force

if ($Startup) {
    $startupDirectory = [Environment]::GetFolderPath('Startup')
    if ([string]::IsNullOrWhiteSpace($startupDirectory)) {
        throw 'ログイン時の自動起動フォルダーを取得できませんでした。'
    }
    $shortcutPath = Join-Path $startupDirectory 'VoiceMixer Spk Router.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $null
    try {
        $shortcut = $shell.CreateShortcut($shortcutPath)
        $shortcut.TargetPath = $executable
        $shortcut.Arguments = '--config "' + $configPath + '"'
        $shortcut.WorkingDirectory = $Directory
        $shortcut.Description = 'VoiceMixerSpkRouter'
        $shortcut.WindowStyle = 7
        $shortcut.Save()
    } finally {
        if ($null -ne $shortcut) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) }
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
    }
    Write-Host "ログイン時の自動起動を登録しました: $shortcutPath"
}
if ($Start) {
    & (Join-Path $PSScriptRoot 'Start-SpkRouter.ps1') -Directory $Directory
}
Write-Host "配備先: $Directory"
Write-Host "設定: $configPath"
