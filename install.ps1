param(
    [string]$Version = $env:VERSION,
    [string]$InstallDir = $(if ($env:WAKEZILLA_INSTALL_DIR) { $env:WAKEZILLA_INSTALL_DIR } else { $env:INSTALL_DIR }),
    [string]$Target = $env:TARGET,
    [string]$Repo = $(if ($env:REPO) { $env:REPO } else { "guibeira/wakezilla" }),
    [switch]$NoPath
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$BinName = "wakezilla"
$ExeName = "wakezilla.exe"
$WakezillaServiceNames = @("wakezilla-proxy", "wakezilla-client")

function Write-Info {
    param([string]$Message)
    Write-Host $Message
}

function Write-Warn {
    param([string]$Message)
    Write-Warning $Message
}

function Stop-Install {
    param(
        [string]$Stage,
        [string]$Message
    )
    throw "error[$Stage]: $Message"
}

function Get-WakezillaTarget {
    param(
        [string]$TargetOverride = $Target,
        [string]$Architecture = ""
    )

    if ($TargetOverride) {
        return $TargetOverride
    }

    if (-not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows
        )) {
        Stop-Install "platform" "install.ps1 is only supported on Windows"
    }

    if (-not $Architecture) {
        $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    }

    switch -Regex ($Architecture.ToLowerInvariant()) {
        "^(x64|x86_64|amd64)$" { return "x86_64-pc-windows-msvc" }
        default {
            Stop-Install "platform" "unsupported Windows architecture: $Architecture"
        }
    }
}

function Resolve-InstallDir {
    param([string]$RequestedInstallDir = $InstallDir)

    if ($RequestedInstallDir) {
        return [System.IO.Path]::GetFullPath($RequestedInstallDir)
    }

    $localAppData = [Environment]::GetFolderPath("LocalApplicationData")
    if (-not $localAppData) {
        $localAppData = $env:LOCALAPPDATA
    }
    if (-not $localAppData) {
        Stop-Install "install" "LOCALAPPDATA is not set; pass -InstallDir to choose an install directory"
    }

    return (Join-Path $localAppData "Programs\wakezilla\bin")
}

function Get-ReleaseApiUrl {
    param(
        [string]$RepoName,
        [string]$RequestedVersion
    )

    if ($RequestedVersion) {
        $normalized = $RequestedVersion -replace "^wakezilla/v", "" -replace "^v", ""
        return "https://api.github.com/repos/$RepoName/releases/tags/v$normalized"
    }

    return "https://api.github.com/repos/$RepoName/releases/latest"
}

function Invoke-GitHubJson {
    param([string]$Url)

    $headers = @{
        Accept                 = "application/vnd.github+json"
        "X-GitHub-Api-Version" = "2022-11-28"
    }

    if ($env:GITHUB_TOKEN) {
        $authHeaders = $headers.Clone()
        $authHeaders["Authorization"] = "Bearer $($env:GITHUB_TOKEN)"
        try {
            return Invoke-RestMethod -Uri $Url -Headers $authHeaders
        }
        catch {
            Write-Warn "GitHub request with GITHUB_TOKEN failed; retrying without authentication"
        }
    }

    Invoke-RestMethod -Uri $Url -Headers $headers
}

function Get-ReleaseJson {
    param(
        [string]$RepoName,
        [string]$RequestedVersion
    )

    Invoke-GitHubJson (Get-ReleaseApiUrl -RepoName $RepoName -RequestedVersion $RequestedVersion)
}

function Get-ReleaseVersion {
    param([object]$Release)

    ($Release.tag_name -replace "^wakezilla/v", "" -replace "^v", "")
}

function Get-AssetName {
    param(
        [string]$VersionValue,
        [string]$TargetValue
    )

    "$BinName-$VersionValue-$TargetValue.tar.gz"
}

function Get-AssetUrl {
    param(
        [object]$Release,
        [string]$VersionValue,
        [string]$TargetValue
    )

    $assetName = Get-AssetName -VersionValue $VersionValue -TargetValue $TargetValue
    $asset = $Release.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1
    if ($asset) {
        return $asset.browser_download_url
    }

    return $null
}

function Get-AvailableTargets {
    param(
        [object]$Release,
        [string]$VersionValue
    )

    $prefix = "$BinName-$VersionValue-"
    $Release.assets |
        ForEach-Object { $_.name } |
        Where-Object { $_.StartsWith($prefix) -and $_.EndsWith(".tar.gz") } |
        ForEach-Object { $_.Substring($prefix.Length, $_.Length - $prefix.Length - ".tar.gz".Length) } |
        Sort-Object -Unique
}

function Get-ChecksumUrl {
    param(
        [string]$RepoName,
        [string]$VersionValue
    )

    "https://github.com/$RepoName/releases/download/v$VersionValue/SHA256SUMS"
}

function Save-Url {
    param(
        [string]$Url,
        [string]$Path,
        [string]$Label
    )

    Write-Info "downloading $Label..."
    Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing
}

