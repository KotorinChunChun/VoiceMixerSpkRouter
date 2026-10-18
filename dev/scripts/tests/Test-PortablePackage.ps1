#Requires -Version 7.0
param([Parameter(Mandatory)][string]$Zip,[switch]$SmokeTest)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$projectRoot=Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$zipPath=(Resolve-Path -LiteralPath $Zip).Path
$testRoot=Join-Path $projectRoot ('tmp/portable-release/展開 テスト-'+[guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
Expand-Archive -LiteralPath $zipPath -DestinationPath $testRoot
$roots=@(Get-ChildItem -LiteralPath $testRoot)
if($roots.Count -ne 1 -or $roots[0].Name -ne 'VoiceMixerSpkRouter'){throw 'ZIPのルート構成が異なります'}
$package=$roots[0].FullName
$files=@(Get-ChildItem -LiteralPath $package -File -Recurse)
$listed=@{}
foreach($line in [IO.File]::ReadAllLines((Join-Path $package 'SHA256SUMS.txt'))){
 if($line -notmatch '^([0-9a-f]{64})  (.+)$'){throw '内部ハッシュ一覧の形式が異なります'}
 $expected=$Matches[1];$relative=$Matches[2]
 $path=[IO.Path]::GetFullPath((Join-Path $package $relative))
 if(-not $path.StartsWith($package+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw 'ハッシュ一覧に範囲外のパスがあります'}
 if($listed.ContainsKey($relative)){throw 'ハッシュ一覧の重複'}
 if((Get-FileHash -LiteralPath $path).Hash.ToLowerInvariant() -ne $expected){throw "内容が一致しません: $relative"}
 $listed[$relative]=$true
}
foreach($file in $files){
 $relative=[IO.Path]::GetRelativePath($package,$file.FullName).Replace('\','/')
 if($relative -ne 'SHA256SUMS.txt' -and -not $listed.ContainsKey($relative)){throw "未登録の同梱物: $relative"}
 if($relative -match '(?i)(spk-settings|spk-status|spk-router-project)\.json|\.(key|pem|pdb|log)$|(^|/)(private|temp|node_modules)/'){throw "不要物の混入: $relative"}
}
$zipHash=(Get-FileHash -LiteralPath $zipPath).Hash.ToLowerInvariant()
if(([IO.File]::ReadAllText($zipPath+'.sha256').Split(' ')[0]) -ne $zipHash){throw 'ZIPの外部ハッシュが異なります'}
foreach($required in @('voicemixer-spk.exe','はじめに.html','browser-capture.html','README.txt','VERSION.txt','browser-extension/manifest.json')){
 if(-not(Test-Path -LiteralPath (Join-Path $package $required) -PathType Leaf)){throw "必須ファイルなし: $required"}
}
$extension=Join-Path $package 'browser-extension'
$manifest=Get-Content -LiteralPath (Join-Path $extension 'manifest.json') -Raw | ConvertFrom-Json
foreach($relative in @($manifest.background.service_worker,$manifest.side_panel.default_path,$manifest.options_page)+@($manifest.icons.PSObject.Properties.Value)){
 if(-not(Test-Path -LiteralPath (Join-Path $extension $relative) -PathType Leaf)){throw "拡張の参照先なし: $relative"}
}
$vswhere=Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$dumpbin=@(& $vswhere -latest -products '*' -find 'VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe')[0]
$exe=Join-Path $package 'voicemixer-spk.exe'
$dependencies=& $dumpbin /dependents $exe
if($LASTEXITCODE -ne 0){throw 'DLL依存を検査できません'}
$dependencies | Set-Content -LiteralPath (Join-Path $testRoot 'dependencies.txt')
if(($dependencies -join "`n") -match '(?i)VCRUNTIME|MSVCP|api-ms-win-crt|ucrtbase'){throw '外部CRT依存が残っています'}
# PEヘッダーを読み、配布対象がAMD64であることを確認する。
$pe=[IO.File]::ReadAllBytes($exe);$offset=[BitConverter]::ToInt32($pe,0x3c)
if([BitConverter]::ToUInt16($pe,$offset+4) -ne 0x8664){throw 'x64のEXEではありません'}
function Invoke-Cli([string[]]$Arguments){
 $suffix=[guid]::NewGuid().ToString('N')
 $out=Join-Path $testRoot ($suffix+'.out');$err=Join-Path $testRoot ($suffix+'.err')
 $probe=Start-Process -FilePath $exe -ArgumentList $Arguments -WorkingDirectory $package -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden -PassThru
 try{if(-not $probe.WaitForExit(15000)){throw 'CLIの終了待ちがタイムアウト'};if($probe.ExitCode -ne 0){throw 'CLIが失敗しました'};return [IO.File]::ReadAllText($out).Trim()}finally{$probe.Dispose()}
}
$version=Invoke-Cli @('--version')
$metadata=[IO.File]::ReadAllText((Join-Path $package 'VERSION.txt'))
if($version -notmatch '^VoiceMixerSpkRouter v([0-9.]+)$' -or -not $metadata.StartsWith($version+"`n")){throw 'EXEと版情報が異なります'}
$appVersion=$Matches[1]
$smokePassed=$false
if($SmokeTest){
 if(@(Get-Process -Name voicemixer-spk -ErrorAction SilentlyContinue).Count -ne 0){throw '利用中のSpkRouterを正常終了してから、専用展開先の起動試験を行ってください'}
 Add-Type -TypeDefinition @"
using System;using System.Runtime.InteropServices;
public static class PortableWindow {
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls,string title);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
}
"@
 $app=$null
 try{
  for($attempt=0;$attempt -lt 2;$attempt++){
   $start=@{FilePath=$exe;WorkingDirectory=$package;WindowStyle='Hidden';PassThru=$true}
   if($attempt -eq 1){$start.ArgumentList='--settings'}
   $app=Start-Process @start
   $deadline=[DateTime]::UtcNow.AddSeconds(15)
   do{
    $window=[PortableWindow]::FindWindow('VoiceMixerSpkSettingsV1',[NullString]::Value)
    [uint32]$owner=0;[void][PortableWindow]::GetWindowThreadProcessId($window,[ref]$owner)
    $ready=$owner -eq $app.Id -and [PortableWindow]::IsWindowVisible($window) -and [PortableWindow]::GetDlgItem($window,502) -ne [IntPtr]::Zero
    if($ready){break};Start-Sleep -Milliseconds 80
   }while([DateTime]::UtcNow -lt $deadline)
   if(-not $ready){throw '配布EXEの設定画面を確認できません'}
   $settings=Get-Content -LiteralPath (Join-Path $package 'spk-settings.json') -Raw | ConvertFrom-Json
   if($settings.mode -ne 'stopped' -or $settings.outputs.Count -ne 2 -or @($settings.outputs | Where-Object enabled).Count -ne 0){throw '初期の停止・2出力OFFが異なります'}
   if([string]::IsNullOrWhiteSpace($settings.browser_token)){throw '初回トークンがありません'}
   if($attempt -eq 0){$initialToken=$settings.browser_token}else{if($initialToken -cne $settings.browser_token){throw '再起動で設定が変わりました'}}
   [void](Invoke-Cli @('--stop'))
   if(-not $app.WaitForExit(15000)){throw '配布EXEが正常終了しません'}
   $app.Dispose();$app=$null
  }
  $smokePassed=$true
 }finally{
  if($null -ne $app){
   if(-not $app.HasExited){[void](Invoke-Cli @('--stop'));if(-not $app.WaitForExit(15000)){throw '検証用プロセスが終了しません'}}
   $app.Dispose()
  }
 }
}
$result=[pscustomobject]@{version=$appVersion;zip=$zipPath;sha256=$zipHash;files=$files.Count;hashesVerified=$true;staticCrt=$true;x64=$true;extensionReferences=$true;initialLaunchAndRestart=$smokePassed;extracted=$package}
$result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $testRoot 'result.json') -Encoding utf8
$result
