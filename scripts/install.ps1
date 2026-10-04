# Installs Suru from its GitHub releases on Windows; re-running it upgrades the install in place.
#
#   irm https://raw.githubusercontent.com/suru-ai/suru/main/scripts/install.ps1 | iex
#
# Environment:
#   SURU_VERSION      Release to install, e.g. v0.1.1. Defaults to the latest Suru release.
#   SURU_INSTALL_DIR  Directory the binary goes in. Defaults to %LOCALAPPDATA%\Programs\suru.
#   SURU_YES          Set to 1 to answer yes to every question, for unattended installs.
#   GITHUB_TOKEN      Sent to GitHub when set, which lifts the anonymous API rate limit.
#
# Runs on Windows PowerShell 5.1 and PowerShell 7. Everything lives in one function that reports failure by
# throwing: piped into iex the script runs in the caller's own session, which `exit` would close.

function Install-Suru {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell draws a progress bar that slows downloads to a crawl.
    $ProgressPreference = 'SilentlyContinue'

    if ($PSVersionTable.PSVersion.Major -ge 6 -and -not $IsWindows) {
        throw 'This script installs Suru on Windows. On Linux and macOS, use install.sh.'
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'suru-ai/suru'
    $assumeYes = $env:SURU_YES -eq '1'

    # Answers $null when there is no one to ask, which every caller treats as a no.
    function Confirm-Step([string] $Question) {
        if ($assumeYes) { return $true }
        if (-not [Environment]::UserInteractive -or [Console]::IsInputRedirected) { return $null }
        try { $reply = Read-Host "$Question [y/N]" } catch { return $null }
        return $reply -match '^(y|yes)$'
    }

    # The machine's own architecture, which an emulated PowerShell's environment misreports.
    $arch = try { [string][Runtime.InteropServices.RuntimeInformation]::OSArchitecture } catch { $null }
    if (-not $arch) {
        $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    }
    $target = switch ($arch.ToUpperInvariant()) {
        { $_ -in 'X64', 'AMD64' } { 'x86_64-pc-windows-msvc' }
        'ARM64' { 'aarch64-pc-windows-msvc' }
        default { throw "$arch Windows is not supported. Suru is built for x64 and ARM64 Windows." }
    }

    $installDir = if ($env:SURU_INSTALL_DIR) { $env:SURU_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\suru' }
    $dest = Join-Path $installDir 'suru.exe'

    # Asks before adding the install directory to the user's PATH, and only when it is not already there.
    function Add-ToPath {
        $entry = $installDir.TrimEnd('\')
        $environment = Get-Item 'HKCU:\Environment'
        # Read and written unexpanded, so entries such as %USERPROFILE%\bin survive the edit.
        $userPath = [string]$environment.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
        $present = @($userPath -split ';') + @($env:Path -split ';') |
            Where-Object { $_ -and [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $entry }
        if ($present) { return }

        if (Confirm-Step "$installDir is not on your PATH. Add it?") {
            $kind = if ($userPath) { $environment.GetValueKind('Path') } else { 'ExpandString' }
            $newPath = if ($userPath) { $userPath.TrimEnd(';') + ';' + $installDir } else { $installDir }
            Set-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Value $newPath -Type $kind
            # Writing the registry tells no one; setting any user variable through .NET broadcasts the change,
            # so newly opened terminals see the new PATH.
            [Environment]::SetEnvironmentVariable('SURU_PATH_BROADCAST', '1', 'User')
            [Environment]::SetEnvironmentVariable('SURU_PATH_BROADCAST', $null, 'User')
            $env:Path = $env:Path.TrimEnd(';') + ';' + $installDir
            Write-Host "Added $installDir to your PATH."
        }
        else {
            Write-Host "$installDir is not on your PATH. Add it to run suru from anywhere."
        }
    }

    $headers = @{ Accept = 'application/vnd.github+json' }
    if ($env:GITHUB_TOKEN) { $headers.Authorization = "Bearer $env:GITHUB_TOKEN" }
    if ($env:SURU_VERSION) {
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/$env:SURU_VERSION" -Headers $headers -UseBasicParsing
        }
        catch {
            throw "Could not find the Suru release $env:SURU_VERSION. $_"
        }
    }
    else {
        # The repository releases its Relay too, under tags of its own, so Suru's latest release is the newest
        # published one tagged like v1.2.3, whatever GitHub marks as latest.
        try {
            $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases?per_page=100" -Headers $headers -UseBasicParsing
        }
        catch {
            throw "Could not read the latest Suru release from GitHub. If GitHub is rate limiting you, set GITHUB_TOKEN and try again. $_"
        }
        $release = $releases |
            Where-Object { -not $_.draft -and -not $_.prerelease -and $_.tag_name -match '^v\d+\.\d+\.\d+$' } |
            Select-Object -First 1
        if (-not $release) { throw "GitHub lists no Suru release." }
    }
    $tag = $release.tag_name

    $installed = $null
    if (Test-Path $dest) {
        $installed = try { (& $dest --version) -split ' ' | Select-Object -Last 1 } catch { $null }
        if ("v$installed" -eq $tag) {
            Write-Host "Suru $tag is already installed at $dest."
            Add-ToPath
            return
        }
    }
    $kept = if ($installed) { "v$installed" } else { 'the installed version' }

    $file = "suru-$tag-$target.zip"
    $asset = $release.assets | Where-Object { $_.name -eq $file }
    if (-not $asset) { throw "Suru $tag has no build for $target." }
    if ($asset.digest -notmatch '^sha256:') { throw "Suru $tag publishes no checksum for $file, so it cannot be verified." }
    $digest = $asset.digest -replace '^sha256:', ''

    # Windows will not replace an executable any process is running, a Client's as much as the Server's.
    # Asked before anything is downloaded, and acted on only once the new binary is verified.
    function Get-Running {
        $resolved = (Resolve-Path $dest).Path
        Get-Process -Name suru -ErrorAction SilentlyContinue | Where-Object { $_.Path -ieq $resolved }
    }
    $running = (Test-Path $dest) -and (Get-Running)
    if ($running) {
        $answer = Confirm-Step 'Suru is running. Stopping it will interrupt any work in progress. Stop it and continue?'
        if ($null -eq $answer) {
            throw "Suru is running and there is no terminal to ask on. Close it and run 'suru server stop', or set SURU_YES=1, and try again."
        }
        if (-not $answer) { throw "Suru is still running, so $kept stays installed." }
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) "suru-install-$([Guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading Suru $tag for $target"
        $archive = Join-Path $tmp $file
        if ($env:GITHUB_TOKEN) {
            # The only download a private repository allows.
            $download = @{ Accept = 'application/octet-stream'; Authorization = "Bearer $env:GITHUB_TOKEN" }
            Invoke-WebRequest -Uri $asset.url -Headers $download -OutFile $archive -UseBasicParsing
        }
        else {
            Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -UseBasicParsing
        }

        $actual = (Get-FileHash -Path $archive -Algorithm SHA256).Hash
        if ($actual -ine $digest) {
            throw "$file does not match its published checksum (expected $digest, got $($actual.ToLowerInvariant()))."
        }
        Expand-Archive -Path $archive -DestinationPath $tmp

        if ($running) {
            Write-Host 'Stopping Suru'
            # The Server is asked to shut down cleanly; whatever is still running after that is ended.
            try { & $dest server stop *> $null } catch { Write-Verbose "Suru server stop failed: $_" }
            $left = @(Get-Running)
            if ($left) {
                $left | Stop-Process -Force
                $left | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
            }
        }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $binary = Join-Path $tmp "suru-$tag-$target\suru.exe"
        # A process that has just exited can hold its executable for a moment longer.
        for ($attempt = 1; ; $attempt++) {
            try {
                Copy-Item -Path $binary -Destination $dest -Force
                break
            }
            catch {
                if ($attempt -ge 10) { throw "Could not replace $dest, so $kept stays installed. $_" }
                Start-Sleep -Milliseconds 300
            }
        }
    }
    finally {
        Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    Write-Host "Installed Suru $tag to $dest"
    Add-ToPath
}

Install-Suru
