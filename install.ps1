# Install the Windows executable for the SSH account, without administrator rights.
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'manymux'),
    [string]$Version = $env:MM_VERSION
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or [Environment]::OSVersion.Version.Build -lt 17763 -or -not [Environment]::Is64BitOperatingSystem) {
    throw 'manymux requires 64-bit Windows 10 version 1809 or newer.'
}
if ($Version -and $Version -notmatch '^(nightly|v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?)$') {
    throw 'MM_VERSION must be a release tag such as v0.3.0, or nightly.'
}

$asset = 'mm-windows-x86_64.exe'
$releases = 'https://github.com/rayfish/manymux/releases'
$channels = if ($Version) { @("$releases/download/$Version") } else { @("$releases/latest/download", "$releases/download/nightly") }
[IO.Directory]::CreateDirectory($InstallDir) | Out-Null
$staged = Join-Path $InstallDir ('.mm-' + [Guid]::NewGuid().ToString('N') + '.exe')
$sidecar = $staged + '.sha256'
$target = Join-Path $InstallDir 'mm.exe'
try {
    $downloaded = $false
    foreach ($base in $channels) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $staged
            $downloaded = $true
            break
        } catch {
            # An absent release asset may fall back to nightly. Authentication,
            # network and verification failures must not select another build.
            if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -eq 404) { continue }
            throw
        }
    }
    if (-not $downloaded) { throw "No published $asset was found." }
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile $sidecar
    $checksum = [IO.File]::ReadAllText($sidecar).Trim()
    if ($checksum -notmatch '^([a-fA-F0-9]{64})\s+\*?mm-windows-x86_64\.exe$') {
        throw 'The release checksum file is invalid.'
    }
    $expected = $Matches[1]
    $actual = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
    if ($actual -ne $expected) { throw 'Checksum mismatch; refusing to install mm.exe.' }
    if ([IO.File]::Exists($target)) {
        # Atomic replacement leaves a working installation intact on failure.
        if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $actual) {
            try { [IO.File]::Replace($staged, $target, $null) }
            catch { throw 'Cannot replace mm.exe. Stop the node with mm stop, then retry installation.' }
        }
    } else {
        [IO.File]::Move($staged, $target)
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $InstallDir) -join ';'), 'User')
    }
    # Current SSH sessions retain the old PATH. The agent launcher also checks
    # this exact installation directory, so the first retry can reach it.
    $env:Path = $InstallDir + ';' + $env:Path
    Write-Host "Installed mm.exe in $InstallDir"
} finally {
    foreach ($file in @($staged, $sidecar)) {
        if ([IO.File]::Exists($file)) { [IO.File]::Delete($file) }
    }
}
