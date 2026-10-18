#Requires -Version 7.0
# Start/Install共通。SpkRouterのCargo操作だけを扱い、配備・設定・起動は行わない。
function Invoke-SpkRouterCargo {
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot,
        [Parameter(Mandatory)]
        [ValidateSet('TargetDirectory', 'DebugBuild', 'ReleaseBuild')]
        [string]$Operation
    )

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargoが見つかりません。Rustをインストールしてください。'
    }
    $backendRoot = $ProjectRoot
    $manifest = Join-Path $backendRoot 'Cargo.toml'
    # backend側の.cargo設定も含め、metadataとbuildを同じ作業フォルダーで実行する。
    Push-Location -LiteralPath $backendRoot
    try {
        if ($Operation -eq 'TargetDirectory') {
            $metadataJson = & cargo metadata --manifest-path $manifest --format-version 1 --no-deps
            if ($LASTEXITCODE -ne 0) {
                throw "Cargoの出力先取得に失敗しました。終了コード: $LASTEXITCODE"
            }
            $metadata = ($metadataJson -join [Environment]::NewLine) | ConvertFrom-Json
            return [string]$metadata.target_directory
        }

        $buildArguments = @('build', '--manifest-path', $manifest)
        $configurationName = 'デバッグ'
        if ($Operation -eq 'ReleaseBuild') {
            $buildArguments += '--release'
            $configurationName = 'リリース'
        }
        $buildArguments += @('--bin', 'voicemixer-spk')
        # Cargoの標準出力を関数の戻り値へ混ぜない。ビルドログは画面へ流す。
        & cargo @buildArguments | Out-Host
        if ($LASTEXITCODE -ne 0) {
            throw "VoiceMixerSpkRouterの${configurationName}ビルドに失敗しました。終了コード: $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}
