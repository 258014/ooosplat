[CmdletBinding()]
param(
    [switch]$ExportHelp,
    [switch]$Help
)

if ($Help) {
    @'
用法：
  powershell -ExecutionPolicy Bypass -File scripts/verify_env.ps1
  powershell -ExecutionPolicy Bypass -File scripts/verify_env.ps1 -ExportHelp

-ExportHelp 会在当前目录的 docs/ 下导出四个 COLMAP 子命令帮助文件。
'@ | Write-Output
    exit 0
}

# OOOSplat 环境验证脚本（Windows）。
$ErrorActionPreference = 'Continue'

$checks = @()

function Add-Check {
    param(
        [string]$Name,
        [string]$Command,
        [string[]]$Arguments
    )

    $commandInfo = Get-Command $Command -ErrorAction SilentlyContinue
    if ($null -eq $commandInfo) {
        Write-Host ("❌ {0,-28} 命令缺失：请安装 {1} 后重试（不自动安装）" -f $Name, $Command)
        $script:checks += [pscustomobject]@{ Mark = '❌'; Name = $Name; Status = '命令缺失' }
        return
    }

    $output = & $commandInfo.Source @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    $firstLine = ($output | Select-Object -First 1 | Out-String).Trim()
    if ([string]::IsNullOrWhiteSpace($firstLine)) { $firstLine = '命令已执行，无文本输出' }
    if ($exitCode -eq 0) {
        Write-Host ("✅ {0,-28} {1}" -f $Name, $firstLine)
        $script:checks += [pscustomobject]@{ Mark = '✅'; Name = $Name; Status = $firstLine }
    } else {
        Write-Host ("❌ {0,-28} 命令失败（退出码 {1}）" -f $Name, $exitCode)
        $script:checks += [pscustomobject]@{ Mark = '❌'; Name = $Name; Status = "命令失败（退出码 $exitCode）" }
    }
}

function Export-ColmapHelp {
    param(
        [string]$OutputPath,
        [string[]]$Arguments
    )

    $commandInfo = Get-Command colmap -ErrorAction SilentlyContinue
    if ($null -eq $commandInfo) {
        Write-Host "❌ 无法导出 $OutputPath：未找到 colmap"
        return
    }

    $output = & $commandInfo.Source @Arguments -h 2>&1
    $output | Set-Content -LiteralPath $OutputPath -Encoding UTF8
    if ($LASTEXITCODE -eq 0) {
        Write-Host "✅ 已导出 $OutputPath"
    } else {
        Write-Host "⚠️ 已保存 $OutputPath，但 colmap 退出码为 $LASTEXITCODE（请检查文件内容）"
    }
}

Write-Host 'OOOSplat 环境验证'
Write-Host '================='
Add-Check 'ffmpeg' 'ffmpeg' @('-version')
Add-Check 'ffprobe' 'ffprobe' @('-version')
Add-Check 'colmap -h' 'colmap' @('-h')
Add-Check 'colmap global_mapper -h' 'colmap' @('global_mapper', '-h')
Add-Check 'cargo' 'cargo' @('--version')
Add-Check 'node' 'node' @('--version')

if ($ExportHelp) {
    $docsPath = Join-Path (Get-Location) 'docs'
    New-Item -ItemType Directory -Path $docsPath -Force | Out-Null
    Export-ColmapHelp (Join-Path $docsPath 'colmap_global_mapper_help.txt') @('global_mapper')
    Export-ColmapHelp (Join-Path $docsPath 'colmap_mapper_help.txt') @('mapper')
    Export-ColmapHelp (Join-Path $docsPath 'colmap_feature_extractor_help.txt') @('feature_extractor')
    Export-ColmapHelp (Join-Path $docsPath 'colmap_sequential_matcher_help.txt') @('sequential_matcher')
}

Write-Host "`n状态汇总"
$checks | Format-Table -Property Mark, Name, Status -AutoSize
if (($checks | Where-Object { $_.Mark -eq '❌' }).Count -gt 0) {
    exit 1
}
