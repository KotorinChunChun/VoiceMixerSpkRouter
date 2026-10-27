#Requires -Version 7.0
param([switch]$SkipBuild)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$project=Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Push-Location -LiteralPath $project
try {
    if(-not $SkipBuild){
        & (Join-Path $PSScriptRoot 'Build-Portable.ps1') | Out-Host
        if(-not $?){throw 'アプリの配布ビルドに失敗しました。'}
    }
    $info=& (Join-Path $PSScriptRoot 'Build-Portable.ps1') -SkipBuild
    $version=$info.Version
    $definition=Get-Content tool.json -Raw | ConvertFrom-Json
    if($definition.version -ne $version){throw '製品定義の版が異なります。'}
    $stage=Join-Path $project ('release/Installer/staging/'+[guid]::NewGuid().ToString('N'))
    $payload=Join-Path $stage 'VoiceMixerSpkRouter'
    [void][IO.Directory]::CreateDirectory($payload)
    Get-ChildItem -LiteralPath $info.Package -Force | Copy-Item -Destination $payload -Recurse
    Move-Item -LiteralPath (Join-Path $payload 'voicemixer-spk.exe') -Destination (Join-Path $payload 'VoiceMixerSpkRouter.exe')
    Copy-Item -LiteralPath (Join-Path $project 'tool.json') -Destination $payload
    [IO.File]::WriteAllText((Join-Path $payload 'README.md'),"# VoiceMixerSpkRouter"+[Environment]::NewLine+"VoiceMixerSpkRouter.exeを起動してください。初期設定は同梱の はじめに.html、更新・削除と本体連携はdocs/kotorintoys-integration.mdを参照してください。")
    foreach($name in @('はじめに.html','README.txt')){
        $guide=Join-Path $payload $name
        [IO.File]::WriteAllText($guide,([IO.File]::ReadAllText($guide).Replace('voicemixer-spk.exe','VoiceMixerSpkRouter.exe')))
    }
    [IO.File]::WriteAllText((Join-Path $payload 'README.txt'),"スタートメニューのVoiceMixerSpkRouterから起動してください。"+[Environment]::NewLine+"詳しい手順は はじめに.html を開いてください。"+[Environment]::NewLine+"Rust・Node.js・PowerShell 7は不要です。"+[Environment]::NewLine)
    $docs=Join-Path $payload 'docs';[void][IO.Directory]::CreateDirectory($docs)
    Copy-Item -LiteralPath (Join-Path $project 'docs/kotorintoys-integration.md') -Destination $docs
    & dotnet publish maintenance/SpkRouter.Maintenance.csproj -c Release -r win-x64 --self-contained true -p:DebugType=None -p:DebugSymbols=false -o (Join-Path $payload 'maintenance') --nologo | Out-Host
    if($LASTEXITCODE){throw '更新・削除補助の発行に失敗しました。'}
    foreach($file in @('SpkRouter.Maintenance.exe','coreclr.dll','hostfxr.dll','hostpolicy.dll')){
        if(-not(Test-Path -LiteralPath (Join-Path $payload ('maintenance/'+$file)))){throw "ランタイム同梱不足: $file"}
    }
    $exe=Get-Item -LiteralPath (Join-Path $payload 'VoiceMixerSpkRouter.exe')
    if($exe.VersionInfo.ProductName -ne $definition.name -or "$($exe.VersionInfo.FileMajorPart).$($exe.VersionInfo.FileMinorPart).$($exe.VersionInfo.FileBuildPart)" -ne $version){throw 'EXEの製品名・版が異なります。'}
    $all=@(Get-ChildItem -LiteralPath $payload -File -Recurse | Where-Object Name -ne 'SHA256SUMS.txt')
    $hashes=foreach($file in $all|Sort-Object FullName){
        $relative=[IO.Path]::GetRelativePath($payload,$file.FullName).Replace('\','/')
        if($relative -match '(?i)(spk-settings|spk-status)\.json|\.pdb$|(^|/)(private|temp|node_modules)/'){throw "不要物の混入: $relative"}
        (Get-FileHash -LiteralPath $file.FullName).Hash.ToLowerInvariant()+'  '+$relative
    }
    [IO.File]::WriteAllLines((Join-Path $payload 'SHA256SUMS.txt'),[string[]]$hashes)
    & (Join-Path $PSScriptRoot 'Prepare-Inno.ps1') | Out-Host
    if(-not $?){throw 'Innoの準備に失敗しました。'}
    $output=Join-Path $project ('release/Installer/v'+$version)
    [void][IO.Directory]::CreateDirectory($output)
    $iscc=Join-Path $project 'target/installer-tools/inno-7.1.0/ISCC.exe'
    & $iscc --quiet-progress "--define=ProductVersion=$version" "--define=Payload=$payload" "--define=OutputDirectory=$output" (Join-Path $project 'installers/SpkRouter.iss') | Out-Host
    if($LASTEXITCODE){throw 'インストーラの作成に失敗しました。'}
    $setup=Join-Path $output "VoiceMixerSpkRouter-$version-setup.exe"
    $hash=(Get-FileHash -LiteralPath $setup).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $output 'VoiceMixerSpkRouter-SHA256SUMS.txt'),$hash+'  '+[IO.Path]::GetFileName($setup)+[Environment]::NewLine)
    $catalog=[ordered]@{schemaVersion=1;distributionStatus='published';packages=@([ordered]@{
        id=$definition.id;name=$definition.name;version=$version;appId=$definition.appId;
        file=[IO.Path]::GetFileName($setup);sha256=$hash;signature='unsigned-public-build'
    })}
    $catalog | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $output 'VoiceMixerSpkRouter-packages.json') -Encoding utf8NoBOM
    [pscustomobject]@{Version=$version;Installer=$setup;Sha256=$hash;Length=(Get-Item $setup).Length;Payload=$payload;SourceCommit=$info.SourceCommit}
} finally {Pop-Location}
