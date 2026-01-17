# Terminal Frame Timing Benchmark
# Compares scrolling smoothness across terminal emulators using PresentMon

param(
    [int]$Duration = 10,  # seconds to record
    [string]$OutputDir = ".\benchmark_results"
)

$ErrorActionPreference = "Stop"

# Create output directory
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$resultDir = Join-Path $OutputDir $timestamp
New-Item -ItemType Directory -Force -Path $resultDir | Out-Null

Write-Host "Benchmark results will be saved to: $resultDir" -ForegroundColor Cyan

# Check for PresentMon CLI
$presentMonPath = $null
$searchPaths = @(
    "$env:ProgramFiles\Intel\PresentMon\PresentMonConsoleApplication\PresentMon-2.4.0-x64.exe",
    "$env:ProgramFiles\Intel\PresentMon\PresentMonConsoleApplication\PresentMon*.exe",
    "$env:ProgramFiles\Intel\PresentMon\PresentMonApplication\PresentMon.exe",
    "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\Intel.PresentMon_*\PresentMon.exe"
)
foreach ($path in $searchPaths) {
    $found = Get-Item $path -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found) {
        $presentMonPath = $found.FullName
        break
    }
}
if (-not $presentMonPath) {
    $presentMon = Get-Command PresentMon.exe -ErrorAction SilentlyContinue
    if ($presentMon) { $presentMonPath = $presentMon.Source }
}
if (-not $presentMonPath) {
    Write-Host "PresentMon not found. Install with: winget install Intel.PresentMon" -ForegroundColor Red
    exit 1
}
Write-Host "Using PresentMon: $presentMonPath" -ForegroundColor Green

