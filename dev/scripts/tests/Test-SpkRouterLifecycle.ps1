#Requires -Version 7.0
<#
SpkRouterのデバッグEXEで、設定保存・IPC・診断・終了の回帰を確認する。
release EXEはエラー時にMessageBoxを表示するため、この無人試験では使用しない。
音声デバイスは使用せず、停止とブラウザ接続待ちだけを検証する。
#>
param(
    [Parameter(Mandatory)]
    [string]$Executable,
    [string]$Root = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$allowedRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'tmp/spk-router-refactoring/lifecycle'))
$executablePath = (Resolve-Path -LiteralPath $Executable).ProviderPath
if ((Split-Path -Leaf $executablePath) -ne 'voicemixer-spk.exe' -or
    (Split-Path -Leaf (Split-Path -Parent $executablePath)) -ne 'debug') {
    throw 'ExecutableにはCargoのdebugフォルダー内のvoicemixer-spk.exeを指定してください。release EXEはエラー時のMessageBoxにより無人試験できません。'
}
if (Get-Process -Name voicemixer-spk -ErrorAction SilentlyContinue) {
    throw 'SpkRouterが起動中です。既存アプリを終了してから試験してください。'
}
if ([string]::IsNullOrWhiteSpace($Root)) { $Root = $allowedRoot }
$rootPath = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Root))
$allowedPrefix = $allowedRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not [string]::Equals($rootPath, $allowedRoot, [StringComparison]::OrdinalIgnoreCase) -and
    -not $rootPath.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "検証先Rootは次のフォルダー配下に限定します: $allowedRoot"
}
# 空白を含むパスで、EXEに渡す--configの引用も毎回確認する。
$testRoot = Join-Path $rootPath ('run ' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$configPath = Join-Path $testRoot 'spk settings.json'
$statusPath = Join-Path $testRoot 'spk-status.json'
$alternateConfig = Join-Path $testRoot '別の設定先/other settings.json'
$resultsPath = Join-Path $testRoot 'results.json'
$results = [Collections.Generic.List[object]]::new()
$script:resident = $null
$script:commandNumber = 0
$script:lastCommandStdout = $null
$settingsLock = $null
$fileLock = $null
$mutex = $null

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Add-Case([string]$Name) {
    $results.Add([ordered]@{ case = $Name; result = '成功' })
    Write-Host "成功: $Name"
}

function Config-Hash {
    (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash
}

function Read-Settings {
    Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
}

function Read-Status {
    if (-not (Test-Path -LiteralPath $statusPath -PathType Leaf)) { return $null }
    try { return Get-Content -LiteralPath $statusPath -Raw | ConvertFrom-Json }
    catch { return $null } # atomic replacementの境界では次のポーリングで再読込する。
}

function Process-Arguments([string]$Config, [string[]]$CommandArguments) {
    # Start-Processは引数配列を空白で結合する。設定ファイル名はここで引用する。
    @('--config', ('"' + $Config + '"')) + $CommandArguments
}

function Invoke-SpkCommand {
    param(
        [string]$Name,
        [string[]]$CommandArguments,
        [string]$Config = $configPath,
        [switch]$ExpectFailure,
        [int]$TimeoutSeconds = 15
    )
    $script:commandNumber++
    $prefix = Join-Path $testRoot ('{0:D2}-{1}' -f $script:commandNumber, $Name)
    $script:lastCommandStdout = $prefix + '.stdout.log'
    $process = $null
    try {
        $process = Start-Process -FilePath $executablePath `
            -ArgumentList (Process-Arguments $Config $CommandArguments) `
            -WorkingDirectory $testRoot -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput ($prefix + '.stdout.log') `
            -RedirectStandardError ($prefix + '.stderr.log')
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            throw "操作が時間内に終了しません: $Name。ログ: $prefix"
        }
        $process.Refresh()
        if ($ExpectFailure) {
            Assert-Condition ($process.ExitCode -ne 0) "失敗を期待した操作が成功しました: $Name"
        } else {
            Assert-Condition ($process.ExitCode -eq 0) "操作失敗: $Name、終了コード=$($process.ExitCode)。ログ: $prefix"
        }
    } finally {
        if ($null -ne $process) {
            try {
                if (-not $process.HasExited) {
                    $process.Kill() # この関数が起動した単発PIDだけを対象にする。
                    [void]$process.WaitForExit(5000)
                }
            } finally { $process.Dispose() }
        }
    }
}

function Last-DiagnosticPhase {
    $statuses = @(Get-Content -LiteralPath $script:lastCommandStdout |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        ForEach-Object { $_ | ConvertFrom-Json })
    Assert-Condition ($statuses.Count -gt 0) '診断の状態JSONが出力されませんでした。'
    $statuses[-1].phase
}

function Start-TestResident([string[]]$CommandArguments) {
    Assert-Condition ($null -eq $script:resident) '前の試験プロセスが残っています。'
    $script:commandNumber++
    $prefix = Join-Path $testRoot ('{0:D2}-resident' -f $script:commandNumber)
    $script:resident = Start-Process -FilePath $executablePath `
        -ArgumentList (Process-Arguments $configPath $CommandArguments) `
        -WorkingDirectory $testRoot -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput ($prefix + '.stdout.log') `
        -RedirectStandardError ($prefix + '.stderr.log')
}

function Assert-ResidentAlive {
    Assert-Condition ($null -ne $script:resident) '試験プロセスがありません。'
    $script:resident.Refresh()
    Assert-Condition (-not $script:resident.HasExited) '試験常駐が予期せず終了しました。検証先のログを確認してください。'
}

function Wait-Status([string]$Mode, [string]$Phase, [switch]$AllowExited) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        if (-not $AllowExited) { Assert-ResidentAlive }
        $state = Read-Status
        if ($null -ne $state -and $state.mode -eq $Mode -and $state.phase -eq $Phase) { return }
        Start-Sleep -Milliseconds 50
    } while ($timer.Elapsed.TotalSeconds -lt 15)
    throw "状態待ちがタイムアウトしました: mode=$Mode、phase=$Phase。検証先: $testRoot"
}

function Assert-StateStable([string]$Mode, [string]$Phase, [string]$ExpectedHash) {
    # IPC送信プロセスの正常終了だけでは、常駐側の保存処理完了を保証しない。
    # UIのポーリングを十分に跨ぎ、遅れて設定・状態が変わっていないことを確認する。
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        Assert-ResidentAlive
        $state = Read-Status
        Assert-Condition ($null -ne $state -and $state.mode -eq $Mode -and $state.phase -eq $Phase) '維持すべき取得状態が変わりました。'
        Assert-Condition ((Config-Hash) -eq $ExpectedHash) '維持すべき設定ファイルが変わりました。'
        Start-Sleep -Milliseconds 50
    } while ($timer.Elapsed.TotalSeconds -lt 1)
}

