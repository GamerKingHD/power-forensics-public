#Requires -Version 5.1
<#
.SYNOPSIS
    Authoritative Windows release build for power-forensics.

.DESCRIPTION
    Fails closed. Runs every pre-release gate, builds the Rust engine, the
    privileged helper and the Tauri GUI, optionally Authenticode-signs the
    binaries and installer, assembles the portable ZIP, and writes the final
    hashes, release manifest and SHA256SUMS into dist-release/v<version>/.

    Signing is opt-in through environment variables; without a certificate the
    build is produced and reported as UNSIGNED BUILD.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/release-windows.ps1
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$SkipTests,
    [switch]$SkipSign,
    [switch]$AllowDirty
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$GuiDir = Join-Path $RepoRoot 'pf-gui'
$Product = 'power-forensics'

function Write-Header([string]$Text) { Write-Host "`n=== $Text ===" -ForegroundColor Cyan }
function Fail([string]$Message) { Write-Host "RELEASE FAILED: $Message" -ForegroundColor Red; exit 1 }

function Invoke-Step {
    param([string]$Label, [string]$Exe, [string[]]$Arguments, [string]$WorkDir = $RepoRoot)
    Write-Header $Label
    Push-Location $WorkDir
    try {
        & $Exe @Arguments
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($code -ne 0) { Fail "$Label (exit $code)" }
}

function Get-SignTool {
    if ($env:PF_SIGNTOOL -and (Test-Path -LiteralPath $env:PF_SIGNTOOL)) { return $env:PF_SIGNTOOL }
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $roots = @("${env:ProgramFiles(x86)}\Windows Kits\10\bin", "${env:ProgramFiles}\Windows Kits\10\bin")
    foreach ($root in $roots) {
        if (Test-Path -LiteralPath $root) {
            $found = Get-ChildItem -Path $root -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\x64\\' } |
                Sort-Object FullName -Descending | Select-Object -First 1
            if ($found) { return $found.FullName }
        }
    }
    return $null
}

function Invoke-Sign {
    param([string]$Path, [string]$SignTool)
    $ts = if ($env:PF_SIGN_TIMESTAMP_URL) { $env:PF_SIGN_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }
    $args = @('sign', '/fd', 'sha256', '/td', 'sha256', '/tr', $ts)
    if ($env:PF_SIGN_CERT) {
        $args += @('/f', $env:PF_SIGN_CERT)
        if ($env:PF_SIGN_CERT_PASSWORD) { $args += @('/p', $env:PF_SIGN_CERT_PASSWORD) }
    } elseif ($env:PF_SIGN_CERT_SHA1) {
        $args += @('/sha1', $env:PF_SIGN_CERT_SHA1)
    } else {
        throw 'no signing certificate configured'
    }
    Write-Host "signing $(Split-Path -Leaf $Path)"
    & $SignTool @args $Path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed for $Path (exit $LASTEXITCODE)" }
}

function Assert-Signature {
    param([string]$Path)
    $sig = Get-AuthenticodeSignature -FilePath $Path
    if ($sig.Status -ne 'Valid') { throw "signature is not valid for ${Path}: $($sig.Status)" }
    if ($env:PF_SIGN_PUBLISHER) {
        $subject = $sig.SignerCertificate.Subject
        if ($subject -notlike "*$($env:PF_SIGN_PUBLISHER)*") {
            throw "signer '$subject' does not match PF_SIGN_PUBLISHER"
        }
    }
    if (-not $sig.TimeStamperCertificate) { throw "RFC3161 timestamp is missing for $Path" }
    return $sig
}

# --------------------------------------------------------------------------
if ($env:OS -ne 'Windows_NT') { Fail 'this release build targets Windows x86-64' }

$tauriConf = Get-Content -Raw (Join-Path $GuiDir 'src-tauri\tauri.conf.json') | ConvertFrom-Json
if (-not $Version) { $Version = $tauriConf.version }
if ($Version -ne $tauriConf.version) { Fail "requested version $Version != tauri.conf.json $($tauriConf.version)" }

$DistDir = Join-Path $RepoRoot "dist-release\v$Version"
$SetupName = "$Product-$Version-x64-setup.exe"
$PortableName = "$Product-$Version-x64-portable.zip"

Write-Header "power-forensics release $Version"

# --- Git provenance -------------------------------------------------------
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot '.git'))) {
    Fail 'no Git repository; a release must be traceable to an exact commit'
}
$gitCommit = (& git -C $RepoRoot rev-parse HEAD 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $gitCommit) { Fail 'cannot resolve HEAD commit' }
$gitTag = (& git -C $RepoRoot describe --tags --exact-match HEAD 2>$null)
if ($LASTEXITCODE -ne 0) { $gitTag = '' }
$dirty = & git -C $RepoRoot status --porcelain
if ($dirty -and -not $AllowDirty) { Fail 'working tree is dirty; commit before releasing' }
Write-Host "commit: $gitCommit"
Write-Host "tag:    $(if ($gitTag) { $gitTag } else { '<none>' })"

