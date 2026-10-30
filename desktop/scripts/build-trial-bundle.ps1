#Requires -Version 5.1
<#
.SYNOPSIS
    Build or preflight-check an unsigned local trial desktop bundle for llmos-task-manager (Stage E E0 / W77-PRE-C).

.DESCRIPTION
    Supports two explicit modes:
      1. -CheckOnly (or --check-only): Fast non-destructive environment and configuration gate
         verifying node (>= 20), npm, cargo, rustc, desktop/package.json, desktop/src-tauri/tauri.conf.json,
         desktop/src-tauri/Cargo.toml, and bundle icons without invoking heavy frontend/Rust builds.
      2. Full local trial build: Installs desktop frontend dependencies if needed, runs `npm run build`
         (TypeScript + Vite), and invokes `npx tauri build --no-sign` with a temporary bundle-activation
         override file so tracked repository configuration remains clean.

    Honest boundary:
      Output artifacts are strictly for local developer trial on the current workstation and are NOT
      signed or notarized with Apple Developer ID or Microsoft EV hardware-token commercial CA certificates
      (commercial release notarization remains in E-deferred / RISK-B-06).
#>
[CmdletBinding()]
param(
    [switch]$CheckOnly,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ExtraArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

foreach ($arg in $ExtraArgs) {
    if ($arg -eq "--check-only") {
        $CheckOnly = $true
    }
    elseif (-not [string]::IsNullOrWhiteSpace($arg)) {
        Write-Error "[build-trial-bundle] Unknown argument: $arg. Usage: build-trial-bundle.ps1 [-CheckOnly | --check-only]"
        exit 1
    }
}

# Honest boundary statement (decoded via Unicode escapes below so Windows PowerShell 5.1 ANSI-no-BOM
# script parsing and PowerShell 7 UTF-8 parsing both render identical non-garbled Chinese text):
# 本产物仅限本机开发试用，未经过 Apple Developer ID / Microsoft EV 硬件令牌商业 CA 公证签名，商业发行签名维持在 E-deferred / RISK-B-06 .
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$BoundaryBanner = [regex]::Unescape('\u8bda\u5b9e\u8fb9\u754c\u58f0\u660e\uff1a\u672c\u4ea7\u7269\u4ec5\u9650\u672c\u673a\u5f00\u53d1\u8bd5\u7528\uff0c\u672a\u7ecf\u8fc7 Apple Developer ID / Microsoft EV \u786c\u4ef6\u4ee4\u724c\u5546\u4e1a CA \u516c\u8bc1\u7b7e\u540d\uff0c\u5546\u4e1a\u53d1\u884c\u7b7e\u540d\u7ef4\u6301\u5728 E-deferred / RISK-B-06')

Write-Output "================================================================================"
Write-Output "[build-trial-bundle] $BoundaryBanner"
Write-Output "================================================================================"

$DesktopDir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

function Assert-CommandAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Name
    )
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -eq $cmd) {
        throw "[build-trial-bundle] Required tool '$Name' was not found in PATH."
    }
}

Assert-CommandAvailable -Name "node"
Assert-CommandAvailable -Name "npm"
Assert-CommandAvailable -Name "cargo"
Assert-CommandAvailable -Name "rustc"

$NodeVersionRaw = (& node --version).Trim()
if ($NodeVersionRaw -notmatch '^v?(?<major>\d+)\.') {
    throw "[build-trial-bundle] Unable to parse Node.js version from '$NodeVersionRaw'."
}
$NodeMajor = [int]$Matches["major"]
if ($NodeMajor -lt 20) {
    throw "[build-trial-bundle] Node.js >= 20 is required by desktop/package.json (detected $NodeVersionRaw)."
}

$NpmVersionRaw = (& npm --version).Trim()
$CargoVersionRaw = (& cargo --version).Trim()
$RustcVersionRaw = (& rustc --version).Trim()

$PackageJsonPath = Join-Path $DesktopDir "package.json"
$TauriConfPath = Join-Path $DesktopDir "src-tauri\tauri.conf.json"
$TauriCargoPath = Join-Path $DesktopDir "src-tauri\Cargo.toml"
$IconPngPath = Join-Path $DesktopDir "src-tauri\icons\icon.png"
$IconIcoPath = Join-Path $DesktopDir "src-tauri\icons\icon.ico"

foreach ($requiredFile in @($PackageJsonPath, $TauriConfPath, $TauriCargoPath, $IconPngPath, $IconIcoPath)) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "[build-trial-bundle] Missing required desktop project file: $requiredFile"
    }
}

