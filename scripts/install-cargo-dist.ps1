$ErrorActionPreference = "Stop"

$distVersion = "0.31.0"
$installerSha256 = "ffec5b52cfbe29465d831150b01f8a254668fc271e5102fab7aea7da5d51ec69"
$installerUrl = "https://github.com/axodotdev/cargo-dist/releases/download/v$distVersion/cargo-dist-installer.ps1"
$installer = Join-Path ([System.IO.Path]::GetTempPath()) "cargo-dist-installer-$PID.ps1"

try {
    Invoke-WebRequest -Uri $installerUrl -OutFile $installer
    $actualSha256 = (Get-FileHash -Algorithm SHA256 -Path $installer).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $installerSha256) {
        throw "cargo-dist installer checksum mismatch: expected $installerSha256, got $actualSha256"
    }
    & $installer
} finally {
    Remove-Item -Path $installer -Force -ErrorAction SilentlyContinue
}
