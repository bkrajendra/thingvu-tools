# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs thingsvu-tools: the MCP server, the KiCad plugin, and (if you agree) the entry that
# makes your AI assistant start the server.
#
#   From the extracted release zip:   .\install.ps1
#   Without downloading anything:     irm https://raw.githubusercontent.com/bkrajendra/thingsvu-tools/main/scripts/install.ps1 | iex
#   Unattended:                       .\install.ps1 -Yes
#
# It never needs administrator rights: everything goes under your user profile.
#
#   -Version <x.y.z|latest>   which release to download when not run from the zip (default: newest)
#   -InstallDir <path>        where kicad-ai-mcp.exe goes (default: %LOCALAPPDATA%\thingsvu\kicad-ai\bin)
#   -KicadVersion <list>      plugin folders to install into, e.g. 11.0 (default: every KiCad 11 /
#                             10.99 user folder found under Documents\KiCad)
#   -NoPlugin                 skip the KiCad plugin
#   -SkipClients              do not touch Claude Code or Claude Desktop
#   -Clients <list>           which clients to offer: ClaudeCode, ClaudeDesktop (default: both)
#   -Yes                      answer yes to every question (except downloading Freerouting)
#   -NoPartsLookup            do not offer the pcbparts MCP (see below)
#   -WithFreerouting          also set up Freerouting, the optional autorouter (a separate
#                             open-source program from its GitHub release; and, when no Java 25
#                             is installed and its Java build is needed, Eclipse Temurin's Java
#                             runtime from Adoptium). Each download is checked against its
#                             published checksum, and the result must route a test board.
#   -FreeroutingOnly          only set up Freerouting, for an existing install
#   -FreeroutingPath <path>   use a Freerouting you already have (its .exe or .jar) instead
#   -JavaPath <path>          the Java that runs a Freerouting .jar
#   -DownloadFreerouting      fetch Freerouting from its release even if a copy is found here
#                             (for example to move to a newer version)
#
#   Add the autorouter to an existing install, in one line:
#     & ([scriptblock]::Create((irm https://raw.githubusercontent.com/bkrajendra/thingsvu-tools/main/scripts/install.ps1))) -FreeroutingOnly
param(
    [string]$Version = 'latest',
    [string]$InstallDir,
    [string[]]$KicadVersion,
    [switch]$NoPlugin,
    [switch]$SkipClients,
    [string[]]$Clients = @('ClaudeCode', 'ClaudeDesktop'),
    [switch]$Yes,
    [switch]$NoPartsLookup,
    [string]$DocumentsDir,
    [string]$ClaudeDesktopConfig,
    [string]$Repository = 'bkrajendra/thingsvu-tools',
    # The parts service the assistant uses to look up part numbers (a separate MCP server); only
    # the installer's own test changes this.
    [string]$PartsLookupUrl = 'https://pcbparts.dev/mcp',
    [switch]$WithFreerouting,
    [switch]$DownloadFreerouting,
    [switch]$FreeroutingOnly,
    [string]$FreeroutingPath,
    [string]$JavaPath,
    [string]$FreeroutingVersion = 'latest',
    [string]$FreeroutingRepository = 'freerouting/freerouting',
    # Where Java runtimes are listed (Eclipse Adoptium), and how long a router may take on the
    # test board; only the installer's own test changes these or skips the check.
    [string]$AdoptiumApi = 'https://api.adoptium.net',
    [int]$FreeroutingCheckSeconds = 45,
    [switch]$SkipFreeroutingCheck,
    # Where releases are listed; only the installer's own test changes this.
    [string]$ApiBase = 'https://api.github.com'
)
$ErrorActionPreference = 'Stop'

if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'thingsvu\kicad-ai\bin' }
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

function Step($text) { Write-Host "`n== $text" -ForegroundColor Cyan }
function Ask($question) {
    if ($Yes) { return $true }
    try {
        $answer = Read-Host "$question [Y/n]"
        return ($answer -eq '' -or $answer -match '^[yY]')
    }
    catch { return $false }   # not interactive: do nothing without being told to
}


