[CmdletBinding()]
param(
    # Root that holds the OOOSplat project folders.
    [string]$ProjectsRoot = 'D:\123',

    # Only harvest runs started at or after this moment. The default is today at
    # 04:00 local time, so that samples come from the current engine/pipeline
    # build rather than from earlier runs with different behaviour.
    [datetime]$Since = (Get-Date).Date.AddHours(4),

    # Where the CSV and fitted report are written.
    [string]$OutputDir = '',

    # Mapper runs that registered fewer than this fraction of their input images
    # are treated as degenerate and kept out of the fit: the time a mapper spends
    # failing is not the cost of reconstructing the scene.
    [double]$MinRegisteredRatio = 0.5
)

# Harvests mapper cost samples from OOOSplat's own completed runs.
#
# This is the production-faithful alternative to a synthetic sweep: every sample
# carries the real preset (which sets matching density), the real number of images
# COLMAP received, the real registered ratio, and the wall time COLMAP itself
# reported. Nothing is re-run and nothing is modified; projects are only read.
#
# Sources per project:
#   project.json  -> status, preset, quality, input/output counts, duration
#   state.json    -> frames.estimatedFrames / extractedFrames / filteredFrames,
#                    mapperBackend (which backend actually ran)
#   logs/colmap.log -> "Reconstruction done in X seconds" for the mapper stage
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $root 'mapper_samples'
}
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$OutputDir = (Resolve-Path -LiteralPath $OutputDir).Path

if (-not (Test-Path -LiteralPath $ProjectsRoot -PathType Container)) {
    throw "Projects root not found: $ProjectsRoot"
}

Write-Host "Projects root : $ProjectsRoot"
Write-Host "Harvest since : $($Since.ToString('yyyy-MM-dd HH:mm:ss'))"
Write-Host ""

$samples = @()
$skipped = @()

foreach ($project in Get-ChildItem -LiteralPath $ProjectsRoot -Directory) {
    $metadataPath = Join-Path $project.FullName 'project.json'
    if (-not (Test-Path -LiteralPath $metadataPath -PathType Leaf)) { continue }

    $started = $project.LastWriteTime
    if ($started -lt $Since) {
        $skipped += "$($project.Name)：项目时间 $($started.ToString('MM-dd HH:mm')) 早于阈值"
        continue
    }

    $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
    if ($metadata.status -ne 'completed') {
        $skipped += "$($project.Name)：状态为 $($metadata.status)，不是已完成运行"
        continue
    }

    $statePath = Join-Path $project.FullName 'state.json'
    $filtered = $null
    $extracted = $null
    $backend = ''
    if (Test-Path -LiteralPath $statePath -PathType Leaf) {
        $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
        $filtered = $state.frames.filteredFrames
        $extracted = $state.frames.extractedFrames
        $backend = [string]$state.mapperBackend
    }

    # Images COLMAP actually received: the post-filter count when the smart filter
    # ran, otherwise the raw extraction count.
    $mapped = if ($null -ne $filtered -and $filtered -gt 0) { [int]$filtered }
              elseif ($null -ne $extracted -and $extracted -gt 0) { [int]$extracted }
              else { $null }
    if ($null -eq $mapped) {
        $skipped += "$($project.Name)：state.json 没有可用的帧数"
        continue
    }

    # The mapper's own wall time comes from COLMAP's final summary line.
    $logPath = Join-Path $project.FullName 'logs\colmap.log'
    if (-not (Test-Path -LiteralPath $logPath -PathType Leaf)) {
        $skipped += "$($project.Name)：缺少 logs/colmap.log"
        continue
    }
    $log = Get-Content -LiteralPath $logPath -Raw
    $mapperMs = $null
    if ($log -match 'Reconstruction done in ([0-9]+(?:\.[0-9]+)?) seconds') {
        $mapperMs = [math]::Round([double]$Matches[1] * 1000, 1)
    }
    if ($null -eq $mapperMs) {
        $skipped += "$($project.Name)：日志中没有 'Reconstruction done in ... seconds'"
        continue
    }

    $inputImages = $metadata.output.inputImages
    $registered = $metadata.output.registeredImages
    $samples += [pscustomobject]@{
        Project        = $project.Name
        StartedAt      = $started.ToString('yyyy-MM-dd HH:mm:ss')
        Preset         = [string]$metadata.quality
        Backend        = if ([string]::IsNullOrWhiteSpace($backend)) { 'unknown' } else { $backend }
        MappedFrames   = $mapped
        Registered     = $registered
        RegisteredRate = if ($mapped -gt 0) { [math]::Round($registered / $mapped, 4) } else { $null }
        MapperMs       = $mapperMs
        ProjectMs      = $metadata.durationMs
    }
}

if ($samples.Count -eq 0) {
    Write-Warning '没有可用的样本。'
    foreach ($note in $skipped) { Write-Host "  跳过 $note" }
    exit 0
}

$csvPath = Join-Path $OutputDir 'mapper_samples.csv'
$samples | Sort-Object StartedAt | Export-Csv -LiteralPath $csvPath -NoTypeInformation -Encoding UTF8

