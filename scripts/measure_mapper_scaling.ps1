[CmdletBinding()]
param(
    # Directory holding the frames a real run fed to COLMAP (usually
    # <project>/work/frames or <project>/work/frames_filtered). The sweep only
    # ever reads from it and never modifies it.
    [Parameter(Mandatory = $true)]
    [string]$FramesDir,

    # Frame counts to measure. Each level is built by keeping every k-th frame.
    [int[]]$Levels = @(80, 160, 320, 640),

    # Timed repeats per backend and level. The median is reported.
    [int]$Repeats = 2,

    [string]$Colmap = '',

    [switch]$Cpu,

    # SIFT limits mirroring the Balanced preset, so the match graph the mapper
    # times is the one production builds. COLMAP 4.x keeps max_image_size in the
    # FeatureExtraction namespace but the other SIFT knobs in SiftExtraction.
    [int]$MaxImageSize = 1600,
    [int]$MaxNumFeatures = 8192,

    [string]$OutputDir = ''
)

# Measures how COLMAP mapper wall time scales with the number of input images, so
# the runtime-estimate coefficients in src-tauri/src/pipeline/estimate.rs can be
# regressed against this machine and this engine build instead of guessed.
#
# Only mapper time is measured: features and matches are produced once per level
# before any timing starts, so the numbers isolate reconstruction. Background
# work, a different GPU mode or a different engine build invalidates the result.
#
# Errors stay non-terminating because COLMAP writes progress and warnings to
# stderr; every step below checks $LASTEXITCODE explicitly instead.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot

if ([string]::IsNullOrWhiteSpace($Colmap)) {
    $Colmap = Join-Path $root 'engines/colmap/bin/colmap.exe'
}
if (-not (Test-Path -LiteralPath $Colmap -PathType Leaf)) {
    throw "COLMAP not found: $Colmap"
}
$framesSource = (Resolve-Path -LiteralPath $FramesDir -ErrorAction Stop).Path
if (-not (Test-Path -LiteralPath $framesSource -PathType Container)) {
    throw "Frames directory not found: $framesSource"
}
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $root 'mapper_scaling'
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$OutputDir = (Resolve-Path -LiteralPath $OutputDir).Path

$allFrames = Get-ChildItem -LiteralPath $framesSource -File |
    Where-Object { $_.Extension -in @('.jpg', '.jpeg', '.png') } |
    Sort-Object Name
if ($allFrames.Count -lt 10) {
    throw "Need at least 10 frames in $framesSource, found $($allFrames.Count)"
}
Write-Host "Source frames: $($allFrames.Count) in $framesSource"

$gpuValue = if ($Cpu) { '0' } else { '1' }

function Invoke-Timed {
    param(
        [string]$Label,
        [string[]]$Arguments,
        [string]$LogPath
    )

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    & $Colmap @Arguments *> $LogPath
    $exitCode = $LASTEXITCODE
    $stopwatch.Stop()
    return [pscustomobject]@{
        Label     = $Label
        ElapsedMs = [math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1)
        ExitCode  = $exitCode
        LogPath   = $LogPath
    }
}

function Get-RegisteredImages {
    param([string]$ModelPath, [string]$LogPath)

    if (-not (Test-Path -LiteralPath (Join-Path $ModelPath 'images.bin'))) {
        return $null
    }
    & $Colmap 'model_analyzer' '--path' $ModelPath *> $LogPath
    if ($LASTEXITCODE -ne 0) {
        return $null
    }
    $text = Get-Content -LiteralPath $LogPath -Raw
    if ($text -match '(?im)(?:Registered images|Images registered)\D+(\d+)') {
        return [int]$Matches[1]
    }
    return $null
}

function Get-Stride {
    param([int]$Total, [int]$Target)

    if ($Target -le 0) { return 1 }
    $stride = [math]::Floor($Total / $Target)
    if ($stride -lt 1) { return 1 }
    return [int]$stride
}

