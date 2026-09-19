#!/usr/bin/env bash
# OOOSplat COLMAP 全局 SfM 与增量 SfM 基线测量脚本。
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  cat >&2 <<'EOF'
用法：
  bash scripts/measure_baseline.sh "/path/to/OOOSplat 项目"

项目目录下必须存在：
  colmap/database.db
  frames/
EOF
  exit 2
fi

project_dir="$1"
database_path="$project_dir/colmap/database.db"
image_path="$project_dir/frames"
report_path="$project_dir/baseline_report.md"

if [[ ! -f "$database_path" ]]; then
  printf '❌ 缺少数据库：%s\n' "$database_path" >&2
  exit 1
fi
if [[ ! -d "$image_path" ]]; then
  printf '❌ 缺少图像目录：%s\n' "$image_path" >&2
  exit 1
fi
if ! command -v colmap >/dev/null 2>&1; then
  printf '❌ 未找到 colmap；请先准备锁定版本的 COLMAP 4.0.4。\n' >&2
  exit 1
fi

mkdir -p "$project_dir"
: > "$report_path"

cat >"$report_path" <<EOF
# COLMAP 基线测量报告

- 项目目录：\`$project_dir\`
- 数据库：\`$database_path\`
- 图像目录：\`$image_path\`
- 测量时间：$(date -u '+%Y-%m-%dT%H:%M:%SZ')
- 峰值内存口径：Linux 使用 GNU \/usr\/bin\/time 的 Maximum resident set size；macOS 使用 BSD \/usr\/bin\/time 的 maximum resident set size；其他环境标记为不可用。

> 本报告比较同一数据库与同一图像目录上的 \`global_mapper\` 和 \`mapper\`。请勿把两次运行混用不同素材或不同参数。

EOF

measure_memory() {
  # Linux 使用 GNU time（KiB），macOS 使用 BSD time（字节）。
  local time_probe
  time_probe=$(mktemp "${TMPDIR:-/tmp}/ooosplat-time-check.XXXXXX")
  if /usr/bin/time -v true >"$time_probe" 2>&1 && grep -q 'Maximum resident set size' "$time_probe"; then
    rm -f "$time_probe"
    printf 'gnu'
  elif /usr/bin/time -l true >"$time_probe" 2>&1 && grep -q 'maximum resident set size' "$time_probe"; then
    rm -f "$time_probe"
    printf 'bsd'
  else
    rm -f "$time_probe"
    printf '不可用'
  fi
}

run_model() {
  local label="$1"
  local output_path="$2"
  shift 2
  local log_path="$project_dir/baseline_${label}.log"
  local start_seconds end_seconds elapsed_seconds exit_code memory_tool
  start_seconds=$(date +%s)
  exit_code=0
  memory_tool=$(measure_memory)
  if [[ "$memory_tool" == 'gnu' ]]; then
    /usr/bin/time -v colmap "$@" >"$log_path" 2>&1 || exit_code=$?
  elif [[ "$memory_tool" == 'bsd' ]]; then
    /usr/bin/time -l colmap "$@" >"$log_path" 2>&1 || exit_code=$?
  else
    colmap "$@" >"$log_path" 2>&1 || exit_code=$?
  fi
  end_seconds=$(date +%s)
  elapsed_seconds=$(awk -v start="$start_seconds" -v end="$end_seconds" 'BEGIN { printf "%.3f", end-start }')

  local peak_memory='不可用'
  if [[ "$memory_tool" == 'gnu' ]]; then
    peak_memory=$(sed -n 's/^[[:space:]]*Maximum resident set size (kbytes): //p' "$log_path" | tail -n 1)
    [[ -n "$peak_memory" ]] || peak_memory='不可用'
    [[ "$peak_memory" == '不可用' ]] || peak_memory="$peak_memory KiB"
  elif [[ "$memory_tool" == 'bsd' ]]; then
    peak_memory=$(sed -n 's/^[[:space:]]*maximum resident set size: //p' "$log_path" | tail -n 1)
    [[ -n "$peak_memory" ]] || peak_memory='不可用'
    [[ "$peak_memory" == '不可用' ]] || peak_memory="$peak_memory bytes"
  fi

  local model_dir="$output_path/0"
  local cameras='否' images='否' points='否'
  [[ -f "$model_dir/cameras.bin" ]] && cameras='是'
  [[ -f "$model_dir/images.bin" ]] && images='是'
  [[ -f "$model_dir/points3D.bin" ]] && points='是'

  printf '%s\n' "| $label | $elapsed_seconds | $peak_memory | $exit_code | $cameras | $images | $points |" >> "$report_path"
  printf '%s\n' "$elapsed_seconds" > "$project_dir/.baseline_${label}_elapsed"
  printf '%s\n' "$peak_memory" > "$project_dir/.baseline_${label}_memory"
  printf '%s\n' "$exit_code" > "$project_dir/.baseline_${label}_exit"
  printf '%s\n' "$log_path"
}

cat >>"$report_path" <<'EOF'
## 重建执行结果

| 方法 | 墙钟耗时（秒） | 峰值内存 | 退出码 | cameras.bin | images.bin | points3D.bin |
| --- | ---: | --- | ---: | --- | --- | --- |
EOF

run_model global "$project_dir/baseline_global" global_mapper --database_path "$database_path" --image_path "$image_path" --output_path "$project_dir/baseline_global" >/dev/null
run_model incremental "$project_dir/baseline_incremental" mapper --database_path "$database_path" --image_path "$image_path" --output_path "$project_dir/baseline_incremental" >/dev/null

extract_model_stats() {
  local label="$1"
  local model_path="$project_dir/baseline_${label}/0"
  local analyzer_log="$project_dir/baseline_${label}_model_analyzer.log"
  local analyzer_exit=0
  if [[ ! -d "$model_path" ]]; then
    printf '不可用|不可用|不可用|模型目录不存在|%s\n' "$analyzer_log"
    return 0
  fi
  colmap model_analyzer --path "$model_path" >"$analyzer_log" 2>&1 || analyzer_exit=$?
  if [[ "$analyzer_exit" -ne 0 ]]; then
    printf '不可用|不可用|不可用|model_analyzer 退出码 %s|%s\n' "$analyzer_exit" "$analyzer_log"
    return 0
  fi
  local registered points error
  registered=$(sed -nE 's/.*(Registered images|Images registered)[^0-9]*([0-9]+).*/\2/p' "$analyzer_log" | head -n 1)
  points=$(sed -nE 's/.*(Points|3D points)[^0-9]*([0-9]+).*/\2/p' "$analyzer_log" | head -n 1)
  error=$(sed -nE 's/.*(mean reprojection error|mean.*reprojection error)[^0-9]*([0-9]+([.][0-9]+)?).*/\2/p' "$analyzer_log" | head -n 1)
  [[ -n "$registered" ]] || registered='未识别（见原始日志）'
  [[ -n "$points" ]] || points='未识别（见原始日志）'
  [[ -n "$error" ]] || error='未识别（见原始日志）'
  printf '%s|%s|%s|%s|%s\n' "$registered" "$points" "$error" "$analyzer_exit" "$analyzer_log"
}

global_stats=$(extract_model_stats global)
incremental_stats=$(extract_model_stats incremental)
IFS='|' read -r global_registered global_points global_error global_analyzer_exit global_analyzer_log <<< "$global_stats"
IFS='|' read -r incremental_registered incremental_points incremental_error incremental_analyzer_exit incremental_analyzer_log <<< "$incremental_stats"

global_elapsed=$(<"$project_dir/.baseline_global_elapsed")
incremental_elapsed=$(<"$project_dir/.baseline_incremental_elapsed")
read -r speedup <<< "$(awk -v incremental="$incremental_elapsed" -v global="$global_elapsed" 'BEGIN { if (global > 0) printf "%.3f", incremental/global; else print "不可用" }')"
image_count=$(find "$image_path" -type f \( -iname '*.jpg' -o -iname '*.jpeg' -o -iname '*.png' \) -print | wc -l | tr -d ' ')
registration_delta='不可用'
if [[ "$image_count" =~ ^[0-9]+$ && "$image_count" -gt 0 && "$global_registered" =~ ^[0-9]+$ && "$incremental_registered" =~ ^[0-9]+$ ]]; then
  registration_delta=$(awk -v total="$image_count" -v global="$global_registered" -v incremental="$incremental_registered" 'BEGIN { printf "global %.3f%%，mapper %.3f%%，差异 %.3f 个百分点", global/total*100, incremental/total*100, (global-incremental)/total*100 }')
fi

cat >>"$report_path" <<EOF

## 模型统计

| 方法 | 注册图像数 | 三维点数 | 平均重投影误差 | analyzer 退出码 |
| --- | ---: | ---: | ---: | ---: |
| global_mapper | $global_registered | $global_points | $global_error | $global_analyzer_exit |
| mapper | $incremental_registered | $incremental_points | $incremental_error | $incremental_analyzer_exit |

原始日志：
- global_mapper：\`$project_dir/baseline_global.log\`
- mapper：\`$project_dir/baseline_incremental.log\`
- global_mapper model_analyzer：\`$global_analyzer_log\`
- mapper model_analyzer：\`$incremental_analyzer_log\`

## 对比结论

- 提速倍数（mapper 耗时 / global_mapper 耗时）：**$speedup 倍**。
- 注册率差异：**$registration_delta**（分母为 frames/ 下的 JPG/JPEG/PNG 文件数；若字段未识别，请依据上方原始日志补录；脚本不会猜测）。
EOF

if [[ "$speedup" != '不可用' ]] && awk -v speedup="$speedup" 'BEGIN { exit !(speedup < 3) }'; then
  cat >>"$report_path" <<'EOF'

⚠️ 本次加速比低于预期，建议在更长/更复杂的素材上复测，或重新评估本优化方案的优先级。
EOF
fi

rm -f "$project_dir"/.baseline_global_elapsed "$project_dir"/.baseline_incremental_elapsed "$project_dir"/.baseline_global_memory "$project_dir"/.baseline_incremental_memory "$project_dir"/.baseline_global_exit "$project_dir"/.baseline_incremental_exit
printf '✅ 报告已写入：%s\n' "$report_path"
