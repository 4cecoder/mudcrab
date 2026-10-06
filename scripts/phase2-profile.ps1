[CmdletBinding()]
param(
    [ValidateSet("all", "synthetic", "rural", "dense", "water", "stress", "stability")]
    [string]$Scenario = "all",
    [string]$Assets,
    [string]$Worldspace = "0x3c",
    [int]$RuralGridX = 0,
    [int]$RuralGridY = 0,
    [int]$DenseGridX = 0,
    [int]$DenseGridY = 0,
    [int]$WaterGridX = 0,
    [int]$WaterGridY = 0,
    [int]$Repetitions = 3,
    [int]$DurationSeconds = 120,
    [int]$StabilitySeconds = 1800,
    [string]$Baseline = "profiling-baseline.json",
    [switch]$UpdateBaseline,
    [switch]$Quick,
    [switch]$SkipBuild,
    # Import the functions below and stop, instead of running a campaign: phase2-baseline.ps1
    # dot-sources this file with -AsLibrary so both scripts invoke the engine the same way.
    [switch]$AsLibrary
)

$ErrorActionPreference = "Stop"
# `ConvertFrom-Json -AsHashtable` (used for report and baseline files) arrived in PowerShell 6, so
# under Windows PowerShell 5.1 the failure would surface as a puzzling "parameter not found".
if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "Phase 2 campaign scripts need PowerShell 7 (pwsh): run again as 'pwsh -File scripts/$(Split-Path -Leaf $PSCommandPath) ...'"
}
$script:repository = Split-Path -Parent $PSScriptRoot

function Get-ProfilingRepository {
    return $script:repository
}

# Where the release engine binary is. `CARGO_TARGET_DIR`, when the caller set it, wins over the
# repository's own `target` directory (the worktree convention on this machine).
function Get-EngineBinaryPath {
    $targetRoot = $script:repository + "\target"
    if ($env:CARGO_TARGET_DIR) {
        $targetRoot = if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
            $env:CARGO_TARGET_DIR
        } else {
            Join-Path $script:repository $env:CARGO_TARGET_DIR
        }
    }
    return Join-Path $targetRoot "release\engine.exe"
}

# The hardware label a run identifies itself by: CPU, GPU and driver, reduced to one token so it
# can be passed on the command line and compared between runs of the same machine.
function Get-BenchmarkHardware {
    $cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name) -replace '[^a-zA-Z0-9]+', '-'
    $gpuInfo = Get-CimInstance Win32_VideoController | Select-Object -First 1
    $gpu = $gpuInfo.Name -replace '[^a-zA-Z0-9]+', '-'
    $driver = $gpuInfo.DriverVersion -replace '[^a-zA-Z0-9.]+', '-'
    return "$cpu-$gpu-driver-$driver".Trim('-')
}

# Builds the release engine once. `-Jobs 1` keeps the campaign's single-job build (the profiling
# run wants the machine quiet, not hot); the baseline command builds with cargo's default.
function Invoke-EngineReleaseBuild {
    param([int]$Jobs = 0)
    $arguments = @("build", "--release", "--bin", "engine")
    if ($Jobs -gt 0) { $arguments += "-j$Jobs" }
    Push-Location $script:repository
    try {
        # Cargo writes its progress to stderr, which PowerShell 5.1 would turn into an error record
        # under this script's `$ErrorActionPreference = "Stop"`; the preference is relaxed for the
        # call so a build cannot abort on its own progress lines.
        $previous = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            & cargo @arguments
            $exitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previous
        }
        if ($exitCode -ne 0) { throw "Release build failed with exit code $exitCode" }
    } finally {
        Pop-Location
    }
}

# What this campaign is: the commit it was built from and whether the worktree was dirty.
#
# Git writes line-ending and progress warnings to stderr, which PowerShell 5.1 turns into an error
# record under this script's `$ErrorActionPreference = "Stop"`, so the preference is relaxed around
# the two calls (a warning is not a failed command).
function Get-BenchmarkRunIdentity {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $commit = (& git -C $script:repository rev-parse --short=12 HEAD 2>$null)
        if (-not $commit) { $commit = "unknown" }
        & git -C $script:repository diff --quiet --ignore-submodules HEAD 2>$null
        $dirty = $LASTEXITCODE -ne 0
    } finally {
        $ErrorActionPreference = $previous
    }
    return [ordered]@{ commit = $commit; dirty = $dirty }
}

