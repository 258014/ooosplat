[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$ProjectDir
)

# OOOSplat COLMAP 全局 SfM 与增量 SfM 基线测量脚本（Windows）。
$ErrorActionPreference = 'Continue'
$resolvedProject = (Resolve-Path -LiteralPath $ProjectDir -ErrorAction Stop).Path
$databasePath = Join-Path $resolvedProject 'colmap\database.db'
$imagePath = Join-Path $resolvedProject 'frames'
$reportPath = Join-Path $resolvedProject 'baseline_report.md'

if (-not (Test-Path -LiteralPath $databasePath -PathType Leaf)) { throw "缺少数据库：$databasePath" }
if (-not (Test-Path -LiteralPath $imagePath -PathType Container)) { throw "缺少图像目录：$imagePath" }
$colmap = Get-Command colmap -ErrorAction SilentlyContinue
if ($null -eq $colmap) { throw '未找到 colmap；请先准备锁定版本的 COLMAP 4.0.4。' }

function Invoke-ColmapMeasured {
    param(
        [string]$Label,
        [string]$Subcommand,
        [string]$OutputPath
    )

    $logPath = Join-Path $resolvedProject "baseline_${Label}.log"
    $arguments = @(
        $Subcommand,
        '--database_path', $databasePath,
        '--image_path', $imagePath,
        '--output_path', $OutputPath
    )
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    # Start-Process 需要一个完整参数字符串；Windows 路径中的引号必须成对转义。
    $quotedArguments = @()
    foreach ($argument in $arguments) {
        $quotedArguments += '"' + ($argument -replace '"', '\\"') + '"'
    }
    $argumentString = $quotedArguments -join ' '
    $stderrPath = "$logPath.err"
    $process = Start-Process -FilePath $colmap.Source -ArgumentList $argumentString -RedirectStandardOutput $logPath -RedirectStandardError $stderrPath -PassThru
    $peakBytes = [int64]0
    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            if ($process.WorkingSet64 -gt $peakBytes) { $peakBytes = $process.WorkingSet64 }
        } catch {
            # 进程退出瞬间可能无法刷新，保留已采集的最大值。
        }
        Start-Sleep -Milliseconds 250
    }
    $process.Refresh()
    if ($process.WorkingSet64 -gt $peakBytes) { $peakBytes = $process.WorkingSet64 }
    $exitCode = $process.ExitCode
    $stopwatch.Stop()
    $elapsed = [math]::Round($stopwatch.Elapsed.TotalSeconds, 3)
    if (Test-Path -LiteralPath $stderrPath) {
        Add-Content -LiteralPath $logPath -Value (Get-Content -LiteralPath $stderrPath -Raw)
        Remove-Item -LiteralPath $stderrPath -Force
    }

    if ($peakBytes -gt 0) {
        $peakMemory = '{0:N0} bytes' -f $peakBytes
    } else {
        $peakMemory = '不可用（无法读取进程工作集）'
    }
    $modelDir = Join-Path $OutputPath '0'
    if (Test-Path -LiteralPath (Join-Path $modelDir 'cameras.bin')) {
        $cameras = '是'
    } else {
        $cameras = '否'
    }
    if (Test-Path -LiteralPath (Join-Path $modelDir 'images.bin')) {
        $images = '是'
    } else {
        $images = '否'
    }
    if (Test-Path -LiteralPath (Join-Path $modelDir 'points3D.bin')) {
        $points = '是'
    } else {
        $points = '否'
    }

    return [pscustomobject]@{
        Label = $Label
        Elapsed = $elapsed
        PeakMemory = $peakMemory
        ExitCode = $exitCode
        Cameras = $cameras
        Images = $images
        Points = $points
        LogPath = $logPath
        ModelPath = $modelDir
    }
}

function Get-ModelStats {
    param([pscustomobject]$Measurement)

    $analyzerLog = Join-Path $resolvedProject "baseline_$($Measurement.Label)_model_analyzer.log"
    if (-not (Test-Path -LiteralPath $Measurement.ModelPath -PathType Container)) {
        return [pscustomobject]@{ Registered = '不可用'; Points = '不可用'; Error = '模型目录不存在'; AnalyzerExitCode = '不可用'; AnalyzerLog = $analyzerLog }
    }

    & $colmap.Source 'model_analyzer' '--path' $Measurement.ModelPath *> $analyzerLog
    $analyzerExit = $LASTEXITCODE
    if ($analyzerExit -ne 0) {
        return [pscustomobject]@{ Registered = '不可用'; Points = '不可用'; Error = 'model_analyzer 执行失败'; AnalyzerExitCode = $analyzerExit; AnalyzerLog = $analyzerLog }
    }

    $text = Get-Content -LiteralPath $analyzerLog -Raw
    if ($text -match '(?im)(?:Registered images|Images registered)\D+(\d+)') {
        $registered = $Matches[1]
    } else {
        $registered = '未识别（见原始日志）'
    }
    if ($text -match '(?im)(?:Points|3D points)\D+(\d+)') {
        $points = $Matches[1]
    } else {
        $points = '未识别（见原始日志）'
    }
    if ($text -match '(?im)mean[^\r\n]*reprojection error\D+([0-9]+(?:\.[0-9]+)?)') {
        $error = $Matches[1]
    } else {
        $error = '未识别（见原始日志）'
    }
    return [pscustomobject]@{ Registered = $registered; Points = $points; Error = $error; AnalyzerExitCode = $analyzerExit; AnalyzerLog = $analyzerLog }
}

