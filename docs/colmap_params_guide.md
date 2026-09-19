# COLMAP 参数导出与使用说明

本项目锁定 COLMAP 4.0.4。环境验证脚本支持可选的 `--export-help` 模式，用于把当前机器实际安装版本的参数帮助保存到 `docs/`，不自动安装、升级或替换 COLMAP。

## 导出命令

在 OOOSplat 仓库根目录执行：

### Linux/macOS

```bash
bash scripts/verify_env.sh --export-help
```

### Windows PowerShell

```powershell
powershell -ExecutionPolicy Bypass -File scripts/verify_env.ps1 -ExportHelp
```

也可以手动执行下列命令：

```bash
colmap global_mapper -h > docs/colmap_global_mapper_help.txt
colmap mapper -h > docs/colmap_mapper_help.txt
colmap feature_extractor -h > docs/colmap_feature_extractor_help.txt
colmap sequential_matcher -h > docs/colmap_sequential_matcher_help.txt
```

PowerShell 等价写法：

```powershell
colmap global_mapper -h | Out-File -FilePath docs/colmap_global_mapper_help.txt -Encoding utf8
colmap mapper -h | Out-File -FilePath docs/colmap_mapper_help.txt -Encoding utf8
colmap feature_extractor -h | Out-File -FilePath docs/colmap_feature_extractor_help.txt -Encoding utf8
colmap sequential_matcher -h | Out-File -FilePath docs/colmap_sequential_matcher_help.txt -Encoding utf8
```

## 为什么必须先导出

COLMAP 命令的参数名称、默认值、可用选项和子命令帮助文本可能随版本、构建选项或平台发行包变化。后续脚本和流水线只能根据当前实际输出确认参数，不能凭记忆抄写参数，也不能把其他版本的帮助当作 4.0.4 的契约。

导出的文本同时是基线实验的可审计证据：它能记录本次实验究竟使用了哪些命令能力，并帮助解释不同机器或不同构建之间的结果差异。建议把这些文件与 `baseline_report.md` 一起归档；如果重新安装或替换了 COLMAP 构建，应重新导出并复核差异。

## 导出文件说明

- `colmap_global_mapper_help.txt`：全局 SfM 基线命令的实际参数。
- `colmap_mapper_help.txt`：增量式 SfM 基线命令的实际参数。
- `colmap_feature_extractor_help.txt`：生成特征时使用的参数依据。
- `colmap_sequential_matcher_help.txt`：生成匹配时使用的参数依据。

环境验证脚本发现 `colmap` 缺失时只报告缺失并给出人工准备提示，不会自动安装；这符合项目的零配置、可审计和禁止隐式升级要求。
