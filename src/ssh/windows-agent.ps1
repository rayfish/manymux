# PowerShell must not decode the protocol as text. The child inherits the SSH
# handles directly, without entering a PowerShell pipeline.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try {
    $command = Get-Command mm.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($command) {
        $binary = $command.Source
    } else {
        $binary = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'manymux\mm.exe'
    }
    $exists = [IO.File]::Exists($binary)
    # A PowerShell SSH default shell can turn native exit 127 into exit 1.
    # Discovery reports availability on stdout before any request is sent.
    if ($manymuxProbe) {
        if ($exists) { [Console]::Write('manymux-windows') }
        else { [Console]::Write('manymux-windows-missing') }
        exit 0
    }
    if (-not $exists) { exit 127 }
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $binary
    $start.Arguments = 'agent'
    $start.UseShellExecute = $false
    $process = [Diagnostics.Process]::Start($start)
    $process.WaitForExit()
    $code = $process.ExitCode
    $process.Dispose()
    # Only the missing-file branch can authorise retrying a request.
    if ($code -eq 127) { exit 1 }
    exit $code
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
