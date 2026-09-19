#!/usr/bin/env bash
# OOOSplat 环境验证脚本（Linux/macOS）。
set -euo pipefail

print_help() {
  cat <<'EOF'
用法：
  bash scripts/verify_env.sh              检查本机环境
  bash scripts/verify_env.sh --export-help 检查环境并导出 COLMAP 参数帮助

--export-help 会在 docs/ 下生成：
  colmap_global_mapper_help.txt
  colmap_mapper_help.txt
  colmap_feature_extractor_help.txt
  colmap_sequential_matcher_help.txt
EOF
}

export_help() {
  local output_path="$1"
  shift
  if ! command -v colmap >/dev/null 2>&1; then
    printf '❌ 无法导出 %s：未找到 colmap\n' "$output_path" >&2
    return 0
  fi
  if colmap "$@" -h >"$output_path" 2>&1; then
    printf '✅ 已导出 %s\n' "$output_path"
  else
    local exit_code=$?
    printf '⚠️ 已保存 %s，但 colmap 退出码为 %s（请检查文件内容）\n' "$output_path" "$exit_code"
  fi
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  print_help
  exit 0
fi

export_requested=false
if [[ "${1:-}" == "--export-help" ]]; then
  export_requested=true
  shift
fi
if [[ "$#" -ne 0 ]]; then
  printf '❌ 未知参数：%s\n' "$1" >&2
  print_help >&2
  exit 2
fi

printf '%s\n' 'OOOSplat 环境验证'
printf '%s\n' '================='

names=()
statuses=()
versions=()

run_check() {
  local name="$1"
  local command_name="$2"
  shift 2
  local output
  local exit_code=0
  names+=("$name")
  if ! command -v "$command_name" >/dev/null 2>&1; then
    statuses+=("❌")
    versions+=("命令缺失")
    printf '❌ %-28s 命令缺失：请安装 %s 后重试（不自动安装）\n' "$name" "$command_name"
    return 0
  fi
  output="$('$@' 2>&1)" || exit_code=$?
  if [[ "$exit_code" -eq 0 ]]; then
    statuses+=("✅")
    versions+=("$(printf '%s' "$output" | sed -n '1p')")
    printf '✅ %-28s %s\n' "$name" "$(printf '%s' "$output" | sed -n '1p')"
  else
    statuses+=("❌")
    versions+=("命令失败（退出码 $exit_code）")
    printf '❌ %-28s 命令失败（退出码 %s）\n' "$name" "$exit_code"
  fi
}

run_check 'ffmpeg' ffmpeg ffmpeg -version
run_check 'ffprobe' ffprobe ffprobe -version
run_check 'colmap -h' colmap colmap -h
run_check 'colmap global_mapper -h' colmap colmap global_mapper -h
run_check 'cargo' cargo cargo --version
run_check 'node' node node --version

if [[ "$export_requested" == true ]]; then
  mkdir -p docs
  export_help 'docs/colmap_global_mapper_help.txt' global_mapper
  export_help 'docs/colmap_mapper_help.txt' mapper
  export_help 'docs/colmap_feature_extractor_help.txt' feature_extractor
  export_help 'docs/colmap_sequential_matcher_help.txt' sequential_matcher
fi

printf '\n状态汇总\n'
printf '%-4s %-28s %s\n' '标记' '检查项' '版本或状态'
printf '%-4s %-28s %s\n' '----' '----------------------------' '------------------------------'
for index in "${!names[@]}"; do
  printf '%-4s %-28s %s\n' "${statuses[$index]}" "${names[$index]}" "${versions[$index]}"
done

for status in "${statuses[@]}"; do
  if [[ "$status" == '❌' ]]; then
    exit 1
  fi
done