$globalOutput = Join-Path $resolvedProject 'baseline_global'
$incrementalOutput = Join-Path $resolvedProject 'baseline_incremental'
$global = Invoke-ColmapMeasured 'global' 'global_mapper' $globalOutput
$incremental = Invoke-ColmapMeasured 'incremental' 'mapper' $incrementalOutput
$globalStats = Get-ModelStats $global
$incrementalStats = Get-ModelStats $incremental

if ($global.Elapsed -gt 0) {
    $speedup = [math]::Round($incremental.Elapsed / $global.Elapsed, 3)
} else {
    $speedup = '不可用'
}
$imageCount = @(Get-ChildItem -LiteralPath $imagePath -File -Recurse | Where-Object { $_.Extension -in @('.jpg', '.jpeg', '.png') }).Count
$registrationDelta = '不可用'
if ($imageCount -gt 0 -and ($globalStats.Registered -as [int]) -ne $null -and ($incrementalStats.Registered -as [int]) -ne $null) {
    $globalRate = [int]$globalStats.Registered / $imageCount * 100
    $incrementalRate = [int]$incrementalStats.Registered / $imageCount * 100
    $delta = $globalRate - $incrementalRate
    $registrationDelta = "global {0:N3}%，mapper {1:N3}%，差异 {2:N3} 个百分点" -f $globalRate, $incrementalRate, $delta
}

$lines = @(
    '# COLMAP 基线测量报告',
    '',
    "- 项目目录：``$resolvedProject``",
    "- 数据库：``$databasePath``",
    "- 图像目录：``$imagePath``",
    "- 测量时间：$((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))",
    '- 峰值内存口径：Windows 通过 PowerShell 轮询 COLMAP 进程 WorkingSet64，采样间隔约 250 毫秒；子进程峰值可能未被采到。',
    '',
    '> 本报告比较同一数据库与同一图像目录上的 `global_mapper` 和 `mapper`。请勿把两次运行混用不同素材或不同参数。',
    '',
    '## 重建执行结果',
    '',
    '| 方法 | 墙钟耗时（秒） | 峰值内存 | 退出码 | cameras.bin | images.bin | points3D.bin |',
    '| --- | ---: | --- | ---: | --- | --- | --- |',
    "| global_mapper | $($global.Elapsed) | $($global.PeakMemory) | $($global.ExitCode) | $($global.Cameras) | $($global.Images) | $($global.Points) |",
    "| mapper | $($incremental.Elapsed) | $($incremental.PeakMemory) | $($incremental.ExitCode) | $($incremental.Cameras) | $($incremental.Images) | $($incremental.Points) |",
    '',
    '## 模型统计',
    '',
    '| 方法 | 注册图像数 | 三维点数 | 平均重投影误差 | analyzer 退出码 |',
    '| --- | ---: | ---: | ---: | ---: |',
    "| global_mapper | $($globalStats.Registered) | $($globalStats.Points) | $($globalStats.Error) | $($globalStats.AnalyzerExitCode) |",
    "| mapper | $($incrementalStats.Registered) | $($incrementalStats.Points) | $($incrementalStats.Error) | $($incrementalStats.AnalyzerExitCode) |",
    '',
    '原始日志：',
    "- global_mapper：``$($global.LogPath)``",
    "- mapper：``$($incremental.LogPath)``",
    "- global_mapper model_analyzer：``$($globalStats.AnalyzerLog)``",
    "- mapper model_analyzer：``$($incrementalStats.AnalyzerLog)``",
    '',
    '## 对比结论',
    '',
    "- 提速倍数（mapper 耗时 / global_mapper 耗时）：**$speedup 倍**。",
    "- 注册率差异：**$registrationDelta**（分母为 frames/ 下的 JPG/JPEG/PNG 文件数；若字段未识别，请依据上方原始日志补录；脚本不会猜测）。"
)
if ($speedup -is [double] -and $speedup -lt 3) {
    $lines += ''
    $lines += '⚠️ 本次加速比低于预期，建议在更长/更复杂的素材上复测，或重新评估本优化方案的优先级。'
}
$lines | Set-Content -LiteralPath $reportPath -Encoding UTF8
Write-Host "✅ 报告已写入：$reportPath"
