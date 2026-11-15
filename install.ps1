# install.ps1: Installer for migratr on Windows (https://github.com/rafters-studio/migratr)
#
# The Windows counterpart of install.sh. Installs the newest published release
# unless a version is given. No version is written into this file, so every
# release updates the installer. Runs in Windows PowerShell 5.1 and PowerShell 7.
#
# Usage:
#   irm https://raw.githubusercontent.com/rafters-studio/migratr/main/install.ps1 | iex
#   $env:MIGRATR_VERSION = "v0.1.1"; irm ... | iex

# Everything runs inside one script block, so piping to iex leaves no functions
# or variables behind in the caller's session, and a failure throws instead of
# calling exit, which would close the caller's window.
& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'

  $Repo = 'rafters-studio/migratr'
  $BinaryName = 'migratr'
  $InstallDir = Join-Path $HOME '.local\bin'
  $CargoHint = 'install with: cargo install migratr-cli'

  function Write-Info([string]$Message) {
    Write-Host 'info' -ForegroundColor Blue -NoNewline
    Write-Host ": $Message"
  }

  function Write-Warn([string]$Message) {
    Write-Host 'warn' -ForegroundColor Yellow -NoNewline
    Write-Host ": $Message"
  }

  function Write-Success([string]$Message) {
    Write-Host 'success' -ForegroundColor Green -NoNewline
    Write-Host ": $Message"
  }

  # Prints the error and stops the install. throw, not exit: under iex, exit
  # would close the caller's PowerShell window.
  function Stop-Install([string]$Message) {
    Write-Host 'error' -ForegroundColor Red -NoNewline
    Write-Host ": $Message"
    throw "migratr was not installed: $Message"
  }

  # Windows PowerShell 5.1 may not offer TLS 1.2 by default; github.com needs it.
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  function Get-HttpStatus($ErrorRecord) {
    $response = $ErrorRecord.Exception.Response
    if ($null -ne $response) { return [int]$response.StatusCode }
    return $null
  }

  function Save-Url([string]$Url, [string]$OutFile) {
    try {
      Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
    } catch {
      $status = Get-HttpStatus $_
      if ($null -ne $status) {
        Stop-Install "Could not fetch $Url (HTTP $status)"
      }
      Stop-Install "Could not fetch $Url ($($_.Exception.Message))"
    }
  }

  # Detect platform. Only Windows on x64 has a published Windows build.
  function Get-Platform {
    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
      Stop-Install "install.ps1 installs migratr on Windows only; on Linux and macOS use install.sh"
    }
    # A 32-bit PowerShell on 64-bit Windows reports the real architecture in PROCESSOR_ARCHITEW6432.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
      'AMD64' { return 'windows-x64' }
      'ARM64' { Stop-Install "windows-arm64 builds are not published; $CargoHint" }
      default { Stop-Install "Unsupported architecture: windows-$arch; $CargoHint" }
    }
  }

  # Resolve the version tag to install: MIGRATR_VERSION, then the latest release.
  function Resolve-Version {
    $version = $env:MIGRATR_VERSION
    if ($version) {
      if ($version.StartsWith('v')) { return $version }
      return "v$version"
    }

    Write-Info 'Resolving latest version from GitHub...'
    $latestUrl = "https://github.com/$Repo/releases/latest"
    # The redirect's Location ends with the tag, e.g. .../releases/tag/v0.1.0
    $location = $null
    try {
      $request = [System.Net.HttpWebRequest]::Create($latestUrl)
      $request.AllowAutoRedirect = $false
      $request.Method = 'HEAD'
      $response = $request.GetResponse()
      $location = $response.Headers['Location']
      $response.Close()
    } catch {
      # Some runtimes raise the 302 itself as an error; its Location still holds the tag.
      $errorResponse = $_.Exception.InnerException.Response
      if ($null -eq $errorResponse) { $errorResponse = $_.Exception.Response }
      if ($null -ne $errorResponse -and $errorResponse.Headers['Location']) {
        $location = $errorResponse.Headers['Location']
      } else {
        Stop-Install "Could not fetch $latestUrl ($($_.Exception.Message))"
      }
    }

    $tag = $null
    if ($location) { $tag = ($location -split '/')[-1] }
    if ($tag -notmatch '^v[0-9]') {
      Stop-Install 'Could not resolve the latest migratr release. Specify a version: $env:MIGRATR_VERSION = "v0.1.1"'
    }
    return $tag
  }

  # Verify the SHA-256 of the downloaded archive against checksums.txt.
  # Stops, naming the archive, on a missing line or a mismatch.
  function Confirm-Checksum([string]$Archive, [string]$ChecksumsFile) {
    $archiveName = Split-Path $Archive -Leaf
    Write-Info 'Verifying checksum...'

    # checksums.txt lines are "<hash>  <name>"; match the name field exactly.
    $expected = $null
    foreach ($line in Get-Content -LiteralPath $ChecksumsFile) {
      $fields = $line.Trim() -split '\s+'
      if ($fields.Count -ge 2 -and $fields[1] -ceq $archiveName) {
        $expected = $fields[0].ToLowerInvariant()
        break
      }
    }
    if (-not $expected) {
      Stop-Install "No checksum found for $archiveName in checksums.txt."
    }

    $actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
      Stop-Install "Checksum verification failed for $archiveName. The download may be corrupted."
    }
    Write-Success 'Checksum verified.'
  }

  # Add the install dir to the user's PATH (idempotent). Reads and writes the
  # registry value unexpanded, so entries such as %USERPROFILE%\bin survive.
  function Add-ToUserPath([string]$Dir) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
      $current = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      $want = $Dir.TrimEnd('\')
      foreach ($entry in ($current -split ';')) {
        if (-not $entry) { continue }
        $expanded = [Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\')
        if ($expanded -ieq $want) { return }
      }

      Write-Info "Adding $Dir to your user PATH..."
      if ($current -and -not $current.EndsWith(';')) { $current += ';' }
      $key.SetValue('Path', "$current$Dir", [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
      $key.Close()
    }

    # Writing the registry does not tell running programs PATH changed; setting
    # a user variable through .NET broadcasts the change, so clear a dummy one.
    $dummy = 'MIGRATR_INSTALL_PATH_REFRESH'
    [Environment]::SetEnvironmentVariable($dummy, '1', 'User')
    [Environment]::SetEnvironmentVariable($dummy, $null, 'User')
    Write-Warn 'Restart your terminal to use migratr from PATH.'
  }

  Write-Host 'migratr Installer' -ForegroundColor White
  Write-Host ''

  $platform = Get-Platform
  $artifactName = "$BinaryName-$platform"
  Write-Info "Detected platform: $platform"

  $version = Resolve-Version
  Write-Info "Installing migratr $version..."

  $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("migratr-install-" + [guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  $staged = $null
  try {
    $baseUrl = "https://github.com/$Repo/releases/download/$version"
    $archiveFile = "$artifactName.zip"
    $archiveUrl = "$baseUrl/$archiveFile"
    $archivePath = Join-Path $tmp $archiveFile
    $checksumsPath = Join-Path $tmp 'checksums.txt'

    Write-Info "Downloading $archiveFile..."
    Save-Url $archiveUrl $archivePath
    Save-Url "$baseUrl/checksums.txt" $checksumsPath

    # Verify integrity before anything is installed.
    Confirm-Checksum $archivePath $checksumsPath

    $extractDir = Join-Path $tmp 'extract'
    try {
      Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir
    } catch {
      Stop-Install "Could not extract $archiveUrl ($($_.Exception.Message))"
    }
    $extracted = Join-Path $extractDir "$BinaryName.exe"
    if (-not (Test-Path -LiteralPath $extracted -PathType Leaf)) {
      Stop-Install "$archiveUrl does not contain $BinaryName.exe"
    }

    Write-Info "Installing to $InstallDir..."
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $target = Join-Path $InstallDir "$BinaryName.exe"

    # Stage beside the target and prove the binary runs before it replaces
    # anything, so a failure leaves an earlier install as it was.
    $staged = Join-Path $InstallDir "$BinaryName.staged.exe"
    Copy-Item -LiteralPath $extracted -Destination $staged -Force
    $installedVersion = $null
    try {
      $installedVersion = & $staged --version 2>$null
    } catch {
      $installedVersion = $null
    }
    if ($LASTEXITCODE -ne 0 -or -not $installedVersion) {
      Stop-Install "The downloaded $BinaryName.exe failed to run --version; $target was not changed"
    }

    try {
      if (Test-Path -LiteralPath $target) {
        # A bare $null would reach .NET as "" and fail; NullString passes a real null (no backup).
        [System.IO.File]::Replace($staged, $target, [System.Management.Automation.Language.NullString]::Value)
      } else {
        [System.IO.File]::Move($staged, $target)
      }
    } catch {
      Stop-Install "Could not replace $target ($($_.Exception.Message)); is migratr running?"
    }
    $staged = $null

    Add-ToUserPath $InstallDir

    Write-Success "Installed $installedVersion to $target"
  } finally {
    if ($staged -and (Test-Path -LiteralPath $staged)) {
      Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
}