$lines = @(
    '# Mapper 成本样本（来自真实运行）',
    '',
    "- 项目根目录：``$ProjectsRoot``",
    "- 仅采集：$($Since.ToString('yyyy-MM-dd HH:mm:ss')) 及之后",
    "- 采集时间：$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))",
    '',
    '> 每个样本都带真实档位（决定匹配密度）、真实送入 COLMAP 的图像数、真实注册率，',
    '> 以及 COLMAP 自报的重建耗时。没有重跑任何实验，也没有修改任何项目文件。',
    '',
    '## 样本',
    '',
    '| 项目 | 档位 | 后端 | 送入帧数 | 注册数 | 注册率 | 重建耗时（s） |',
    '| --- | --- | --- | ---: | ---: | ---: | ---: |'
)
foreach ($row in $samples | Sort-Object MappedFrames) {
    $lines += ('| {0} | {1} | {2} | {3} | {4} | {5:P1} | {6:N1} |' -f `
        $row.Project, $row.Preset, $row.Backend, $row.MappedFrames, $row.Registered,
        $row.RegisteredRate, ($row.MapperMs / 1000))
}
$lines += ''
$lines += '## 幂律拟合（按后端 + 档位分组）'
$lines += ''
$lines += '同一组内至少需要 3 个不同帧数档且注册率达标才能拟合；不足时只报缺口，不给系数。'
$lines += ''

$groups = $samples | Group-Object { "$($_.Backend)|$($_.Preset)" }
foreach ($group in $groups | Sort-Object Name) {
    $usable = @($group.Group | Where-Object { $null -ne $_.RegisteredRate -and $_.RegisteredRate -ge $MinRegisteredRatio })
    $degenerate = @($group.Group | Where-Object { $null -eq $_.RegisteredRate -or $_.RegisteredRate -lt $MinRegisteredRatio })
    $lines += "### $($group.Name)"
    $lines += ''

    $distinct = @($usable | Select-Object -ExpandProperty MappedFrames -Unique)
    if ($distinct.Count -lt 3) {
        $lines += "- 无法拟合：注册率达标的不同帧数档只有 $($distinct.Count) 个，需要 3 个以上。"
        $lines += '- 继续用同一档位与后端跑不同规模的素材，样本会自动补齐。'
    } else {
        $xs = $usable | ForEach-Object { [math]::Log($_.MappedFrames) }
        $ys = $usable | ForEach-Object { [math]::Log($_.MapperMs) }
        $xMean = ($xs | Measure-Object -Average).Average
        $yMean = ($ys | Measure-Object -Average).Average
        $sxx = 0.0; $sxy = 0.0; $syy = 0.0
        for ($i = 0; $i -lt $xs.Count; $i++) {
            $dx = $xs[$i] - $xMean; $dy = $ys[$i] - $yMean
            $sxx += $dx * $dx; $sxy += $dx * $dy; $syy += $dy * $dy
        }
        if ($sxx -le 0) {
            $lines += '- 无法拟合：所有样本帧数相同。'
        } else {
            $b = $sxy / $sxx
            $a = [math]::Exp($yMean - $b * $xMean)
            $ssRes = 0.0
            for ($i = 0; $i -lt $xs.Count; $i++) {
                $ssRes += [math]::Pow($ys[$i] - ([math]::Log($a) + $b * $xs[$i]), 2)
            }
            $r2 = if ($syy -gt 0) { 1 - ($ssRes / $syy) } else { 0 }
            $lines += ('- a = {0:N4}，b = {1:N4}，R² = {2:N4}（{3} 个样本）' -f $a, $b, $r2, $usable.Count)
            if ($usable.Count -lt 5) {
                $lines += '- ⚠️ 样本少于 5 个：系数仅供趋势参考，不要直接写进代码。'
            }
            if ($r2 -lt 0.9) {
                $lines += '- ⚠️ R² 低于 0.9：素材难度或后台负载差异可能主导了结果。'
            }
            Write-Host ("{0,-28} a = {1,10:N4}  b = {2:N4}  R2 = {3:N4}  (n={4})" -f `
                $group.Name, $a, $b, $r2, $usable.Count)
        }
    }
    if ($degenerate.Count -gt 0) {
        $lines += ''
        $lines += '未参与拟合（退化或注册率不足）：'
        foreach ($row in $degenerate) {
            $lines += ('- {0}：{1}/{2} 注册' -f $row.Project, $row.Registered, $row.MappedFrames)
        }
    }
    $lines += ''
}

$lines += '## 被跳过的项目'
$lines += ''
if ($skipped.Count -eq 0) {
    $lines += '- 无'
} else {
    foreach ($note in $skipped) { $lines += "- $note" }
}

$reportPath = Join-Path $OutputDir 'mapper_samples_report.md'
$lines | Set-Content -LiteralPath $reportPath -Encoding UTF8

Write-Host ''
Write-Host "样本数: $($samples.Count)"
Write-Host "CSV:    $csvPath"
Write-Host "Report: $reportPath"
