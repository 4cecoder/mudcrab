#Requires -Version 7
# Fixture tests for the baseline comparison rules in scripts/phase2-baseline.ps1: coverage of a
# metric across repeats, the samples/runs shown for a spread, and the data set identity check.
#
# The functions under test are extracted from the scripts with the PowerShell parser and run here,
# so no engine, GPU, converted data or campaign is needed. Run from the repository root:
#   pwsh -NoProfile -File scripts/tests/test_phase2_baseline_comparison.ps1
# The script exits non-zero when any fixture fails.
$ErrorActionPreference = "Stop"
$repository = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

# Defines the named functions from a script in this scope, taken from the parser's own extent of
# each definition. A missing or duplicated name is an error, so a rename cannot pass silently.
function Import-ScriptFunctions([string]$Path, [string[]]$Names) {
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) { throw "$Path does not parse: $($errors -join '; ')" }
    $definitions = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true))
    return @(foreach ($name in $Names) {
        $found = @($definitions | Where-Object { $_.Name -eq $name })
        if ($found.Count -ne 1) { throw "$Path defines $($found.Count) functions named $name; expected one" }
        $found[0].Extent.Text
    })
}

$baselineScript = Join-Path $repository "scripts/phase2-baseline.ps1"
$profileScript = Join-Path $repository "scripts/phase2-profile.ps1"
foreach ($text in @(Import-ScriptFunctions $profileScript @("Get-Median", "Get-RegressionComparison"))) {
    . ([scriptblock]::Create($text))
}
$baselineFunctions = @(
    "Get-Values", "Get-Spread", "Get-SummaryMap", "Get-CountMap", "Get-SummaryLine", "Format-Spread", "Get-FieldDifferences", "Format-FieldValue",
    "Get-LaggingPasses", "Assert-AssetIdentityKnown", "Assert-ComparisonAssetIdentities",
    "Assert-WindowComparable", "Get-ComparisonRefusal", "Get-Comparison"
)
foreach ($text in @(Import-ScriptFunctions $baselineScript $baselineFunctions)) {
    . ([scriptblock]::Create($text))
}

$script:passed = 0
$script:failures = [System.Collections.Generic.List[string]]::new()

function Assert-True([bool]$Condition, [string]$Name, [string]$Detail = "") {
    if ($Condition) { $script:passed++; return }
    $script:failures.Add("${Name}: $Detail")
}

function Assert-Throws([scriptblock]$Block, [string]$Pattern, [string]$Name) {
    try {
        & $Block
    } catch {
        Assert-True ($_.Exception.Message -match $Pattern) $Name "message did not match '$Pattern': $($_.Exception.Message)"
        return
    }
    Assert-True $false $Name "expected a refusal matching '$Pattern', but the check passed"
}

function Assert-Passes([scriptblock]$Block, [string]$Name) {
    try {
        & $Block
    } catch {
        Assert-True $false $Name "expected no refusal, got: $($_.Exception.Message)"
        return
    }
    Assert-True $true $Name
}

# A baseline-shaped report with one standing-still window. `$Metrics` maps a metric name to its
# per-repeat values, `$null` where that repeat did not measure it.
function New-Report([int]$Repeats, [hashtable]$Metrics) {
    $window = [ordered]@{
        average_fps = Get-Spread $Metrics.average_fps
        p95_ms = Get-Spread $Metrics.p95
        p99_ms = Get-Spread $Metrics.p99
        memory = [ordered]@{
            process_peak_gib = Get-Spread $Metrics.peak
            process_growth_gib = Get-Spread $Metrics.growth
        }
    }
    return [ordered]@{
        repeats = $Repeats
        scenarios = @{ rural = [ordered]@{ windows = [ordered]@{ standing_still = $window; streaming = $null } } }
    }
}

