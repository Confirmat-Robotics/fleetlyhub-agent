# Run from any directory; keep user credentials outside the downloaded ZIP.
$ErrorActionPreference = 'Stop'
$stateArguments = @()
if (-not ($args | Where-Object { $_ -match '^--state-dir(?:=|$)' })) {
    $userData = [Environment]::GetFolderPath('LocalApplicationData')
    if (-not $userData) { throw 'Cannot locate this Windows user''s local application data.' }
    $stateArguments = @('--state-dir', (Join-Path $userData 'Fleetly Hub\Agent'))
}
$previousPath = $env:PATH
try {
    # The Agent's default media command resolves the bundled executable here.
    $env:PATH = "$PSScriptRoot;$previousPath"
    & "$PSScriptRoot\fleetlyhub-agent.exe" @stateArguments @args
    $agentExitCode = $LASTEXITCODE
} finally {
    $env:PATH = $previousPath
}
exit $agentExitCode
