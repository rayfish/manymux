# PowerShell must not decode the protocol as text. The child inherits the SSH
# handles directly, without entering a PowerShell pipeline.
$ErrorActionPreference = 'Stop'
try {
    $command = Get-Command mm.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($command) {
        $binary = $command.Source
    } else {
        $binary = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'manymux\mm.exe'
    }
    if (-not [IO.File]::Exists($binary)) { exit 127 }
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
