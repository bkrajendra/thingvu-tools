# SPDX-License-Identifier: GPL-3.0-or-later
#
# Removes what install.ps1 installed: the MCP server, the KiCad plugin (every KiCad version), the
# editor registrations, the logs, and the entries in Claude Code and Claude Desktop. Your
# schematics and everything else in KiCad are never touched.
#
#   .\uninstall.ps1          asks before changing Claude Code / Claude Desktop
#   .\uninstall.ps1 -Yes     asks nothing
param(
    [switch]$Yes,
    [string[]]$Clients = @('ClaudeCode', 'ClaudeDesktop'),
    [string]$DataDir,
    [string]$DocumentsDir,
    [string]$ClaudeDesktopConfig
)
$ErrorActionPreference = 'Stop'

if (-not $DataDir) { $DataDir = Join-Path $env:LOCALAPPDATA 'thingsvu\kicad-ai' }
if (-not $DocumentsDir) { $DocumentsDir = [Environment]::GetFolderPath('MyDocuments') }
if (-not $ClaudeDesktopConfig) { $ClaudeDesktopConfig = Join-Path $env:APPDATA 'Claude\claude_desktop_config.json' }

# Runs a program and returns its exit code. Windows PowerShell 5.1 turns anything a program writes
# to stderr into an error when $ErrorActionPreference is 'Stop', even for a harmless message such as
# "No MCP server named ... in user scope"; only the exit code says whether it worked.
function Invoke-Native {
    param([string]$File, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $File @Arguments 2>&1 | Out-Null; return $LASTEXITCODE }
    finally { $ErrorActionPreference = $previous }
}

function Ask($question) {
    if ($Yes) { return $true }
    try { $a = Read-Host "$question [Y/n]"; return ($a -eq '' -or $a -match '^[yY]') }
    catch { return $false }
}

# The plugin, in every KiCad version folder.
foreach ($dir in @(Get-ChildItem (Join-Path $DocumentsDir 'KiCad') -Directory -ErrorAction SilentlyContinue)) {
    $target = Join-Path $dir.FullName 'plugins\com.thingsvu.kicad-ai'
    if (Test-Path $target) {
        Remove-Item -Recurse -Force $target
        Write-Host "Removed the plugin from KiCad $($dir.Name)"
    }
}

# The server, registrations, and logs.
if (Test-Path $DataDir) {
    try { Remove-Item -Recurse -Force $DataDir; Write-Host "Removed $DataDir" }
    catch { Write-Warning "Could not remove $DataDir (is an AI assistant running the server? close it and try again)." }
}

# AI assistants.
if ($Clients -contains 'ClaudeCode' -and (Get-Command claude -ErrorAction SilentlyContinue)) {
    if (Ask 'Remove the KiCad tools from Claude Code?') {
        $null = Invoke-Native 'claude' @('mcp', 'remove', 'kicad-ai', '--scope', 'user')
        Write-Host 'Claude Code: removed "kicad-ai" (if it was there).'
    }
    if (Ask 'Also remove the pcbparts parts-lookup MCP from Claude Code (skip this if you use it for other things)?') {
        $null = Invoke-Native 'claude' @('mcp', 'remove', 'pcbparts', '--scope', 'user')
        Write-Host 'Claude Code: removed "pcbparts" (if it was there).'
    }
}
if ($Clients -contains 'ClaudeDesktop' -and (Test-Path $ClaudeDesktopConfig)) {
    $cfg = $null
    try { $cfg = Get-Content $ClaudeDesktopConfig -Raw | ConvertFrom-Json } catch { }
    if ($cfg -and $cfg.mcpServers -and ($cfg.mcpServers.PSObject.Properties.Name -contains 'kicad-ai') -and (Ask 'Remove the KiCad tools from Claude Desktop?')) {
        Copy-Item $ClaudeDesktopConfig ("$ClaudeDesktopConfig.bak-" + (Get-Date -Format 'yyyyMMddHHmmss'))
        $cfg.mcpServers.PSObject.Properties.Remove('kicad-ai')
        [IO.File]::WriteAllText($ClaudeDesktopConfig, ($cfg | ConvertTo-Json -Depth 20), (New-Object Text.UTF8Encoding $false))
        Write-Host 'Claude Desktop: removed "kicad-ai".'
    }
}
Write-Host 'Done. Restart KiCad and your AI assistant.'