# --- Optional: the autorouter ---------------------------------------------------------------
# Freerouting is a separate open-source project (GPL-3.0, github.com/freerouting/freerouting).
# It is set up only when asked (-WithFreerouting, -FreeroutingOnly, or a yes to the question).
# Whatever is found or downloaded must first route a small test board: Freerouting 2.5.0's native
# Windows build, for one, crashes on KiCad boards and hangs. In order, the first that works wins:
#   1. -FreeroutingPath (a copy you name; with -JavaPath for a jar);
#   2. a Freerouting already here (a running one, or Freerouting's own Windows install);
#   3. the native command-line build from its release (no Java), checked against its checksum;
#   4. its .jar from the release (checked against GitHub's digest) with a Java 25 found here, or,
#      when there is none, Eclipse Temurin's Java 25 runtime from Adoptium (checked likewise).
# Downloads go under %LOCALAPPDATA%\thingsvu\kicad-ai; the server finds the result in config.json.
$FreeroutingTestBoard = @'
(pcb "C:\Users\bkraj\AppData\Local\Temp\thingsvu-probe12r-10588\board.dsn"
  (parser
    (string_quote ")
    (space_in_quoted_tokens on)
    (host_cad " KiCad's Pcbnew")
    (host_version "10.99.0-5896-gd033635628"))
  (resolution um 10)
  (unit um)
  (structure
    (layer F.Cu
      (type signal)
      (property
        (index 0)))
    (layer B.Cu
      (type signal)
      (property
        (index 1)))
    (boundary
      (path pcb 0 134554 -80000.5 134660 -80008.1 134766 -80023.4 134871 -80046.1 134974 -80076.3 135074 -80113.7 135172 -80158.2 135266 -80209.6 135356 -80267.5 135442 -80331.7 135522 -80401.8 135598 -80477.5 135668 -80558.5 135733 -80644.2 135790 -80734.4 135842 -80828.4 135886 -80925.8 135924 -81026.1 135954 -81128.9 135977 -81233.6 135992 -81339.6 136000 -81446.4 136000 -81500 136000 -106500 136000 -106554 135992 -106660 135977 -106766 135954 -106871 135924 -106974 135886 -107074 135842 -107172 135790 -107266 135733 -107356 135668 -107442 135598 -107522 135522 -107598 135442 -107668 135356 -107733 135266 -107790 135172 -107842 135074 -107886 134974 -107924 134871 -107954 134766 -107977 134660 -107992 134554 -108000 134500 -108000 101500 -108000 101446 -108000 101340 -107992 101234 -107977 101129 -107954 101026 -107924 100926 -107886 100828 -107842 100734 -107790 100644 -107733 100558 -107668 100478 -107598 100402 -107522 100332 -107442 100267 -107356 100210 -107266 100158 -107172 100114 -107074 100076 -106974 100046 -106871 100023 -106766 100008 -106660 100000 -106554 100000 -106500 100000 -81500 100000 -81446.4 100008 -81339.6 100023 -81233.6 100046 -81128.9 100076 -81026.1 100114 -80925.8 100158 -80828.4 100210 -80734.4 100267 -80644.2 100332 -80558.5 100402 -80477.5 100478 -80401.8 100558 -80331.7 100644 -80267.5 100734 -80209.6 100828 -80158.2 100926 -80113.7 101026 -80076.3 101129 -80046.1 101234 -80023.4 101340 -80008.1 101446 -80000.5 101500 -80000 134500 -80000 134554 -80000.5))
    (via "Via[0-1]_600:300_um")
    (rule
      (width 200)
      (clearance 200)
      (clearance 50
        (type smd_smd))))
  (placement
    (component Resistor_SMD:R_0603_1608Metric
      (place R3 118500 -88500 front 0
        (PN 1k)))
    (component LED_SMD:LED_0603_1608Metric
      (place D2 118500 -86000 front 0
        (PN PWR))))
  (library
    (image Resistor_SMD:R_0603_1608Metric
      (outline
        (polygon signal 0 1475 -725 -1475 -725 -1475 725 1475 725))
      (outline
        (path signal 120 -237.258 522.5 237.258 522.5))
      (outline
        (path signal 120 -237.258 -522.5 237.258 -522.5))
      (outline
        (path signal 100 -800 412.5 800 412.5 800 -412.5 -800 -412.5 -800 412.5))
      (pin RoundRect[T]Pad_800.000000x950.000000_200.761000_um_0.000000_0 1 -825 0)
      (pin RoundRect[T]Pad_800.000000x950.000000_200.761000_um_0.000000_0 2 825 0))
    (image LED_SMD:LED_0603_1608Metric
      (outline
        (polygon signal 0 1475 -725 -1475 -725 -1475 725 1475 725))
      (outline
        (path signal 120 -1485 735 -1485 -735))
      (outline
        (path signal 120 -1485 -735 800 -735))
      (outline
        (path signal 120 800 735 -1485 735))
      (outline
        (path signal 100 -800 100 -800 -400))
      (outline
        (path signal 100 -800 -400 800 -400))
      (outline
        (path signal 100 -500 400 -800 100))
      (outline
        (path signal 100 800 400 -500 400))
      (outline
        (path signal 100 800 -400 800 400))
      (pin RoundRect[T]Pad_875.000000x950.000000_219.582000_um_0.000000_0 1 -787.5 0)
      (pin RoundRect[T]Pad_875.000000x950.000000_219.582000_um_0.000000_0 2 787.5 0))
    (padstack Round[A]Pad_1700.000000_um
      (shape
        (circle F.Cu 1700))
      (shape
        (circle B.Cu 1700))
      (attach off))
    (padstack RoundRect[T]Pad_2000.000000x1500.000000_376.427000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -1001.43 375 -985.778 482.408 -931.355 593.733 -843.733 681.355 -732.408 735.778 -624.999 751.426 625 751.427 732.408 735.778 843.733 681.355 931.355 593.733 985.778 482.408 1001.43 374.999 1001.43 -375 985.778 -482.408 931.355 -593.733 843.733 -681.355 732.408 -735.778 624.999 -751.426 -625 -751.427 -732.408 -735.778 -843.733 -681.355 -931.355 -593.733 -985.778 -482.408 -1001.43 -374.999 -1001.43 375))
      (attach off))
    (padstack RoundRect[T]Pad_2000.000000x3800.000000_501.903000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -1001.9 1400 -990.433 1506.69 -940.51 1640.54 -854.899 1754.9 -740.537 1840.51 -606.687 1890.43 -499.999 1901.9 500 1901.9 606.687 1890.43 740.537 1840.51 854.899 1754.9 940.51 1640.54 990.433 1506.69 1001.9 1400 1001.9 -1400 990.433 -1506.69 940.51 -1640.54 854.899 -1754.9 740.537 -1840.51 606.687 -1890.43 499.999 -1901.9 -500 -1901.9 -606.687 -1890.43 -740.537 -1840.51 -854.899 -1754.9 -940.51 -1640.54 -990.433 -1506.69 -1001.9 -1400 -1001.9 1400))
      (attach off))
    (padstack RoundRect[T]Pad_2500.000000x1800.000000_250.951000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -1250.95 650 -1231.85 746.035 -1177.45 827.449 -1096.04 881.848 -999.999 900.95 1000 900.951 1096.04 881.848 1177.45 827.449 1231.85 746.035 1250.95 649.999 1250.95 -650 1231.85 -746.035 1177.45 -827.449 1096.04 -881.848 999.999 -900.95 -1000 -900.951 -1096.04 -881.848 -1177.45 -827.449 -1231.85 -746.035 -1250.95 -649.999 -1250.95 650))
      (attach off))
    (padstack RoundRect[T]Pad_800.000000x950.000000_200.761000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -400.761 275 -385.479 351.828 -341.959 416.959 -276.828 460.479 -199.999 475.76 200 475.761 276.828 460.479 341.959 416.959 385.479 351.828 400.76 274.999 400.761 -275 385.479 -351.828 341.959 -416.959 276.828 -460.479 199.999 -475.76 -200 -475.761 -276.828 -460.479 -341.959 -416.959 -385.479 -351.828 -400.76 -274.999 -400.761 275))
      (attach off))
    (padstack RoundRect[T]Pad_875.000000x950.000000_219.582000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -438.332 256.25 -421.617 340.28 -374.018 411.518 -302.78 459.117 -218.75 475.832 218.75 475.832 302.78 459.117 374.018 411.518 421.617 340.28 438.332 256.25 438.332 -256.25 421.617 -340.28 374.018 -411.518 302.78 -459.117 218.75 -475.832 -218.75 -475.832 -302.78 -459.117 -374.018 -411.518 -421.617 -340.28 -438.332 -256.25 -438.332 256.25))
      (attach off))
    (padstack RoundRect[T]Pad_900.000000x950.000000_225.856000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -450.856 250 -433.664 336.431 -384.704 409.704 -311.431 458.664 -225 475.856 225 475.856 311.431 458.664 384.704 409.704 433.664 336.431 450.856 250 450.856 -250 433.664 -336.431 384.704 -409.704 311.431 -458.664 225 -475.856 -225 -475.856 -311.431 -458.664 -384.704 -409.704 -433.664 -336.431 -450.856 -250 -450.856 250))
      (attach off))
    (padstack RoundRect[T]Pad_1000.000000x1450.000000_250.951000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -500.951 475 -481.848 571.035 -427.449 652.449 -346.035 706.848 -249.999 725.95 250 725.951 346.035 706.848 427.449 652.449 481.848 571.035 500.95 474.999 500.951 -475 481.848 -571.035 427.449 -652.449 346.035 -706.848 249.999 -725.95 -250 -725.951 -346.035 -706.848 -427.449 -652.449 -481.848 -571.035 -500.95 -474.999 -500.951 475))
      (attach off))
    (padstack RoundRect[T]Pad_1150.000000x1800.000000_250.951000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -575.951 650 -556.848 746.035 -502.449 827.449 -421.035 881.848 -324.999 900.95 325 900.951 421.035 881.848 502.449 827.449 556.848 746.035 575.95 649.999 575.951 -650 556.848 -746.035 502.449 -827.449 421.035 -881.848 324.999 -900.95 -325 -900.951 -421.035 -881.848 -502.449 -827.449 -556.848 -746.035 -575.95 -649.999 -575.951 650))
      (attach off))
    (padstack RoundRect[T]Pad_1250.000000x1750.000000_250.951000_um_0.000000_0
      (shape
        (polygon F.Cu 0 -625.951 625 -606.848 721.035 -552.449 802.449 -471.035 856.848 -374.999 875.95 375 875.951 471.035 856.848 552.449 802.449 606.848 721.035 625.95 624.999 625.951 -625 606.848 -721.035 552.449 -802.449 471.035 -856.848 374.999 -875.95 -375 -875.951 -471.035 -856.848 -552.449 -802.449 -606.848 -721.035 -625.95 -624.999 -625.951 625))
      (attach off))
    (padstack Rect[A]Pad_1700.000000x1700.000000_um
      (shape
        (rect F.Cu -850 -850 850 850))
      (shape
        (rect B.Cu -850 -850 850 850))
      (attach off))
    (padstack "Via[0-1]_600:300_um"
      (shape
        (circle F.Cu 600))
      (shape
        (circle B.Cu 600))
      (attach off)))
  (network
    (net "Net-(D2-A)"
      (pins R3-2 D2-2))
    (class kicad_default "Net-(D2-A)"
      (circuit
        (use_via "Via[0-1]_600:300_um"))
      (rule
        (width 200)
        (clearance 200))))
  (wiring))