# Both sides fully measured, unless a test says otherwise.
$full = @{
    average_fps = @(60.0, 60.0, 60.0); p95 = @(10.0, 10.0, 10.0); p99 = @(12.0, 12.0, 12.0)
    peak = @(2.0, 2.0, 2.0); growth = @(0.1, 0.1, 0.1)
}
# Comparison metrics retain their existing scope-qualified keys; unscored entries use metric names.
$scope = "rural/standing_still"
$metric = "frame_ms_p95"
$metricKey = "$scope/$metric"

# --- Coverage of a metric across repeats -----------------------------------------------------------

$newPartial = @{ average_fps = @(60.0, 60.0, 60.0); p95 = @(5.0, $null, $null); p99 = @(12.0, 12.0, 12.0); peak = @(2.0, 2.0, 2.0); growth = @(0.1, 0.1, 0.1) }
$comparison = Get-Comparison -Old (New-Report 3 $full) -New (New-Report 3 $newPartial)
Assert-True (-not $comparison.metrics[$scope].Contains($metricKey)) "incomplete current repeats are not scored" "scored: $($comparison.metrics[$scope][$metricKey] | ConvertTo-Json -Compress)"
Assert-True ($comparison.unscored[$scope].Contains($metric)) "incomplete current repeats are listed as unscored"
Assert-True ($comparison.unscored[$scope][$metric].reason -match "1 of 3 current repeats") "the unscored reason states the current coverage" $comparison.unscored[$scope][$metric].reason
Assert-True ($comparison.metrics[$scope].Contains("$scope/frame_ms_p99")) "a fully measured metric beside it is still scored"

$oldPartial = @{ average_fps = @(60.0, 60.0, 60.0); p95 = @(10.0, 10.0, $null); p99 = @(12.0, 12.0, 12.0); peak = @(2.0, 2.0, 2.0); growth = @(0.1, 0.1, 0.1) }
$comparison = Get-Comparison -Old (New-Report 3 $oldPartial) -New (New-Report 3 $full)
Assert-True (-not $comparison.metrics[$scope].Contains($metricKey)) "incomplete baseline repeats are not scored"
Assert-True ($comparison.unscored[$scope][$metric].reason -match "2 of 3 baseline repeats") "the unscored reason states the baseline coverage" $comparison.unscored[$scope][$metric].reason

# A one-repeat campaign measures every metric in its only repeat, so it is complete and scored.
$oneRepeatOld = New-Report 1 @{ average_fps = @(60.0); p95 = @(10.0); p99 = @(12.0); peak = @(2.0); growth = @(0.1) }
$oneRepeatNew = New-Report 1 @{ average_fps = @(60.0); p95 = @(10.5); p99 = @(12.0); peak = @(2.0); growth = @(0.1) }
$comparison = Get-Comparison -Old $oneRepeatOld -New $oneRepeatNew
Assert-True ($comparison.metrics[$scope].Contains($metricKey)) "a complete one-repeat metric is scored"
Assert-True (-not $comparison.unscored.Contains($scope)) "a complete one-repeat campaign has nothing unscored"

# A metric no repeat measured is omitted from both maps: no fabricated zero, no fabricated verdict.
$allNull = @{ average_fps = @(60.0, 60.0, 60.0); p95 = @($null, $null, $null); p99 = @(12.0, 12.0, 12.0); peak = @(2.0, 2.0, 2.0); growth = @(0.1, 0.1, 0.1) }
$comparison = Get-Comparison -Old (New-Report 3 $full) -New (New-Report 3 $allNull)
Assert-True (-not $comparison.metrics[$scope].Contains($metricKey)) "an all-null metric is not scored"
Assert-True (-not ($comparison.unscored.Contains($scope) -and $comparison.unscored[$scope].Contains($metric))) "an all-null metric is not listed as unscored either"

# Markdown shows the coverage of a partial spread next to its range.
$partialSpread = Get-Spread @(8.0, $null, $null)
Assert-True ((Format-Spread $partialSpread) -match "1/3 samples") "a partial spread shows samples/runs" (Format-Spread $partialSpread)
$completeSpread = Get-Spread @(8.0, 9.0, 10.0)
Assert-True ((Format-Spread $completeSpread) -match "3/3 samples") "a complete spread shows samples/runs" (Format-Spread $completeSpread)
Assert-True ((Format-Spread $null) -eq "n/a") "a metric no repeat measured reads n/a"

