param(
    [Parameter(Mandatory = $true)]
    [string[]]$Files
)

$ErrorActionPreference = 'Stop'
$certificateThumbprint = $env:ARC_LIVE_SIGNING_CERT_SHA1
$timestampUrl = if ($env:ARC_LIVE_TIMESTAMP_URL) { $env:ARC_LIVE_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }

if (-not $certificateThumbprint) {
    throw 'ARC_LIVE_SIGNING_CERT_SHA1 is not set. Refusing to create a public unsigned release.'
}

$signTool = Get-ChildItem -Path "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1

if (-not $signTool) {
    throw 'signtool.exe was not found. Install the Windows SDK Signing Tools.'
}

foreach ($file in $Files) {
    $resolved = (Resolve-Path -LiteralPath $file).Path
    & $signTool.FullName sign /sha1 $certificateThumbprint /fd SHA256 /tr $timestampUrl /td SHA256 $resolved
    if ($LASTEXITCODE -ne 0) {
        throw "Signing failed: $resolved"
    }
    & $signTool.FullName verify /pa /all $resolved
    if ($LASTEXITCODE -ne 0) {
        throw "Signature verification failed: $resolved"
    }
}

