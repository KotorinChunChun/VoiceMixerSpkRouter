#Requires -Version 7.0
param(
    [ValidateSet('Last', 'Device', 'Application', 'Browser', 'Stopped', 'Stop', 'Settings')]
    [string]$Mode = 'Last',
    [string]$Directory = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'release/SpkRouter'),
    [switch]$Debug
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'SpkRouter.Common.ps1')
$projectRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$Directory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Directory)
$configPath = Join-Path $Directory 'spk-settings.json'
$executable = Join-Path $Directory 'voicemixer-spk.exe'

if ($Debug) {
    $targetDirectory = Invoke-SpkRouterCargo -ProjectRoot $projectRoot -Operation TargetDirectory
    $executable = Join-Path $targetDirectory 'debug/voicemixer-spk.exe'
    # ソース修正後も、起動中のEXEを再リンクせず終了指示を送れるようにする。
    if ($Mode -ne 'Stop' -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        Invoke-SpkRouterCargo -ProjectRoot $projectRoot -Operation DebugBuild
    }
}

if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    $installer = Join-Path $PSScriptRoot 'Install-SpkRouter.ps1'
    throw "実行ファイルがありません: $executable`n先に pwsh -File `"$installer`" -Directory `"$Directory`" を実行してください。"
}
[IO.Directory]::CreateDirectory($Directory) | Out-Null

# Start-Processは配列を空白で結合するため、設定パス自体を引用符で囲む。
$arguments = @('--config', ('"' + $configPath + '"'))
switch ($Mode) {
    'Device' { $arguments += @('--mode', 'device') }
    'Application' { $arguments += @('--mode', 'application') }
    'Browser' { $arguments += @('--mode', 'browser') }
    'Stopped' { $arguments += @('--mode', 'stopped') }
    'Stop' { $arguments += '--stop' }
    'Settings' { $arguments += '--settings' }
}
$process = Start-Process -FilePath $executable -ArgumentList $arguments -WorkingDirectory $Directory -WindowStyle Hidden -PassThru
try {
    if ($process.WaitForExit(1000)) {
        if ($process.ExitCode -ne 0) {
            throw "VoiceMixerSpkRouterの起動操作に失敗しました。終了コード: $($process.ExitCode)"
        }
        # 既存プロセスへのIPC送信は、正常終了する単発プロセスになる。
        Write-Host '起動操作を受け付けました。'
    } else {
        Write-Host "VoiceMixerSpkRouterを起動しました。PID: $($process.Id)"
    }
} finally {
    $process.Dispose()
}