# Exercise the report aggregation path, including absent GPU-pass keys, before formatting.
$gpuReports = @(
    @{ passes = @{ opaque = @{ mean = 8.0; count = 20 } } },
    @{ passes = @{} },
    @{ passes = @{} }
)
$gpuPasses = Get-SummaryMap $gpuReports { param($report) $report.passes }
Assert-True ($gpuPasses.opaque.samples -eq 1 -and $gpuPasses.opaque.runs -eq 3) "GPU pass aggregation retains missing-repeat coverage"
Assert-True ((Get-SummaryLine $gpuPasses) -match "1/3 samples") "GPU pass summary shows actual repeat coverage" (Get-SummaryLine $gpuPasses)
$lagging = Get-LaggingPasses @{
    gpu_passes = @{
        measured_frames = Get-Spread @(100, 100, 100)
        passes = $gpuPasses
        pass_samples = Get-CountMap $gpuReports { param($report) $report.passes }
    }
}
Assert-True (($lagging.passes -join "; ") -match "1/3 samples") "lagging GPU pass list shows repeat coverage" ($lagging.passes -join "; ")

# --- Data set identity -------------------------------------------------------------------------------

function New-AssetWindow([string]$Assets, $Identity, [string]$Unavailable, [string]$Hash = "h1") {
    $inputs = [ordered]@{
        worldspace = "Tamriel"; assets = $Assets; fly_speed = $null; synthetic_instances = $null
        assets_identity = if ($null -eq $Identity) { [ordered]@{ world_database_bytes = $null; world_database_modified_utc = $null; cell_cache_bytes = $null; configuration_hash = $null; schema_version = $null } } else { $Identity }
        assets_identity_unavailable = $Unavailable
        stream_radius = 3; duration_seconds = 120; warmup_frames = 60
    }
    return [ordered]@{
        scenario_inputs = $inputs
        fingerprint = [ordered]@{ graphics = [ordered]@{ stream_radius = 3 }; opened_world_database_schema = 9 }
    }
}
function New-Identity([string]$Hash) {
    return [ordered]@{ world_database_bytes = 100; world_database_modified_utc = "2026-10-01T00:00:00.0000000Z"; cell_cache_bytes = 50; configuration_hash = $Hash; schema_version = 9 }
}
$unknownReason = "conversion-manifest.json records no configuration_hash for the data set"

# Both windows name one data set with no known identity and the same unavailable reason: refused.
$sameUnknown = New-AssetWindow "D:\Mudcrab\Converted" (New-Identity "") $unknownReason
Assert-Throws { Assert-WindowComparable $sameUnknown $sameUnknown "baseline.json" "rural/standing_still" } "no known identity" "two unknown identities for the same path are refused"

# One side unknown, the other known: refused, the reason names the unknown side's data set.
$known = New-AssetWindow "D:\Mudcrab\Converted" (New-Identity "h1") $null
$unknownSide = New-AssetWindow "D:\Mudcrab\Converted" (New-Identity "") $unknownReason
Assert-Throws { Assert-WindowComparable $known $unknownSide "baseline.json" "rural/standing_still" } "no known identity" "an unknown identity on the current side is refused"
Assert-Throws { Assert-WindowComparable $unknownSide $known "baseline.json" "rural/standing_still" } "no known identity" "an unknown identity on the baseline side is refused"

# The synthetic scene loads no data set, so its unavailable reason is not an identity problem.
$synthetic = New-AssetWindow $null $null "this window was started without -Assets, so it loaded no converted data set"
Assert-Passes { Assert-WindowComparable $synthetic $synthetic "baseline.json" "synthetic/standing_still" } "synthetic windows without assets still compare"