function Get-ChecksumForAsset {
    param(
        [string]$ChecksumsText,
        [string]$AssetName
    )

    foreach ($line in ($ChecksumsText -split "`r?`n")) {
        if ($line -match "^\s*([0-9a-fA-F]{64})\s+\*?(.+?)\s*$") {
            if ($Matches[2] -eq $AssetName) {
                return $Matches[1].ToLowerInvariant()
            }
        }
    }

    Stop-Install "checksum" "checksum not found for $AssetName"
}

function Assert-Checksum {
    param(
        [string]$File,
        [string]$ChecksumsText,
        [string]$AssetName
    )

    $expected = Get-ChecksumForAsset -ChecksumsText $ChecksumsText -AssetName $AssetName
    $actual = (Get-FileHash -Algorithm SHA256 -Path $File).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Stop-Install "checksum" "checksum verification failed for $AssetName"
    }
}

function Expand-WakezillaArchive {
    param(
        [string]$Archive,
        [string]$OutDir,
        [string]$BinaryName = $ExeName
    )

    if (-not (Get-Command tar -ErrorAction SilentlyContinue)) {
        Stop-Install "dependency" "tar is required to extract the release archive"
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    & tar -xzf $Archive -C $OutDir
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "extract" "failed to extract $(Split-Path -Leaf $Archive)"
    }

    $binary = Get-ChildItem -Path $OutDir -Recurse -File -Filter $BinaryName |
        Select-Object -First 1
    if (-not $binary) {
        Stop-Install "binary_lookup" "binary $BinaryName not found in downloaded asset"
    }

    $binary.FullName
}

function Restart-WakezillaServicesAfterInstall {
    param([string[]]$ServiceNames)

    foreach ($serviceName in $ServiceNames) {
        try {
            Write-Info "starting Windows service $serviceName..."
            Start-Service -Name $serviceName -ErrorAction Stop
        }
        catch {
            Write-Warn "installed $BinName, but failed to restart Windows service '$serviceName': $($_.Exception.Message)"
        }
    }
}

function Stop-WakezillaServicesForInstall {
    param([string[]]$ServiceNames = $WakezillaServiceNames)

    $restartServices = @()
    foreach ($serviceName in $ServiceNames) {
        $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
        if (-not $service) {
            continue
        }

        $wasRunning = $service.Status -eq "Running"
        if ($service.Status -ne "Stopped") {
            try {
                Write-Info "stopping Windows service $serviceName before updating $ExeName..."
                Stop-Service -Name $serviceName -Force -ErrorAction Stop
                $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(30))
            }
            catch {
                if ($restartServices.Count -gt 0) {
                    Restart-WakezillaServicesAfterInstall -ServiceNames $restartServices
                }
                Stop-Install "service_stop" "failed to stop Windows service '$serviceName' before updating $ExeName. Re-run the installer from an elevated PowerShell, or stop Wakezilla services manually and retry. Details: $($_.Exception.Message)"
            }
        }

        if ($wasRunning) {
            $restartServices += $serviceName
        }
    }

    $restartServices
}

function Test-SamePath {
    param(
        [string]$Left,
        [string]$Right
    )

    if (-not $Left -or -not $Right) {
        return $false
    }

    try {
        $Left = [System.IO.Path]::GetFullPath($Left)
    }
    catch {
    }

    try {
        $Right = [System.IO.Path]::GetFullPath($Right)
    }
    catch {
    }

    $Left.TrimEnd("\") -ieq $Right.TrimEnd("\")
}

function Get-WakezillaProcessesForInstall {
    param([string]$ExecutablePath)

    if (-not (Test-Path $ExecutablePath)) {
        return @()
    }

    try {
        @(Get-CimInstance Win32_Process -Filter "name = '$ExeName'" -ErrorAction Stop |
            Where-Object { Test-SamePath $_.ExecutablePath $ExecutablePath })
    }
    catch {
        Write-Warn "failed to inspect running $ExeName processes before install: $($_.Exception.Message)"
        @()
    }
}

function Stop-WakezillaProcessesForInstall {
    param([string]$ExecutablePath)

    $processes = @(Get-WakezillaProcessesForInstall -ExecutablePath $ExecutablePath)
    if ($processes.Count -eq 0) {
        return
    }

    foreach ($process in $processes) {
        try {
            Write-Info "stopping running $ExeName process $($process.ProcessId) before updating..."
            Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop
        }
        catch {
            Stop-Install "process_stop" "failed to stop running $ExeName process $($process.ProcessId) before updating $ExecutablePath. Stop Wakezilla manually and retry. Details: $($_.Exception.Message)"
        }
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    foreach ($process in $processes) {
        while ([DateTime]::UtcNow -lt $deadline) {
            $running = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)" -ErrorAction SilentlyContinue
            if (-not $running) {
                break
            }
            Start-Sleep -Milliseconds 250
        }

        $stillRunning = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)" -ErrorAction SilentlyContinue
        if ($stillRunning) {
            Stop-Install "process_stop" "running $ExeName process $($process.ProcessId) did not exit before updating $ExecutablePath"
        }
    }
}

