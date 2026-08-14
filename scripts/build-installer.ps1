param(
    [switch]$UnsignedDevelopmentBuild,
    [ValidateSet('stable', 'beta')]
    [string]$Channel = 'stable'
)

$ErrorActionPreference = 'Stop'

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true, ValueFromPipeline = $true)]
        [string]$Content
    )
    process {
        $encoding = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($LiteralPath, $Content, $encoding)
    }
}

$projectDirectory = Split-Path -Parent $PSScriptRoot
$cargoManifest = Get-Content -LiteralPath (Join-Path $projectDirectory 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($cargoManifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw 'Could not read workspace version from Cargo.toml.'
}
$version = $versionMatch.Groups[1].Value
$stageDirectory = Join-Path $projectDirectory 'target\installer-stage'
$outputDirectory = Join-Path $projectDirectory 'dist'
$msiPath = Join-Path $outputDirectory "ARC-Live-$version-x64.msi"
$setupPath = Join-Path $outputDirectory "ARC-Live-Setup-$version.exe"
$portablePath = Join-Path $outputDirectory "ARC-Live-Portable-$version-windows-x64.zip"
$bundleSource = Get-Content -LiteralPath (Join-Path $projectDirectory 'installer\Bundle.wxs') -Raw
if ($bundleSource -match '<Variable\s+Name="Wix') {
    throw 'Bundle defines a variable with the Burn-reserved Wix prefix. Use WixStandardBootstrapperApplication attributes instead.'
}

New-Item -ItemType Directory -Force -Path $stageDirectory, $outputDirectory | Out-Null

Push-Location $projectDirectory
try {
    if (-not $UnsignedDevelopmentBuild -and -not $env:ARC_LIVE_UPDATE_FEED_URL) {
        throw 'ARC_LIVE_UPDATE_FEED_URL must be set to the public HTTPS stable.json URL.'
    }
    cargo build --release -p arc-live -p arc-live-capture-service
    if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed.' }

    Copy-Item -LiteralPath 'target\release\arc-live.exe' -Destination $stageDirectory -Force
    Copy-Item -LiteralPath 'target\release\arc-live-capture-service.exe' -Destination $stageDirectory -Force
    Copy-Item -LiteralPath 'vendor\windivert\WinDivert-2.2.2-A\x64\WinDivert.dll' -Destination $stageDirectory -Force
    Copy-Item -LiteralPath 'vendor\windivert\WinDivert-2.2.2-A\x64\WinDivert64.sys' -Destination $stageDirectory -Force
    Copy-Item -LiteralPath 'THIRD-PARTY-NOTICES.md' -Destination $stageDirectory -Force
    Copy-Item -LiteralPath 'widget-config.json' -Destination (Join-Path $stageDirectory 'widget-config.default.json') -Force

    $portableDirectory = Join-Path $projectDirectory 'target\portable-stage'
    New-Item -ItemType Directory -Force -Path $portableDirectory | Out-Null
    Copy-Item -LiteralPath 'target\release\arc-live.exe' -Destination $portableDirectory -Force
    Copy-Item -LiteralPath 'vendor\windivert\WinDivert-2.2.2-A\x64\WinDivert.dll' -Destination $portableDirectory -Force
    Copy-Item -LiteralPath 'vendor\windivert\WinDivert-2.2.2-A\x64\WinDivert64.sys' -Destination $portableDirectory -Force
    Copy-Item -LiteralPath 'THIRD-PARTY-NOTICES.md' -Destination $portableDirectory -Force
    Copy-Item -LiteralPath 'widget-config.json' -Destination $portableDirectory -Force

    $executables = @(
        (Join-Path $stageDirectory 'arc-live.exe'),
        (Join-Path $stageDirectory 'arc-live-capture-service.exe')
    )
    if (-not $UnsignedDevelopmentBuild) {
        & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Files $executables
    }
    Copy-Item -LiteralPath (Join-Path $stageDirectory 'arc-live.exe') -Destination $portableDirectory -Force
    if (Test-Path -LiteralPath $portablePath) {
        Remove-Item -LiteralPath $portablePath -Force
    }
    Compress-Archive -Path (Join-Path $portableDirectory '*') -DestinationPath $portablePath -CompressionLevel Optimal

    $localDotNet = Join-Path $projectDirectory '.tools\dotnet'
    $localWix = Join-Path $projectDirectory '.tools\wix\wix.exe'
    if (Test-Path -LiteralPath $localDotNet) {
        $env:DOTNET_ROOT = $localDotNet
        $env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
    }
    $wix = if (Test-Path -LiteralPath $localWix) {
        Get-Item -LiteralPath $localWix
    } else {
        Get-Command wix.exe -ErrorAction SilentlyContinue
    }
    if (-not $wix) {
        throw 'WiX 4/5 CLI was not found. Install it with: dotnet tool install --global wix'
    }

    & $wix.FullName build 'installer\Product.wxs' -arch x64 -d "StageDir=$stageDirectory" -d "ProductVersion=$version" -o $msiPath
    if ($LASTEXITCODE -ne 0) { throw 'MSI build failed.' }
    if (-not $UnsignedDevelopmentBuild) {
        & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Files @($msiPath)
    }

    $extensionRoots = @(
        (Join-Path $projectDirectory '.wix\extensions'),
        (Join-Path $env:USERPROFILE '.wix\extensions')
    ) | Where-Object { Test-Path -LiteralPath $_ }
    $balExtension = Get-ChildItem -Path $extensionRoots -Filter 'WixToolset.BootstrapperApplications.wixext.dll' -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $balExtension) {
        throw 'WiX BootstrapperApplications extension is missing. Run: wix extension add WixToolset.Bal.wixext/5.0.2'
    }
    & $wix.FullName build 'installer\Bundle.wxs' -arch x64 -ext $balExtension.FullName -d "ProjectDir=$projectDirectory" -d "MsiPath=$msiPath" -d "ProductVersion=$version" -o $setupPath
    if ($LASTEXITCODE -ne 0) { throw 'Setup bundle build failed.' }
    if (-not $UnsignedDevelopmentBuild) {
        & (Join-Path $PSScriptRoot 'sign-windows.ps1') -Files @($setupPath)
    }

    $hash = (Get-FileHash -LiteralPath $setupPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $manifest = [ordered]@{
        schema_version = 1
        channel = $Channel
        version = $version
        published_at = (Get-Date).ToUniversalTime().ToString('o')
        installer_url = if ($env:ARC_LIVE_INSTALLER_URL) {
            $env:ARC_LIVE_INSTALLER_URL
        } else {
            "https://github.com/sokol-rc/ArcTwitchWidget/releases/download/v$version/ARC-Live-Setup-$version.exe"
        }
        sha256 = $hash
        size = (Get-Item -LiteralPath $setupPath).Length
        silent_args = '/quiet /norestart'
    }
    $manifestPath = Join-Path $outputDirectory "$Channel.json"
    $manifest | ConvertTo-Json | Write-Utf8NoBom -LiteralPath $manifestPath

    $wingetDirectory = Join-Path $outputDirectory 'winget'
    New-Item -ItemType Directory -Force -Path $wingetDirectory | Out-Null
    $packageIdentifier = 'ArcLive.ARC-Live'
    $installerUrl = $manifest.installer_url
    @"
PackageIdentifier: $packageIdentifier
PackageVersion: $version
InstallerType: burn
Installers:
- Architecture: x64
  InstallerUrl: $installerUrl
  InstallerSha256: $hash
  InstallerSwitches:
    Silent: /quiet /norestart
    SilentWithProgress: /passive /norestart
ManifestType: installer
ManifestVersion: 1.10.0
"@ | Write-Utf8NoBom -LiteralPath (Join-Path $wingetDirectory "$packageIdentifier.installer.yaml")
    @"
PackageIdentifier: $packageIdentifier
PackageVersion: $version
DefaultLocale: ru-RU
ManifestType: version
ManifestVersion: 1.10.0
"@ | Write-Utf8NoBom -LiteralPath (Join-Path $wingetDirectory "$packageIdentifier.yaml")
    @"
PackageIdentifier: $packageIdentifier
PackageVersion: $version
PackageLocale: ru-RU
Publisher: ARC Live
PackageName: ARC Live
ShortDescription: Статистика ARC Raiders для OBS в реальном времени
License: MIT
Tags:
- arc-raiders
- obs
- overlay
ManifestType: defaultLocale
ManifestVersion: 1.10.0
"@ | Write-Utf8NoBom -LiteralPath (Join-Path $wingetDirectory "$packageIdentifier.locale.ru-RU.yaml")
    Write-Output $setupPath
}
finally {
    Pop-Location
}
