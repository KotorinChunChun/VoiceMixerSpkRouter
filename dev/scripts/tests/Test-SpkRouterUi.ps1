#Requires -Version 7.0
<#
専用設定・出力OFFで、SpkRouterの保存復旧・編集排他・表示期限・起動設定を検証する。
マウス・キーの合成入力は使わない。撮影は対象ウィンドウだけとする。
#>
param([Parameter(Mandatory)][string]$Executable, [switch]$Screenshots)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Executable = (Resolve-Path -LiteralPath $Executable).Path
if (Get-Process -Name voicemixer-spk -ErrorAction SilentlyContinue) { throw 'SpkRouterを終了してから検証してください。' }
if ($Screenshots) { . (Join-Path $env:USERPROFILE '.codex/skills/windows-ui-automation/scripts/ui-toolkit.ps1') }
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
public static class SpkUiTest {
 [StructLayout(LayoutKind.Sequential)] public struct Rect { public int left,top,right,bottom; }
 [StructLayout(LayoutKind.Sequential)] public struct ComboInfo { public int size; public Rect item,button; public uint state; public IntPtr combo,edit,list; }
 [DllImport("user32.dll")] public static extern bool GetComboBoxInfo(IntPtr h,ref ComboInfo info);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out Rect rect);
 [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr h);
 [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", SetLastError=true)] static extern IntPtr Number(IntPtr h,uint m,UIntPtr w,IntPtr l,uint flags,uint timeout,out UIntPtr value);
 [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr String(IntPtr h,uint m,UIntPtr w,string l,uint flags,uint timeout,out UIntPtr value);
 [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr Buffer(IntPtr h,uint m,UIntPtr w,StringBuilder l,uint flags,uint timeout,out UIntPtr value);
 public static long Send(IntPtr h,uint m,long w=0,long l=0) { UIntPtr result; if(Number(h,m,new UIntPtr((ulong)w),new IntPtr(l),2,2000,out result)==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());return unchecked((long)result.ToUInt64()); }
 public static void Text(IntPtr h,string text) { UIntPtr result;if(String(h,12,UIntPtr.Zero,text,2,2000,out result)==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error()); }
 public static string Read(IntPtr h) { UIntPtr result;var value=new StringBuilder(4096);if(Buffer(h,13,new UIntPtr(4096),value,2,2000,out result)==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error());return value.ToString(); }
}
'@
$project = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$run = Join-Path $project ('tmp/spk-router-v109/run-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($run)
$config = Join-Path $run 'spk-settings.json'
$status = Join-Path $run 'spk-status.json'
$script:resident = $null
$script:passed = 0
$lock = $null
$occupied = $null
function Assert-Case([bool]$Value, [string]$Message) { if (-not $Value) { throw $Message } }
function Pass([string]$Name) { $script:passed++; Write-Host "成功 $script:passed`: $Name" }
function Invoke-Cli([string[]]$Operation) {
    $process = Start-Process -FilePath $Executable -ArgumentList (@('--config', ('"' + $config + '"')) + $Operation) -WindowStyle Hidden -PassThru
    try { if (-not $process.WaitForExit(15000)) { $process.Kill(); throw '専用CLIが終了しませんでした。' }; Assert-Case ($process.ExitCode -eq 0) '専用CLIが失敗しました。' } finally { $process.Dispose() }
}
function Wait-Test([scriptblock]$Condition, [string]$Message) {
    $deadline = [DateTime]::UtcNow.AddSeconds(12)
    do { if ($script:resident -and $script:resident.HasExited) { throw '専用常駐が異常終了しました。' }; if (& $Condition) { return }; Start-Sleep -Milliseconds 50 } while ([DateTime]::UtcNow -lt $deadline)
    throw $Message
}
function Window([string]$Class) {
    $handle = [SpkUiTest]::FindWindow($Class, [NullString]::Value)
    if ($handle -eq [IntPtr]::Zero) { return $handle }
    [uint32]$owner = 0
    [void][SpkUiTest]::GetWindowThreadProcessId($handle, [ref]$owner)
    Assert-Case ($script:resident -and $owner -eq $script:resident.Id) '専用常駐以外のウィンドウは操作しません。'
    return $handle
}
function Control([IntPtr]$Editor, [int]$Id) { $handle = [SpkUiTest]::GetDlgItem($Editor,$Id); Assert-Case ($handle -ne [IntPtr]::Zero) "設定欄がありません: $Id"; return $handle }
function Command([IntPtr]$Target, [int]$Id) { [void][SpkUiTest]::Send($Target,0x0111,$Id,0) }
function Select-Page([IntPtr]$Editor, [int]$Page) {
    $tab = Control $Editor 12
    Assert-Case ([SpkUiTest]::Send($tab,0x1304) -eq 2) '標準タブが2枚ありません。'
    $point = (20 -shl 16) -bor (80 + 145 * $Page)
    # タブ自身のクリック処理からTCN_SELCHANGEを発生させる。物理入力は使用しない。
    [void][SpkUiTest]::Send($tab,0x0201,1,$point)
    [void][SpkUiTest]::Send($tab,0x0202,0,$point)
    Wait-Test { [SpkUiTest]::Send($tab,0x130b) -eq $Page -and [SpkUiTest]::IsWindowVisible((Control $Editor 1055)) -eq ($Page -eq 1) } 'タブの選択とページ表示が一致しません。'
}
function State([string]$Mode) {
    try { $value = [IO.File]::ReadAllText($status) | ConvertFrom-Json; return $value.mode -eq $Mode -and $value.phase -eq $(if ($Mode -eq 'stopped') { 'stopped' } else { 'waiting' }) } catch { return $false }
}
function Start-Resident([string[]]$Operation) {
    $script:resident = Start-Process -FilePath $Executable -ArgumentList (@('--config', ('"' + $config + '"')) + $Operation) -WindowStyle Hidden -PassThru
    Wait-Test { (Window 'VoiceMixerSpkRouterV1') -ne [IntPtr]::Zero } '常駐を確認できません。'
}
function Stop-Resident {
    if ($script:resident -and -not $script:resident.HasExited) { Invoke-Cli @('--stop'); Assert-Case ($script:resident.WaitForExit(15000)) '専用常駐が終了しません。' }
    if ($script:resident) { $script:resident.Dispose(); $script:resident = $null }
}
function Open-Editor {
    Invoke-Cli @('--settings')
    Wait-Test { (Window 'VoiceMixerSpkSettingsV1') -ne [IntPtr]::Zero } '設定画面がありません。'
    $window = Window 'VoiceMixerSpkSettingsV1'
    Wait-Test { [SpkUiTest]::GetDlgItem($window,502) -ne [IntPtr]::Zero } '設定画面を構築できません。'
    return $window
}
function Apply-Editor([IntPtr]$Editor) {
    [SpkUiTest]::Text((Control $Editor 334),'')
    Command $Editor 501
    Wait-Test { [SpkUiTest]::IsWindowEnabled((Control $Editor 501)) -and [SpkUiTest]::Read((Control $Editor 334)) -eq '適用しました' } '設定を適用できません。'
}
function Save-Fails([IntPtr]$Editor, [string]$Hash) {
    [SpkUiTest]::Text((Control $Editor 334),'')
    Command $Editor 501
    Wait-Test { [SpkUiTest]::IsWindowEnabled((Control $Editor 501)) -and [SpkUiTest]::Read((Control $Editor 334)).Length -gt 0 } '保存失敗が編集画面へ戻りません。'
    Assert-Case ([SpkUiTest]::IsWindowVisible($Editor)) '保存失敗で画面が閉じました。'
    Assert-Case ((Get-FileHash -LiteralPath $config).Hash -eq $Hash) '保存失敗で設定が変わりました。'
    Wait-Test { State 'browser' } '旧ブラウザ経路へ復旧しません。'
}
function Photo([IntPtr]$Handle, [string]$Name) {
    if ($Screenshots) { $result = @(Save-WindowScreenshot -WindowHandle $Handle -Path (Join-Path $run $Name)); Assert-Case ($result[-1] -eq $true) '対象ウィンドウの撮影に失敗しました。' }
}
try {
    Invoke-Cli @('--init')
    $settings = [IO.File]::ReadAllText($config) | ConvertFrom-Json
    Assert-Case (@($settings.hotkeys[0..3] | Where-Object { -not $_.shift -or $_.alt -or $_.mode -ne 'coexist' }).Count -eq 0) '新規設定がShift・共存ではありません。'
    $port = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0)
    try { $port.Start(); $settings.browser_port = $port.LocalEndpoint.Port } finally { $port.Stop() }
    $settings.browser_token = 'spk-router-v109-validation-only'
    $settings.browser_stream_key = 'spk-router-v109-validation'
    [IO.File]::WriteAllText($config,($settings | ConvertTo-Json -Depth 10),[Text.UTF8Encoding]::new($false))
    Start-Resident @('--mode','browser')
    Wait-Test { State 'browser' } 'ブラウザ待機に移りません。'
    $originalPort = $settings.browser_port
    Invoke-Cli @('--settings')
    Wait-Test { (Window 'VoiceMixerSpkSettingsV1') -ne [IntPtr]::Zero } '設定画面がありません。'
    $editor = Window 'VoiceMixerSpkSettingsV1'
    Wait-Test { [SpkUiTest]::IsWindowVisible($editor) -and [SpkUiTest]::GetDlgItem($editor,502) -ne [IntPtr]::Zero } '明示した設定画面が表示されません。'
    Assert-Case ([SpkUiTest]::Read((Control $editor 501)) -eq '適用' -and [SpkUiTest]::Read((Control $editor 502)) -eq '閉じる') '下部ボタンの表記が異なります。'
    Assert-Case ([SpkUiTest]::Read((Control $editor 350)) -eq 'インストール案内' -and [SpkUiTest]::Read((Control $editor 352)) -eq '設定案内') 'ブラウザの案内リンクがありません。'
    Assert-Case ([SpkUiTest]::IsWindowVisible((Control $editor 353)) -and [SpkUiTest]::Read((Control $editor 353)).Contains('GitHub')) '公開GitHubリンクがありません。'
    $hostWindow = Window 'VoiceMixerSpkRouterV1'
    $hash = (Get-FileHash -LiteralPath $config).Hash
    foreach ($operation in @(1,2,4,5,6,104,205)) { Command $hostWindow $operation }
    Start-Sleep -Milliseconds 400
    Assert-Case ((Get-FileHash -LiteralPath $config).Hash -eq $hash -and (State 'browser')) '設定中に別操作が反映されました。'
    Pass '設定編集中はキー以外の取得・停止・出力・表示・トレイ終了を抑止'

    $combo = Control $editor 302
    $info = [SpkUiTest+ComboInfo]::new()
    $info.size = [Runtime.InteropServices.Marshal]::SizeOf($info)
    [void][SpkUiTest]::Send($combo,0x014f,1)
    Assert-Case ([SpkUiTest]::GetComboBoxInfo($combo,[ref]$info)) 'アプリ一覧の実ウィンドウを取得できません。'
    Wait-Test { [SpkUiTest]::IsWindowVisible($info.list) } 'アプリ一覧が展開しません。'
    $closed = [SpkUiTest+Rect]::new(); $expanded = [SpkUiTest+Rect]::new()
    [void][SpkUiTest]::GetWindowRect($combo,[ref]$closed)
    [void][SpkUiTest]::GetWindowRect($info.list,[ref]$expanded)
    Assert-Case ([Math]::Abs(($expanded.right-$expanded.left)-($closed.right-$closed.left)) -le 3) '展開後の横幅が入力欄から大きくはみ出しました。'
    Photo $editor 'app-dropdown.png'
    [void][SpkUiTest]::Send($combo,0x014f,0)
    Pass '長いアプリ候補があっても展開後の幅は欄の実幅に一致'

    [SpkUiTest]::Text((Control $editor 340),'音声入力用')
    [SpkUiTest]::Text((Control $editor 341),'配信用録音')
    [void][SpkUiTest]::Send((Control $editor 333),0x014e,1)
    [void][SpkUiTest]::Send((Control $editor 335),0x014e,4)
    $lock = [IO.File]::Open($config,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
    Save-Fails $editor $hash
    Assert-Case ([SpkUiTest]::Read((Control $editor 340)) -eq '音声入力用') '編集中の用途名が失われました。'
    $lock.Dispose(); $lock = $null
    Pass '保存先ロックで旧設定と編集中の用途名を保持'

    $occupied = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0)
    $occupied.Start()
    [SpkUiTest]::Text((Control $editor 330),[string]$occupied.LocalEndpoint.Port)
    Save-Fails $editor $hash
    Pass '音声設定の反映失敗で旧待受と設定へ復旧'
    $occupied.Stop(); $occupied = $null
    [SpkUiTest]::Text((Control $editor 330),[string]$originalPort)
    [SpkUiTest]::Text((Control $editor 334),'')
    Photo $editor 'basic.png'
    Select-Page $editor 1
    Wait-Test { [SpkUiTest]::IsWindowVisible((Control $editor 1055)) -and -not [SpkUiTest]::IsWindowVisible((Control $editor 330)) } 'キー設定ページへ切り替わりません。'
    Assert-Case (-not [SpkUiTest]::IsWindowVisible((Control $editor 353))) 'GitHubリンクがキー設定にも残っています。'
    Assert-Case ([SpkUiTest]::Read((Control $editor 354)).StartsWith('共存：') -and [SpkUiTest]::Read((Control $editor 355)).StartsWith('上書き：')) 'キー設定の説明ラベルがありません。'
    $previous = $null
    foreach ($row in @(5,4,0,1,2,3)) {
        $bounds = [SpkUiTest+Rect]::new()
        [void][SpkUiTest]::GetWindowRect((Control $editor (1000+$row*10)),[ref]$bounds)
        if ($previous) { Assert-Case ($previous.bottom -lt $bounds.top) 'キー設定の行順が異なります。' }
        $previous = $bounds
        $right = 0
        foreach ($col in @(0,2,1,4,3,5,6)) {
            $cell = [SpkUiTest+Rect]::new()
            [void][SpkUiTest]::GetWindowRect((Control $editor (1000+$row*10+$col)),[ref]$cell)
            Assert-Case ($cell.left -ge $right) '修飾キーの横順が異なります。'
            $right = $cell.right
        }
    }
    [void][SpkUiTest]::Send((Control $editor 1006),0x014e,1)
    Pass '操作行と修飾キーの順序、モード選択、ブラウザの案内リンクを確認'
    foreach ($id in @(1041,1042,1043,1044,1045,1051,1052,1053,1054,1055)) {
        Assert-Case (-not [SpkUiTest]::IsWindowEnabled((Control $editor $id))) "未割当のキー欄がグレーアウトされません: $id"
    }
    [void][SpkUiTest]::Send((Control $editor 1040),0x00f5)
    Wait-Test { [SpkUiTest]::IsWindowEnabled((Control $editor 1045)) } '未割当の行を有効にしてもキーを選べません。'
    [void][SpkUiTest]::Send((Control $editor 1040),0x00f5)
    Wait-Test { -not [SpkUiTest]::IsWindowEnabled((Control $editor 1045)) } '無効へ戻してもキーが編集できます。'
    $key = [SpkUiTest]::Read((Control $editor 1005))
    [void][SpkUiTest]::Send((Control $editor 1000),0x00f5)
    Wait-Test { -not [SpkUiTest]::IsWindowEnabled((Control $editor 1005)) } '無効キーがグレーアウトされません。'
    Assert-Case (-not [SpkUiTest]::IsWindowEnabled((Control $editor 1006))) '無効行のモードがグレーアウトされません。'
    Photo $editor 'keys.png'
    [void][SpkUiTest]::Send((Control $editor 1000),0x00f5)
    Wait-Test { [SpkUiTest]::IsWindowEnabled((Control $editor 1005)) } '再有効化しても編集できません。'
    Assert-Case ([SpkUiTest]::Read((Control $editor 1005)) -eq $key) '有効切替でキー選択が失われました。'
    Select-Page $editor 0
    Wait-Test { [SpkUiTest]::IsWindowVisible((Control $editor 330)) } '基本設定へ戻りません。'
    Pass '標準タブで編集を保持し、キー欄のグレーアウト・再有効化・未割当からの編集が可能'

    Select-Page $editor 1
    Command $editor 501
    Wait-Test { [SpkUiTest]::IsWindowEnabled((Control $editor 501)) -and [SpkUiTest]::Read((Control $editor 334)) -eq '適用しました' } '適用成功が画面へ戻りません。'
    Assert-Case ([SpkUiTest]::IsWindowVisible($editor) -and [SpkUiTest]::Send((Control $editor 12),0x130b) -eq 1) '適用後に画面が閉じたかタブが変わりました。'
    Assert-Case (-not [SpkUiTest]::IsWindowEnabled((Control $editor 1045))) '適用後に無効なキー欄が有効になりました。'
    $saved = [IO.File]::ReadAllText($config) | ConvertFrom-Json
    Assert-Case ($saved.output_labels[0] -eq '音声入力用' -and $saved.output_labels[1] -eq '配信用録音' -and $saved.overlay_mode -eq 'brief' -and $saved.startup_mode -eq 'stopped') '用途名・表示・起動設定が保存されていません。'
    Assert-Case ($saved.outputs.Count -eq 2 -and $saved.hotkeys.Count -eq 6) '出力数・キー操作数が異なります。'
    Assert-Case ($saved.hotkeys[0].mode -eq 'override' -and $saved.hotkeys[1].mode -eq 'coexist') 'キーごとのモードが保存されません。'
    Wait-Test { State 'browser' } '表示だけの保存で取得状態が変わりました。'
    $savedHash = (Get-FileHash -LiteralPath $config).Hash
    Command $hostWindow 4
    Start-Sleep -Milliseconds 100
    Assert-Case ((State 'browser') -and (Get-FileHash -LiteralPath $config).Hash -eq $savedHash) '適用後にキー以外の編集制限が解除されました。'
    Select-Page $editor 0
    [SpkUiTest]::Text((Control $editor 340),'未適用の変更')
    Command $editor 502
    Wait-Test { -not [SpkUiTest]::IsWindow($editor) } '閉じる操作が完了しません。'
    Invoke-Cli @('--settings')
    Wait-Test { (Window 'VoiceMixerSpkSettingsV1') -ne [IntPtr]::Zero } '再表示できません。'
    $editor = Window 'VoiceMixerSpkSettingsV1'
    Wait-Test { [SpkUiTest]::GetDlgItem($editor,502) -ne [IntPtr]::Zero } '再表示した設定画面を構築できません。'
    Assert-Case ([SpkUiTest]::Read((Control $editor 340)) -eq '音声入力用' -and (Get-FileHash -LiteralPath $config).Hash -eq $savedHash) '閉じる操作で適用済みの値まで失われました。'
    Command $editor 502
    Wait-Test { -not [SpkUiTest]::IsWindow($editor) } '再表示後の設定画面を閉じられません。'
    Pass '適用後も同じタブで編集を継続し、閉じるで適用後の変更だけ破棄'
    $overlay = Window 'VoiceMixerSpkOverlayV1'
    $bounds = [SpkUiTest+Rect]::new()
    Assert-Case ([SpkUiTest]::GetWindowRect($overlay,[ref]$bounds) -and $bounds.right - $bounds.left -eq 464) 'オーバーレイがMicと同じ464px幅ではありません。'
    Photo $overlay 'overlay.png'
    Wait-Test { -not [SpkUiTest]::IsWindowVisible($overlay) } '数秒表示が消えません。'
    Start-Sleep -Milliseconds 800
    Assert-Case (-not [SpkUiTest]::IsWindowVisible($overlay)) '定期更新で再表示されました。'
    Pass '表示と用途名だけの保存で取得を維持し、数秒表示は更新で延長しない'

    Command $hostWindow 104
    Wait-Test { [SpkUiTest]::IsWindowVisible($overlay) } '常時表示になりません。'
    Command $hostWindow 104
    Wait-Test { -not [SpkUiTest]::IsWindowVisible($overlay) } 'オフへ切り替えても消えません。'
    Command $hostWindow 4
    Wait-Test { State 'stopped' } '取得停止になりません。'
    Assert-Case (-not [SpkUiTest]::IsWindowVisible($overlay)) 'オフ中に停止操作で再表示されました。'
    Pass '表示3段階・即時オフ・オフ中の非再表示と取得停止'

    Command $hostWindow 3
    Wait-Test { State 'browser' } 'ブラウザ待機へ戻りません。'
    Stop-Resident
    Start-Resident @()
    Wait-Test { State 'stopped' } '固定起動モードが前回のブラウザより優先されません。'
    $reloaded = [IO.File]::ReadAllText($config) | ConvertFrom-Json
    Assert-Case ($reloaded.mode -eq 'browser' -and $reloaded.output_labels[0] -eq '音声入力用' -and $reloaded.outputs.Count -eq 2) '再起動で保存済み設定が失われました。'
    Stop-Resident
    Pass '通常終了・再起動で固定モードと保存済みの用途名を保持'

    Start-Resident @()
    Wait-Test { State 'stopped' } '追加検証の停止状態を確認できません。'
    $editor = Open-Editor
    foreach ($id in @(345,346)) { [void][SpkUiTest]::Send((Control $editor $id),0x00f5) }
    Wait-Test { -not [SpkUiTest]::IsWindowEnabled((Control $editor 301)) -and -not [SpkUiTest]::IsWindowEnabled((Control $editor 302)) } '使用OFFの入力設定が無効になりません。'
    [void][SpkUiTest]::Send((Control $editor 333),0x014e,2)
    Apply-Editor $editor
    $sourceSaved = [IO.File]::ReadAllText($config) | ConvertFrom-Json
    Assert-Case (-not $sourceSaved.input_enabled[0] -and -not $sourceSaved.input_enabled[1] -and $sourceSaved.input_enabled[2]) '取得元の使用設定が保存されません。'
    Photo $editor 'sources-disabled.png'
    Command $editor 502
    Wait-Test { -not [SpkUiTest]::IsWindow($editor) } '設定画面が閉じません。'
    $hostWindow = Window 'VoiceMixerSpkRouterV1'
    $overlay = Window 'VoiceMixerSpkOverlayV1'
    Wait-Test { [SpkUiTest]::IsWindowVisible($overlay) } '常時表示を確認できません。'
    Photo $overlay 'overlay-browser-only.png'
    Invoke-Cli @('--mode','device')
    Start-Sleep -Milliseconds 200
    Assert-Case (State 'stopped') '無効な入力スピーカーへ切り替わりました。'
    Invoke-Cli @('--mode','browser')
    Wait-Test { State 'browser' } '有効なブラウザを選択できません。'
    Pass '使用OFFの取得元は設定欄と候補を無効にし、有効な取得元だけ選択できる'

    $editor = Open-Editor
    [void][SpkUiTest]::Send((Control $editor 347),0x00f5)
    Wait-Test { -not [SpkUiTest]::IsWindowEnabled((Control $editor 330)) } '使用OFFのブラウザ欄が無効になりません。'
    Apply-Editor $editor
    Wait-Test { State 'stopped' } '取得中のブラウザを使用OFFにしても停止しません。'
    $allOff = [IO.File]::ReadAllText($config) | ConvertFrom-Json
    Assert-Case (@($allOff.input_enabled | Where-Object { $_ }).Count -eq 0 -and $allOff.mode -eq 'stopped') '全取得元OFFと停止が保存されません。'
    Assert-Case ($allOff.browser_token -eq $sourceSaved.browser_token -and $allOff.hotkeys[2].key -eq $sourceSaved.hotkeys[2].key) '使用OFFで接続設定・キー割当が失われました。'
    $released = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,$allOff.browser_port)
    try { $released.Start() } finally { $released.Stop() }
    Command $editor 502
    Wait-Test { -not [SpkUiTest]::IsWindow($editor) } '全OFFの設定画面を閉じられません。'
    Photo $overlay 'overlay-stopped-only.png'
    Stop-Resident
    Pass '選択中の取得元の使用OFFで停止し、ブラウザ待受を解放して接続設定とキーを保持'
    [pscustomobject]@{ passed = $script:passed; outputCount = 2; outputKeys = 0; physicalKeysTested = $false; mstscConnectionTested = $false } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run 'results.json') -Encoding utf8
} finally {
    if ($lock) { $lock.Dispose() }
    if ($occupied) { $occupied.Stop() }
    try { Stop-Resident } catch { if ($script:resident -and -not $script:resident.HasExited) { $script:resident.Kill() } }
}
Write-Host "SpkRouter UI実EXE検証: $script:passed 件成功。証跡: $run"