# The converted asset set a world scenario loads, checked for the four files every one of them
# needs, or $null when no -Assets was given. A relative path is taken from the repository root, the
# directory both campaign scripts run from.
function Resolve-ConvertedAssetSet {
    param([string]$Assets)
    if (-not $Assets) { return $null }
    $candidate = if ([IO.Path]::IsPathRooted($Assets)) { $Assets } else { Join-Path $script:repository $Assets }
    $resolved = (Resolve-Path -LiteralPath $candidate).Path
    foreach ($required in @("conversion-manifest.json", "integration-report.json", "skyrim_world.db", "cell_cache.rkyv")) {
        if (-not (Test-Path -LiteralPath (Join-Path $resolved $required) -PathType Leaf)) {
            throw "Converted asset set is missing $required"
        }
    }
    return $resolved
}

# The engine's arguments for one window of one scenario: the scenario's own definition (fixture
# or world, its grid, its camera movement) and the output paths this run writes.
#
# `-StandingStill` is the same scenario with the camera left in place: it drops the
# `--auto-fly-speed` a moving scenario would add, so the phase2-baseline command can measure the
# same scene once standing still and once streaming.
function Get-ScenarioArguments {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$RunDirectory,
        [Parameter(Mandatory)][string]$Commit,
        [Parameter(Mandatory)][string]$Hardware,
        [string]$RunId = "run-1",
        [string]$RunLabel,
        [switch]$DirtyWorktree,
        [switch]$StandingStill,
        [int]$DurationSeconds = 120,
        [int]$StabilitySeconds = 1800,
        [string]$ResolvedAssets,
        [string]$Worldspace = "0x3c",
        [int]$RuralGridX = 0,
        [int]$RuralGridY = 0,
        [int]$DenseGridX = 0,
        [int]$DenseGridY = 0,
        [int]$WaterGridX = 0,
        [int]$WaterGridY = 0
    )
    $arguments = @(
        "--benchmark-duration", $(if ($Name -eq "stability") { "$StabilitySeconds" } else { "$DurationSeconds" }),
        "--benchmark-output", (Join-Path $RunDirectory "acceptance.json"),
        "--profile-output", $RunDirectory,
        "--profile-scenario", $Name,
        "--profile-run-id", $RunId,
        "--profile-commit", $Commit,
        "--profile-hardware", $Hardware,
        "--accept-min-fps", "0",
        "--accept-p95-ms", "1000000",
        "--accept-max-memory-growth-gib", "1000000"
    )
    if ($RunLabel) { $arguments += @("--run-label", $RunLabel) }
    if ($DirtyWorktree) { $arguments += "--profile-dirty-worktree" }
    if ($Name -eq "synthetic") {
        $arguments += @("--benchmark-only", "--synthetic-instances", "250000")
    } else {
        $arguments += @("--assets", $ResolvedAssets, "--worldspace", $Worldspace)
        if ($Name -eq "water") {
            $arguments += @("--grid-x", "$WaterGridX", "--grid-y", "$WaterGridY")
        } elseif ($Name -in @("dense", "stability")) {
            $arguments += @("--grid-x", "$DenseGridX", "--grid-y", "$DenseGridY")
        } else {
            $arguments += @("--grid-x", "$RuralGridX", "--grid-y", "$RuralGridY")
        }
        if ($Name -eq "stress") {
            $arguments += @("--stream-radius", "3")
            if (-not $StandingStill) { $arguments += @("--auto-fly-speed", "5000") }
        }
        if ($Name -eq "stability" -and -not $StandingStill) {
            $arguments += @("--auto-fly-speed", "900")
        }
    }
    return $arguments
}