$measurements = @()
# Must be an array up front: in PowerShell `$null += "text"` yields a String, so
# every skipped entry would silently collapse into one concatenated line.
$failures = @()
foreach ($level in $Levels) {
    $stride = Get-Stride -Total $allFrames.Count -Target $level
    $levelDir = Join-Path $OutputDir ("n{0}" -f $level)
    $levelFrames = Join-Path $levelDir 'frames'
    if (Test-Path -LiteralPath $levelDir) {
        Remove-Item -LiteralPath $levelDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $levelFrames | Out-Null

    $selected = @()
    for ($index = 0; $index -lt $allFrames.Count; $index += $stride) {
        $selected += $allFrames[$index]
    }
    foreach ($frame in $selected) {
        Copy-Item -LiteralPath $frame.FullName -Destination $levelFrames
    }
    $frameCount = $selected.Count
    Write-Host ""
    Write-Host "=== Level $level -> $frameCount frames (stride $stride) ==="

    $database = Join-Path $levelDir 'database.db'
    # Warm-up: features and matches are produced before timing so that the mapper
    # measurement isolates reconstruction cost.
    Write-Host "  preparing database (features + sequential matches, not timed)"
    $prepareLog = Join-Path $levelDir 'prepare.log'
    & $Colmap 'feature_extractor' '--database_path' $database '--image_path' $levelFrames `
        '--ImageReader.camera_model' 'SIMPLE_RADIAL' '--ImageReader.single_camera' '1' `
        '--FeatureExtraction.use_gpu' $gpuValue `
        '--FeatureExtraction.max_image_size' $MaxImageSize `
        '--SiftExtraction.max_num_features' $MaxNumFeatures *> $prepareLog
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "feature_extractor failed for level $level; see $prepareLog"
        $failures += "feature_extractor 失败（目标 $level 帧，实际 $frameCount 帧）：$prepareLog"
        continue
    }
    & $Colmap 'sequential_matcher' '--database_path' $database `
        '--FeatureMatching.use_gpu' $gpuValue `
        '--SequentialMatching.overlap' '15' '--SequentialMatching.quadratic_overlap' '1' *>> $prepareLog
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "sequential_matcher failed for level $level; see $prepareLog"
        $failures += "sequential_matcher 失败（目标 $level 帧，实际 $frameCount 帧）：$prepareLog"
        continue
    }

    foreach ($backend in @('mapper', 'global_mapper')) {
        $times = @()
        $registered = $null
        for ($repeat = 1; $repeat -le $Repeats; $repeat++) {
            $outPath = Join-Path $levelDir "$backend-$repeat"
            New-Item -ItemType Directory -Force -Path $outPath | Out-Null
            $logPath = Join-Path $levelDir "$backend-$repeat.log"
            $result = Invoke-Timed -Label $backend -LogPath $logPath -Arguments @(
                $backend, '--database_path', $database, '--image_path', $levelFrames,
                '--output_path', $outPath
            )
            if ($result.ExitCode -eq 0) {
                $times += $result.ElapsedMs
                if ($null -eq $registered) {
                    $registered = Get-RegisteredImages -ModelPath (Join-Path $outPath '0') `
                        -LogPath (Join-Path $levelDir "$backend-$repeat-analyzer.log")
                }
            } else {
                Write-Warning "  $backend repeat $repeat exited $($result.ExitCode); see $logPath"
                $failures += "$backend 退出码 $($result.ExitCode)（$frameCount 帧，第 $repeat 次）：$logPath"
            }
        }
        if ($times.Count -eq 0) {
            Write-Warning "  $backend produced no successful run at $frameCount frames"
            $failures += "$backend 在 $frameCount 帧上没有成功运行（无有效计时）"
            continue
        }
        $sorted = $times | Sort-Object
        $median = if ($sorted.Count % 2 -eq 1) {
            $sorted[[int]($sorted.Count / 2)]
        } else {
            ($sorted[$sorted.Count / 2 - 1] + $sorted[$sorted.Count / 2]) / 2
        }
        Write-Host ("  {0,-15} median {1,10:N0} ms over {2} run(s)" -f $backend, $median, $times.Count)
        $measurements += [pscustomobject]@{
            Frames     = $frameCount
            Backend    = $backend
            MedianMs   = [math]::Round($median, 1)
            Runs       = $times.Count
            Registered = $registered
        }
    }
}

if ($measurements.Count -eq 0) {
    throw "No successful mapper measurements; nothing to fit."
}

$csvPath = Join-Path $OutputDir 'mapper_scaling.csv'
$measurements | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8

# Log-log least squares on t = a * n^b  =>  ln t = ln a + b ln n
function Get-PowerFit {
    param([object[]]$Points)

    $valid = $Points | Where-Object { $_.Frames -gt 0 -and $_.MedianMs -gt 0 }
    if ($valid.Count -lt 3) {
        return [pscustomobject]@{
            Coefficient = $null; Exponent = $null; RSquared = $null
            Points = $valid.Count; Note = '需要至少 3 个有效帧数档才能拟合'
        }
    }
    $xs = $valid | ForEach-Object { [math]::Log($_.Frames) }
    $ys = $valid | ForEach-Object { [math]::Log($_.MedianMs) }
    $xMean = ($xs | Measure-Object -Average).Average
    $yMean = ($ys | Measure-Object -Average).Average
    $sxx = 0.0; $sxy = 0.0; $syy = 0.0
    for ($index = 0; $index -lt $xs.Count; $index++) {
        $dx = $xs[$index] - $xMean
        $dy = $ys[$index] - $yMean
        $sxx += $dx * $dx
        $sxy += $dx * $dy
        $syy += $dy * $dy
    }
    if ($sxx -le 0) {
        return [pscustomobject]@{
            Coefficient = $null; Exponent = $null; RSquared = $null
            Points = $valid.Count; Note = '所有样本帧数相同，无法拟合斜率'
        }
    }
    $b = $sxy / $sxx
    $a = [math]::Exp($yMean - $b * $xMean)
    $ssRes = 0.0
    for ($index = 0; $index -lt $xs.Count; $index++) {
        $predicted = [math]::Log($a) + $b * $xs[$index]
        $ssRes += [math]::Pow($ys[$index] - $predicted, 2)
    }
    $rSquared = if ($syy -gt 0) { 1 - ($ssRes / $syy) } else { 0 }
    return [pscustomobject]@{
        Coefficient = $a; Exponent = $b; RSquared = $rSquared
        Points = $valid.Count; Note = ''
    }
}

$fits = @{}
foreach ($backend in @('mapper', 'global_mapper')) {
    $points = $measurements | Where-Object { $_.Backend -eq $backend }
    $fits[$backend] = Get-PowerFit -Points $points
}

$lines = @(
    '# COLMAP mapper 规模回归报告',
    '',
    "- 帧来源：``$framesSource``",
    "- COLMAP：``$Colmap``",
    "- GPU 模式：$(if ($Cpu) { 'CPU (--*Extraction.use_gpu 0)' } else { 'GPU' })",
    "- SIFT 上限：max_image_size=$MaxImageSize，max_num_features=$MaxNumFeatures",
    "- 每档重复次数：$Repeats（取中位数）",
    "- 测量时间：$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))",
    '',
    '> 只有 mapper 被计时：特征提取与顺序匹配在计时前完成，因此这些数字隔离了重建开销。',
    '> 测量期间的后台负载、GPU 模式变化或引擎版本变化都会使结果失效。',
    '',
    '## 测量结果',
    '',
    '| 帧数 | 后端 | 中位耗时（ms） | 有效次数 | 注册图像数 |',
    '| ---: | --- | ---: | ---: | ---: |'
)
foreach ($row in $measurements | Sort-Object Frames, Backend) {
    $registered = if ($null -eq $row.Registered) { '不可用' } else { $row.Registered }
    $lines += "| $($row.Frames) | $($row.Backend) | $($row.MedianMs) | $($row.Runs) | $registered |"
}
$lines += ''
$lines += '## 失败与跳过记录'
$lines += ''
if ($failures.Count -eq 0) {
    $lines += '- 无：所有档位与后端都完成了计时。'
} else {
    $lines += '> 下面这些档位没有进入拟合。请先确认缺口不会让拟合结果失真，再考虑是否采用系数。'
    $lines += ''
    foreach ($failure in $failures) {
        $lines += "- $failure"
    }
}
$lines += ''
$lines += '## 幂律拟合 t = a · n^b'
$lines += ''
foreach ($backend in @('mapper', 'global_mapper')) {
    $fit = $fits[$backend]
    $lines += "### $backend"
    $lines += ''
    if ($null -eq $fit.Coefficient) {
        $lines += "- 未能拟合：$($fit.Note)（有效样本 $($fit.Points) 档）"
    } else {
        $lines += ('- a = {0:N4}，b = {1:N4}，R² = {2:N4}（有效样本 {3} 档）' -f `
            $fit.Coefficient, $fit.Exponent, $fit.RSquared, $fit.Points)
        if ($fit.RSquared -lt 0.9) {
            $lines += '- ⚠️ R² 低于 0.9：样本可能被背景负载、内存换页或场景难度差异污染，建议增加帧数档与重复次数后复测。'
        }
        if ($fit.Exponent -lt 1) {
            $lines += '- ⚠️ 指数小于 1：重建耗时通常随图像数超线性增长，请确认测量确实隔离了 mapper。'
        }
    }
    $lines += ''
}
$lines += '## 回填建议'
$lines += ''
$lines += '把拟合结果写入 `src-tauri/src/pipeline/estimate.rs` 的两个系数常量，并在注释中记录素材、帧数档、日期与 R²。'
$lines += '在把数值当真之前请先确认 R² 与指数是否落在合理区间；本脚本不会替你做这个判断。'
$lines | Set-Content -LiteralPath (Join-Path $OutputDir 'mapper_scaling_report.md') -Encoding UTF8

Write-Host ""
Write-Host "CSV:    $csvPath"
Write-Host "Report: $(Join-Path $OutputDir 'mapper_scaling_report.md')"
Write-Host ""
Write-Host "Fitted power laws (t = a * n^b, t in ms):"
foreach ($backend in @('mapper', 'global_mapper')) {
    $fit = $fits[$backend]
    if ($null -eq $fit.Coefficient) {
        Write-Host ("  {0,-15} not fitted: {1}" -f $backend, $fit.Note)
    } else {
        Write-Host ("  {0,-15} a = {1,10:N4}  b = {2:N4}  R2 = {3:N4}" -f `
            $backend, $fit.Coefficient, $fit.Exponent, $fit.RSquared)
    }
}
Write-Host ""
Write-Host "Current constants in estimate.rs for comparison:"
Write-Host "  Incremental: 176.0 * n^1.5"
Write-Host "  Global:       40.0 * n^1.1"
