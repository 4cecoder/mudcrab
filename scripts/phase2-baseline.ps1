[CmdletBinding()]
param(
    # Which scenes to measure. The default is the five the baseline compares: the synthetic scene
    # is the one a machine with no converted data can run, the four world scenes are the walk, the
    # dense grid, the water and the streaming stress. `stability` is left out because it is a soak
    # window (30 minutes by default), not a baseline; `all` selects it too.
    [ValidateSet("all", "synthetic", "rural", "dense", "water", "stress", "stability")]
    [string[]]$Scenarios = @(),
    # The converted legal Skyrim asset set every world scenario loads. Not needed for `synthetic`.
    [string]$Assets,
    [int]$Repeats = 3,
    # Every window runs this long, except `stability`, which uses -StabilitySeconds.
    [int]$DurationSeconds = 120,
    [int]$StabilitySeconds = 1800,
    [string]$Out,
    [switch]$NoBuild,
    # A previous baseline.json to compare against. Without it, an existing `baseline.json` in -Out
    # is used. The comparison is refused when the two runs' renderer backend or adapter differ.
    [string]$CompareTo,
    [string]$Worldspace = "0x3c",
    [int]$RuralGridX = 0,
    [int]$RuralGridY = 0,
    [int]$DenseGridX = 0,
    [int]$DenseGridY = 0,
    [int]$WaterGridX = 0,
    [int]$WaterGridY = 0
)

$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "scripts/phase2-baseline.ps1 needs PowerShell 7 (pwsh): run it as 'pwsh -File scripts/phase2-baseline.ps1 ...'"
}
# The scenario definitions, the engine invocation and the regression helper live in
# phase2-profile.ps1; -AsLibrary imports them without starting a campaign, so both commands run
# the engine the same way.
#
# Dot-sourcing runs that file in this scope, so its own parameter defaults overwrite the same
# names here (-Assets, the window lengths, -Worldspace and the grids). The values this command was
# called with are put back afterwards, or a caller's -Assets and -DurationSeconds would be lost.
$sharedNames = @(
    "Assets", "DurationSeconds", "StabilitySeconds", "Worldspace",
    "RuralGridX", "RuralGridY", "DenseGridX", "DenseGridY", "WaterGridX", "WaterGridY"
)
$requestedValues = @{}
foreach ($name in $sharedNames) { $requestedValues[$name] = Get-Variable -Name $name -ValueOnly }
. (Join-Path $PSScriptRoot "phase2-profile.ps1") -AsLibrary
foreach ($name in $sharedNames) { Set-Variable -Name $name -Value $requestedValues[$name] }

$defaultScenarios = @("synthetic", "rural", "dense", "water", "stress")
$requested = if ($Scenarios.Count -eq 0) {
    # Like the profiling campaign, the default without -Assets is the one scene that needs no
    # converted data instead of a request that immediately fails.
    if ($Assets) { $defaultScenarios } else { @("synthetic") }
} elseif ($Scenarios -contains "all") {
    if ($Assets) { @("synthetic", "rural", "dense", "water", "stress", "stability") } else { @("synthetic") }
} else { $Scenarios }
if ($Repeats -lt 1) { throw "Repeats must be at least 1" }
if ($requested.Where({ $_ -ne "synthetic" }).Count -gt 0 -and -not $Assets) {
    throw "Scenario $(($requested -ne 'synthetic') -join ', ') requires -Assets with a converted legal Skyrim asset set"
}

$repository = Get-ProfilingRepository
$engine = Get-EngineBinaryPath
if (-not $Out) { $Out = Join-Path $repository "target\phase2-baseline" }
$Out = if ([IO.Path]::IsPathRooted($Out)) { $Out } else { Join-Path $repository $Out }
$testTemp = Join-Path $Out "tmp"
New-Item -ItemType Directory -Force -Path $Out, $testTemp | Out-Null
$env:TEMP = $testTemp
$env:TMP = $testTemp

$resolvedAssets = Resolve-ConvertedAssetSet -Assets $Assets