function Stop-TestResident {
    if ($null -eq $script:resident) { return }
    try {
        $script:resident.Refresh()
        if (-not $script:resident.HasExited) {
            # 他の常駐が現れた場合にはグローバルIPCを送らず、自分のPIDだけを後始末する。
            $foreign = @(Get-Process -Name voicemixer-spk -ErrorAction SilentlyContinue |
                Where-Object Id -ne $script:resident.Id)
            if ($foreign.Count -eq 0) {
                try {
                    Invoke-SpkCommand -Name cleanup-stop -CommandArguments @('--stop')
                    [void]$script:resident.WaitForExit(5000)
                } catch { Write-Warning '試験常駐の通常終了に失敗したため、この試験のPIDだけを終了します。' }
            }
            $script:resident.Refresh()
            if (-not $script:resident.HasExited) {
                $script:resident.Kill()
                [void]$script:resident.WaitForExit(5000)
            }
        }
    } finally {
        $script:resident.Dispose()
        $script:resident = $null
    }
}

try {
    Invoke-SpkCommand -Name initialize -CommandArguments @('--init')
    $settings = Read-Settings
    Assert-Condition ($settings.mode -eq 'stopped' -and @($settings.outputs | Where-Object enabled).Count -eq 0) '初期設定が停止・出力OFFではありません。'
    Add-Case '初期設定は停止・出力OFF'

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    try {
        $listener.Start()
        $settings.browser_port = $listener.LocalEndpoint.Port
    } finally { $listener.Stop() }
    $settings.overlay_mode = 'off'
    $settings.browser_token = 'spk-router-lifecycle-test'
    $settings.browser_stream_key = 'lifecycle-' + [guid]::NewGuid().ToString('N')
    [IO.File]::WriteAllText($configPath, ($settings | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
    $initialHash = Config-Hash

    Invoke-SpkCommand -Name initialize-existing -CommandArguments @('--init') -ExpectFailure
    Assert-Condition ((Config-Hash) -eq $initialHash) '既存設定の初期化拒否で内容が変わりました。'
    Add-Case '既存設定の初期化を拒否して内容保持'

    Invoke-SpkCommand -Name diagnose-stopped -CommandArguments @('--check', 'stopped') -TimeoutSeconds 25
    Assert-Condition ((Last-DiagnosticPhase) -eq 'stopped') '停止診断が停止状態で終了しませんでした。'
    Assert-Condition ((Config-Hash) -eq $initialHash) '停止診断で設定が保存されました。'
    Add-Case '停止診断成功・設定不変'

    Invoke-SpkCommand -Name diagnose-waiting -CommandArguments @('--check', 'browser') -ExpectFailure -TimeoutSeconds 25
    Assert-Condition ((Last-DiagnosticPhase) -eq 'waiting') 'ブラウザ診断が接続待ち以外の理由で終了しました。'
    Assert-Condition ((Config-Hash) -eq $initialHash) '接続待ち診断で設定が保存されました。'
    Add-Case 'ブラウザ接続待ち診断は失敗・設定不変'

    Start-TestResident -CommandArguments @('--mode', 'browser')
    Wait-Status browser waiting
    Assert-Condition ((Read-Settings).mode -eq 'browser') '新規起動時の指定モードが保存されませんでした。'
    $browserHash = Config-Hash
    Add-Case '新規起動の指定モードを保存'

    Invoke-SpkCommand -Name set-same-mode -CommandArguments @('--mode', 'browser')
    Assert-StateStable browser waiting $browserHash
    Add-Case '既存常駐への同モード指定は停止へトグルしない'

    Invoke-SpkCommand -Name alternate-config-control -Config $alternateConfig -CommandArguments @('--mode', 'stopped')
    Wait-Status stopped stopped
    Assert-Condition ((Read-Settings).mode -eq 'stopped') '別設定パスからの操作が既存常駐に反映されませんでした。'
    Assert-Condition (-not (Test-Path -LiteralPath (Split-Path -Parent $alternateConfig))) 'IPC転送だけの操作で別設定先が生成されました。'
    Add-Case '別設定パスの操作を既存常駐へ転送'

    $stoppedHash = Config-Hash
    Invoke-SpkCommand -Name alternate-config-diagnosis -Config $alternateConfig -CommandArguments @('--check', 'stopped') -ExpectFailure
    Assert-StateStable stopped stopped $stoppedHash
    Assert-Condition (-not (Test-Path -LiteralPath (Split-Path -Parent $alternateConfig))) '拒否すべき診断が別設定先を生成しました。'
    Add-Case '別設定パスでも診断の多重起動を拒否'

    Invoke-SpkCommand -Name prepare-save-failure -CommandArguments @('--mode', 'browser')
    Wait-Status browser waiting
    $browserHash = Config-Hash
    try {
        $settingsLock = [IO.File]::Open($configPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        Invoke-SpkCommand -Name rejected-mode-save -CommandArguments @('--mode', 'stopped')
        Assert-StateStable browser waiting $browserHash
        Add-Case '設定置換失敗時は保存内容と音声状態を保持'
    } finally {
        if ($null -ne $settingsLock) { $settingsLock.Dispose(); $settingsLock = $null }
    }
    Invoke-SpkCommand -Name mode-after-unlock -CommandArguments @('--mode', 'stopped')
    Wait-Status stopped stopped
    Assert-Condition ((Read-Settings).mode -eq 'stopped') '保存先の解放後も設定を変更できません。'
    Add-Case '保存先解放後は切替と保存が成功'

    Invoke-SpkCommand -Name stop -CommandArguments @('--stop')
    Assert-Condition ($script:resident.WaitForExit(15000)) '終了操作後も試験常駐が残っています。'
    Assert-Condition ($script:resident.ExitCode -eq 0) '試験常駐が異常終了しました。'
    Wait-Status stopped exited -AllowExited
    $script:resident.Dispose()
    $script:resident = $null
    Add-Case '通常終了でexited状態を保存'

    try {
        $fileLock = [IO.File]::Open((Join-Path $testRoot 'spk-instance.lock'), [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $createdNew = $false
        $mutex = [Threading.Mutex]::new($false, 'Local\VoiceMixerSpkRouter.v1', [ref]$createdNew)
        Assert-Condition $createdNew '終了後も名前付きmutexが残っています。'
        Add-Case '終了後にディレクトリロックと名前付きmutexを再取得'
    } finally {
        if ($null -ne $mutex) { $mutex.Dispose(); $mutex = $null }
        if ($null -ne $fileLock) { $fileLock.Dispose(); $fileLock = $null }
    }

    $savedHash = Config-Hash
    Start-TestResident -CommandArguments @()
    Wait-Status stopped stopped
    Assert-StateStable stopped stopped $savedHash
    Invoke-SpkCommand -Name stop-restarted -CommandArguments @('--stop')
    Assert-Condition ($script:resident.WaitForExit(15000)) '再起動した試験常駐が終了しません。'
    Assert-Condition ($script:resident.ExitCode -eq 0) '再起動した試験常駐が異常終了しました。'
    Wait-Status stopped exited -AllowExited
    $script:resident.Dispose()
    $script:resident = $null
    Add-Case '保存した停止状態で再起動し再終了'

    Assert-Condition ($results.Count -eq 13) '完了した試験ケース数が不足しています。'
    Write-Host "SpkRouterライフサイクル試験: $($results.Count)件成功。検証先: $testRoot"
} finally {
    try {
        if ($null -ne $settingsLock) { $settingsLock.Dispose() }
        if ($null -ne $fileLock) { $fileLock.Dispose() }
        if ($null -ne $mutex) { $mutex.Dispose() }
        Stop-TestResident
    } finally {
        # 設定全体・認証トークンは報告へ含めない。失敗時も完了済みケースを残す。
        $results | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $resultsPath -Encoding utf8
    }
}