$PackageJson = Get-Content -LiteralPath $PackageJsonPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ([string]::IsNullOrWhiteSpace($PackageJson.scripts.build)) {
    throw "[build-trial-bundle] desktop/package.json is missing scripts.build."
}
$TauriCliSpec = $PackageJson.devDependencies.'@tauri-apps/cli'
if ([string]::IsNullOrWhiteSpace($TauriCliSpec)) {
    throw "[build-trial-bundle] desktop/package.json is missing devDependencies['@tauri-apps/cli']."
}

$TauriConf = Get-Content -LiteralPath $TauriConfPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ([string]::IsNullOrWhiteSpace($TauriConf.productName) -or [string]::IsNullOrWhiteSpace($TauriConf.identifier)) {
    throw "[build-trial-bundle] desktop/src-tauri/tauri.conf.json is missing productName or identifier."
}

Write-Output "[build-trial-bundle] Environment verified:"
Write-Output "  repo_root    = $RepoRoot"
Write-Output "  desktop_dir  = $DesktopDir"
Write-Output "  product_name = $($TauriConf.productName) ($($TauriConf.version))"
Write-Output "  node         = $NodeVersionRaw"
Write-Output "  npm          = $NpmVersionRaw"
Write-Output "  cargo        = $CargoVersionRaw"
Write-Output "  rustc        = $RustcVersionRaw"
Write-Output "  tauri_cli    = $TauriCliSpec"

if ($CheckOnly) {
    Write-Output "[build-trial-bundle] CHECK_ONLY_OK (preflight passed; skipping npm build and tauri bundle)"
    Write-Output "[build-trial-bundle] $BoundaryBanner"
    exit 0
}

$TempOverrideConfig = Join-Path ([System.IO.Path]::GetTempPath()) ("llmos-trial-tauri-override-" + $PID + ".json")
$OverridePayload = '{"bundle":{"active":true,"icon":["icons/icon.png","icons/icon.ico"]}}'

Push-Location $DesktopDir
try {
    [System.IO.File]::WriteAllText($TempOverrideConfig, $OverridePayload, (New-Object System.Text.UTF8Encoding($false)))

    $NodeModulesDir = Join-Path $DesktopDir "node_modules"
    $PackageLockPath = Join-Path $DesktopDir "package-lock.json"
    if (-not (Test-Path -LiteralPath $NodeModulesDir -PathType Container)) {
        if (Test-Path -LiteralPath $PackageLockPath -PathType Leaf) {
            Write-Output "[build-trial-bundle] Installing frontend dependencies via npm ci..."
            & npm ci --no-audit --no-fund
        }
        else {
            Write-Output "[build-trial-bundle] Installing frontend dependencies via npm install..."
            & npm install --no-audit --no-fund
        }
        if ($LASTEXITCODE -ne 0) {
            throw "[build-trial-bundle] Frontend dependency installation failed with exit code $LASTEXITCODE."
        }
    }

    Write-Output "[build-trial-bundle] Running frontend TypeScript check and Vite production bundle (npm run build)..."
    & npm run build
    if ($LASTEXITCODE -ne 0) {
        throw "[build-trial-bundle] 'npm run build' failed with exit code $LASTEXITCODE."
    }

    if (((& rustc -vV) -match 'host:\s*x86_64-pc-windows-gnu') -and (Test-Path -LiteralPath "C:\msys64\ucrt64\bin\gcc.exe" -PathType Leaf)) {
        if ([string]$env:PATH -notmatch 'msys64[\\/]ucrt64[\\/]bin') {
            $env:PATH = "C:\msys64\ucrt64\bin;" + [string]$env:PATH
            Write-Output "[build-trial-bundle] Windows GNU host detected: prepended C:\msys64\ucrt64\bin to PATH for windres.exe."
        }
        if ([string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
            $env:RUSTFLAGS = "-C linker=C:/msys64/ucrt64/bin/gcc.exe -C link-self-contained=no"
            Write-Output "[build-trial-bundle] Windows GNU host detected: injected RUSTFLAGS for UCRT64 gcc host build-script linking."
        }
    }

    Write-Output "[build-trial-bundle] Building local unsigned trial desktop bundle (npx tauri build --no-sign)..."
    & npx tauri build --no-sign --config $TempOverrideConfig
    if ($LASTEXITCODE -ne 0) {
        throw "[build-trial-bundle] 'npx tauri build --no-sign' failed with exit code $LASTEXITCODE."
    }

    $BundleOutDir = Join-Path $DesktopDir "src-tauri\target\release\bundle"
    Write-Output "================================================================================"
    Write-Output "[build-trial-bundle] TRIAL_BUNDLE_OK output_dir=$BundleOutDir"
    Write-Output "[build-trial-bundle] $BoundaryBanner"
    Write-Output "================================================================================"
}
finally {
    Pop-Location
    if (Test-Path -LiteralPath $TempOverrideConfig) {
        Remove-Item -LiteralPath $TempOverrideConfig -Force -ErrorAction SilentlyContinue
    }
}