# Known and matching identities compare; a different configuration hash is a different workload.
Assert-Passes { Assert-WindowComparable $known (New-AssetWindow "D:\Mudcrab\Converted" (New-Identity "h1") $null) "baseline.json" "rural/standing_still" } "matching known identities compare"
$differentHash = New-AssetWindow "D:\Mudcrab\Converted" (New-Identity "h2") $null
Assert-Throws { Assert-WindowComparable $known $differentHash "baseline.json" "rural/standing_still" } "not the same workload" "differing known identities are refused"

# -CompareTo refuses by throwing; a baseline picked up from -Out warns and is dropped instead.
$explicitCompare = $true
Assert-Throws { Get-ComparisonRefusal { Assert-WindowComparable $sameUnknown $sameUnknown "baseline.json" "rural/standing_still" } } "no known identity" "an explicit comparison throws on an unknown identity"
$explicitCompare = $false
$refusal = Get-ComparisonRefusal { Assert-WindowComparable $sameUnknown $sameUnknown "baseline.json" "rural/standing_still" }
Assert-True ($refusal -match "no known identity") "an implicit comparison returns the refusal for a warning" $refusal
Assert-True ($null -eq (Get-ComparisonRefusal { Assert-WindowComparable $known $known "baseline.json" "rural/standing_still" })) "an implicit comparison that passes returns no refusal"

# Preflight uses a controlled identity provider and never invokes the engine or a campaign.
function Get-AssetsIdentity([string]$ResolvedAssets) { return $script:preflightIdentity }
$script:preflightIdentity = @{ unavailable = $unknownReason }
$oldKnown = @{ scenarios = @{ rural = @{ windows = @{ standing_still = $known } } } }
$oldUnknown = @{ scenarios = @{ rural = @{ windows = @{ standing_still = $unknownSide } } } }
$windowCounts = @{ rural = 1; synthetic = 1 }
$explicitCompare = $true
Assert-Throws { Get-ComparisonRefusal { Assert-ComparisonAssetIdentities $oldKnown "D:\Mudcrab\Converted" @("synthetic", "rural") $windowCounts "baseline.json" } } "no known identity" "preflight refuses a current unknown identity before timed windows"
$script:preflightIdentity = @{ unavailable = $null }
Assert-Throws { Assert-ComparisonAssetIdentities $oldUnknown "D:\Mudcrab\Converted" @("rural") $windowCounts "baseline.json" } "no known identity" "preflight refuses an unknown baseline identity"
Assert-Passes { Assert-ComparisonAssetIdentities $oldKnown "D:\Mudcrab\Converted" @("rural") $windowCounts "baseline.json" } "preflight accepts known identities"
$script:preflightIdentity = @{ unavailable = $unknownReason }
Assert-Passes { Assert-ComparisonAssetIdentities $oldUnknown $null @("synthetic") $windowCounts "baseline.json" } "synthetic-only preflight needs no asset identity"
$explicitCompare = $false
$refusal = Get-ComparisonRefusal { Assert-ComparisonAssetIdentities $oldKnown "D:\Mudcrab\Converted" @("rural") $windowCounts "baseline.json" }
Assert-True ($refusal -match "no known identity") "implicit preflight returns a warning reason instead of throwing" $refusal

$tokens = $null; $errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($baselineScript, [ref]$tokens, [ref]$errors)
$preflightCalls = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq "Assert-ComparisonAssetIdentities" }, $true))
$engineCalls = @($ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq "Invoke-EngineWindow" }, $true))
Assert-True ($preflightCalls.Count -eq 1 -and $engineCalls.Count -eq 1 -and $preflightCalls[0].Extent.StartOffset -lt $engineCalls[0].Extent.StartOffset) "campaign identity preflight precedes its timed engine invocation"

# --- Result ------------------------------------------------------------------------------------------

Write-Host "phase2 baseline comparison fixtures: $script:passed passed, $($script:failures.Count) failed"
foreach ($failure in $script:failures) { Write-Host "FAILED $failure" }
if ($script:failures.Count -gt 0) { exit 1 }
