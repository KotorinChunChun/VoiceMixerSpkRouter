#Requires -Version 7.0
param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$ResidentConfig,
    [Parameter(Mandatory)][int]$ResidentId
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$exe = (Resolve-Path -LiteralPath $Executable).Path
$config = (Resolve-Path -LiteralPath $ResidentConfig).Path
$statusPath = Join-Path (Split-Path $config) 'spk-status.json'
$resident = Get-Process -Id $ResidentId
$initialPath = $resident.Path
if ($resident.ProcessName -notin @('voicemixer-spk', 'VoiceMixerSpkRouter') -or -not $initialPath) {
    throw '比較対象のSpkRouter常駐を特定できません。'
}
$initialStarted = $resident.StartTime
$initialHash = (Get-FileHash -LiteralPath $config).Hash
$initialMode = (Get-Content -LiteralPath $statusPath -Raw | ConvertFrom-Json).mode
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
function Get-ResidentReply {
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', "KotorinToys.$sid.voice-mixer-spk-router", [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
        try {
            $pipe.Connect(2500)
            $writer = [IO.StreamWriter]::new($pipe)
            $writer.AutoFlush = $true
            $reader = [IO.StreamReader]::new($pipe)
            $pending = $reader.ReadLineAsync()
            $writer.WriteLine("status")
            $line = $pending.WaitAsync([TimeSpan]::FromSeconds(5)).GetAwaiter().GetResult()
            if ($line) { return $line | ConvertFrom-Json }
        } catch [TimeoutException], [IO.IOException] {
            # 読み取り専用statusだけを、次の接続で再試行する。
        } finally { $pipe.Dispose() }
        Start-Sleep -Milliseconds 120
    }
    throw '通常版の管理IPCから応答を取得できません。'
}
function Check([bool]$Ok, [string]$Label) {
    if (-not $Ok) { throw "失敗: $Label" }
    Write-Host "成功: $Label"
}
$reply = Get-ResidentReply
Check ($reply.pid -eq $ResidentId -and $reply.protocolVersion -eq 2 -and $reply.ready) '通常管理IPCの対象とPIDを照合'
$beforeVersion = $reply.version
$root = Join-Path ([IO.Path]::GetTempPath()) ('SpkRouter-acceptance-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$children = [Collections.Generic.List[object]]::new()
function New-Child([string[]]$Arguments, [string]$TempRoot = $root) {
    $info = [Diagnostics.ProcessStartInfo]::new($exe)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardInputEncoding = [Text.UTF8Encoding]::new($false)
    $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $info.Environment['TEMP'] = $TempRoot
    $info.Environment['TMP'] = $TempRoot
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::Start($info)
    $child = [pscustomobject]@{ Process = $process; Pid = $process.Id; Started = $process.StartTime; Session = $null; Errors = $process.StandardError.ReadToEndAsync() }
    $children.Add($child)
    return $child
}
function Read-Reply($Child) {
    $raw = $Child.Process.StandardOutput.ReadLineAsync().WaitAsync([TimeSpan]::FromSeconds(10)).GetAwaiter().GetResult()
    if ([string]::IsNullOrWhiteSpace($raw) -or $raw -match 'fixture-only|browserToken|browser_token|[A-Z]:\\') {
        throw 'JSON応答が空、または応答に除外対象の値があります。'
    }
    $reply = $raw | ConvertFrom-Json
    if ($reply.schemaVersion -ne 1 -or ($Child.Session -and $reply.sessionId -ne $Child.Session)) { throw '異なる版またはセッションの応答です。' }
    return $reply
}
function Start-Session {
    $child = New-Child @('--test-session')
    $ready = Read-Reply $child
    if ($ready.event -ne 'ready' -or $ready.revision -ne 0 -or $ready.sessionId -cnotmatch '^[a-f0-9]{32}$') { throw '初期応答が不正です。' }
    $child.Session = $ready.sessionId
    return $child
}
function Invoke-Request($Child, [string]$Command, [hashtable]$Fields = @{}) {
    $id = [guid]::NewGuid().ToString('N')
    $request = @{ schemaVersion = 1; requestId = $id; command = $Command }
    foreach ($key in $Fields.Keys) { $request[$key] = $Fields[$key] }
    $Child.Process.StandardInput.WriteLine(($request | ConvertTo-Json -Compress -Depth 10))
    $Child.Process.StandardInput.Flush()
    $r = Read-Reply $Child
    if ($r.requestId -ne $id -or $r.command -ne $Command) { throw '要求と応答が一致しません。' }
    return $r
}
function Stop-Owned($Child) {
    if ($Child.Process.HasExited) { return }
    $actual = Get-Process -Id $Child.Pid -ErrorAction SilentlyContinue
    if ($actual -and $actual.StartTime -eq $Child.Started -and $actual.Path -eq $exe) {
        $Child.Process.Kill()
        if (-not $Child.Process.WaitForExit(5000)) { throw '試験用プロセスが終了しません。' }
    }
}
function Session-Path($Child) {
    if ($Child.Session -cnotmatch '^[a-f0-9]{32}$') { throw '試験領域の識別子が不正です。' }
    return Join-Path $root "VoiceMixerSpkRouter-test-$($Child.Session)"
}
function Fixture-Hash($Child) { return (Get-FileHash -LiteralPath (Join-Path (Session-Path $Child) 'settings.json')).Hash }

try {
    $a = Start-Session
    $b = Start-Session
    Check ($a.Pid -ne $b.Pid -and $a.Session -ne $b.Session -and $a.Pid -ne $ResidentId -and $b.Pid -ne $ResidentId) '通常版を動かしたまま2つの試験セッションを並行起動'
    $aInitial = Fixture-Hash $a
    $bInitial = Fixture-Hash $b

    $r = Invoke-Request $a 'validate-route' @{ mode = 'device'; patch = @{ outputs = @(@{deviceId='speaker-alias';enabled=$true},$null) } }
    Check ($r.result -eq 'rejected' -and $r.issues[0].code -eq 'route_loop' -and $r.issues[0].field -eq 'outputs[0].deviceId' -and $r.revision -eq 0) '実IDの別名を循環として拒否'
    $r = Invoke-Request $a 'validate-route' @{ mode='device' }
    Check ($r.result -eq 'valid' -and $r.issues[0].severity -eq 'warning' -and $r.issues[0].code -eq 'route_loop') '出力OFFの循環は予告'
    $r = Invoke-Request $a 'validate-route' @{ mode='device'; patch=@{outputs=@(@{deviceId='disconnected';enabled=$true},$null)} }
    Check ($r.result -eq 'rejected' -and $r.issues[0].code -eq 'device_unavailable') '未接続の有効出力を識別'
    $r = Invoke-Request $a 'validate-route' @{ mode='device'; patch=@{outputs=@(@{deviceId='headphones';enabled=$true},@{deviceId='communications';enabled=$true})} }
    Check ($r.result -eq 'valid' -and $r.candidate.effectiveOutputs[0] -eq 'headphones' -and $null -eq $r.candidate.effectiveOutputs[1] -and $r.issues[0].code -eq 'output_1_preferred' -and (Fixture-Hash $a) -eq $aInitial) '出力1優先と読み取り専用の検証結果'

    $r = Invoke-Request $a 'apply' @{expectedRevision=0;patch=@{mode='device';outputs=@(@{deviceId='headphones'},@{deviceId='cable'})}}
    Check ($r.result -eq 'applied' -and $r.revision -eq 1 -and $r.state.mode -eq 'device' -and (Fixture-Hash $a) -ne $aInitial) '設定案を適用・永続化し、確定後だけrevisionを進める'
    $r = Invoke-Request $a 'set-output' @{expectedRevision=1;output=1;enabled=$true}
    Check ($r.result -eq 'applied' -and $r.revision -eq 2 -and $r.state.outputsEnabled[0] -and -not $r.state.outputsEnabled[1]) '出力1の個別ON'
    $r = Invoke-Request $a 'set-output' @{expectedRevision=2;output=2;enabled=$true}
    Check ($r.result -eq 'applied' -and $r.revision -eq 3 -and $r.state.effectiveOutputs[1] -eq 'cable') '出力2の個別ON'
    $r = Invoke-Request $a 'set-output' @{expectedRevision=3;output=2;enabled=$true}
    Check ($r.result -eq 'unchanged' -and $r.revision -eq 3) '同値再指定は反転・保存しない'
    $r = Invoke-Request $a 'set-output' @{expectedRevision=1;output=2;enabled=$false}
    Check ($r.result -eq 'rejected' -and $r.issues[0].code -eq 'stale_revision' -and $r.state.outputsEnabled[1]) '古いrevisionを拒否'
    $r = Invoke-Request $a 'apply' @{expectedRevision=3;patch=@{outputs=@(@{deviceId='speaker-alias'},$null)}}
    Check ($r.result -eq 'rejected' -and $r.revision -eq 3 -and $r.state.effectiveOutputs[0] -eq 'headphones') '適用時の循環再検証と既存経路保持'

    $lastHash = Fixture-Hash $a
    foreach ($fault in @('audio','save','rollback')) {
        $r = Invoke-Request $a 'set-output' @{expectedRevision=3;output=2;enabled=$false;fault=$fault}
        $expected = @{audio='audio_apply_failed';save='save_failed';rollback='rollback_failed'}[$fault]
        Check ($r.result -eq 'failed' -and $r.issues[0].code -eq $expected -and $r.revision -eq 3 -and (Fixture-Hash $a) -eq $lastHash) "失敗時に設定とrevisionを維持: $fault"
        if ($fault -eq 'rollback') {
            Check ($r.state.phase -eq 'error' -and $null -eq $r.state.effectiveOutputs) '復旧失敗を経路不明として報告'
        } else {
            Check ($r.state.effectiveOutputs[1] -eq 'cable') '失敗後に旧経路を復旧'
        }
    }
    $r = Invoke-Request $a 'apply' @{expectedRevision=3;patch=@{}}
    Check ($r.result -eq 'applied' -and $r.revision -eq 3 -and $r.state.effectiveOutputs[1] -eq 'cable') '同じ設定の再適用で復旧'

    $r = Invoke-Request $a 'apply' @{expectedRevision=3;patch=@{outputs=@(@{arbitraryPath='C:/never-read'},$null)}}
    Check ($r.issues[0].code -eq 'unknown_field' -and $r.revision -eq 3) '許可リスト外の入れ子フィールドを拒否'
    foreach ($invalid in @(@{text='{';code='invalid_json'},@{text=('x'*65537);code='line_too_long'})) {
        $a.Process.StandardInput.WriteLine($invalid.text)
        $a.Process.StandardInput.Flush()
        $r = Read-Reply $a
        Check ($r.issues[0].code -eq $invalid.code -and $r.revision -eq 3) "不正入力で状態を保持: $($invalid.code)"
    }

    $r = Invoke-Request $a 'set-output' @{expectedRevision=3;output=1;enabled=$false}
    Check ($r.result -eq 'applied' -and $r.revision -eq 4 -and -not $r.state.outputsEnabled[0] -and $r.state.outputsEnabled[1] -and $r.state.effectiveOutputs[1] -eq 'cable') '出力1のOFFは出力2に影響しない'
    $r = Invoke-Request $b 'status'
    Check ($r.revision -eq 0 -and $r.state.mode -eq 'stopped' -and (Fixture-Hash $b) -eq $bInitial) '他方の試験セッションは不変'

    $beforeEditor = Fixture-Hash $a
    $r = Invoke-Request $a 'open-settings'
    Check ($r.state.settingsEditor.page -eq 'basic' -and -not $r.state.settingsEditor.dirty) '設定を開くと基本設定を選択'
    $r = Invoke-Request $a 'edit-settings' @{patch=@{browserPort=0;overlayMode='brief'}}
    Check ($r.state.settingsEditor.dirty -and (Fixture-Hash $a) -eq $beforeEditor) '基本設定の編集は下書きだけを変更'
    $r = Invoke-Request $a 'apply-settings' @{expectedRevision=4}
    Check ($r.result -eq 'rejected' -and $r.issues[0].code -eq 'invalid_settings' -and $r.state.settingsEditor.dirty -and (Fixture-Hash $a) -eq $beforeEditor) 'ポート0を拒否して下書きと確定設定を保持'
    $null = Invoke-Request $a 'edit-settings' @{patch=@{browserPort=18766}}
    $null = Invoke-Request $a 'select-settings-page' @{page='keys'}
    $r = Invoke-Request $a 'edit-settings' @{patch=@{overlayMode='off'}}
    Check ($r.issues[0].code -eq 'wrong_settings_page' -and $r.state.settingsEditor.page -eq 'keys') 'キー設定で基本設定の変更を拒否'
    $key = $r.state.hotkeys[4] | ConvertTo-Json -Compress | ConvertFrom-Json -AsHashtable
    $key.enabled = $true
    $key.key = 'F8'
    $null = Invoke-Request $a 'edit-settings' @{patch=@{hotkeys=@($null,$null,$null,$null,$key,$null)}}
    $r = Invoke-Request $a 'open-settings'
    Check ($r.state.settingsEditor.page -eq 'keys' -and $r.state.settingsEditor.dirty -and (Fixture-Hash $a) -eq $beforeEditor) '再表示で選択中のタブと下書きを保持'
    $r = Invoke-Request $a 'apply-settings' @{expectedRevision=4;fault='save'}
    Check ($r.result -eq 'failed' -and $r.revision -eq 4 -and $r.state.settingsEditor.dirty -and (Fixture-Hash $a) -eq $beforeEditor) '保存失敗でも下書きと直前の設定を保持'
    $r = Invoke-Request $a 'apply-settings' @{expectedRevision=4}
    Check ($r.result -eq 'applied' -and $r.revision -eq 5 -and $r.state.settingsEditor.page -eq 'keys' -and -not $r.state.settingsEditor.dirty -and $r.state.overlayMode -eq 'brief' -and $r.state.hotkeys[4].key -eq 'F8' -and (Fixture-Hash $a) -ne $beforeEditor) '両タブの編集を適用して同じタブに留まる'
    $afterEditor = Fixture-Hash $a
    $null = Invoke-Request $a 'select-settings-page' @{page='basic'}
    $null = Invoke-Request $a 'edit-settings' @{patch=@{overlayMode='off'}}
    $r = Invoke-Request $a 'close-settings'
    Check ($null -eq $r.state.settingsEditor -and $r.state.overlayMode -eq 'brief' -and $r.revision -eq 5 -and (Fixture-Hash $a) -eq $afterEditor) '閉じると最後の適用以降の下書きだけ破棄'
    $r = Invoke-Request $a 'open-settings'
    $null = Invoke-Request $a 'edit-settings' @{patch=@{overlayMode='off'}}
    $null = Invoke-Request $a 'apply' @{expectedRevision=5;patch=@{browserPort=18767}}
    $r = Invoke-Request $a 'apply-settings' @{expectedRevision=6}
    Check ($r.issues[0].code -eq 'stale_settings_draft' -and $r.state.browserPort -eq 18767 -and $r.state.overlayMode -eq 'brief') '開いた後の確定設定を古い下書きで上書きしない'
    $null = Invoke-Request $a 'close-settings'
    $null = Invoke-Request $a 'shutdown'
    Check ($a.Process.WaitForExit(5000) -and $a.Process.ExitCode -eq 0 -and -not (Test-Path -LiteralPath (Session-Path $a))) 'shutdownで対象子だけ終了し試験領域を回収'
    $crash = Start-Session
    Stop-Owned $crash
    Check ($crash.Process.HasExited -and (Invoke-Request $b 'status').revision -eq 0) '所有する試験子の異常終了が他方に波及しない'
    $b.Process.StandardInput.Close()
    Check ($b.Process.WaitForExit(5000) -and $b.Process.ExitCode -eq 0 -and -not (Test-Path -LiteralPath (Session-Path $b))) 'EOFでも試験領域を回収'
    $bad = New-Child @('--test-session','--config',$config,'--stop')
    Check ($bad.Process.WaitForExit(5000) -and $bad.Process.ExitCode -ne 0) '通常引数との併用を拒否'
    $missing = Join-Path $root 'missing'
    $bad = New-Child @('--test-session') $missing
    Check ($bad.Process.WaitForExit(5000) -and $bad.Process.ExitCode -ne 0 -and -not (Test-Path -LiteralPath $missing)) '試験領域作成失敗でも通常起動へ移行しない'

    $after = Get-Process -Id $ResidentId
    $reply = Get-ResidentReply
    Check ($after.Path -eq $initialPath -and $after.StartTime -eq $initialStarted -and $reply.pid -eq $ResidentId -and $reply.ready -and $reply.version -eq $beforeVersion -and $reply.protocolVersion -eq 2) '通常版のPID・開始時刻・EXE・管理契約が不変'
    Check ((Get-FileHash -LiteralPath $config).Hash -eq $initialHash -and (Get-Content -LiteralPath $statusPath -Raw | ConvertFrom-Json).mode -eq $initialMode) '通常設定のSHA-256と音声モードが不変'
} finally {
    foreach ($child in $children) {
        Stop-Owned $child
        if ($child.Session) {
            $dir = Session-Path $child
            $owner = Join-Path $dir 'owner'
            if ((Test-Path -LiteralPath $dir -PathType Container) -and
                -not ((Get-Item -LiteralPath $dir).Attributes -band [IO.FileAttributes]::ReparsePoint) -and
                (Test-Path -LiteralPath $owner -PathType Leaf) -and
                -not ((Get-Item -LiteralPath $owner).Attributes -band [IO.FileAttributes]::ReparsePoint) -and
                (Get-Content -LiteralPath $owner -Raw) -ceq $child.Session) {
                foreach ($name in @('settings.json', "settings.$($child.Pid).tmp", 'owner')) {
                    $file = Join-Path $dir $name
                    if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file }
                }
                if (-not (Get-ChildItem -LiteralPath $dir -Force)) { Remove-Item -LiteralPath $dir }
            }
        }
        $child.Process.Dispose()
    }
    if (-not (Get-ChildItem -LiteralPath $root -Force)) { Remove-Item -LiteralPath $root }
}
