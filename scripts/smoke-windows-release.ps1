param(
    [string]$DistribDir = "target/distrib"
)

$ErrorActionPreference = "Stop"

$archives = @(Get-ChildItem $DistribDir -Filter "*-pc-windows-msvc.zip")
if ($archives.Count -ne 1) {
    throw "expected exactly one Windows release archive, found $($archives.Count)"
}

$smokeRoot = Join-Path ([System.IO.Path]::GetTempPath()) "bazel-mcp-windows-smoke"
if (Test-Path $smokeRoot) {
    Remove-Item $smokeRoot -Recurse -Force
}
Expand-Archive -Path $archives[0].FullName -DestinationPath $smokeRoot

$binaries = @(Get-ChildItem $smokeRoot -Recurse -Filter "bazel-mcp.exe")
if ($binaries.Count -ne 1) {
    throw "expected exactly one bazel-mcp.exe in the Windows archive, found $($binaries.Count)"
}

$version = & $binaries[0].FullName --version
if ($LASTEXITCODE -ne 0 -or $version -notmatch '^bazel-mcp [0-9]+\.[0-9]+\.[0-9]+') {
    throw "packaged Windows binary did not report a valid version: $version"
}

Write-Output $version
