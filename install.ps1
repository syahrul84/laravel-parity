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

# another laravel-parity earlier on PATH (an older `cargo install`) would run instead of this one
$first = Get-Command laravel-parity -All -ErrorAction SilentlyContinue | Select-Object -First 1
if ($first -and ($first.Source -ne $exe)) {
    Write-Host "Warning: 'laravel-parity' on PATH starts $($first.Source), not $exe."
    Write-Host "  Remove it (cargo uninstall laravel-parity), or put $BinDir earlier on PATH."
}

if (Get-Command claude -ErrorAction SilentlyContinue) {
    # Windows PowerShell 5.1 turns a native command's stderr into an error record, and "Stop" would end the script
    # here on a first install ("No MCP server named laravel-parity"): nothing to remove is fine
    $ErrorActionPreference = "Continue"
    claude mcp remove --scope user laravel-parity *> $null
    claude mcp add --scope user laravel-parity -e "LARAVEL_PARITY_HOME=$HomeDir" -- "$exe" serve
    $added = $LASTEXITCODE -eq 0
    $ErrorActionPreference = "Stop"
    if ($added) {
        Write-Host "Registered the MCP server with Claude Code (all projects)."
    } else {
        Write-Host "Could not register the MCP server; run:"
        Write-Host "  claude mcp add --scope user laravel-parity -e LARAVEL_PARITY_HOME=$HomeDir -- `"$exe`" serve"
    }
} else {
    Write-Host "Claude Code not found; register later with:"
    Write-Host "  claude mcp add --scope user laravel-parity -e LARAVEL_PARITY_HOME=$HomeDir -- `"$exe`" serve"
}
Write-Host "Next: laravel-parity guide"
