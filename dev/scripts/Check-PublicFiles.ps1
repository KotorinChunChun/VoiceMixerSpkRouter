#Requires -Version 7.0
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Push-Location -LiteralPath $projectRoot
try {
    $files = @(& git -c core.quotepath=false ls-files)
    if ($LASTEXITCODE -ne 0 -or $files.Count -eq 0) { throw '追跡ファイルを確認できません。' }
    $forbidden = '(?i)(^|/)(release|target|tmp|node_modules|dist|private|temp|recordings|replay-cache)/|(^|/)(spk-settings|spk-status|spk-router-project)\.json$|\.(exe|dll|pdb|key|pem|onnx|gguf)$|(^|/)\.env($|\.)'
    $secret = '(?i)gh[pousr]_[a-z0-9]{20,}|github_pat_[a-z0-9_]{30,}|sk-(?:proj-)?[a-z0-9_-]{24,}|AKIA[0-9A-Z]{16}|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----'
    foreach ($file in $files) {
        if ($file -match $forbidden) { throw "公開対象に個人設定・成果物等があります: $file" }
        if ([IO.Path]::GetExtension($file) -in @('.png','.wav','.ico')) { continue }
        $body = [IO.File]::ReadAllText((Join-Path $projectRoot $file))
        if ($body -match $secret) { throw "認証情報らしい内容を確認してください: $file" }
    }
    $metadataText = & cargo metadata --offline --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw '依存関係を確認できません。' }
    $metadata = $metadataText | ConvertFrom-Json
    $prefix = [IO.Path]::GetFullPath($projectRoot).TrimEnd('\') + '\'
    foreach ($package in $metadata.packages) {
        if (-not [IO.Path]::GetFullPath($package.manifest_path).StartsWith($prefix,[StringComparison]::OrdinalIgnoreCase)) { throw '独立先以外のワークスペース依存があります。' }
    }
    Write-Host "公開対象$($files.Count)ファイルと独立したワークスペースを確認しました。"
} finally { Pop-Location }
