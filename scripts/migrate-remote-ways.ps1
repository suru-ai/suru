# Rewrites the Remotes in remotes.json written before a Remote was reached by
# ways (#486, #493) into the shape the current build reads. A Server that finds
# an old Remote there refuses to start with
#   read Pairing records "...\remotes.json": missing field `ways`
#
#   addresses: ["1.2.3.4:7777", ...]  -> ways: [{"direct": "1.2.3.4:7777"}, ...]
#   last_good_address: "1.2.3.4:7777" -> answered: [{"direct": "1.2.3.4:7777"}]
#   last_answered: {...}              -> answered: [{...}]
#
# Only Remotes still in an old shape change, so the script is idempotent: a
# second run migrates nothing. Runs on Windows PowerShell 5.1 and PowerShell 7.
# Stop the Suru server for the Channel first: a running server holds its
# Remotes in memory and would write its own copy back.
#
# Usage: migrate-remote-ways.ps1 [-DryRun] [-Channel NAME | -File PATH]
#   -DryRun   Report what would change without writing anything.
#   -Channel  The Channel whose remotes.json to migrate (default:
#             $env:SURU_CHANNEL, else release). Honors SURU_DATA_DIR and
#             SURU_STATE_DIR.
#   -File     Migrate this remotes.json instead of a Channel's.

[CmdletBinding()]
param(
    [switch]$DryRun,
    [string]$Channel = $(if ($env:SURU_CHANNEL) { $env:SURU_CHANNEL } else { 'release' }),
    [string]$File
)

$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    Write-Error "migrate-remote-ways: $Message" -ErrorAction Continue
    exit 1
}

# Mirrors the roots main.rs resolves through the dirs crate: on Windows both
# data and state live under %LOCALAPPDATA%, and a non-release Channel lives in
# a subdirectory of each.
$dataBase = if ($env:SURU_DATA_DIR) { $env:SURU_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'suru' }
$stateBase = if ($env:SURU_STATE_DIR) { $env:SURU_STATE_DIR } else { Join-Path $env:LOCALAPPDATA 'suru' }
function Get-ChannelRoot([string]$Base) {
    if ($Channel -eq 'release') { $Base } else { Join-Path $Base $Channel }
}

if (-not $File) {
    $File = Join-Path (Get-ChannelRoot $dataBase) 'remotes.json'
    $runtime = Join-Path (Get-ChannelRoot $stateBase) 'runtime.json'
    if (-not $DryRun -and (Test-Path -LiteralPath $runtime)) {
        $serverPid = (Get-Content -LiteralPath $runtime -Raw | ConvertFrom-Json).pid
        if ($serverPid -and (Get-Process -Id $serverPid -ErrorAction SilentlyContinue)) {
            Fail "the $Channel Suru server (pid $serverPid) is running; stop it first"
        }
    }
}
if (-not (Test-Path -LiteralPath $File -PathType Leaf)) {
    Write-Output "No remotes.json at $File; nothing to migrate."
    exit 0
}

function Test-OldShape($Remote) {
    $names = $Remote.PSObject.Properties.Name
    ($names -contains 'addresses') -or ($names -contains 'last_good_address') -or ($names -contains 'last_answered')
}

function Read-Remotes([string]$Path) {
    # Piped through Write-Output so a one-element array is not unrolled.
    $parsed = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    , @($parsed)
}

function Convert-Remote($Remote) {
    $names = $Remote.PSObject.Properties.Name
    $answered = New-Object System.Collections.ArrayList
    if ($names -contains 'last_good_address' -and $null -ne $Remote.last_good_address) {
        [void]$answered.Add([pscustomobject]@{ direct = $Remote.last_good_address })
    }
    if ($names -contains 'last_answered' -and $null -ne $Remote.last_answered) {
        [void]$answered.Add($Remote.last_answered)
    }
    if ($names -contains 'answered') {
        foreach ($way in @($Remote.answered)) { [void]$answered.Add($way) }
    }

    $migrated = [ordered]@{}
    foreach ($property in $Remote.PSObject.Properties) {
        switch ($property.Name) {
            'addresses' {
                $migrated['ways'] = @(foreach ($address in @($property.Value)) { [pscustomobject]@{ direct = $address } })
            }
            { $_ -in 'last_good_address', 'last_answered', 'answered' } { }
            default { $migrated[$property.Name] = $property.Value }
        }
    }
    if ($answered.Count -gt 0) { $migrated['answered'] = $answered.ToArray() }
    [pscustomobject]$migrated
}

try {
    $remotes = Read-Remotes $File
} catch {
    Fail "cannot read ${File}: $_"
}
$pending = @($remotes | Where-Object { Test-OldShape $_ }).Count
Write-Output ('{0,8}  remotes: addresses/last_good_address/last_answered -> ways/answered' -f $pending)
if ($DryRun -or $pending -eq 0) {
    exit 0
}

$backup = "$File.bak-" + (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmss')
Copy-Item -LiteralPath $File -Destination $backup
Write-Output "Backed up $File to $backup"

$migrated = @(foreach ($remote in $remotes) {
    if (Test-OldShape $remote) { Convert-Remote $remote } else { $remote }
})
# -InputObject keeps a one-element list an array; the depth covers a Way
# nested in a Remote nested in the list.
$json = ConvertTo-Json -InputObject $migrated -Depth 10 -Compress
# Rewritten in place, without a byte order mark, so the file keeps its
# owner-only ACL and serde reads it.
[System.IO.File]::WriteAllText((Resolve-Path -LiteralPath $File).Path, "$json`n", (New-Object System.Text.UTF8Encoding $false))

# Idempotence doubles as verification: nothing the migration covers remains.
$after = Read-Remotes $File
$left = @($after | Where-Object { Test-OldShape $_ }).Count
if ($left -ne 0) {
    Fail "$left Remotes still in an old shape after migrating; restore $backup"
}
Write-Output "Migrated $File; a second pass finds nothing left."
