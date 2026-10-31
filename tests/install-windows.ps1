# Run without a network dependency or a real installation.
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ('mm-install-' + [Guid]::NewGuid().ToString('N'))
$originalPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$processPath = $env:Path
$global:mmInstallerTest = @{ Corrupt = $false; Downloads = @() }
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [string]$Uri, [string]$OutFile)
    $global:mmInstallerTest.Downloads += $Uri
    if ($Uri.EndsWith('.sha256')) {
        $exe = $OutFile.Substring(0, $OutFile.Length - '.sha256'.Length)
        $digest = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
        if ($global:mmInstallerTest.Corrupt) { $digest = '0' * 64 }
        [IO.File]::WriteAllText($OutFile, "$digest  mm-windows-x86_64.exe`n")
    } else {
        [IO.File]::WriteAllBytes($OutFile, [byte[]](1, 2, 3, 4))
    }
}
function Assert($condition, $message) { if (-not $condition) { throw $message } }
try {
    $destination = Join-Path $root 'directory with spaces'
    & "$PSScriptRoot/../install.ps1" -InstallDir $destination -Version v0.3.0
    $target = Join-Path $destination 'mm.exe'
    Assert ([IO.File]::Exists($target)) 'the installer must create mm.exe'
    Assert (([IO.File]::ReadAllBytes($target) -join ',') -eq '1,2,3,4') 'the verified bytes must be installed'
    Assert ($global:mmInstallerTest.Downloads.Count -eq 2) 'the installer must download an executable and checksum'
    Assert (($global:mmInstallerTest.Downloads | Where-Object { $_ -notmatch '/download/v0.3.0/' }).Count -eq 0) 'a pinned release must stay pinned'
    # A concurrent bootstrap or a second install must not replace a running
    # executable when it already contains the exact verified release.
    & "$PSScriptRoot/../install.ps1" -InstallDir $destination -Version v0.3.0
    $entries = [Environment]::GetEnvironmentVariable('Path', 'User') -split ';'
    Assert (@($entries | Where-Object { $_ -eq $destination }).Count -eq 1) 'PATH must not gain duplicates'
    $global:mmInstallerTest.Corrupt = $true
    $rejected = $false
    try { & "$PSScriptRoot/../install.ps1" -InstallDir $destination -Version v0.3.0 }
    catch { $rejected = $_.Exception.Message -match 'Checksum mismatch' }
    Assert $rejected 'a bad checksum must fail'
    Assert (([IO.File]::ReadAllBytes($target) -join ',') -eq '1,2,3,4') 'verification failure must preserve the installed executable'
    Assert (@(Get-ChildItem -LiteralPath $destination -Force).Count -eq 1) 'temporary downloads must be removed'
    $before = $global:mmInstallerTest.Downloads.Count
    $rejected = $false
    try { & "$PSScriptRoot/../install.ps1" -InstallDir $destination -Version '../other' }
    catch { $rejected = $true }
    Assert $rejected 'an invalid tag must fail before downloading'
    Assert ($global:mmInstallerTest.Downloads.Count -eq $before) 'invalid tags must not reach the network'
    Write-Host 'Windows installer tests passed'
} finally {
    Remove-Variable mmInstallerTest -Scope Global
    [Environment]::SetEnvironmentVariable('Path', $originalPath, 'User')
    $env:Path = $processPath
    if ([IO.Directory]::Exists($root)) { Remove-Item -LiteralPath $root -Recurse -Force }
}