'@

# Does Freerouting at $Program (with $Java for a jar) route the test board within the time limit?
function Test-Freerouting([string]$Program, [string]$Java) {
    if (-not $Program -or -not (Test-Path $Program -PathType Leaf)) { return $false }
    if ($Program -like '*.jar' -and -not $Java) { return $false }
    $dir = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-frcheck-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $dir | Out-Null
    try {
        $dsn = Join-Path $dir 'check.dsn'
        $ses = Join-Path $dir 'check.ses'
        [IO.File]::WriteAllText($dsn, $FreeroutingTestBoard)
        $routerArgs = @('-de', "`"$dsn`"", '-do', "`"$ses`"", '--gui.enabled=false', '--api_server.enabled=false',
            '--mcp_server.enabled=false', '--usage_and_diagnostic_data.disable_analytics=true')
        $file = $Program
        if ($Program -like '*.jar') { $file = $Java; $routerArgs = @('-jar', "`"$Program`"") + $routerArgs }
        $p = Start-Process -FilePath $file -ArgumentList $routerArgs -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $dir 'out.txt') -RedirectStandardError (Join-Path $dir 'err.txt')
        if (-not $p.WaitForExit($FreeroutingCheckSeconds * 1000)) {
            try { $p.Kill() } catch { }
            Start-Sleep -Milliseconds 500
            return $false
        }
        return (Test-Path $ses)
    }
    catch { return $false }
    finally { Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
}

# The major version of a Java, or 0.
function Get-JavaMajor([string]$Java) {
    try {
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $text = (& $Java -version 2>&1 | Out-String)
        $ErrorActionPreference = $previous
        if ($text -match 'version "(\d+)') { return [int]$Matches[1] }
    } catch { }
    return 0
}

# A Java 25 or newer on this machine, or $null.
function Find-Java {
    $candidates = @()
    if ($JavaPath) { $candidates += $JavaPath }
    if ($env:JAVA_HOME) { $candidates += (Join-Path $env:JAVA_HOME 'bin\java.exe') }
    $onPath = Get-Command java -ErrorAction SilentlyContinue
    if ($onPath) { $candidates += $onPath.Source }
    $own = Join-Path (Split-Path $InstallDir) 'java'
    if (Test-Path $own) { $candidates += @(Get-ChildItem $own -Recurse -Filter java.exe -ErrorAction SilentlyContinue | ForEach-Object FullName) }
    foreach ($root in @("$env:ProgramFiles\Java", "$env:ProgramFiles\Eclipse Adoptium", "$env:ProgramFiles\Zulu", "$env:ProgramFiles\Microsoft")) {
        if (Test-Path $root) { $candidates += @(Get-ChildItem $root -Recurse -Filter java.exe -ErrorAction SilentlyContinue | ForEach-Object FullName) }
    }
    foreach ($c in ($candidates | Select-Object -Unique)) {
        if ((Test-Path $c) -and (Get-JavaMajor $c) -ge 25) { return $c }
    }
    return $null
}

# Downloads $Url to $Path and checks its SHA-256.
function Get-Checked([string]$Url, [string]$Path, [string]$Sha256, [string]$What) {
    $headers = @{ 'User-Agent' = 'thingsvu-installer' }
    Invoke-WebRequest $Url -OutFile $Path -Headers $headers -UseBasicParsing
    $actual = (Get-FileHash $Path -Algorithm SHA256).Hash.ToLower()
    if (-not $Sha256 -or $actual -ne $Sha256.ToLower()) { throw "The $What download does not match its published checksum; not using it." }
    Write-Host "  checksum verified"
}

function Install-Freerouting {
    $dataDir = Split-Path $InstallDir
    New-Item -ItemType Directory -Force $dataDir | Out-Null
    $configFile = Join-Path $dataDir 'config.json'
    $program = $null
    $java = $null

    # 1. A copy the user names.
    if ($FreeroutingPath) {
        if (-not (Test-Path $FreeroutingPath -PathType Leaf)) { throw "No Freerouting at $FreeroutingPath." }
        $program = (Resolve-Path $FreeroutingPath).Path
        if ($program -like '*.jar') { $java = Find-Java }
        if (-not $SkipFreeroutingCheck) {
            Write-Host "Checking that $program routes a test board..."
            if (-not (Test-Freerouting $program $java)) {
                throw "Freerouting at $program did not route the test board$(if ($program -like '*.jar' -and -not $java) { ' (no Java 25 found: pass -JavaPath)' })."
            }
        }
    }

    # 2. One that is already here.
    if (-not $program -and -not $SkipFreeroutingCheck -and -not $DownloadFreerouting) {
        $found = @()
        Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object { $_.CommandLine -match 'freerouting[^\\/]*\.jar' } | ForEach-Object {
            if ($_.CommandLine -match '"?([^"]*?java[w]?\.exe)"?.*?-jar\s+"?([^"]*?freerouting[^"\\/]*\.jar)') {
                $found += , @($Matches[2], ($Matches[1] -replace 'javaw\.exe$', 'java.exe'))
            }
        }
        foreach ($root in @("$env:LOCALAPPDATA\Programs\Freerouting", "$env:ProgramFiles\Freerouting", "$env:LOCALAPPDATA\Freerouting")) {
            $jar = Get-ChildItem $root -Recurse -Filter 'freerouting*.jar' -ErrorAction SilentlyContinue | Select-Object -First 1
            $rt = Get-ChildItem $root -Recurse -Filter 'java.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($jar -and $rt) { $found += , @($jar.FullName, $rt.FullName) }
        }
        foreach ($f in $found) {
            Write-Host "Found Freerouting at $($f[0]); checking that it routes a test board..."
            if (Test-Freerouting $f[0] $f[1]) { $program = $f[0]; $java = $f[1]; break }
        }
    }

    # 3 and 4. From its release.
    if (-not $program) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $headers = @{ 'User-Agent' = 'thingsvu-installer' }
        $frApi = "$ApiBase/repos/$FreeroutingRepository/releases"
        $release = if ($FreeroutingVersion -eq 'latest') {
            Invoke-RestMethod "$frApi/latest" -Headers $headers
        } else {
            $tag = if ($FreeroutingVersion.StartsWith('v')) { $FreeroutingVersion } else { "v$FreeroutingVersion" }
            Invoke-RestMethod "$frApi/tags/$tag" -Headers $headers
        }
        $digest = { param($a) if ($a.digest -match '^sha256:([0-9a-fA-F]{64})$') { $Matches[1] } }
        $target = Join-Path $dataDir 'freerouting'
        $work = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-freerouting-" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory $work | Out-Null
        try {
            # 3. The native build: no Java needed, when it works.
            $cli = $release.assets | Where-Object { $_.name -like 'freerouting-cli-*-windows-x64.zip' } | Select-Object -First 1
            $sums = $release.assets | Where-Object { $_.name -like 'freerouting-cli-*checksums*' } | Select-Object -First 1
            if ($cli -and $sums) {
                Write-Host "Downloading $($cli.name) ($([math]::Round($cli.size / 1MB)) MB) from github.com/$FreeroutingRepository ($($release.tag_name))"
                $sumsFile = Join-Path $work $sums.name
                Invoke-WebRequest $sums.browser_download_url -OutFile $sumsFile -Headers $headers -UseBasicParsing
                $expected = Get-Content $sumsFile | ForEach-Object {
                    if ($_ -match '([0-9a-fA-F]{64})\s+\*?(\S+)' -and (Split-Path $Matches[2] -Leaf) -eq $cli.name) { $Matches[1] }
                } | Select-Object -First 1
                $zip = Join-Path $work $cli.name
                Get-Checked $cli.browser_download_url $zip $expected 'Freerouting'
                if (Test-Path $target) { Remove-Item -Recurse -Force $target }
                Expand-Archive $zip $target
                $exe = Get-ChildItem $target -Recurse -File -Filter 'freerouting*.exe' | Select-Object -First 1
                if ($exe) {
                    if ($SkipFreeroutingCheck) { $program = $exe.FullName }
                    else {
                        Write-Host 'Checking that it routes a test board...'
                        if (Test-Freerouting $exe.FullName $null) { $program = $exe.FullName }
                        else {
                            Write-Host "  Freerouting's native build $($release.tag_name) does not route KiCad boards on this machine; using its Java build instead."
                            Remove-Item -Recurse -Force $target -ErrorAction SilentlyContinue
                        }
                    }
                }
            }
            # 4. The jar, with Java.
            if (-not $program) {
                $jarAsset = $release.assets | Where-Object { $_.name -match '^freerouting-[\d.]+\.jar$' } | Select-Object -First 1
                if (-not $jarAsset) { throw "Freerouting $($release.tag_name) has no build this installer can use. Install Freerouting yourself and run again with -FreeroutingPath." }
                $java = Find-Java
                if (-not $java) {
                    # Eclipse Temurin's Java 25 runtime, only for Freerouting.
                    $jre = (Invoke-RestMethod "$AdoptiumApi/v3/assets/latest/25/hotspot?os=windows&architecture=x64&image_type=jre&vendor=eclipse" -Headers $headers) | Select-Object -First 1
                    $pkg = $jre.binary.package
                    Write-Host "No Java 25 found. Downloading $($pkg.name) ($([math]::Round($pkg.size / 1MB)) MB) from Eclipse Adoptium, for Freerouting only"
                    $jreZip = Join-Path $work $pkg.name
                    Get-Checked $pkg.link $jreZip $pkg.checksum 'Java runtime'
                    $javaDir = Join-Path $dataDir 'java'
                    if (Test-Path $javaDir) { Remove-Item -Recurse -Force $javaDir }
                    Expand-Archive $jreZip $javaDir
                    $java = (Get-ChildItem $javaDir -Recurse -Filter java.exe | Select-Object -First 1).FullName
                }
                Write-Host "Downloading $($jarAsset.name) ($([math]::Round($jarAsset.size / 1MB)) MB) from github.com/$FreeroutingRepository"
                New-Item -ItemType Directory -Force $target | Out-Null
                $jarPath = Join-Path $target $jarAsset.name
                Get-Checked $jarAsset.browser_download_url $jarPath (& $digest $jarAsset) 'Freerouting'
                if (-not $SkipFreeroutingCheck) {
                    Write-Host 'Checking that it routes a test board...'
                    if (-not (Test-Freerouting $jarPath $java)) { throw "Freerouting $($release.tag_name) did not route the test board with Java at $java." }
                }
                $program = $jarPath
            }
        }
        finally { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    }
    if (-not $program) { throw 'No working Freerouting was found or installed.' }
    Write-Host "Using Freerouting: $program$(if ($java) { " (Java: $java)" })"

    $settings = [ordered]@{}
    if (Test-Path $configFile) {
        try {
            $old = Get-Content $configFile -Raw | ConvertFrom-Json
            foreach ($p in $old.PSObject.Properties) { $settings[$p.Name] = $p.Value }
        } catch { Write-Warning "Replacing an unreadable $configFile" }
    }
    $settings['freerouting'] = $program
    if ($java) { $settings['java'] = $java } elseif ($settings.Contains('java')) { $settings.Remove('java') }
    $settings | ConvertTo-Json | Set-Content -Encoding UTF8 $configFile
    Write-Host "Autorouting is on: $configFile names Freerouting. Restart your assistant to pick it up."
}

if ($FreeroutingOnly) {
    Step 'Setting up the autorouter (Freerouting)'
    Install-Freerouting
    return
}

# --- 1. Find the files ----------------------------------------------------------------------
Step 'Finding the files'
$package = $null
if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'bin\kicad-ai-mcp.exe'))) {
    $package = $PSScriptRoot
    Write-Host "Using the files next to this script ($package)"
}
else {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $headers = @{ 'User-Agent' = 'thingsvu-installer' }
    $api = "$ApiBase/repos/$Repository/releases"
    if ($Version -eq 'latest') {
        # /releases/latest skips pre-releases, which every 0.x release is.
        # Do not pipe the response into Where-Object: Windows PowerShell 5.1 passes a JSON array down
        # the pipeline as ONE object, so the filter would test the array, not each release.
        $release = $null
        $listed = Invoke-RestMethod "${api}?per_page=10" -Headers $headers
        foreach ($candidate in $listed) {
            if (-not $candidate.draft) { $release = $candidate; break }
        }
    }
    else {
        $tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }
        $release = Invoke-RestMethod "$api/tags/$tag" -Headers $headers
    }
    if (-not $release) {
        throw "No published release found in $Repository yet. A release appears a few minutes after a new version is pushed; try again shortly, or download the zip from https://github.com/$Repository/releases."
    }
    $zipAsset = $release.assets | Where-Object { $_.name -like '*-windows-x64.zip' } | Select-Object -First 1
    $sumsAsset = $release.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' } | Select-Object -First 1
    if (-not $zipAsset -or -not $sumsAsset) { throw "Release $($release.tag_name) has no Windows package." }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("thingsvu-install-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $work | Out-Null
    Write-Host "Downloading $($zipAsset.name) ($($release.tag_name))"
    $zipPath = Join-Path $work $zipAsset.name
    Invoke-WebRequest $zipAsset.browser_download_url -OutFile $zipPath -Headers $headers -UseBasicParsing
    Invoke-WebRequest $sumsAsset.browser_download_url -OutFile (Join-Path $work 'SHA256SUMS.txt') -Headers $headers -UseBasicParsing

    $expected = Get-Content (Join-Path $work 'SHA256SUMS.txt') | ForEach-Object {
        if ($_ -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $Matches[2] -eq $zipAsset.name) { $Matches[1].ToLower() }
    } | Select-Object -First 1
    $actual = (Get-FileHash $zipPath -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected -ne $actual) { throw "The download does not match its published checksum; not installing." }
    Write-Host 'Checksum verified.'
    $package = Join-Path $work 'files'
    Expand-Archive $zipPath $package
}
$mcpSource = Join-Path $package 'bin\kicad-ai-mcp.exe'
$pluginExe = Join-Path $package 'bin\kicad-ai-plugin.exe'
$pluginFiles = Join-Path $package 'plugin'
foreach ($f in @($mcpSource, $pluginExe, (Join-Path $pluginFiles 'plugin.json'))) {
    if (-not (Test-Path $f)) { throw "The package is incomplete: $f is missing." }
}

# --- 2. The MCP server ----------------------------------------------------------------------
Step 'Installing the MCP server'
New-Item -ItemType Directory -Force $InstallDir | Out-Null
$mcp = Join-Path $InstallDir 'kicad-ai-mcp.exe'
try { Copy-Item $mcpSource $mcp -Force }
catch { throw "Cannot replace $mcp. An AI assistant is probably running it: close the assistant and run the installer again." }
Write-Host "Installed $mcp"
# Keep the uninstaller where it can be found later, even after a one-line install.
$uninstaller = Join-Path $package 'uninstall.ps1'
if (Test-Path $uninstaller) { Copy-Item $uninstaller (Join-Path (Split-Path $InstallDir) 'uninstall.ps1') -Force }

# --- 3. The KiCad plugin --------------------------------------------------------------------
$pluginTargets = @()
if (-not $NoPlugin) {
    Step 'Installing the KiCad plugin'
    $versions = $KicadVersion
    if (-not $versions) {
        $kicadDocs = Join-Path $DocumentsDir 'KiCad'
        $found = @(Get-ChildItem $kicadDocs -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^\d+\.\d+$' })
        $versions = @($found | Where-Object { [version]($_.Name + '.0') -ge [version]'10.99.0' } | ForEach-Object { $_.Name })
        if ($found | Where-Object { $_.Name -match '^10\.\d$' -and [version]($_.Name + '.0') -lt [version]'10.99.0' }) {
            Write-Host 'KiCad 10 found: it cannot run schematic-editor plugins, so no plugin is installed there.'
            Write-Host 'The MCP server still works with KiCad 10 through the schematic file (provider "file").'
        }
    }
    if (-not $versions) {
        Write-Host "No KiCad 11 user folder found under $DocumentsDir\KiCad."
        Write-Host 'Start KiCad 11 once, then run this installer again (or pass -KicadVersion 11.0).'
    }
    foreach ($v in $versions) {
        $target = Join-Path $DocumentsDir "KiCad\$v\plugins\com.thingsvu.kicad-ai"
        New-Item -ItemType Directory -Force (Join-Path $target 'icons') | Out-Null
        Copy-Item $pluginExe (Join-Path $target 'kicad-ai-plugin.exe') -Force
        Copy-Item (Join-Path $pluginFiles 'plugin.json') (Join-Path $target 'plugin.json') -Force
        Copy-Item (Join-Path $pluginFiles 'icons\*.png') (Join-Path $target 'icons') -Force
        Write-Host "Installed the plugin for KiCad $v ($target)"
        $pluginTargets += $v
    }
}

# --- 4. AI assistants -----------------------------------------------------------------------
$configured = @()
if (-not $SkipClients) {
    Step 'Connecting your AI assistant'
    if ($Clients -contains 'ClaudeCode') {
        $claude = Get-Command claude -ErrorAction SilentlyContinue
        if (-not $claude) {
            Write-Host 'Claude Code: not found on PATH (skipping).'
        }
        elseif (Ask 'Add the KiCad tools to Claude Code?') {
            # Replace an earlier entry; it is fine if there was none.
            $null = Invoke-Native 'claude' @('mcp', 'remove', 'kicad-ai', '--scope', 'user')
            $code = Invoke-Native 'claude' @('mcp', 'add', '--scope', 'user', 'kicad-ai', '--', $mcp)
            if ($code -eq 0) { $configured += 'Claude Code'; Write-Host 'Claude Code: added "kicad-ai".' }
            else { Write-Warning 'Claude Code: "claude mcp add" failed; add it by hand (see the README).' }
        }
    }
    if ($Clients -contains 'ClaudeDesktop') {
        # The Microsoft Store edition keeps its settings in its own package folder, not in %APPDATA%.
        if (-not $PSBoundParameters.ContainsKey('ClaudeDesktopConfig')) {
            $store = Get-ChildItem (Join-Path $env:LOCALAPPDATA 'Packages') -Filter 'Claude_*' -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($store) {
                $storeDir = Join-Path $store.FullName 'LocalCache\Roaming\Claude'
                if ((Test-Path $storeDir) -and -not (Test-Path (Split-Path $ClaudeDesktopConfig))) {
                    $ClaudeDesktopConfig = Join-Path $storeDir 'claude_desktop_config.json'
                }
            }
        }
        if (-not (Test-Path (Split-Path $ClaudeDesktopConfig))) {
            Write-Host 'Claude Desktop: not found (skipping).'
        }
        elseif (Ask 'Add the KiCad tools to Claude Desktop?') {
            $raw = if (Test-Path $ClaudeDesktopConfig) { Get-Content $ClaudeDesktopConfig -Raw } else { '' }
            $cfg = $null
            $valid = $true
            if ($raw -and $raw.Trim()) {
                try { $cfg = $raw | ConvertFrom-Json }
                catch { $valid = $false; Write-Warning "Claude Desktop: $ClaudeDesktopConfig is not valid JSON; leaving it alone." }
            }
            if ($valid) {
                if ($null -eq $cfg) { $cfg = [pscustomobject]@{} }
                if (-not ($cfg.PSObject.Properties.Name -contains 'mcpServers')) {
                    $cfg | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{})
                }
                $entry = [pscustomobject]@{ command = $mcp; args = @() }
                if ($cfg.mcpServers.PSObject.Properties.Name -contains 'kicad-ai') { $cfg.mcpServers.'kicad-ai' = $entry }
                else { $cfg.mcpServers | Add-Member -NotePropertyName 'kicad-ai' -NotePropertyValue $entry }
                if (Test-Path $ClaudeDesktopConfig) {
                    $backup = "$ClaudeDesktopConfig.bak-" + (Get-Date -Format 'yyyyMMddHHmmss')
                    Copy-Item $ClaudeDesktopConfig $backup
                    Write-Host "Claude Desktop: kept your old settings in $backup"
                }
                [IO.File]::WriteAllText($ClaudeDesktopConfig, ($cfg | ConvertTo-Json -Depth 20), (New-Object Text.UTF8Encoding $false))
                $configured += 'Claude Desktop'
                Write-Host 'Claude Desktop: added "kicad-ai".'
            }
        }
    }
}

# --- 4a. Parts lookup: the pcbparts MCP -----------------------------------------------------------
# thingsvu-tools finds which parts a BOM still needs (bom_review, parts_lookup); the search itself is
# done by the pcbparts MCP, a separate free service (no key). Part values and package names go to
# it when the assistant searches, so it is asked about every time.
$partsNote = $null
if (-not $SkipClients -and -not $NoPartsLookup) {
    Step 'Looking up parts (pcbparts MCP)'
    Write-Host 'To fill in manufacturer and supplier part numbers, your assistant needs a parts-lookup service.'
    Write-Host "thingsvu-tools works with the pcbparts MCP ($PartsLookupUrl): JLCPCB, Mouser and DigiKey parts, stock and prices."
    Write-Host 'It is a separate service run by someone else; the part values and packages your assistant searches for are sent to it. Nothing else is.'
    $claudeCmd = Get-Command claude -ErrorAction SilentlyContinue
    if ($Clients -contains 'ClaudeCode' -and $claudeCmd) {
        if (Ask 'Add the pcbparts MCP to Claude Code?') {
            $null = Invoke-Native 'claude' @('mcp', 'remove', 'pcbparts', '--scope', 'user')
            $code = Invoke-Native 'claude' @('mcp', 'add', '--scope', 'user', '--transport', 'http', 'pcbparts', $PartsLookupUrl)
            if ($code -eq 0) { $partsNote = 'added to Claude Code'; Write-Host 'Claude Code: added "pcbparts".' }
            else { Write-Warning "Claude Code: could not add pcbparts; run: claude mcp add --transport http pcbparts $PartsLookupUrl" }
        }
        else { $partsNote = "not added; to add it later: claude mcp add --transport http pcbparts $PartsLookupUrl" }
    }
    if ($Clients -contains 'ClaudeDesktop') {
        Write-Host "Claude Desktop: add $PartsLookupUrl yourself under Settings > Connectors > Add custom connector (this installer does not edit that for remote servers)."
        if (-not $partsNote) { $partsNote = "for Claude Desktop add $PartsLookupUrl as a custom connector" }
    }
}

# --- 4b. The autorouter (optional) -----------------------------------------------------------
$routerNote = $null
if ($WithFreerouting -or $FreeroutingPath) {
    Step 'Setting up the autorouter (Freerouting)'
    try { Install-Freerouting; $routerNote = 'on (Freerouting)' }
    catch { Write-Warning "Autorouting was not set up: $($_.Exception.Message)"; $routerNote = 'not set up (see the warning above)' }
}
elseif (-not $Yes) {
    $answer = $null
    try { $answer = Read-Host "Also set up whole-board autorouting? It uses Freerouting, a separate open-source program from github.com/freerouting/freerouting (and a Java runtime from Adoptium if you have none) [y/N]" } catch { }
    if ($answer -match '^[yY]') {
        Step 'Setting up the autorouter (Freerouting)'
        try { Install-Freerouting; $routerNote = 'on (Freerouting)' }
        catch { Write-Warning "Autorouting was not set up: $($_.Exception.Message)"; $routerNote = 'not set up (see the warning above)' }
    }
}

# --- 5. What next ---------------------------------------------------------------------------
Step 'Done'
Write-Host "MCP server:  $mcp"
if ($pluginTargets) { Write-Host ('KiCad plugin: installed for KiCad ' + ($pluginTargets -join ', ')) }
if ($configured) { Write-Host ('Connected:   ' + ($configured -join ', ') + ' (restart it to pick up the new tools)') }
if ($routerNote) { Write-Host "Autorouter:  $routerNote" }
if ($partsNote) { Write-Host "Parts lookup: $partsNote" }
Write-Host @"

Next steps
  1. Other AI assistants: run this server over stdio. The command is
       $mcp
     For VS Code, add it to .vscode/mcp.json:
       { "servers": { "kicad-ai": { "type": "stdio", "command": "$($mcp -replace '\\', '/')" } } }
  2. KiCad 11 (live editing): Preferences > Plugins > enable the KiCad API, restart KiCad, open the
     Schematic Editor, and press the "KiCad AI: connect" toolbar button.
  3. KiCad 10 or no KiCad running: ask your assistant to open the .kicad_sch file with the "file" provider.
  4. Part numbers: add the pcbparts MCP ($PartsLookupUrl) to your assistant if you have not; then ask
     "Review the BOM and fill in the part numbers".
  5. Try it: "Open <your project>.kicad_sch and call session_status".
"@
