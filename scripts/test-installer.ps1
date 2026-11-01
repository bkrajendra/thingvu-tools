# SPDX-License-Identifier: GPL-3.0-or-later
#
# Smoke test for the release package and its installer, run in a sandbox (a temporary "Documents",
# "AppData", and install folder), so nothing on the machine is touched.
#
#   .\scripts\package.ps1 -BinDir target\debug -OutDir dist-test
#   .\scripts\test-installer.ps1 -DistDir dist-test
param([Parameter(Mandatory)][string]$DistDir)
$ErrorActionPreference = 'Stop'

function Check($condition, $message) {
    if (-not $condition) { throw "FAILED: $message" }
    Write-Host "  ok  $message"
}
function Run-Script($path, [string[]]$arguments) {
    # A fresh Windows PowerShell, as a user's would be.
    $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $path @arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$path failed:`n$($output -join "`n")" }
    return ($output -join "`n")
}

$zip = Get-ChildItem $DistDir -Filter 'thingsvu-tools-v*-windows-x64.zip' | Select-Object -First 1
if (-not $zip) { throw "no release zip in $DistDir" }
$sandbox = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-installer-test-" + [guid]::NewGuid().ToString('N'))
$documents = Join-Path $sandbox 'Documents'
$data = Join-Path $sandbox 'data'
$package = Join-Path $sandbox 'package'
$desktop = Join-Path $sandbox 'Claude\claude_desktop_config.json'
New-Item -ItemType Directory $documents, "$documents\KiCad\10.99", "$documents\KiCad\10.0", (Split-Path $desktop) | Out-Null
Expand-Archive $zip.FullName $package
$version = (Get-Content (Join-Path $package 'VERSION.txt')).Trim()
$other = '{ "mcpServers": { "other": { "command": "C:\\other.exe", "args": ["--x"] } }, "theme": "dark" }'
[IO.File]::WriteAllText($desktop, $other)