function Get-Median {
    param([double[]]$Values)
    $ordered = @($Values | Sort-Object)
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if ($ordered.Count % 2) { return $ordered[$middle] }
    return ($ordered[$middle - 1] + $ordered[$middle]) / 2.0
}

function Get-RegressionComparison {
    param([string]$Metric, [double]$Old, [double]$New)
    $absoluteDelta = if ($Metric -eq "average_fps") { $Old - $New } else { $New - $Old }
    $regression = $absoluteDelta / $Old * 100.0
    $noiseFloor = switch ($Metric) {
        "average_fps" { 5.0 }
        { $_ -in @("frame_ms_p95", "frame_ms_p99") } { 1.5 }
        { $_ -in @("peak_process_memory_gib", "process_memory_growth_gib") } { 0.05 }
        default { 0.0 }
    }
    $materialRegression = $absoluteDelta -gt $noiseFloor
    $status = if ($materialRegression -and $regression -gt 10.0) {
        "fail"
    } elseif ($materialRegression -and $regression -gt 5.0) {
        "warn"
    } else {
        "pass"
    }
    return [ordered]@{
        regression_percent = $regression
        regression_absolute = $absoluteDelta
        noise_floor = $noiseFloor
        status = $status
    }
}

# Runs one window of one scenario and returns its report (`frame-metrics.json`, the JSON the engine
# also writes as `acceptance.json`). Throws when the engine exits non-zero.
function Invoke-EngineWindow {
    param(
        [Parameter(Mandatory)][string]$Engine,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$RunDirectory,
        [Parameter(Mandatory)][string]$Label,
        [switch]$AsHashtable
    )
    New-Item -ItemType Directory -Force -Path $RunDirectory | Out-Null
    # The engine logs to stderr; under `$ErrorActionPreference = "Stop"` PowerShell 5.1 would treat
    # the first log line as an error, so the preference is relaxed for the run.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Engine @Arguments
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($exitCode -ne 0) { throw "$Label failed with exit code $exitCode" }
    $raw = Get-Content -LiteralPath (Join-Path $RunDirectory "frame-metrics.json") -Raw
    if ($AsHashtable) { return ($raw | ConvertFrom-Json -AsHashtable) }
    return ($raw | ConvertFrom-Json)
}

if ($AsLibrary) { return }

$engine = Get-EngineBinaryPath
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$hardware = Get-BenchmarkHardware
$campaign = Join-Path $script:repository "target\profiling\$stamp-$hardware"
$testTemp = Join-Path $script:repository "target\test-temp"
New-Item -ItemType Directory -Force -Path $campaign, $testTemp | Out-Null
$env:TEMP = $testTemp
$env:TMP = $testTemp

if ($Quick) {
    $DurationSeconds = 3
    $StabilitySeconds = 5
    $Repetitions = 1
}
if ($Repetitions -lt 1) { throw "Repetitions must be at least 1" }
$benchmarkPriority = [Diagnostics.ProcessPriorityClass]::AboveNormal
try {
    (Get-Process -Id $PID).PriorityClass = $benchmarkPriority
} catch {
    throw "Could not set the profiling runner priority to $benchmarkPriority`: $($_.Exception.Message)"
}

if (-not $SkipBuild -or -not (Test-Path -LiteralPath $engine -PathType Leaf)) {
    Invoke-EngineReleaseBuild -Jobs 1
}

$identity = Get-BenchmarkRunIdentity
$commit = $identity.commit
$dirty = $identity.dirty
$baselinePath = if ([IO.Path]::IsPathRooted($Baseline)) { $Baseline } else { Join-Path $script:repository $Baseline }
$baselineData = if (Test-Path -LiteralPath $baselinePath) {
    Get-Content -LiteralPath $baselinePath -Raw | ConvertFrom-Json -AsHashtable
} else { @{} }

$resolvedAssets = Resolve-ConvertedAssetSet -Assets $Assets

