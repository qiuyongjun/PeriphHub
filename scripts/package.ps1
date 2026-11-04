[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (-not $Version) {
        $match = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"'
        if (-not $match) { throw '无法从 Cargo.toml 读取版本号' }
        $Version = $match.Matches[0].Groups[1].Value
    }

    if (-not $SkipBuild) {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build --release 失败' }
    }

    $exe = Join-Path $root 'target\release\PeriphHub.exe'
    if (-not (Test-Path $exe)) { throw "找不到构建产物：$exe" }

    $name = "PeriphHub-v$Version-win-x64"
    $dist = Join-Path $root 'dist'
    $stage = Join-Path $dist $name
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Path (Join-Path $stage 'docs') -Force | Out-Null

    Copy-Item $exe $stage
    Copy-Item 'README.md', 'LICENSE' $stage
    Copy-Item 'docs\RUN.md' (Join-Path $stage 'docs')

    $zip = Join-Path $dist "$name.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip

    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumFile = Join-Path $dist 'SHA256SUMS.txt'
    "$hash  $name.zip" | Set-Content -Path $sumFile -Encoding ascii
    Remove-Item -Recurse -Force $stage

    Write-Host "打包完成：$zip"
    Write-Host "SHA256：$hash"
}
finally {
    Pop-Location
}