# Scrolling command - outputs many lines to trigger scrolling
# Using PowerShell to generate output (works in all terminals)
$scrollCommand = "powershell -NoProfile -Command `"1..50000 | ForEach-Object { Write-Host ('Line ' + `$_) }`""

# Terminal configurations
# Set $EnabledTerminals to control which terminals to test
$EnabledTerminals = @("WezTerm")  # Options: "WezTerm", "WindowsTerminal", "WindowsTerminalDev", "Alacritty"

$scrollScript = "S:\projects\wezterm\benchmark_scroll.cmd"

# Get screen dimensions for consistent window positioning (left half of screen)
Add-Type -AssemblyName System.Windows.Forms
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$winWidth = [math]::Floor($screen.Width / 2)
$winHeight = $screen.Height
$winX = 0
$winY = 0

$allTerminals = @(
    @{
        Name = "WezTerm"
        Process = "wezterm-gui"
        Executable = "S:\projects\wezterm\target\release\wezterm-gui.exe"
        # WezTerm: user's config handles window positioning
        Arguments = @("start", "--cwd", ".", "--", "cmd", "/c", $scrollScript)
    },
    @{
        Name = "WindowsTerminal"
        Process = "WindowsTerminal"
        # Windows Terminal doesn't support position via CLI
        Executable = "wt.exe"
        Arguments = @("-w", "new", "cmd", "/c", $scrollScript)
    },
    @{
        Name = "WindowsTerminalDev"
        Process = "WindowsTerminal"
        # Windows Terminal doesn't support position via CLI
        Executable = "wtd.exe"
        Arguments = @("-w", "new", "cmd", "/c", $scrollScript)
    },
    @{
        Name = "Alacritty"
        Process = "alacritty"
        Executable = "C:\Program Files\Alacritty\alacritty.exe"
        # Alacritty: user's config handles window positioning
        Arguments = @("-e", "cmd", "/c", $scrollScript)
    }
)

$terminals = $allTerminals | Where-Object { $EnabledTerminals -contains $_.Name }

# Results storage
$results = @()

foreach ($terminal in $terminals) {
    Write-Host "`n========================================" -ForegroundColor Yellow
    Write-Host "Testing: $($terminal.Name)" -ForegroundColor Yellow
    Write-Host "========================================" -ForegroundColor Yellow

    $csvFile = Join-Path $resultDir "$($terminal.Name).csv"
    $warmupSeconds = 5  # Let terminal start and stabilize

    # Remember existing processes so we don't kill them later
    $existingProcs = Get-Process -Name $terminal.Process -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id

    # Launch terminal FIRST
    Write-Host "Launching terminal..." -ForegroundColor Gray
    Write-Host "  $($terminal.Executable) $($terminal.Arguments -join ' ')" -ForegroundColor DarkGray
    $proc = Start-Process -FilePath $terminal.Executable -ArgumentList $terminal.Arguments -PassThru -WindowStyle Normal

    # Wait for terminal to start
    Start-Sleep -Seconds 2

    # Find the NEW terminal process (not the ones that existed before)
    $allProcs = Get-Process -Name $terminal.Process -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
    $newProcIds = $allProcs | Where-Object { $existingProcs -notcontains $_ }

    if ($newProcIds.Count -eq 0) {
        Write-Host "Could not find new process: $($terminal.Process)" -ForegroundColor Red
        continue
    }

    $terminalProcId = $newProcIds | Select-Object -First 1
    Write-Host "  Tracking process ID: $terminalProcId" -ForegroundColor Gray

    # Position window to left half of screen (optional, errors are non-fatal)
    $termProc = Get-Process -Id $terminalProcId -ErrorAction SilentlyContinue
    if ($termProc -and $termProc.MainWindowHandle -ne [IntPtr]::Zero) {
        try {
            $hwnd = $termProc.MainWindowHandle
            $moveWindowSig = '[DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hWnd, int X, int Y, int nWidth, int nHeight, bool bRepaint);'
            $moveWindow = Add-Type -MemberDefinition $moveWindowSig -Name "WinAPI_$([guid]::NewGuid().ToString('N'))" -Namespace Win32 -PassThru -ErrorAction Stop
            $moveWindow::MoveWindow($hwnd, $winX, $winY, $winWidth, $winHeight, $true) | Out-Null
            Write-Host "  Window positioned to left half of screen" -ForegroundColor Gray
        } catch {
            Write-Host "  (Window positioning skipped - not critical)" -ForegroundColor DarkGray
        }
    }

    # Warmup period - let terminal and scrolling stabilize
    Write-Host "Warmup period ($warmupSeconds seconds)..." -ForegroundColor Gray
    for ($i = 0; $i -lt $warmupSeconds; $i++) {
        Start-Sleep -Seconds 1
        Write-Host "." -NoNewline
    }
    Write-Host ""

    # NOW start PresentMon (terminal is already scrolling steadily)
    # Run it hidden to avoid overlapping the terminal window
    Write-Host "Starting measurement ($Duration seconds)..." -ForegroundColor Gray
    $pmArgs = @(
        "--stop_existing_session"
        "--timed", $Duration
        "--process_name", "$($terminal.Process).exe"
        "--output_file", $csvFile
        "--no_console_stats"
    )
    Write-Host "  $presentMonPath $($pmArgs -join ' ')" -ForegroundColor DarkGray
    $pmProc = Start-Process -FilePath $presentMonPath -ArgumentList $pmArgs -PassThru -WindowStyle Hidden

    # Wait for PresentMon to finish, sampling CPU usage every 200ms
    $cpuSamples = @()
    $lastCpuTime = $null
    $lastSampleTime = $null
    $sampleIntervalMs = 200
    $maxWaitMs = ($Duration + 5) * 1000  # Timeout after Duration + 5 seconds
    $startWait = Get-Date

    while (-not $pmProc.HasExited) {
        # Check timeout
        $elapsed = ((Get-Date) - $startWait).TotalMilliseconds
        if ($elapsed -gt $maxWaitMs) {
            Write-Host " timeout" -ForegroundColor Yellow
            break
        }

        # Sample CPU usage for the terminal process
        $termProc = Get-Process -Id $terminalProcId -ErrorAction SilentlyContinue
        if ($termProc) {
            $currentCpuTime = $termProc.TotalProcessorTime.TotalMilliseconds
            $currentTime = Get-Date

            if ($lastCpuTime -ne $null -and $lastSampleTime -ne $null) {
                $cpuDelta = $currentCpuTime - $lastCpuTime
                $timeDelta = ($currentTime - $lastSampleTime).TotalMilliseconds
                if ($timeDelta -gt 0) {
                    # CPU% = (CPU time used / wall time) * 100
                    $cpuPercent = ($cpuDelta / $timeDelta) * 100
                    $cpuSamples += $cpuPercent
                }
            }
            $lastCpuTime = $currentCpuTime
            $lastSampleTime = $currentTime
        }

        Start-Sleep -Milliseconds $sampleIntervalMs

        # Progress indicator every second
        if ($cpuSamples.Count % 5 -eq 0) {
            Write-Host "." -NoNewline
        }
    }

    # Force kill PresentMon if still running
    if (-not $pmProc.HasExited) {
        $pmProc | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Write-Host " done"

    # Calculate average CPU usage
    $avgCpu = 0
    if ($cpuSamples.Count -gt 0) {
        $avgCpu = ($cpuSamples | Measure-Object -Average).Average
    }
    Write-Host "  CPU samples: $($cpuSamples.Count), Avg: $([math]::Round($avgCpu, 1))%" -ForegroundColor Gray

    # Kill only the terminal we launched (not others)
    Get-Process -Id $terminalProcId -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

    # Parse results
    if (Test-Path $csvFile) {
        Write-Host "Analyzing results..." -ForegroundColor Gray

        $data = Import-Csv $csvFile

        # PresentMon 2.x uses different column names than 1.x
        # Try both: "msBetweenPresents" (v2) and "MsBetweenPresents" (v1)
        $frameTimeColumn = if ($data[0].PSObject.Properties.Name -contains "msBetweenPresents") {
            "msBetweenPresents"
        } elseif ($data[0].PSObject.Properties.Name -contains "MsBetweenPresents") {
            "MsBetweenPresents"
        } elseif ($data[0].PSObject.Properties.Name -contains "MsBetweenDisplayChange") {
            "MsBetweenDisplayChange"
        } else {
            # List available columns for debugging
            Write-Host "  Available columns: $($data[0].PSObject.Properties.Name -join ', ')" -ForegroundColor Yellow
            $null
        }

        $frameTimes = @()
        if ($frameTimeColumn) {
            $frameTimes = $data | Where-Object { $_.$frameTimeColumn -ne "" -and $_.$frameTimeColumn -ne $null } | ForEach-Object { [double]$_.$frameTimeColumn }
        }

        if ($frameTimes.Count -gt 0) {
            $avg = ($frameTimes | Measure-Object -Average).Average
            $min = ($frameTimes | Measure-Object -Minimum).Minimum
            $max = ($frameTimes | Measure-Object -Maximum).Maximum
            $stdDev = [math]::Sqrt(($frameTimes | ForEach-Object { [math]::Pow($_ - $avg, 2) } | Measure-Object -Average).Average)

            # Calculate percentiles
            $sorted = $frameTimes | Sort-Object
            $p50 = $sorted[[math]::Floor($sorted.Count * 0.50)]
            $p95 = $sorted[[math]::Floor($sorted.Count * 0.95)]
            $p99 = $sorted[[math]::Floor($sorted.Count * 0.99)]

            # Count stutters (frames > 2x average)
            $stutterThreshold = $avg * 2
            $stutters = ($frameTimes | Where-Object { $_ -gt $stutterThreshold }).Count
            $stutterPct = ($stutters / $frameTimes.Count) * 100

            $result = [PSCustomObject]@{
                Terminal = $terminal.Name
                FrameCount = $frameTimes.Count
                AvgMs = [math]::Round($avg, 2)
                StdDev = [math]::Round($stdDev, 2)
                MinMs = [math]::Round($min, 2)
                MaxMs = [math]::Round($max, 2)
                P50 = [math]::Round($p50, 2)
                P95 = [math]::Round($p95, 2)
                P99 = [math]::Round($p99, 2)
                Stutters = $stutters
                StutterPct = [math]::Round($stutterPct, 2)
                CpuPct = [math]::Round($avgCpu, 1)
            }
            $results += $result

            Write-Host "  Frames: $($frameTimes.Count)" -ForegroundColor White
            Write-Host "  Avg: $([math]::Round($avg, 2))ms, StdDev: $([math]::Round($stdDev, 2))ms" -ForegroundColor White
            Write-Host "  P50: $([math]::Round($p50, 2))ms, P95: $([math]::Round($p95, 2))ms, P99: $([math]::Round($p99, 2))ms" -ForegroundColor White
            Write-Host "  CPU: $([math]::Round($avgCpu, 1))%" -ForegroundColor White
            Write-Host "  Stutters (>$([math]::Round($stutterThreshold, 1))ms): $stutters ($([math]::Round($stutterPct, 1))%)" -ForegroundColor $(if ($stutterPct -lt 1) { "Green" } elseif ($stutterPct -lt 5) { "Yellow" } else { "Red" })
        } else {
            Write-Host "  No frame data captured" -ForegroundColor Red
        }
    } else {
        Write-Host "  CSV file not created" -ForegroundColor Red
    }
}

# Summary
Write-Host "`n========================================" -ForegroundColor Cyan
Write-Host "SUMMARY" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

if ($results.Count -gt 0) {
    # Show key columns including CPU
    $results | Format-Table Terminal, FrameCount, AvgMs, StdDev, P99, MaxMs, Stutters, CpuPct -AutoSize

    # Save summary
    $summaryFile = Join-Path $resultDir "summary.csv"
    $results | Export-Csv -Path $summaryFile -NoTypeInformation
    Write-Host "Summary saved to: $summaryFile" -ForegroundColor Green

    # Determine winner
    $smoothest = $results | Sort-Object StdDev | Select-Object -First 1
    Write-Host "`nSmoothest (lowest StdDev): $($smoothest.Terminal) with $($smoothest.StdDev)ms std dev, $($smoothest.CpuPct)% CPU" -ForegroundColor Green
} else {
    Write-Host "No results collected" -ForegroundColor Red
}
