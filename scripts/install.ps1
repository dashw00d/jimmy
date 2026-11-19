param(
    [Parameter(Mandatory = $true)][Uri] $Archive,
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA "Jimmy\bin")
)
$ErrorActionPreference = "Stop"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("jimmy-" + [Guid]::NewGuid().ToString())
try {
    New-Item -ItemType Directory -Path $work | Out-Null
    $zip = Join-Path $work "jimmy.zip"
    $sum = Join-Path $work "checksum"
    Invoke-WebRequest -Uri $Archive -OutFile $zip
    Invoke-WebRequest -Uri ($Archive.AbsoluteUri + ".sha256") -OutFile $sum
    $expected = ((Get-Content -LiteralPath $sum -TotalCount 1) -split '\s+')[0]
    $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
    if (-not $expected -or $actual -ne $expected) { throw "Jimmy archive checksum mismatch" }
    $unpacked = Join-Path $work "archive"
    Expand-Archive -LiteralPath $zip -DestinationPath $unpacked
    New-Item -Force -ItemType Directory -Path $InstallDir | Out-Null
    $destination = Join-Path $InstallDir "jimmy.exe"
    Copy-Item -Force -LiteralPath (Join-Path $unpacked "jimmy.exe") -Destination $destination
    Write-Output $destination
} finally {
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
}