# --- Staged sidecars (build output required by the Tauri build) -----------
Invoke-Step 'Stage external binaries (sidecars)' 'npm' @('run', 'sidecar') $GuiDir

# --- Pre-release quality gates -------------------------------------------
if (-not $SkipTests) {
    Invoke-Step 'cargo fmt --check' 'cargo' @('fmt', '--all', '--', '--check')
    Invoke-Step 'cargo clippy -D warnings' 'cargo' @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    Invoke-Step 'cargo test' 'cargo' @('test', '--workspace', '--locked')
    Invoke-Step 'npm audit (high/critical)' 'npm' @('audit', '--audit-level=high') $GuiDir
    Invoke-Step 'npm run typecheck' 'npm' @('run', 'typecheck') $GuiDir
    Invoke-Step 'npm test' 'npm' @('test') $GuiDir
    Invoke-Step 'npm run build' 'npm' @('run', 'build') $GuiDir
    Invoke-Step 'npm run release:check' 'npm' @('run', 'release:check') $GuiDir
} else {
    Write-Host 'WARNING: quality gates skipped (-SkipTests)' -ForegroundColor Yellow
}

# --- Build ----------------------------------------------------------------
Invoke-Step 'cargo build --release' 'cargo' @('build', '--release', '--locked', '--workspace')
Invoke-Step 'tauri build (NSIS installer)' 'npm' @('run', 'tauri', '--', 'build') $GuiDir

$guiExe = Join-Path $RepoRoot 'target\release\power-forensics-gui.exe'
$sidecarExe = Join-Path $RepoRoot 'target\release\power-forensics.exe'
$elevatedExe = Join-Path $RepoRoot 'target\release\pf-elevated.exe'
foreach ($f in @($guiExe, $sidecarExe, $elevatedExe)) {
    if (-not (Test-Path -LiteralPath $f)) { Fail "missing build output: $f" }
}

$installer = Get-ChildItem -Path (Join-Path $RepoRoot 'target\release\bundle\nsis') -Filter '*setup.exe' |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $installer) { Fail 'NSIS installer was not produced' }
Write-Host "installer: $($installer.FullName)"

# --- Signing --------------------------------------------------------------
$signingRequested = (-not $SkipSign) -and (($env:PF_SIGN_CERT) -or ($env:PF_SIGN_CERT_SHA1))
if ($env:PF_SIGN_REQUIRED -eq '1' -and -not $signingRequested) {
    Fail 'PF_SIGN_REQUIRED=1 but no signing certificate is configured'
}
$signed = $false
$signInfo = $null
if ($signingRequested) {
    $signTool = Get-SignTool
    if (-not $signTool) { Fail 'signtool.exe not found; install the Windows SDK or set PF_SIGNTOOL' }
    Write-Header 'Authenticode signing'
    foreach ($f in @($guiExe, $sidecarExe, $elevatedExe, $installer.FullName)) { Invoke-Sign -Path $f -SignTool $signTool }
    Write-Header 'Signature verification'
    $verified = @{}
    foreach ($f in @($guiExe, $sidecarExe, $elevatedExe, $installer.FullName)) {
        $sig = Assert-Signature -Path $f
        $verified[(Split-Path -Leaf $f)] = $sig
    }
    $signed = $true
    $signInfo = [ordered]@{
        subject    = $verified[(Split-Path -Leaf $guiExe)].SignerCertificate.Subject
        thumbprint = $verified[(Split-Path -Leaf $guiExe)].SignerCertificate.Thumbprint
        timestamp  = $verified[(Split-Path -Leaf $guiExe)].TimeStamperCertificate.NotBefore.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    }
} else {
    Write-Host 'UNSIGNED BUILD: no code-signing certificate configured' -ForegroundColor Yellow
}

