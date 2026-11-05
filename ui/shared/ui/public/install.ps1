# Nandatown CLI installer for Windows, served by every Nandatown server at /install.ps1:
#
#   irm https://<your-nandatown-server>/install.ps1 | iex
#
# A copy of scripts/install.ps1 that installs the same CLI build under the name `nandatown`.
#
# Environment overrides:
#   NANDATOWN_VERSION      version tag to install (default: latest)
#   NANDATOWN_INSTALL_DIR  install destination (default: %LOCALAPPDATA%\Programs\nandatown)
#   NANDATOWN_REGISTRY     registry host the build is pulled from
#
# No administrator rights, anonymous pulls, the binary verified against its content-addressed digest. It adds the
# install folder to the *user* PATH; nothing machine-wide changes.

$ErrorActionPreference = 'Stop'

$Registry = if ($env:NANDATOWN_REGISTRY) { $env:NANDATOWN_REGISTRY } else { 'registry.nasiko.dev' }
$Version = if ($env:NANDATOWN_VERSION) { $env:NANDATOWN_VERSION } else { 'latest' }
$Dir = if ($env:NANDATOWN_INSTALL_DIR) { $env:NANDATOWN_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\nandatown' }
$Api = "https://$Registry/v2/nasiko/nasiko"

function Fail($Message) {
    Write-Error "install: error: $Message"
    exit 1
}

# --- 1. platform: only windows/amd64 is built ---------------------------------
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'amd64' }
    'ARM64' { 'amd64' }  # runs under the x64 emulation Windows on ARM includes
    default { Fail "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
Write-Host "→ installing nandatown $Version for windows/$Arch"

# --- 2. resolve the multi-platform index to this platform's manifest ----------
try {
    $Index = Invoke-RestMethod -Uri "$Api/manifests/$Version" `
        -Headers @{ Accept = 'application/vnd.oci.image.index.v1+json' }
} catch { Fail "cannot fetch the CLI build $Version from $Registry" }
$Child = ($Index.manifests | Where-Object { $_.platform.os -eq 'windows' -and $_.platform.architecture -eq $Arch } |
    Select-Object -First 1).digest
if (-not $Child) { Fail "release $Version has no windows/$Arch build" }

# --- 3. resolve the child manifest to its single binary layer ------------------
try {
    $Manifest = Invoke-RestMethod -Uri "$Api/manifests/$Child" `
        -Headers @{ Accept = 'application/vnd.oci.image.manifest.v1+json' }
} catch { Fail "cannot fetch manifest $Child" }
$Layer = $Manifest.layers[0].digest
if (-not $Layer) { Fail "manifest $Child has no layers" }

# --- 4. download and verify against the content-addressed digest ---------------
$Tmp = New-TemporaryFile
try {
    Write-Host "→ downloading $Layer"
    Invoke-WebRequest -Uri "$Api/blobs/$Layer" -OutFile $Tmp -UseBasicParsing
    $Got = 'sha256:' + (Get-FileHash -Algorithm SHA256 $Tmp).Hash.ToLower()
    if ($Got -ne $Layer) { Fail "digest mismatch: expected $Layer, got $Got" }

    # --- 5. install (per-user, no administrator) --------------------------------
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $Exe = Join-Path $Dir 'nandatown.exe'
    Move-Item -Force $Tmp $Exe
} finally {
    if (Test-Path $Tmp) { Remove-Item -Force $Tmp }
}

# --- 6. user PATH, verify, hand off ---------------------------------------------
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($UserPath -split ';') -contains $Dir)) {
    $NewPath = if ($UserPath) { "$UserPath;$Dir" } else { $Dir }
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    Write-Host "→ added $Dir to your user PATH (open a new terminal to use it)"
}
# The binary names itself `nasiko <version>`; only the version is ours to show.
Write-Host "✓ installed nandatown $((& $Exe --version) -replace '^\S+\s+','') to $Exe"