$requested = if ($Scenario -eq "all") {
    if ($resolvedAssets) { @("synthetic", "rural", "dense", "water", "stress", "stability") } else { @("synthetic") }
} else { @($Scenario) }
if ($requested.Where({ $_ -ne "synthetic" }).Count -gt 0 -and -not $resolvedAssets) {
    throw "Scenario '$Scenario' requires -Assets with a converted legal Skyrim asset set"
}

$results = @{}
$regressionFailed = $false
foreach ($name in $requested) {
    $runs = @()
    for ($run = 1; $run -le $Repetitions; $run++) {
        $runDirectory = Join-Path $campaign "$name\run-$run"
        $arguments = Get-ScenarioArguments -Name $name -RunDirectory $runDirectory -Commit $commit -Hardware $hardware `
            -RunId "run-$run" -DirtyWorktree:$dirty -DurationSeconds $DurationSeconds -StabilitySeconds $StabilitySeconds `
            -ResolvedAssets $resolvedAssets -Worldspace $Worldspace `
            -RuralGridX $RuralGridX -RuralGridY $RuralGridY -DenseGridX $DenseGridX -DenseGridY $DenseGridY `
            -WaterGridX $WaterGridX -WaterGridY $WaterGridY
        $runs += Invoke-EngineWindow -Engine $engine -Arguments $arguments -RunDirectory $runDirectory -Label "Profiling run $name/run-$run"
    }

    $median = [ordered]@{
        average_fps = Get-Median @($runs.average_fps)
        frame_ms_p95 = Get-Median @($runs.frame_ms_p95)
        frame_ms_p99 = Get-Median @($runs.frame_ms_p99)
        peak_process_memory_gib = Get-Median @($runs.peak_process_memory_gib)
        process_memory_growth_gib = Get-Median @($runs.process_memory_growth_gib)
    }
    $comparison = [ordered]@{ scenario = $name; repetitions = $Repetitions; median = $median; regressions = @() }
    if ($baselineData.ContainsKey($name)) {
        foreach ($metric in @("average_fps", "frame_ms_p95", "frame_ms_p99", "peak_process_memory_gib", "process_memory_growth_gib")) {
            $old = [double]$baselineData[$name][$metric]
            $new = [double]$median[$metric]
            if ($old -le 0) { continue }
            $regression = Get-RegressionComparison -Metric $metric -Old $old -New $new
            if ($regression.status -eq "fail") { $regressionFailed = $true }
            $comparison.regressions += [ordered]@{
                metric = $metric; baseline = $old; current = $new
                regression_percent = $regression.regression_percent
                regression_absolute = $regression.regression_absolute
                noise_floor = $regression.noise_floor
                status = $regression.status
            }
        }
    }
    $results[$name] = $median
    $comparison | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $campaign "$name\comparison.json")
}

[ordered]@{ format_version = 1; generated_at = (Get-Date).ToString("o"); commit = $commit; dirty = $dirty; hardware = $hardware; scenarios = $results } |
    ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $campaign "campaign.json")
$summary = @(
    "# Phase 2 profiling campaign",
    "",
    "- Commit: $commit",
    "- Dirty worktree: $dirty",
    "- Hardware: $hardware",
    "- Repetitions: $Repetitions",
    "",
    "| Scenario | Median FPS | Frame P95 ms | Frame P99 ms | Peak GiB | Growth GiB |",
    "|---|---:|---:|---:|---:|---:|"
)
foreach ($name in $requested) {
    $value = $results[$name]
    $summary += "| $name | $([Math]::Round($value.average_fps, 2)) | $([Math]::Round($value.frame_ms_p95, 3)) | $([Math]::Round($value.frame_ms_p99, 3)) | $([Math]::Round($value.peak_process_memory_gib, 3)) | $([Math]::Round($value.process_memory_growth_gib, 3)) |"
}
$summary | Set-Content -LiteralPath (Join-Path $campaign "summary.md")
if ($UpdateBaseline) {
    $results | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $baselinePath
    Write-Host "Baseline updated: $baselinePath"
}
if ($regressionFailed) { throw "Profiling regression exceeded 10%. See $campaign" }
Write-Host "Profiling campaign complete: $campaign"