# --- Portable package -----------------------------------------------------
Write-Header 'Assemble portable package'
$staging = Join-Path $env:TEMP "pf-portable-$([guid]::NewGuid().ToString('N'))"
$portableRoot = Join-Path $staging "$Product-$Version-x64"
New-Item -ItemType Directory -Path $portableRoot -Force | Out-Null
Copy-Item $guiExe (Join-Path $portableRoot 'power-forensics-gui.exe')
Copy-Item $sidecarExe (Join-Path $portableRoot 'power-forensics.exe')
Copy-Item $elevatedExe (Join-Path $portableRoot 'pf-elevated.exe')
Copy-Item (Join-Path $RepoRoot 'README.md') $portableRoot
Copy-Item (Join-Path $RepoRoot 'LICENSE') $portableRoot
Copy-Item (Join-Path $RepoRoot 'NOTICE') $portableRoot
Copy-Item (Join-Path $RepoRoot 'PRIVACY.md') $portableRoot
Copy-Item (Join-Path $RepoRoot 'THIRD_PARTY_NOTICES.txt') $portableRoot
Set-Content -LiteralPath (Join-Path $portableRoot 'VERSION') -Value $Version -NoNewline
Set-Content -LiteralPath (Join-Path $portableRoot "$Product.portable") -Value '' -NoNewline
@"
power-forensics $Version - portable

Run power-forensics-gui.exe. No installation is required.
This package stores recordings, settings and logs in a "sessions" directory
next to this file (portable mode is enabled by the power-forensics.portable
marker). Extract to a writable folder; do not run from a read-only location.
See README.md for usage, NOTICE for project attribution, and
THIRD_PARTY_NOTICES.txt for redistributed licenses.
"@ | Set-Content -LiteralPath (Join-Path $portableRoot 'README-PORTABLE.txt')

# --- Release directory ----------------------------------------------------
if (Test-Path -LiteralPath $DistDir) { Remove-Item -LiteralPath $DistDir -Recurse -Force }
New-Item -ItemType Directory -Path $DistDir -Force | Out-Null

Copy-Item $installer.FullName (Join-Path $DistDir $SetupName)

$portableZip = Join-Path $DistDir $PortableName
Compress-Archive -Path $portableRoot -DestinationPath $portableZip -CompressionLevel Optimal
Remove-Item -LiteralPath $staging -Recurse -Force

# --- Hashes, manifest, SHA256SUMS -----------------------------------------
Write-Header 'Generate hashes and release manifest'
$artifactFiles = @(
    (Join-Path $DistDir $SetupName),
    (Join-Path $DistDir $PortableName)
)
$artifacts = @()
foreach ($f in $artifactFiles) {
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower()
    $artifacts += [ordered]@{
        name   = (Split-Path -Leaf $f)
        sha256 = $hash
        bytes  = (Get-Item -LiteralPath $f).Length
    }
}

$rustcVersion = (& rustc --version)
$nodeVersion = (& node --version)
$manifest = [ordered]@{
    schemaVersion = 1
    product       = $Product
    version       = $Version
    platform      = 'windows'
    arch          = 'x86_64'
    license       = 'Apache-2.0'
    attribution   = 'GamerKingHD'
    gitCommit     = $gitCommit
    gitTag        = $(if ($gitTag) { $gitTag } else { $null })
    rustToolchain = $rustcVersion
    nodeVersion   = $nodeVersion
    signed        = $signed
    signer        = $signInfo
    generatedAt   = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    artifacts     = $artifacts
}
$manifestPath = Join-Path $DistDir 'release-manifest.json'
$manifestJson = $manifest | ConvertTo-Json -Depth 8
[System.IO.File]::WriteAllText($manifestPath, $manifestJson, (New-Object System.Text.UTF8Encoding($false)))

$sumLines = @()
foreach ($f in @($artifactFiles + $manifestPath)) {
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower()
    $sumLines += "$hash  $(Split-Path -Leaf $f)"
}
$sumPath = Join-Path $DistDir 'SHA256SUMS.txt'
Set-Content -LiteralPath $sumPath -Value ($sumLines -join "`n") -Encoding ASCII

# --- Final report ---------------------------------------------------------
Write-Header 'Release artifacts'
Get-ChildItem -LiteralPath $DistDir | ForEach-Object { Write-Host ("  {0,-48} {1,12:N0} bytes" -f $_.Name, $_.Length) }
Write-Host "`nSigning: $(if ($signed) { 'SIGNED' } else { 'UNSIGNED BUILD' })" -ForegroundColor $(if ($signed) { 'Green' } else { 'Yellow' })
Write-Host "Output:  $DistDir"
Write-Host 'Release build complete.'
