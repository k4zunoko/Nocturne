param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$TuiArgs
)

$ErrorActionPreference = 'Stop'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$workspaceRoot = Split-Path -Parent $scriptDir

Push-Location $workspaceRoot
try {
    cargo build -p nocturne-backend
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to build nocturne-backend."
    }

    cargo run -p nocturne-tui -- @TuiArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to launch nocturne-tui."
    }
}
finally {
    Pop-Location
}
