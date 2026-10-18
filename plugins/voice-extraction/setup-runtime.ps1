[CmdletBinding()]
param(
    [switch]$AcceptDownload,
    [ValidateSet('cpu','cu121')] [string]$TorchVariant = 'cpu',
    [switch]$BootstrapOnly
)
$ErrorActionPreference = 'Stop'
if (-not $AcceptDownload) { throw 'GB単位の専用Python実行環境を導入します。-AcceptDownloadで明示導入してください。' }
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw '初期対応はWindows x64です。ARM64ネイティブ用DeepFilterLib wheelは未提供です。' }
$env:UV_PYTHON_INSTALL_DIR = Join-Path $PSScriptRoot '.python'
$env:UV_CACHE_DIR = Join-Path $PSScriptRoot '.cache'

function Get-VoiceExtractionUv {
    $existing = Get-Command uv -CommandType Application -ErrorAction SilentlyContinue
    if ($existing) { return $existing.Source }
    # リモートスクリプトは実行しない。公式release assetの固定SHA256を先に照合する。
    $version = '0.11.24'
    $archiveHash = 'af9573a2e36f7020b18ec5fdde20117aae74bbad3f4acb3dc3fc03319f1aa083'
    $archiveBytes = 23805306
    $cacheRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '.cache/bootstrap'))
    [IO.Directory]::CreateDirectory($cacheRoot) | Out-Null
    $archive = Join-Path $cacheRoot "uv-$version.zip"
    if (-not (Test-Path -LiteralPath $archive)) {
        $partial = Join-Path $cacheRoot ("uv-$version-" + [guid]::NewGuid().ToString() + '.partial')
        try {
            Write-Host "uv $version を公式配布元から取得します（$archiveBytes bytes）"
            Invoke-WebRequest -Uri "https://github.com/astral-sh/uv/releases/download/$version/uv-x86_64-pc-windows-msvc.zip" -OutFile $partial -TimeoutSec 120
            if ((Get-Item -LiteralPath $partial).Length -ne $archiveBytes -or (Get-FileHash -LiteralPath $partial -Algorithm SHA256).Hash.ToLowerInvariant() -ne $archiveHash) { throw 'uv配布物の容量またはSHA256が一致しません。実行しません。' }
            Move-Item -LiteralPath $partial -Destination $archive
        } finally {
            if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
        }
    }
    if ((Get-Item -LiteralPath $archive).Length -ne $archiveBytes -or (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $archiveHash) { throw 'キャッシュ内のuv配布物が不正です。専用キャッシュのuv ZIPを退避して再試行してください。' }
    $target = Join-Path $cacheRoot "uv-$version.exe"
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $entries = @($zip.Entries | Where-Object { $_.Name -eq 'uv.exe' })
        if ($entries.Count -ne 1 -or $entries[0].Length -le 0 -or $entries[0].Length -gt 134217728) { throw 'uv ZIPの実行ファイル構成が不正です。' }
        $entry = $entries[0]
        $stream = $entry.Open()
        try { $exeHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant() } finally { $stream.Dispose() }
        if (Test-Path -LiteralPath $target) {
            if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $exeHash) { throw '専用uv実行ファイルが公式ZIPと一致しません。実行しません。' }
        } else {
            $stagedExe = Join-Path $cacheRoot ('uv-' + [guid]::NewGuid().ToString() + '.partial')
            try {
                # ZIP内の相対パスを使用せず、固定した専用ファイルへ展開する。
                $source = $entry.Open()
                $destination = [IO.File]::Open($stagedExe, [IO.FileMode]::CreateNew)
                try { $source.CopyTo($destination); $destination.Flush($true) } finally { $destination.Dispose(); $source.Dispose() }
                if ((Get-FileHash -LiteralPath $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $exeHash) { throw 'uv展開後のSHA256が一致しません。' }
                Move-Item -LiteralPath $stagedExe -Destination $target
            } finally {
                if (Test-Path -LiteralPath $stagedExe) { Remove-Item -LiteralPath $stagedExe -Force }
            }
        }
    } finally { $zip.Dispose() }
    return $target
}

$uvPath = Get-VoiceExtractionUv
if ($BootstrapOnly) { & $uvPath --version; if ($LASTEXITCODE -ne 0) { throw 'uv起動確認に失敗しました。' }; return }
$venvPath = Join-Path $PSScriptRoot '.venv'
if (-not (Test-Path (Join-Path $venvPath 'Scripts/python.exe'))) {
    & $uvPath venv --python 3.11 --managed-python $venvPath
    if ($LASTEXITCODE -ne 0) { throw '専用Python環境の作成に失敗しました。uvの導入状況を確認してください。' }
}
$pythonPath = Join-Path $venvPath 'Scripts/python.exe'
& $uvPath pip install --python $pythonPath 'torch==2.5.1' 'torchaudio==2.5.1' --index-url "https://download.pytorch.org/whl/$TorchVariant"
if ($LASTEXITCODE -ne 0) { throw 'PyTorch導入失敗。導入完了とは扱いません。再実行できます。' }
& $uvPath pip install --python $pythonPath -r (Join-Path $PSScriptRoot 'requirements.txt') -c (Join-Path $PSScriptRoot 'constraints.txt')
if ($LASTEXITCODE -ne 0) { throw '依存関係導入失敗。導入完了とは扱いません。再実行できます。' }
& $pythonPath -c "import torch, torchaudio, df, demucs; print(torch.__version__, torchaudio.__version__)"
if ($LASTEXITCODE -ne 0) { throw '導入後のimport検証に失敗しました。' }
$installedBytes = (Get-ChildItem -LiteralPath $venvPath -File -Recurse | Measure-Object -Property Length -Sum).Sum
@{ installed_at = (Get-Date).ToUniversalTime().ToString('o'); torch_variant = $TorchVariant; python = $pythonPath; installed_bytes = $installedBytes; state = 'installed' } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'runtime-install.json') -Encoding utf8
Write-Output "専用環境導入済み: $installedBytes bytes"