function Install-WakezillaBinary {
    param(
        [string]$Source,
        [string]$DestinationDir
    )

    New-Item -ItemType Directory -Force -Path $DestinationDir | Out-Null
    $destination = Join-Path $DestinationDir $ExeName
    $temporary = Join-Path $DestinationDir ".$BinName.install.$PID.tmp.exe"

    if (Test-Path $temporary) {
        Remove-Item -Force $temporary
    }

    Stop-WakezillaProcessesForInstall -ExecutablePath $destination

    try {
        Copy-Item -Force $Source $temporary
        Move-Item -Force $temporary $destination
    }
    catch {
        if (Test-Path $temporary) {
            Remove-Item -Force $temporary -ErrorAction SilentlyContinue
        }
        Stop-Install "install" "failed to replace $destination. Stop any running Wakezilla service or process and retry from an elevated PowerShell. Details: $($_.Exception.Message)"
    }

    $destination
}

function Add-UserPath {
    param([string]$Directory)

    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($currentUserPath) {
        $parts = $currentUserPath -split ";" | Where-Object { $_ }
    }

    $alreadyPresent = $parts | Where-Object {
        $_.TrimEnd("\") -ieq $Directory.TrimEnd("\")
    } | Select-Object -First 1

    if (-not $alreadyPresent) {
        $nextPath = if ($currentUserPath) { "$currentUserPath;$Directory" } else { $Directory }
        [Environment]::SetEnvironmentVariable("Path", $nextPath, "User")
    }

    $processPath = [Environment]::GetEnvironmentVariable("Path", "Process")
    if ($processPath -notlike "*$Directory*") {
        [Environment]::SetEnvironmentVariable("Path", "$processPath;$Directory", "Process")
    }
}

function Invoke-WakezillaInstall {
    $targetValue = Get-WakezillaTarget
    $binDir = Resolve-InstallDir

    Write-Info "installing $BinName for $targetValue"
    $release = Get-ReleaseJson -RepoName $Repo -RequestedVersion $Version
    $releaseVersion = Get-ReleaseVersion -Release $release
    $assetUrl = Get-AssetUrl -Release $release -VersionValue $releaseVersion -TargetValue $targetValue

    if (-not $assetUrl) {
        [Console]::Error.WriteLine("$BinName v$releaseVersion does not include a prebuilt binary for $targetValue.")
        [Console]::Error.WriteLine("")
        [Console]::Error.WriteLine("Available targets:")
        Get-AvailableTargets -Release $release -VersionValue $releaseVersion |
            ForEach-Object { [Console]::Error.WriteLine(" - $_") }
        Stop-Install "download" "no release asset found for target: $targetValue"
    }

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "$BinName-install-$PID"
    if (Test-Path $tmpDir) {
        Remove-Item -Recurse -Force $tmpDir
    }
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
    $servicesToRestart = @()

    try {
        $assetName = [System.IO.Path]::GetFileName(([System.Uri]$assetUrl).LocalPath)
        $archive = Join-Path $tmpDir $assetName
        $checksumsPath = Join-Path $tmpDir "SHA256SUMS"

        Save-Url -Url $assetUrl -Path $archive -Label $assetName
        Save-Url -Url (Get-ChecksumUrl -RepoName $Repo -VersionValue $releaseVersion) `
            -Path $checksumsPath `
            -Label "SHA256SUMS"
        $checksumsText = Get-Content -Raw -Path $checksumsPath
        Assert-Checksum -File $archive -ChecksumsText $checksumsText -AssetName $assetName

        $binary = Expand-WakezillaArchive -Archive $archive -OutDir (Join-Path $tmpDir "extract")
        $servicesToRestart = @(Stop-WakezillaServicesForInstall)
        $installed = Install-WakezillaBinary -Source $binary -DestinationDir $binDir

        if (-not $NoPath) {
            Add-UserPath -Directory $binDir
        }

        $versionOutput = ""
        try {
            $versionOutput = & $installed --version 2>$null
        }
        catch {
            Write-Warn "$BinName installed, but '$BinName --version' failed"
        }

        if ($versionOutput) {
            $installedVersion = (($versionOutput -split "\s+") | Select-Object -Last 1)
            Write-Info "installed $BinName v$installedVersion to $installed"
        }
        else {
            Write-Warn "$BinName installed, but '$BinName --version' produced no output"
        }

        Write-Info "resolved $BinName v$releaseVersion"
        Write-Info "asset: $assetUrl"
        Write-Info "install dir: $binDir"
        if (-not $NoPath) {
            Write-Info "open a new terminal to use '$BinName' from PATH"
        }
    }
    finally {
        if ($servicesToRestart.Count -gt 0) {
            Restart-WakezillaServicesAfterInstall -ServiceNames $servicesToRestart
        }
        if (Test-Path $tmpDir) {
            Remove-Item -Recurse -Force $tmpDir
        }
    }
}

if ($env:WAKEZILLA_INSTALL_PS1_TEST_MODE) {
    return
}

Invoke-WakezillaInstall