$installArgs = @('-Yes', '-InstallDir', "$data\bin", '-DocumentsDir', $documents, '-Clients', 'ClaudeDesktop', '-ClaudeDesktopConfig', $desktop)
try {
    Write-Host 'Install'
    $text = Run-Script (Join-Path $package 'install.ps1') $installArgs
    $exe = "$data\bin\kicad-ai-mcp.exe"
    Check (Test-Path $exe) 'the MCP server is installed'
    # --version prints on stderr, which Windows PowerShell 5.1 treats as an error when told to stop.
    $reported = & { $ErrorActionPreference = 'Continue'; & $exe --version 2>&1 | Out-String }
    Check ($reported -match [regex]::Escape($version)) "it reports version $version"
    Check (Test-Path "$data\uninstall.ps1") 'the uninstaller is kept next to the server'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\kicad-ai-plugin.exe") 'the plugin is installed for 10.99'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\plugin.json") 'with its manifest next to the executable'
    Check (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai\icons\kicad-ai-light-24.png") 'and its icons'
    Check (-not (Test-Path "$documents\KiCad\10.0\plugins")) 'nothing is installed for KiCad 10, which cannot run the plugin'
    Check ($text -match 'KiCad 10 found') 'and the user is told why'
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check ($cfg.mcpServers.'kicad-ai'.command -eq $exe) 'Claude Desktop starts the installed server'
    Check ($cfg.mcpServers.other.command -eq 'C:\other.exe' -and $cfg.theme -eq 'dark') "the user's other settings are kept"
    Check (@(Get-ChildItem (Split-Path $desktop) -Filter '*.bak-*').Count -eq 1) 'the old settings are backed up'
    Check (-not (Test-Path "$data\freerouting") -and -not (Test-Path "$data\config.json")) '-Yes alone never downloads the autorouter'

    Write-Host 'Install again'
    Run-Script (Join-Path $package 'install.ps1') $installArgs | Out-Null
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check (@($cfg.mcpServers.PSObject.Properties).Count -eq 2) 'a second run does not duplicate the entry'

    Write-Host 'A settings file that is not JSON is left alone'
    [IO.File]::WriteAllText($desktop, 'not json {')
    $text = Run-Script (Join-Path $package 'install.ps1') $installArgs
    Check ((Get-Content $desktop -Raw) -eq 'not json {') 'it is not overwritten'
    [IO.File]::WriteAllText($desktop, ($cfg | ConvertTo-Json -Depth 20))

    Write-Host 'Download from a release server (a local stand-in for GitHub)'
    # Several releases, the newest a draft: the installer must skip it and take the next one.
    $port = Get-Random -Minimum 20000 -Maximum 40000
    $served = Join-Path $sandbox 'served'
    New-Item -ItemType Directory $served | Out-Null
    Copy-Item $zip.FullName $served
    Copy-Item (Join-Path $DistDir 'SHA256SUMS.txt') $served
    $base = "http://localhost:$port"
    $asset = { param($name) @{ name = $name; browser_download_url = "$base/files/$name" } }
    $releases = @(
        @{ tag_name = 'v9.9.9'; draft = $true; prerelease = $true; assets = @() },
        @{ tag_name = "v$version"; draft = $false; prerelease = $true; assets = @((& $asset $zip.Name), (& $asset 'SHA256SUMS.txt')) },
        @{ tag_name = 'v0.0.1'; draft = $false; prerelease = $true; assets = @() }
    )
    $listing = ConvertTo-Json -InputObject $releases -Depth 6
    # A stand-in for Freerouting's release: its native command-line build and its checksums.
    $frStage = Join-Path $sandbox 'frstage'
    New-Item -ItemType Directory "$frStage\freerouting-cli" | Out-Null
    Set-Content "$frStage\freerouting-cli\freerouting.exe" 'not a real program' -Encoding ascii
    $frZip = 'freerouting-cli-9.9.9-windows-x64.zip'
    Compress-Archive "$frStage\freerouting-cli" (Join-Path $served $frZip)
    $frHash = (Get-FileHash (Join-Path $served $frZip) -Algorithm SHA256).Hash.ToLower()
    [IO.File]::WriteAllText((Join-Path $served 'freerouting-cli-9.9.9-checksums.txt'), "$frHash  $frZip`n")
    $frRelease = ConvertTo-Json -Depth 6 -InputObject @{
        tag_name = 'v9.9.9'
        assets = @(
            @{ name = $frZip; size = 1024; browser_download_url = "$base/files/$frZip" },
            @{ name = 'freerouting-cli-9.9.9-checksums.txt'; size = 100; browser_download_url = "$base/files/freerouting-cli-9.9.9-checksums.txt" }
        )
    }
    $job = Start-Job -ArgumentList $port, $listing, $served, $frRelease -ScriptBlock {
        param($port, $listing, $served, $frRelease)
        $listener = New-Object System.Net.HttpListener
        $listener.Prefixes.Add("http://localhost:$port/")
        $listener.Start()
        try {
            while ($true) {
                $context = $listener.GetContext()
                $path = $context.Request.Url.AbsolutePath
                if ($path -like '*/releases') {
                    $bytes = [Text.Encoding]::UTF8.GetBytes($listing)
                    $context.Response.ContentType = 'application/json'
                }
                elseif ($path -like '*/releases/latest') {
                    $bytes = [Text.Encoding]::UTF8.GetBytes($frRelease)
                    $context.Response.ContentType = 'application/json'
                }
                elseif ($path -like '/files/*') {
                    $bytes = [IO.File]::ReadAllBytes((Join-Path $served (Split-Path $path -Leaf)))
                }
                else { $context.Response.StatusCode = 404; $bytes = @() }
                $context.Response.OutputStream.Write($bytes, 0, $bytes.Length)
                $context.Response.Close()
            }
        }
        finally { $listener.Stop() }
    }
    try {
        # Wait until it answers.
        $ready = $false
        for ($i = 0; $i -lt 50 -and -not $ready; $i++) {
            try { Invoke-RestMethod "$base/repos/x/y/releases" | Out-Null; $ready = $true } catch { Start-Sleep -Milliseconds 200 }
        }
        Check $ready 'the local release server is up'
        $remote = Join-Path $sandbox 'remote'
        New-Item -ItemType Directory $remote | Out-Null
        # Run from a folder that has no package next to it, so it has to download.
        Copy-Item (Join-Path $package 'install.ps1') $remote
        $text = Run-Script (Join-Path $remote 'install.ps1') @('-Yes', '-SkipClients', '-InstallDir', "$remote\data\bin", '-DocumentsDir', $documents, '-ApiBase', $base, '-Repository', 'x/y')
        Check ($text -match "Downloading thingsvu-tools-v$([regex]::Escape($version))-windows-x64.zip") 'it skips the draft and picks the newest published release'
        Check ($text -match 'Checksum verified') 'it verifies the checksum'
        Check (Test-Path "$remote\data\bin\kicad-ai-mcp.exe") 'and installs the downloaded server'
        # A download that does not match its checksum is refused.
        [IO.File]::WriteAllText((Join-Path $served 'SHA256SUMS.txt'), ('0' * 64) + "  $($zip.Name)`n")
        $failed = $false
        try { Run-Script (Join-Path $remote 'install.ps1') @('-Yes', '-SkipClients', '-InstallDir', "$remote\other\bin", '-DocumentsDir', $documents, '-ApiBase', $base, '-Repository', 'x/y') | Out-Null }
        catch { $failed = $_.Exception.Message -match 'checksum' }
        Check $failed 'a download that does not match its checksum is refused'
        Check (-not (Test-Path "$remote\other\bin\kicad-ai-mcp.exe")) 'and nothing is installed'

        Write-Host "The autorouter, on its own (a stand-in for Freerouting's release)"
        # The stand-in executable cannot route; the routing check is tested against the real Freerouting.
        $frArgs = @('-FreeroutingOnly', '-SkipFreeroutingCheck', '-DownloadFreerouting', '-InstallDir', "$remote\fr\bin", '-ApiBase', $base, '-FreeroutingRepository', 'fr/fr')
        $text = Run-Script (Join-Path $remote 'install.ps1') $frArgs
        Check ($text -match 'Downloading freerouting-cli-9.9.9-windows-x64.zip') 'it downloads the native command-line build'
        Check ($text -match 'checksum verified') 'and checks it against the published checksums'
        $frConfig = Get-Content "$remote\fr\config.json" -Raw | ConvertFrom-Json
        Check ($frConfig.freerouting -like '*\freerouting\freerouting-cli\freerouting.exe' -and (Test-Path $frConfig.freerouting)) 'config.json names the unpacked executable'
        Check (-not $frConfig.java) 'and no Java, which the native build does not need'
        Check (-not (Test-Path "$remote\fr\bin\kicad-ai-mcp.exe")) '-FreeroutingOnly installs nothing else'
        [IO.File]::WriteAllText((Join-Path $served 'freerouting-cli-9.9.9-checksums.txt'), ('0' * 64) + "  $frZip`n")
        $failed = $false
        try { Run-Script (Join-Path $remote 'install.ps1') @('-FreeroutingOnly', '-SkipFreeroutingCheck', '-DownloadFreerouting', '-InstallDir', "$remote\fr2\bin", '-ApiBase', $base, '-FreeroutingRepository', 'fr/fr') | Out-Null }
        catch { $failed = $_.Exception.Message -match 'checksum' }
        Check $failed 'a Freerouting download that does not match its checksum is refused'
        Check (-not (Test-Path "$remote\fr2\config.json")) 'and autorouting is not set up'
        $text = Run-Script (Join-Path $remote 'install.ps1') @('-FreeroutingOnly', '-SkipFreeroutingCheck', '-InstallDir', "$remote\fr3\bin", '-FreeroutingPath', "$frStage\freerouting-cli\freerouting.exe")
        Check ((Get-Content "$remote\fr3\config.json" -Raw | ConvertFrom-Json).freerouting -like '*freerouting.exe') '-FreeroutingPath uses a copy the user has, without downloading'
    }
    finally { Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue }

    Write-Host 'Claude Code (a stand-in that behaves like the real one)'
    # The real `claude mcp remove` writes to stderr and fails when there is nothing to remove; that
    # must not stop the installer (Windows PowerShell 5.1 turns stderr into an error).
    $fake = Join-Path $sandbox 'fakeclaude'
    New-Item -ItemType Directory $fake | Out-Null
    $log = Join-Path $fake 'calls.txt'
    $cmd = @(
        '@echo off',
        'echo %* >> "%~dp0calls.txt"',
        'if "%2"=="remove" (',
        '  echo No MCP server named "kicad-ai" in user scope 1>&2',
        '  exit /b 1',
        ')',
        'exit /b 0'
    )
    Set-Content (Join-Path $fake 'claude.cmd') $cmd -Encoding ascii
    $savedPath = $env:PATH
    $env:PATH = "$fake;$env:PATH"
    try {
        $text = Run-Script (Join-Path $package 'install.ps1') @('-Yes', '-InstallDir', "$data\bin", '-DocumentsDir', $documents, '-Clients', 'ClaudeCode')
        Check ($text -match 'Claude Code: added') 'the installer carries on after "remove" fails and adds the entry'
        $calls = Get-Content $log -Raw
        Check ($calls -match 'mcp add --scope user kicad-ai -- ') 'with the right command'
        Check ($calls -match [regex]::Escape("$data\bin\kicad-ai-mcp.exe")) 'pointing at the installed server'
        Check ($calls -match 'mcp add --scope user --transport http pcbparts https://pcbparts.dev/mcp') 'the parts-lookup MCP is offered, and added on yes'
        Check ($text -match 'Parts lookup: added to Claude Code') 'and the summary says so'
        Remove-Item $log
        $null = Run-Script (Join-Path $package 'install.ps1') @('-Yes', '-NoPartsLookup', '-InstallDir', "$data\bin", '-DocumentsDir', $documents, '-Clients', 'ClaudeCode')
        Check ((Get-Content $log -Raw) -notmatch 'pcbparts') '-NoPartsLookup never touches it'
        Remove-Item $log
        Run-Script (Join-Path $package 'uninstall.ps1') @('-Yes', '-DataDir', "$sandbox\nothing", '-DocumentsDir', $documents, '-Clients', 'ClaudeCode') | Out-Null
        $calls = Get-Content $log -Raw
        Check ($calls -match 'mcp remove kicad-ai --scope user') 'the uninstaller removes it, and an empty "remove" is not an error'
        Check ($calls -match 'mcp remove pcbparts --scope user') 'and offers to remove the parts-lookup MCP too'
    }
    finally { $env:PATH = $savedPath }

    Write-Host 'Uninstall'
    Run-Script (Join-Path $package 'uninstall.ps1') @('-Yes', '-DataDir', $data, '-DocumentsDir', $documents, '-Clients', 'ClaudeDesktop', '-ClaudeDesktopConfig', $desktop) | Out-Null
    Check (-not (Test-Path $data)) 'the server and its data are removed'
    Check (-not (Test-Path "$documents\KiCad\10.99\plugins\com.thingsvu.kicad-ai")) 'the plugin is removed'
    $cfg = Get-Content $desktop -Raw | ConvertFrom-Json
    Check (-not ($cfg.mcpServers.PSObject.Properties.Name -contains 'kicad-ai')) 'the Claude Desktop entry is removed'
    Check ($cfg.mcpServers.other.command -eq 'C:\other.exe') "the user's other servers stay"
    Check (Test-Path "$documents\KiCad\10.99") "KiCad's own folders stay"
    Write-Host 'Installer smoke test passed.'
}
finally {
    Remove-Item -Recurse -Force $sandbox -ErrorAction SilentlyContinue
}
