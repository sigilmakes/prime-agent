#!/usr/bin/env bash
# Generate a Markdown gallery of mmdflux text + SVG snapshots.
#
# Usage:
#   ./scripts/generate-gallery.sh
#   ./scripts/generate-gallery.sh --out docs/gallery.md
#   ./scripts/generate-gallery.sh simple edge_styles
#
# By default, writes to docs/gallery.md and includes all fixtures.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUTFILE="$REPO/docs/gallery.md"

usage() {
  cat <<'EOF'
Generate a Markdown gallery of mmdflux text + SVG snapshots.

Usage:
  ./scripts/generate-gallery.sh
  ./scripts/generate-gallery.sh --out docs/gallery.md
  ./scripts/generate-gallery.sh simple edge_styles

Options:
  -o, --out <path>   Output Markdown path (default: docs/gallery.md)
  -h, --help         Show this help
EOF
}

names=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -o|--out)
      OUTFILE="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      names+=("$1")
      shift
      ;;
  esac
done

mkdir -p "$(dirname "$OUTFILE")"
outdir="$(cd "$(dirname "$OUTFILE")" && pwd)"

relpath() {
  python3 - <<'PY' "$1" "$2"
import os
import sys
print(os.path.relpath(sys.argv[1], sys.argv[2]))
PY
}

# Collect fixtures for a diagram type, printing one path per line
collect_fixtures() {
  local fixture_dir="$1"
  if [[ ${#names[@]} -gt 0 ]]; then
    for name in "${names[@]}"; do
      local f="$fixture_dir/${name}.mmd"
      if [[ -f "$f" ]]; then
        echo "$f"
      fi
    done
  else
    find "$fixture_dir" -maxdepth 1 -type f -name '*.mmd' | sort
  fi
}

flowchart_files=()
while IFS= read -r f; do
  flowchart_files+=("$f")
done < <(collect_fixtures "$REPO/tests/fixtures/flowchart")

class_files=()
while IFS= read -r f; do
  class_files+=("$f")
done < <(collect_fixtures "$REPO/tests/fixtures/class")

sequence_files=()
while IFS= read -r f; do
  sequence_files+=("$f")
done < <(collect_fixtures "$REPO/tests/fixtures/sequence")

state_files=()
while IFS= read -r f; do
  state_files+=("$f")
done < <(collect_fixtures "$REPO/tests/fixtures/state")

total_count=$(( ${#flowchart_files[@]} + ${#class_files[@]} + ${#sequence_files[@]} + ${#state_files[@]} ))

if [[ $total_count -eq 0 ]]; then
  echo "No fixtures found." >&2
  exit 1
fi

commit_sha="$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo "unknown")"

{
  echo "# mmdflux gallery"
  echo
  echo "_Generated from commit \`$commit_sha\` — $total_count fixtures_"
  echo
  echo "- [Flowchart](#flowchart) (${#flowchart_files[@]})"
  echo "- [Class](#class) (${#class_files[@]})"
  echo "- [Sequence](#sequence) (${#sequence_files[@]})"
  echo "- [State](#state) (${#state_files[@]})"
  echo
} > "$OUTFILE"

missing_text=0
missing_svg=0

# Emit a section of gallery entries for a given diagram type
emit_section() {
  local section_name="$1"
  local fixture_dir="$2"
  local text_dir="$3"
  local svg_dir="$4"
  shift 4
  local files=("$@")

  if [[ ${#files[@]} -eq 0 ]]; then
    return
  fi

  echo "# $section_name" >> "$OUTFILE"
  echo >> "$OUTFILE"

  local fixture_rel text_rel_dir svg_rel_dir
  fixture_rel="tests/fixtures/$(basename "$fixture_dir")"
  text_rel_dir="tests/snapshots/$(basename "$text_dir")"
  svg_rel_dir="tests/svg-snapshots/$(basename "$svg_dir")"

  for f in "${files[@]}"; do
    local name
    name="$(basename "$f" .mmd)"
    local text="$text_dir/${name}.txt"
    local svg="$svg_dir/${name}.svg"

    echo "## $name" >> "$OUTFILE"
    echo >> "$OUTFILE"
    echo "\`${fixture_rel}/${name}.mmd\`" >> "$OUTFILE"
    echo >> "$OUTFILE"

    if [[ -f "$text" ]]; then
      echo "**Text**" >> "$OUTFILE"
      echo >> "$OUTFILE"
      echo '```text' >> "$OUTFILE"
      cat "$text" >> "$OUTFILE"
      printf '\n```\n\n' >> "$OUTFILE"
    else
      echo "> Missing text snapshot: \`${text_rel_dir}/${name}.txt\`" >> "$OUTFILE"
      echo >> "$OUTFILE"
      missing_text=$((missing_text + 1))
    fi

    if [[ -f "$svg" ]]; then
      local svg_rel
      svg_rel="$(relpath "$svg" "$outdir")"
      echo "<details>" >> "$OUTFILE"
      echo "<summary>SVG output</summary>" >> "$OUTFILE"
      echo >> "$OUTFILE"
      echo "![${name} svg](${svg_rel})" >> "$OUTFILE"
      echo >> "$OUTFILE"
      echo "</details>" >> "$OUTFILE"
      echo >> "$OUTFILE"
    else
      echo "> Missing SVG snapshot: \`${svg_rel_dir}/${name}.svg\`" >> "$OUTFILE"
      echo >> "$OUTFILE"
      missing_svg=$((missing_svg + 1))
    fi

    echo "<details>" >> "$OUTFILE"
    echo "<summary>Mermaid source</summary>" >> "$OUTFILE"
    echo >> "$OUTFILE"
    echo '```' >> "$OUTFILE"
    cat "$f" >> "$OUTFILE"
    printf '\n```\n\n' >> "$OUTFILE"
    echo "</details>" >> "$OUTFILE"
    echo >> "$OUTFILE"
  done
}

emit_section "Flowchart" \
  "$REPO/tests/fixtures/flowchart" \
  "$REPO/tests/snapshots/flowchart" \
  "$REPO/tests/svg-snapshots/flowchart" \
  "${flowchart_files[@]}"

emit_section "Class" \
  "$REPO/tests/fixtures/class" \
  "$REPO/tests/snapshots/class" \
  "$REPO/tests/svg-snapshots/class" \
  "${class_files[@]}"

emit_section "Sequence" \
  "$REPO/tests/fixtures/sequence" \
  "$REPO/tests/snapshots/sequence" \
  "$REPO/tests/svg-snapshots/sequence" \
  "${sequence_files[@]}"

emit_section "State" \
  "$REPO/tests/fixtures/state" \
  "$REPO/tests/snapshots/state" \
  "$REPO/tests/svg-snapshots/state" \
  "${state_files[@]}"

if [[ $missing_text -gt 0 || $missing_svg -gt 0 ]]; then
  echo "---" >> "$OUTFILE"
  echo >> "$OUTFILE"
  echo "**Missing snapshots**" >> "$OUTFILE"
  echo >> "$OUTFILE"
  echo "- Text snapshots missing: $missing_text" >> "$OUTFILE"
  echo "- SVG snapshots missing: $missing_svg" >> "$OUTFILE"
  echo >> "$OUTFILE"
fi

echo "Wrote gallery to $OUTFILE"
