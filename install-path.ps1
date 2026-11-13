# Install peek.exe into the user's local programs folder and put that folder
# on the user PATH (idempotent: safe to run again after a rebuild).
$ErrorActionPreference = 'Stop'

$source = 'D:\text-editor\target\release\peek.exe'
if (-not (Test-Path $source)) { throw "missing build output: $source" }

$dest = Join-Path $env:LOCALAPPDATA 'Programs\peek'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item -Path $source -Destination (Join-Path $dest 'peek.exe') -Force
Write-Output "installed: $dest\peek.exe"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($null -eq $userPath) { $userPath = '' }

$parts = @($userPath -split ';' | Where-Object { $_ -ne '' })
$already = $parts | Where-Object { $_.TrimEnd('\') -ieq $dest.TrimEnd('\') }

if ($already) {
    Write-Output 'path: already present'
} else {
    $updated = (@($parts) + $dest) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    Write-Output 'path: added to user PATH'
}

$check = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($check -split ';' | Where-Object { $_.TrimEnd('\') -ieq $dest.TrimEnd('\') }) {
    Write-Output 'verify: entry present in user PATH'
} else {
    throw 'verify: entry missing from user PATH'
}
