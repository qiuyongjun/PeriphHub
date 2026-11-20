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

    # 可选：编译 Inno Setup 安装包（检测到 ISCC.exe 才执行）
    $isccCandidates = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    $iscc = $isccCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($iscc) {
        # ISCC.exe 自身不带版本资源，按卸载注册表判断版本。语言文件是 6.5+
        # 翻译、x64compatible 需要 6.3+，旧版 6.x 会编译失败，提前跳过。
        $regPaths = @(
            'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1',
            'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1',
            'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1'
        )
        $isccVersion = $regPaths |
            ForEach-Object { (Get-ItemProperty $_ -ErrorAction SilentlyContinue).DisplayVersion } |
            Where-Object { $_ } | Select-Object -First 1
        if ($isccVersion -and [version]$isccVersion -lt [version]'6.5') {
            Write-Warning "Inno Setup $isccVersion 过旧（需要 6.5+），跳过安装包编译，仅生成便携版 zip"
            $iscc = $null
        }
    }
    $setupExe = $null
    if ($iscc) {
        Write-Host "使用 ISCC 编译安装包：$iscc"
        & $iscc "/DAppVersion=$Version" (Join-Path $root 'installer\periphhub.iss')
        if ($LASTEXITCODE -ne 0) { throw 'Inno Setup 编译失败' }
        $setupExe = Join-Path $dist "PeriphHub-v$Version-win-x64-setup.exe"
        if (-not (Test-Path $setupExe)) { throw "安装包未生成：$setupExe" }
    }
    else {
        Write-Warning '未找到可用的 Inno Setup 6.5+ (ISCC.exe)，跳过安装包编译，仅生成便携版 zip'
    }

    $sumLines = @()
    $sumLines += "{0}  {1}.zip" -f (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant(), $name
    if ($setupExe) {
        $sumLines += "{0}  {1}" -f (Get-FileHash $setupExe -Algorithm SHA256).Hash.ToLowerInvariant(), (Split-Path -Leaf $setupExe)
    }
    $sumFile = Join-Path $dist 'SHA256SUMS.txt'
    $sumLines | Set-Content -Path $sumFile -Encoding ascii
    Remove-Item -Recurse -Force $stage

    Write-Host "打包完成：$zip"
    if ($setupExe) { Write-Host "安装包：$setupExe" }
    Write-Host "SHA256：$($sumLines -join '; ')"
}
finally {
    Pop-Location
}
