# Install laravel-parity (Windows) and register it with Claude Code:
#   irm https://raw.githubusercontent.com/syahrul84/laravel-parity/main/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = "syahrul84/laravel-parity"
$BinDir = if ($env:LARAVEL_PARITY_BIN) { $env:LARAVEL_PARITY_BIN } else { Join-Path $env:LOCALAPPDATA "laravel-parity\bin" }
$HomeDir = if ($env:LARAVEL_PARITY_HOME) { $env:LARAVEL_PARITY_HOME } else { Join-Path $env:USERPROFILE ".laravel-parity" }

$release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "laravel-parity-install" }
$tag = $release.tag_name
$asset = "laravel-parity-x86_64-pc-windows-msvc.zip"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("laravel-parity-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Invoke-WebRequest "https://github.com/$Repo/releases/download/$tag/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest "https://github.com/$Repo/releases/download/$tag/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")
    $expected = (Get-Content (Join-Path $tmp "SHA256SUMS") | Where-Object { $_ -match " $([regex]::Escape($asset))$" }).Split(" ")[0]
    $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "Checksum mismatch for $asset; nothing installed" }
    Expand-Archive (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $BinDir, (Join-Path $HomeDir "pairs") | Out-Null
    Copy-Item (Join-Path $tmp "laravel-parity.exe") (Join-Path $BinDir "laravel-parity.exe") -Force
} finally {
    Remove-Item -Recurse -Force $tmp
}
$exe = Join-Path $BinDir "laravel-parity.exe"
Write-Host "Installed laravel-parity $tag in $BinDir (pair files and runs in $HomeDir)"

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
    Write-Host "Added $BinDir to your PATH (open a new terminal)."
}

if (Get-Command claude -ErrorAction SilentlyContinue) {
    claude mcp remove --scope user laravel-parity 2>$null | Out-Null
    claude mcp add --scope user laravel-parity -e "LARAVEL_PARITY_HOME=$HomeDir" -- "$exe" serve
    Write-Host "Registered the MCP server with Claude Code (all projects)."
} else {
    Write-Host "Claude Code not found; register later with:"
    Write-Host "  claude mcp add --scope user laravel-parity -e LARAVEL_PARITY_HOME=$HomeDir -- `"$exe`" serve"
}
Write-Host "Next: laravel-parity guide"
