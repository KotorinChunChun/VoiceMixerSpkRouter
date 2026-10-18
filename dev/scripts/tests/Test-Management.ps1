#Requires -Version 7.0
param([Parameter(Mandatory)][string]$Executable)
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
$exe=(Resolve-Path -LiteralPath $Executable).Path
$project=Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
if(@(Get-Process -Name voicemixer-spk,VoiceMixerSpkRouter -ErrorAction SilentlyContinue).Count){throw '既存常駐を正常終了・退避してから検証してください。'}
$root=Join-Path $project ('tmp/management/'+[guid]::NewGuid().ToString('N'));[void][IO.Directory]::CreateDirectory($root)
$config=Join-Path $root 'settings.json';$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$pipeName="KotorinToys.$sid.voice-mixer-spk-router"
Add-Type -TypeDefinition @"
using System;using System.Runtime.InteropServices;
public static class ManagementWindow{
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]public static extern IntPtr FindWindow(string c,string t);
 [DllImport("user32.dll")]public static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
}
"@
function Send([string]$Line){
 $pipe=[IO.Pipes.NamedPipeClientStream]::new('.',$pipeName,[IO.Pipes.PipeDirection]::InOut,[IO.Pipes.PipeOptions]::CurrentUserOnly)
 try{$pipe.Connect(2000);$writer=[IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false),1024,$true);$writer.AutoFlush=$true;$reader=[IO.StreamReader]::new($pipe)
 $writer.WriteLine($Line);$task=$reader.ReadLineAsync();if(-not $task.Wait(2500)){throw '応答なし'};return ($task.Result|ConvertFrom-Json)
 }finally{$pipe.Dispose()}
}
$checks=[Collections.Generic.List[string]]::new()
function Check([bool]$Value,[string]$Name){if(-not $Value){throw $Name};$checks.Add($Name);Write-Host "成功: $Name"}
$process=$null
try{
 $process=Start-Process -FilePath $exe -ArgumentList @('--config',('"'+$config+'"'),'--startup','--launch-origin','host_manual') -WindowStyle Hidden -PassThru
 $deadline=[DateTime]::UtcNow.AddSeconds(15);$reply=$null
 do{try{$reply=Send 'status'}catch{Start-Sleep -Milliseconds 80}}while($null -eq $reply -and [DateTime]::UtcNow -lt $deadline)
 Check ($null -ne $reply -and $reply.pid -eq $process.Id -and $reply.protocolVersion -eq 2 -and $reply.ready -and $reply.launchOrigin -eq 'host_manual') '本体互換のstatusと起動経路'
 Check ([ManagementWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value) -eq [IntPtr]::Zero) '初回でもstartupは設定画面を開かない'
 $again=Start-Process -FilePath $exe -ArgumentList @('--startup','--launch-origin','direct_task') -WindowStyle Hidden -PassThru -Wait
 Check ($again.ExitCode -eq 0 -and [ManagementWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value) -eq [IntPtr]::Zero) '既存常駐へのstartupも設定画面を開かない'
 $again.Dispose()
 $saved=Get-Content -LiteralPath $config -Raw|ConvertFrom-Json
 Check ($saved.mode -eq 'stopped' -and @($saved.outputs|Where-Object enabled).Count -eq 0) '停止・2出力OFFで初期化'
 $hash=(Get-FileHash -LiteralPath $config).Hash
 $bad=Send ('{"command":"stop","expectedPid":'+($process.Id+1)+',"requestId":"wrong"}')
 Check ($bad.result -eq 'refused' -and $bad.requestId -eq 'wrong' -and -not $process.HasExited) 'PID違いの停止を拒否'
 Check ((Send '{"command":"stop"}').result -eq 'refused') 'PIDなしの変更要求を拒否'
 Check ((Send '{"command":"unknown"}').result -eq 'unsupported') '未知の操作を明示'
 Check ((Send 'invalid').result -eq 'failed') '不正な要求から復旧'
 for($n=0;$n -lt 60;$n++){if((Send 'status').pid -ne $process.Id){throw '連続接続のPID不一致'}}
 Check $true '60回の連続接続で再接続競合がない'
 $idle=[IO.Pipes.NamedPipeClientStream]::new('.',$pipeName,[IO.Pipes.PipeDirection]::InOut);try{$idle.Connect(2000);Start-Sleep -Milliseconds 2200}finally{$idle.Dispose()}
 Check ((Send 'status').ready) '要求を送らないクライアントの期限後に復旧'
 $shown=Send ('{"command":"show","expectedPid":'+$process.Id+',"requestId":"show"}')
 Check ($shown.result -eq 'completed' -and $shown.requestId -eq 'show') 'UI巡回でshowが完了'
 $window=[ManagementWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value)
 Check ($window -ne [IntPtr]::Zero) '既存プロセスに設定画面を表示'
 $refused=Send ('{"command":"stop","expectedPid":'+$process.Id+'}')
 Check ($refused.result -eq 'refused' -and -not $process.HasExited) '設定編集中の正常停止を拒否'
 Check ((Get-FileHash -LiteralPath $config).Hash -eq $hash) '管理表示と停止拒否で設定を保持'
 [void][ManagementWindow]::SendMessageW($window,0x0010,[IntPtr]::Zero,[IntPtr]::Zero)
 Start-Sleep -Milliseconds 100
 $stopped=Send ('{"command":"stop","expectedPid":'+$process.Id+',"requestId":"stop"}')
 Check ($stopped.result -eq 'accepted' -and $stopped.requestId -eq 'stop' -and $process.WaitForExit(15000)) '終了受理後に実プロセスの終了を確認'
 $checks|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $root 'results.json')
}finally{
 if($null -ne $process -and -not $process.HasExited){
  $stop=Start-Process -FilePath $exe -ArgumentList '--stop' -WindowStyle Hidden -PassThru -Wait
  $stop.Dispose();[void]$process.WaitForExit(15000)
 }
 if($null -ne $process){$process.Dispose()}
}
