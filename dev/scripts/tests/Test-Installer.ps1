#Requires -Version 7.0
param([Parameter(Mandatory)][string]$Installer,[string]$PreviousInstaller)
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
$setup=(Resolve-Path -LiteralPath $Installer).Path
$project=Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$expectedVersion=(Get-Content -LiteralPath (Join-Path $project 'tool.json') -Raw | ConvertFrom-Json).version
$root=Join-Path $project ('tmp/installer-tests/検証 '+[guid]::NewGuid().ToString('N'));[void][IO.Directory]::CreateDirectory($root)
$install=Join-Path $root '導入先 日本語';$exe=Join-Path $install 'VoiceMixerSpkRouter.exe'
$config=Join-Path $install 'spk-settings.json'
$registry='HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\KotorinToys.VoiceMixerSpkRouter_is1'
$shortcut=Join-Path ([Environment]::GetFolderPath('Programs')) 'VoiceMixerSpkRouter.lnk'
if(Test-Path -LiteralPath $registry){throw '既存の正式導入があります。検証で変更しません。'}
if(Test-Path -LiteralPath $shortcut){throw '既存の製品ショートカットを変更しません。'}
if(@(Get-Process -Name voicemixer-spk,VoiceMixerSpkRouter -ErrorAction SilentlyContinue).Count){throw '利用中の常駐を退避・終了してください。'}
$hostSettings=Join-Path $env:LOCALAPPDATA 'KotorinToys/settings.json'
$hostHash=if(Test-Path -LiteralPath $hostSettings){(Get-FileHash -LiteralPath $hostSettings).Hash}else{'absent'}
function HostScope {
 if(-not(Test-Path -LiteralPath $hostSettings)){return ([ordered]@{mode=$null;task=$null}|ConvertTo-Json -Compress)}
 $hostData=Get-Content -LiteralPath $hostSettings -Raw|ConvertFrom-Json
 $mode=$hostData.startupModes.PSObject.Properties['voice-mixer-spk-router']
 $task=$hostData.ownedTasks.PSObject.Properties['voice-mixer-spk-router']
 return ([ordered]@{mode=if($null -ne $mode){$mode.Value}else{$null};task=if($null -ne $task){$task.Value}else{$null}}|ConvertTo-Json -Depth 20 -Compress)
}
$hostScopeBefore=HostScope
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$pipeName="KotorinToys.$sid.voice-mixer-spk-router"
Add-Type -TypeDefinition @"
using System;using System.Text;using System.Runtime.InteropServices;
public static class InstallerWindow{
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindow(string c,string t);
 [DllImport("user32.dll")]public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")]public static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll",EntryPoint="SendMessageW")]public static extern IntPtr Send(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll",EntryPoint="SendMessageW",CharSet=CharSet.Unicode)]public static extern IntPtr Text(IntPtr h,uint m,IntPtr w,string value);
 [DllImport("user32.dll",EntryPoint="SendMessageW",CharSet=CharSet.Unicode)]public static extern IntPtr Read(IntPtr h,uint m,IntPtr w,StringBuilder value);
}
"@
$checks=[Collections.Generic.List[string]]::new();$process=$null;$number=0
function Check([bool]$Value,[string]$Name){if(-not $Value){throw $Name};$checks.Add($Name);Write-Host "成功: $Name"}
function Send([string]$Command){
 $pipe=[IO.Pipes.NamedPipeClientStream]::new('.',$pipeName,[IO.Pipes.PipeDirection]::InOut,[IO.Pipes.PipeOptions]::CurrentUserOnly)
 try{$pipe.Connect(1500);$writer=[IO.StreamWriter]::new($pipe,[Text.UTF8Encoding]::new($false),1024,$true);$writer.AutoFlush=$true;$reader=[IO.StreamReader]::new($pipe)
 $line=if($Command -eq 'status'){'status'}else{@{command=$Command;expectedPid=$script:process.Id;requestId=[guid]::NewGuid().ToString('N')}|ConvertTo-Json -Compress}
 $writer.WriteLine($line);$task=$reader.ReadLineAsync();if(-not $task.Wait(2500)){throw '管理応答なし'};return ($task.Result|ConvertFrom-Json)
 }finally{$pipe.Dispose()}
}
function Run([string]$File,[string[]]$Arguments){
 $p=Start-Process -FilePath $File -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
 try{if(-not $p.WaitForExit(60000)){throw 'インストーラの終了待ちタイムアウト'};return $p.ExitCode}finally{$p.Dispose()}
}
function Install([string]$File){
 $script:number++
 Run $File @('/VERYSILENT','/SUPPRESSMSGBOXES','/SP-','/NORESTART','/RESTARTEXITCODE=3010','/NOCLOSEAPPLICATIONS','/NOFORCECLOSEAPPLICATIONS','/NORESTARTAPPLICATIONS',('/DIR="'+$install+'"'),('/LOG="'+(Join-Path $root ("setup-$script:number.log"))+'"'))
}
function Uninstall {
 $script:number++
 Run (Join-Path $install 'unins000.exe') @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART',('/LOG="'+(Join-Path $root ("uninstall-$script:number.log"))+'"'))
}
function StartApp {
 $script:process=Start-Process -FilePath $exe -ArgumentList @('--startup','--launch-origin','via_kotorintoys') -WorkingDirectory $install -WindowStyle Hidden -PassThru
 $deadline=[DateTime]::UtcNow.AddSeconds(15);$reply=$null
 do{try{$reply=Send 'status'}catch{Start-Sleep -Milliseconds 80}}while($null -eq $reply -and [DateTime]::UtcNow -lt $deadline)
 Check ($null -ne $reply -and $reply.ready -and $reply.pid -eq $script:process.Id -and $reply.launchOrigin -eq 'via_kotorintoys') '正式導入EXEの管理準備完了'
}
function StopApp {
 Check ((Send 'stop').result -eq 'accepted' -and $script:process.WaitForExit(15000)) '管理契約から正常終了'
 $script:process.Dispose();$script:process=$null
}
function OwnWindow {
 $window=[InstallerWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value);[uint32]$owner=0
 [void][InstallerWindow]::GetWindowThreadProcessId($window,[ref]$owner)
 if($owner -ne $script:process.Id){throw '設定画面の所有PIDが異なります'}
 return $window
}
try {
 $first=if($PreviousInstaller){(Resolve-Path -LiteralPath $PreviousInstaller).Path}else{$setup}
 Check ((Install $first) -eq 0) '初回のサイレント導入'
 Check ((Test-Path -LiteralPath $registry) -and (Get-ItemProperty -LiteralPath $registry).InstallLocation.TrimEnd('\') -eq $install) 'HKCUの正式登録と日本語・空白入り導入先'
 Check ((Test-Path -LiteralPath $shortcut) -and (Test-Path -LiteralPath (Join-Path $install 'tool.json'))) 'ショートカットと製品定義を同梱'
 Check (-not(Test-Path -LiteralPath $config)) '配布設定やトークンを持ち込まない'
 StartApp
 Check ([InstallerWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value) -eq [IntPtr]::Zero) '本体からは無表示で起動'
 $settings=Get-Content -LiteralPath $config -Raw|ConvertFrom-Json
 Check ($settings.mode -eq 'stopped' -and $settings.outputs.Count -eq 2 -and @($settings.outputs|Where-Object enabled).Count -eq 0) '初回設定は停止・2出力OFF'
 StopApp
 $settings.output_labels[0]='保持確認'
 $settings.input_enabled[2]=$false
 $settings|ConvertTo-Json -Depth 20|Set-Content -LiteralPath $config -Encoding utf8NoBOM
 $hash=(Get-FileHash -LiteralPath $config).Hash
 StartApp
 Check ((Send 'show').result -eq 'completed') '設定画面の表示'
 $window=OwnWindow;$field=[InstallerWindow]::GetDlgItem($window,340)
 [void][InstallerWindow]::Text($field,12,[IntPtr]::Zero,'未保存の編集')
 Check ((Install $setup) -ne 0 -and -not $process.HasExited) '編集中はsetup更新を拒否'
 Check ((Uninstall) -ne 0 -and -not $process.HasExited) '編集中はアンインストールを拒否'
 $draft=[Text.StringBuilder]::new(128);[void][InstallerWindow]::Read($field,13,[IntPtr]128,$draft)
 Check ($draft.ToString() -eq '未保存の編集' -and (Get-FileHash -LiteralPath $config).Hash -eq $hash) '更新・削除拒否後も未保存編集と設定を保持'
 [void][InstallerWindow]::Send($window,0x0010,[IntPtr]::Zero,[IntPtr]::Zero);Start-Sleep -Milliseconds 100
 Check ((Install $setup) -eq 0 -and $process.WaitForExit(15000)) '稼働中の正常停止と保持付き更新'
 $process.Dispose();$process=$null
 Check ((Get-FileHash -LiteralPath $config).Hash -eq $hash) '更新後も設定・トークンの全バイトを保持'
 $versionInfo=(Get-Item $exe).VersionInfo
 Check ("$($versionInfo.FileMajorPart).$($versionInfo.FileMinorPart).$($versionInfo.FileBuildPart)" -eq $expectedVersion) '更新先EXEの版を確認'
 StartApp
 Check ((Get-FileHash -LiteralPath $config).Hash -eq $hash) '更新後の再起動と設定再利用'
 StopApp
 Check ((Uninstall) -eq 0) 'サイレント削除'
 Check (-not(Test-Path -LiteralPath $exe) -and -not(Test-Path -LiteralPath $registry) -and -not(Test-Path -LiteralPath $shortcut)) '本体・導入登録・ショートカットを削除'
 Check ((Get-FileHash -LiteralPath $config).Hash -eq $hash) '削除後も設定・トークンを保持'
 Check ((Install $setup) -eq 0) '同じ導入先へ再導入'
 StartApp
 Check ((Get-FileHash -LiteralPath $config).Hash -eq $hash) '再導入後に以前の設定を再利用'
 StopApp
 Check ((Uninstall) -eq 0) '検証用の正式導入を解除'
 $after=if(Test-Path -LiteralPath $hostSettings){(Get-FileHash -LiteralPath $hostSettings).Hash}else{'absent'}
 Check ((HostScope) -ceq $hostScopeBefore) '本体のSpk対象予約を変更していない'
 if($after -ne $hostHash){
  $journal=Join-Path (Split-Path $hostSettings) 'last-removal-operation.json'
  $external=if(Test-Path -LiteralPath $journal){(Get-Content -LiteralPath $journal -Raw|ConvertFrom-Json).targetId}else{''}
  if(-not $external -or $external -eq 'voice-mixer-spk-router'){throw '本体設定の対象外差分を説明できません'}
  Write-Host "他製品の並行操作を検出: $external（Spk対象の予約は一致）"
 }
 $checks|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $root 'results.json')
 Write-Host "証跡: $root"
}finally {
 if($null -ne $process -and -not $process.HasExited){
  $p=Start-Process -FilePath $exe -ArgumentList '--stop' -WindowStyle Hidden -PassThru -Wait;$p.Dispose();[void]$process.WaitForExit(15000)
 }
 if(Test-Path -LiteralPath (Join-Path $install 'unins000.exe')){
  $code=Uninstall;if($code -ne 0){Write-Warning "検証導入の解除に失敗しました: $code"}
 }
}