function Get-WindowDuration([string]$Name) {
    if ($Name -eq "stability") { return $StabilitySeconds }
    return $DurationSeconds
}

# A metric's spread across the repeats: the median the baseline compares on, and the smallest and
# largest value seen, so a reader can tell a stable number from a lucky one. `$null` when none of
# the repeats measured it (the report never zero-fills, and neither does this).
function Get-Spread($Values) {
    $numbers = @($Values | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($numbers.Count -eq 0) { return $null }
    $ordered = @($numbers | Sort-Object)
    return [ordered]@{
        median = Get-Median $numbers
        min = $ordered[0]
        max = $ordered[-1]
        samples = $numbers.Count
    }
}

function Get-Values($Reports, [scriptblock]$Selector) {
    return @($Reports | ForEach-Object { & $Selector $_ })
}

# The spread, across the repeats, of the `mean` of each entry of a `name -> MetricSummary` map
# (the report's GPU passes, CPU schedules and CPU spans). The names are the union of every repeat's
# keys, so a pass or span only a later repeat measured is still reported, with the sample count
# telling the reader how many repeats measured it.
function Get-SummaryMap($Reports, [scriptblock]$Selector) {
    $names = [ordered]@{}
    foreach ($report in $Reports) {
        $map = & $Selector $report
        if ($map) { foreach ($key in @($map.Keys)) { $names[$key] = $true } }
    }
    $summary = [ordered]@{}
    foreach ($name in @($names.Keys)) {
        $values = foreach ($report in $Reports) {
            $map = & $Selector $report
            if ($map -and $map.ContainsKey($name)) { $map[$name].mean } else { $null }
        }
        $summary[$name] = Get-Spread $values
    }
    return $summary
}

# The reasons a section could not measure something, unioned across the repeats: a key any repeat
# reports as unavailable is unavailable for the window, and every distinct reason is kept rather
# than only the first repeat's.
function Get-UnavailableMap($Reports, [scriptblock]$Selector) {
    $perKey = [ordered]@{}
    foreach ($report in $Reports) {
        $map = & $Selector $report
        if (-not $map) { continue }
        foreach ($key in @($map.Keys)) {
            $reason = [string]$map[$key]
            if (-not $perKey.Contains($key)) { $perKey[$key] = @() }
            if ($perKey[$key] -notcontains $reason) { $perKey[$key] += $reason }
        }
    }
    $merged = [ordered]@{}
    foreach ($key in @($perKey.Keys)) { $merged[$key] = ($perKey[$key] -join "; ") }
    return $merged
}

# The fields of a fingerprint every window of one campaign must share: the machine, the build and
# the world-data schemas the numbers were measured on. Everything that legitimately differs between
# windows — the effective graphics settings (a stream-radius override), which database a scenario
# opened, the commit being benchmarked — is kept per window instead.
function Get-FingerprintInvariant($Fingerprint) {
    if ($null -eq $Fingerprint) { return $null }
    $renderer = $Fingerprint.renderer
    return [ordered]@{
        engine_version = $Fingerprint.engine_version
        bevy_version = $Fingerprint.bevy_version
        build_profile = $Fingerprint.build_profile
        resolution = $Fingerprint.resolution
        world_database_schema = $Fingerprint.world_database_schema
        cell_cache_version = $Fingerprint.cell_cache_version
        renderer = if ($null -eq $renderer) { $null } else { [ordered]@{
                backend = $renderer.backend
                adapter = $renderer.adapter
                driver = $renderer.driver
                driver_info = $renderer.driver_info
                device_type = $renderer.device_type
                timestamp_queries_supported = $renderer.timestamp_queries_supported
                pipeline_statistics_supported = $renderer.pipeline_statistics_supported
            } }
    }
}

# The differences between two maps of scalars (nested maps are compared one level deep), as
# human-readable `field : old -> new` lines. An empty list means the two describe the same thing.
function Get-FieldDifferences($Old, $New, [string]$Prefix = "") {
    $names = [ordered]@{}
    foreach ($name in @($Old.Keys)) { $names[$name] = $true }
    foreach ($name in @($New.Keys)) { $names[$name] = $true }
    $differences = @()
    foreach ($name in @($names.Keys)) {
        $oldValue = $Old[$name]
        $newValue = $New[$name]
        $path = if ($Prefix) { "$Prefix.$name" } else { "$name" }
        if ($oldValue -is [System.Collections.IDictionary] -or $newValue -is [System.Collections.IDictionary]) {
            if ($null -eq $oldValue -or $null -eq $newValue) {
                if ($null -ne $oldValue -or $null -ne $newValue) { $differences += "$path : $(Format-FieldValue $oldValue) -> $(Format-FieldValue $newValue)" }
                continue
            }
            $differences += Get-FieldDifferences $oldValue $newValue $path
            continue
        }
        if ((Format-FieldValue $oldValue) -ne (Format-FieldValue $newValue)) {
            $differences += "$path : $(Format-FieldValue $oldValue) -> $(Format-FieldValue $newValue)"
        }
    }
    return $differences
}

function Format-FieldValue($Value) {
    if ($null -eq $Value) { return "null" }
    return "$Value"
}

# The inputs a window's scenario was started with, taken from the engine arguments that produced it
# and the report itself, so two windows' numbers can be shown to describe the same workload.
function Get-ScenarioInputs($Arguments, $Report, [int]$DurationSeconds) {
    $worldspace = Get-ArgumentValue $Arguments "--worldspace"
    $gridX = Get-ArgumentValue $Arguments "--grid-x"
    $gridY = Get-ArgumentValue $Arguments "--grid-y"
    return [ordered]@{
        worldspace = $worldspace
        grid = if ($null -eq $gridX -and $null -eq $gridY) { $null } else { [ordered]@{ x = [int]$gridX; y = [int]$gridY } }
        stream_radius = $Report.fingerprint.graphics.stream_radius
        duration_seconds = $DurationSeconds
        warmup_frames = $Report.warmup_frames
    }
}

# One frame-time metric of one report, by the report's own field name.
function Get-FrameMetric($Report, [string]$Metric) {
    switch ($Metric) {
        "count" { return $Report.frames }
        "mean_ms" { return $Report.frame_ms_mean }
        "p50_ms" { return $Report.frame_ms_p50 }
        "p95_ms" { return $Report.frame_ms_p95 }
        "p99_ms" { return $Report.frame_ms_p99 }
        "worst_ms" { return $Report.frame_ms_worst }
        "average_fps" { return $Report.average_fps }
    }
    return $null
}

# One window of one scenario: what every repeat measured, reduced to medians and spreads, plus the
# provenance that says what the window actually was — the run's fingerprint and the scenario inputs
# it was started with — so it can be set against another run's window. The maps are per pass, per
# schedule and per span; everything the report left `null` stays `null` here, and every section
# carries the report's `unavailable` reasons.
function Get-WindowReport($Reports, [string]$Kind, $Inputs) {
    $first = $Reports[0]
    $window = [ordered]@{
        window = $Kind
        runs = $Reports.Count
        scenario_inputs = $Inputs
        fingerprint = $first.fingerprint
    }
    foreach ($metric in @("count", "mean_ms", "p50_ms", "p95_ms", "p99_ms", "worst_ms", "average_fps")) {
        $window[$metric] = Get-Spread (Get-Values $Reports { param($report) Get-FrameMetric $report $metric })
    }
    $window.gpu_passes = [ordered]@{
        passes = Get-SummaryMap $Reports { param($report) $report.gpu_passes.passes }
        unavailable = Get-UnavailableMap $Reports { param($report) $report.gpu_passes.unavailable }
    }
    $window.memory = [ordered]@{
        process_peak_gib = Get-Spread (Get-Values $Reports { param($report) $report.peak_process_memory_gib })
        process_growth_gib = Get-Spread (Get-Values $Reports { param($report) $report.process_memory_growth_gib })
        cpu = [ordered]@{
            mesh_bytes = Get-Spread (Get-Values $Reports { param($report) $report.memory.cpu.mesh_bytes })
            image_bytes = Get-Spread (Get-Values $Reports { param($report) $report.memory.cpu.image_bytes })
        }
        gpu = [ordered]@{
            mesh_allocator_slab_bytes = Get-Spread (Get-Values $Reports { param($report) $report.memory.gpu.mesh_allocator_slab_bytes })
            mesh_allocator_slabs = Get-Spread (Get-Values $Reports { param($report) $report.memory.gpu.mesh_allocator_slabs })
            mesh_allocator_allocations = Get-Spread (Get-Values $Reports { param($report) $report.memory.gpu.mesh_allocator_allocations })
            render_asset_counts = Get-SpreadPerName $Reports { param($report) $report.memory.gpu.render_asset_counts }
            unavailable = Get-UnavailableMap $Reports { param($report) $report.memory.gpu.unavailable }
        }
    }
    $window.scene = [ordered]@{
        entity_count = Get-Spread (Get-Values $Reports { param($report) $report.scene.entity_count })
        mesh_entities = Get-Spread (Get-Values $Reports { param($report) $report.scene.mesh_entities })
        terrain_patches = Get-Spread (Get-Values $Reports { param($report) $report.scene.terrain_patches })
        water_surfaces = Get-Spread (Get-Values $Reports { param($report) $report.scene.water_surfaces })
        pipeline_statistics_supported = $first.scene.pipeline_statistics_supported
        vertex_shader_invocations = Get-SummaryMap $Reports { param($report) $report.scene.vertex_shader_invocations }
        clipper_primitives_out = Get-SummaryMap $Reports { param($report) $report.scene.clipper_primitives_out }
        draw_calls = Get-Spread (Get-Values $Reports { param($report) $report.scene.draw_calls })
        unavailable = Get-UnavailableMap $Reports { param($report) $report.scene.unavailable }
    }
    $window.cpu = [ordered]@{
        schedules = Get-SummaryMap $Reports { param($report) $report.cpu.schedules }
        spans = Get-SummaryMap $Reports { param($report) $report.cpu.spans }
        render_world_prep = Get-Spread (Get-Values $Reports { param($report) $report.cpu.render_world_prep })
        unavailable = Get-UnavailableMap $Reports { param($report) $report.cpu.unavailable }
    }
    return $window
}

# The same spread for a `name -> number` map (the GPU-asset counts), over the union of the repeats'
# names.
function Get-SpreadPerName($Reports, [scriptblock]$Selector) {
    # The inner scriptblock below must call the *caller's* selector. Inside it, `$Selector` would
    # resolve to the scriptblock itself (a scriptblock closes over the scope that made it), so the
    # selector is copied to a name the closure cannot confuse with itself.
    $select = $Selector
    $names = [ordered]@{}
    foreach ($report in $Reports) {
        $map = & $select $report
        if ($map) { foreach ($key in @($map.Keys)) { $names[$key] = $true } }
    }
    $spread = [ordered]@{}
    foreach ($name in @($names.Keys)) {
        $spread[$name] = Get-Spread (Get-Values $Reports { param($report) (& $select $report)[$name] })
    }
    return $spread
}

function Format-Spread($Spread, [string]$Format = "{0:N3}") {
    if ($null -eq $Spread) { return "n/a" }
    return "$([string]::Format($Format, $Spread.median)) ($([string]::Format($Format, $Spread.min))-$([string]::Format($Format, $Spread.max)))"
}

# One short table per scenario window, plus the two facts a table cannot hold: why a counter is
# missing, and which passes and spans cost the most.
function Get-MarkdownTable($Name, $Window, [string]$Kind) {
    $label = if ($Kind -eq "streaming") { "$Name (streaming)" } else { "$Name (standing still)" }
    $lines = @(
        "",
        "## $label",
        "",
        "| Metric | Median (min-max over $($Window.runs) runs) |",
        "|---|---:|",
        "| Average FPS | $(Format-Spread $Window.average_fps '{0:N2}') |",
        "| Mean frame ms | $(Format-Spread $Window.mean_ms) |",
        "| P50 frame ms | $(Format-Spread $Window.p50_ms) |",
        "| P95 frame ms | $(Format-Spread $Window.p95_ms) |",
        "| P99 frame ms | $(Format-Spread $Window.p99_ms) |",
        "| Worst frame ms | $(Format-Spread $Window.worst_ms) |",
        "| Frames measured | $(Format-Spread $Window.count '{0:N0}') |",
        "| Peak process GiB | $(Format-Spread $Window.memory.process_peak_gib) |",
        "| Memory growth GiB | $(Format-Spread $Window.memory.process_growth_gib) |",
        "| Entities | $(Format-Spread $Window.scene.entity_count '{0:N0}') |",
        "| Mesh entities | $(Format-Spread $Window.scene.mesh_entities '{0:N0}') |",
        "| Terrain patches | $(Format-Spread $Window.scene.terrain_patches '{0:N0}') |",
        "| Water surfaces | $(Format-Spread $Window.scene.water_surfaces '{0:N0}') |"
    )
    $lines += "| Draw calls | null: $($Window.scene.unavailable.draw_calls) |"
    if ($Window.scene.pipeline_statistics_supported) {
        $lines += "| Vertex shader invocations | $(Get-SummaryLine $Window.scene.vertex_shader_invocations) |"
        $lines += "| Clipper primitives out | $(Get-SummaryLine $Window.scene.clipper_primitives_out) |"
    } else {
        $lines += "| Pipeline-statistics counters | null: $($Window.scene.unavailable.pipeline_statistics) |"
    }
    foreach ($schedule in @("main_schedule/Update", "main_schedule/PostUpdate")) {
        if ($Window.cpu.schedules.Contains($schedule)) {
            $lines += "| $schedule mean ms | $(Format-Spread $Window.cpu.schedules[$schedule]) |"
        }
    }
    foreach ($span in @("streaming/plan_cells", "streaming/cell_commit", "streaming/frame_commit", "streaming/arm_models")) {
        if ($Window.cpu.spans.Contains($span)) {
            $lines += "| $span mean ms | $(Format-Spread $Window.cpu.spans[$span]) |"
        }
    }
    $lines += "| CPU spans recorded | $($Window.cpu.spans.Count) (schedules: $($Window.cpu.schedules.Count)) |"
    $topPasses = @($Window.gpu_passes.passes.Keys | Sort-Object { -($Window.gpu_passes.passes[$_].median) } | Select-Object -First 5)
    if ($topPasses.Count -eq 0) {
        $lines += "| Slowest GPU passes | n/a: $($Window.cpu.unavailable.render_world_prep) |"
    } else {
        $lines += "| Slowest GPU passes (mean ms) | $(($topPasses | ForEach-Object { "$_ $(Format-Spread $Window.gpu_passes.passes[$_])" }) -join '; ') |"
    }
    $topSpans = @($Window.cpu.spans.Keys | Sort-Object { -($Window.cpu.spans[$_].median) } | Select-Object -First 5)
    if ($topSpans.Count -gt 0) {
        $lines += "| Slowest CPU spans (mean ms) | $(($topSpans | ForEach-Object { "$_ $(Format-Spread $Window.cpu.spans[$_])" }) -join '; ') |"
    }
    return $lines
}

# The largest pass in a per-pass counter map, as one table cell.
function Get-SummaryLine($Summary) {
    if ($null -eq $Summary -or $Summary.Count -eq 0) { return "null: no counter recorded" }
    $top = @($Summary.Keys | Sort-Object { -($Summary[$_].median) } | Select-Object -First 3)
    return ($top | ForEach-Object { "$_ $(Format-Spread $Summary[$_])" }) -join '; '
}

# Refuses a comparison the issue says cannot be made: two runs on a different backend or adapter.
function Assert-Comparable($Old, $New, [string]$Path) {
    $oldRenderer = $Old.fingerprint.renderer
    $newRenderer = $New.fingerprint.renderer
    if ($null -eq $oldRenderer -or $null -eq $newRenderer) {
        throw "Cannot compare with ${Path}: one of the two baselines recorded no renderer, so their backends cannot be checked"
    }
    if ($oldRenderer.backend -ne $newRenderer.backend -or $oldRenderer.adapter -ne $newRenderer.adapter) {
        throw "Cannot compare with ${Path}: it ran on $($oldRenderer.backend)/$($oldRenderer.adapter) but this run is on $($newRenderer.backend)/$($newRenderer.adapter). Engine results compare only on the same machine and backend"
    }
}

# Per-metric comparison against an older baseline of the same machine and backend, reusing the
# profiling campaign's noise floors.
function Get-Comparison($Old, $New) {
    $comparison = [ordered]@{}
    foreach ($name in @($New.scenarios.Keys)) {
        if (-not $Old.scenarios.ContainsKey($name)) { continue }
        foreach ($kind in @("standing_still", "streaming")) {
            $newWindow = $New.scenarios[$name].windows[$kind]
            $oldWindow = $Old.scenarios[$name].windows[$kind]
            if ($null -eq $newWindow -or $null -eq $oldWindow) { continue }
            $metrics = [ordered]@{
                average_fps = @($newWindow.average_fps, $oldWindow.average_fps)
                frame_ms_p95 = @($newWindow.p95_ms, $oldWindow.p95_ms)
                frame_ms_p99 = @($newWindow.p99_ms, $oldWindow.p99_ms)
                peak_process_memory_gib = @($newWindow.memory.process_peak_gib, $oldWindow.memory.process_peak_gib)
                process_memory_growth_gib = @($newWindow.memory.process_growth_gib, $oldWindow.memory.process_growth_gib)
            }
            $current = @{}
            foreach ($metric in @($metrics.Keys)) {
                $new = $metrics[$metric][0]; $old = $metrics[$metric][1]
                if ($null -eq $new -or $null -eq $old -or $old.median -le 0) { continue }
                $regression = Get-RegressionComparison -Metric $metric -Old $old.median -New $new.median
                $current["$name/$kind/$metric"] = [ordered]@{
                    baseline = $old.median; current = $new.median
                    regression_percent = $regression.regression_percent
                    regression_absolute = $regression.regression_absolute
                    noise_floor = $regression.noise_floor
                    status = $regression.status
                }
            }
            if ($current.Count -gt 0) { $comparison["$name/$kind"] = $current }
        }
    }
    return $comparison
}

$windowsPerScenario = @{}
foreach ($name in $requested) {
    $moving = $false
    if ($name -ne "synthetic") {
        $probe = Get-ScenarioArguments -Name $name -RunDirectory "." -Commit "probe" -Hardware "probe" `
            -DurationSeconds 0 -ResolvedAssets $resolvedAssets
        $moving = ($probe -contains "--auto-fly-speed")
    }
    $windowsPerScenario[$name] = if ($moving) { 2 } else { 1 }
}
$totalWindows = ($requested | ForEach-Object { $windowsPerScenario[$_] * $Repeats } | Measure-Object -Sum).Sum
$totalSeconds = ($requested | ForEach-Object { $windowsPerScenario[$_] * $Repeats * (Get-WindowDuration $_) } | Measure-Object -Sum).Sum
$plan = ($requested | ForEach-Object {
        "$_ x $($windowsPerScenario[$_]) window(s) x $(Get-WindowDuration $_) s"
    }) -join ", "
Write-Host "Baseline plan: $Repeats repeat(s) of $plan"
Write-Host "Expected duration: $totalWindows engine window(s), about $( [Math]::Round($totalSeconds / 60.0, 1) ) minutes of measured time, plus the build"

# The windows are timed, so the runner itself must not be scheduled below the machine's other work
# (the profiling campaign sets the same priority).
$benchmarkPriority = [Diagnostics.ProcessPriorityClass]::AboveNormal
try {
    (Get-Process -Id $PID).PriorityClass = $benchmarkPriority
} catch {
    throw "Could not set the baseline runner priority to $benchmarkPriority`: $($_.Exception.Message)"
}

if (-not $NoBuild -or -not (Test-Path -LiteralPath $engine -PathType Leaf)) {
    Invoke-EngineReleaseBuild
}
$identity = Get-BenchmarkRunIdentity
$hardware = Get-BenchmarkHardware
# Every campaign's report bundles go to their own timestamped directory, so a `run-N` this campaign
# reads back can never be a file an earlier campaign left behind.
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$campaignRoot = Join-Path $Out "runs\$stamp-$hardware"

$comparePath = if ($CompareTo) {
    if ([IO.Path]::IsPathRooted($CompareTo)) { $CompareTo } else { Join-Path $repository $CompareTo }
} else {
    Join-Path $Out "baseline.json"
}
# An explicitly requested comparison against a file that is not there is a mistyped path, not a run
# without a previous baseline, so it is refused rather than silently treated as no comparison.
if ($CompareTo -and -not (Test-Path -LiteralPath $comparePath -PathType Leaf)) {
    throw "Cannot compare: -CompareTo $comparePath does not exist"
}
$previous = if (Test-Path -LiteralPath $comparePath -PathType Leaf) {
    Get-Content -LiteralPath $comparePath -Raw | ConvertFrom-Json -AsHashtable
} else { $null }

# Named `$scenarioReports`, not `$scenarios`: PowerShell variable names are case-insensitive, so a
# local `$scenarios` would be this script's own `-Scenarios` parameter, validation and all.
$scenarioReports = [ordered]@{}
# The machine/build invariants of the campaign, taken from the first window, and the label of the
# window they came from so a later window that disagrees can name both.
$campaignFingerprint = $null
$campaignFingerprintLabel = $null
foreach ($name in $requested) {
    $moves = $windowsPerScenario[$name] -eq 2
    $kinds = if ($moves) { @("standing_still", "streaming") } else { @("standing_still") }
    $reports = @{}
    foreach ($kind in $kinds) {
        $windowReports = @()
        $windowArguments = $null
        for ($run = 1; $run -le $Repeats; $run++) {
            # A moving scenario gets two windows: the scene left standing still (the steady frames),
            # then the scenario's own flight (the streaming frames). Everything else is one window.
            $label = if ($kind -eq "streaming") { "baseline $name streaming $run/$Repeats" } else { "baseline $name $run/$Repeats" }
            $reportLabel = "baseline $name/$kind/run-$run"
            $runDirectory = Join-Path $campaignRoot "$name\$($kind -replace '_', '-')\run-$run"
            $arguments = Get-ScenarioArguments -Name $name -RunDirectory $runDirectory -Commit $identity.commit `
                -Hardware $hardware -RunId "run-$run" -RunLabel $label -DirtyWorktree:$identity.dirty `
                -StandingStill:($kind -eq "standing_still") `
                -DurationSeconds $DurationSeconds -StabilitySeconds $StabilitySeconds `
                -ResolvedAssets $resolvedAssets -Worldspace $Worldspace `
                -RuralGridX $RuralGridX -RuralGridY $RuralGridY -DenseGridX $DenseGridX -DenseGridY $DenseGridY `
                -WaterGridX $WaterGridX -WaterGridY $WaterGridY
            if ($null -eq $windowArguments) { $windowArguments = $arguments }
            $report = Invoke-EngineWindow -Engine $engine -Arguments $arguments -RunDirectory $runDirectory `
                -Label "Baseline run $name/$kind/run-$run" -AsHashtable
            if ($null -eq $report.fingerprint) {
                throw "$reportLabel recorded no run fingerprint in $runDirectory, so the window cannot be shown to be comparable with the others"
            }
            if ($null -eq $campaignFingerprint) {
                $campaignFingerprint = Get-FingerprintInvariant $report.fingerprint
                $campaignFingerprintLabel = $reportLabel
                # Comparing is only meaningful between the same backend and adapter, so a mismatch is
                # refused before the rest of the campaign spends hours producing numbers nobody can
                # set against the older baseline.
                if ($previous) { Assert-Comparable -Old $previous -New $report -Path $comparePath }
            } else {
                # Every window after the first must agree with it on the machine, the build and the
                # schemas, or the campaign is not one comparable set of numbers.
                $differences = @(Get-FieldDifferences $campaignFingerprint (Get-FingerprintInvariant $report.fingerprint))
                if ($differences.Count -gt 0) {
                    throw "The windows of this campaign were not measured on the same machine and build: $reportLabel differs from ${campaignFingerprintLabel} in $($differences -join '; '). A baseline mixes only windows measured on one machine, backend and build"
                }
            }
            $windowReports += $report
        }
        $inputs = Get-ScenarioInputs -Arguments $windowArguments -Report $windowReports[0] -DurationSeconds (Get-WindowDuration $name)
        $reports[$kind] = Get-WindowReport $windowReports $kind $inputs
    }
    $entry = [ordered]@{ moves = $moves; windows = [ordered]@{} }
    foreach ($kind in @("standing_still", "streaming")) {
        $entry.windows[$kind] = if ($reports.ContainsKey($kind)) { $reports[$kind] } else { $null }
    }
    if (-not $moves) {
        $entry.streaming_unavailable = "the $name scenario leaves the camera in place, so it has no streaming window"
    }
    $scenarioReports[$name] = $entry
}

# Named `$baselineReport`, not `$baseline`: the dot-sourced profile script declares a `$Baseline`
# parameter, and PowerShell would treat the two as the same variable.
$baselineReport = [ordered]@{
    format_version = 2
    generated_at = (Get-Date).ToString("o")
    commit = $identity.commit
    dirty = $identity.dirty
    hardware = $hardware
    repeats = $Repeats
    duration_seconds = $DurationSeconds
    stability_seconds = $StabilitySeconds
    # Where this campaign's own report bundles are, so a window's raw evidence can be found again
    # after a later campaign has written to the same -Out.
    run_directory = $campaignRoot
    fingerprint = $campaignFingerprint
    scenarios = $scenarioReports
}
if ($previous) {
    $baselineReport.comparison = [ordered]@{ against = $comparePath; metrics = Get-Comparison -Old $previous -New $baselineReport }
}
$baselineReport | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $Out "baseline.json")

$rendererLine = if ($null -eq $campaignFingerprint.renderer) {
    "not recorded: the run installed no renderer fingerprint"
} else {
    "$($campaignFingerprint.renderer.backend) / $($campaignFingerprint.renderer.adapter) ($($campaignFingerprint.renderer.device_type)), driver $($campaignFingerprint.renderer.driver); GPU timestamps: $($campaignFingerprint.renderer.timestamp_queries_supported); pipeline statistics: $($campaignFingerprint.renderer.pipeline_statistics_supported)"
}
$markdown = @(
    "# Phase 2 engine baseline",
    "",
    "- Commit: $($identity.commit) (dirty worktree: $($identity.dirty))",
    "- Machine: $hardware",
    "- Renderer: $rendererLine",
    "- Repeats: $Repeats; window: $DurationSeconds s ($StabilitySeconds s for stability)",
    "- Generated: $($baselineReport.generated_at)",
    ""
)
foreach ($name in $requested) {
    foreach ($kind in @("standing_still", "streaming")) {
        $window = $scenarioReports[$name].windows[$kind]
        if ($null -eq $window) {
            if ($kind -eq "streaming") { $markdown += @("", "## $name (streaming)", "", $scenarioReports[$name].streaming_unavailable) }
            continue
        }
        $markdown += Get-MarkdownTable $name $window $kind
    }
}
if ($previous) {
    $markdown += @("", "## Comparison against $comparePath", "", "| Metric | Baseline | Current | Change % | Status |", "|---|---:|---:|---:|---|")
    foreach ($scope in @($baselineReport.comparison.metrics.Keys)) {
        foreach ($metric in @($baselineReport.comparison.metrics[$scope].Keys)) {
            $value = $baselineReport.comparison.metrics[$scope][$metric]
            $markdown += "| $metric | $([Math]::Round($value.baseline, 4)) | $([Math]::Round($value.current, 4)) | $([Math]::Round($value.regression_percent, 2)) | $($value.status) |"
        }
    }
}
$markdown | Set-Content -LiteralPath (Join-Path $Out "baseline.md")
Write-Host "Baseline complete: $(Join-Path $Out 'baseline.json') and $(Join-Path $Out 'baseline.md')"
