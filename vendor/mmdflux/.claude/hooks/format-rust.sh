#!/bin/bash
# Format Rust files after Claude edits them

# Read JSON input from stdin
input=$(cat)

# Extract file_path from tool_input
file_path=$(echo "$input" | jq -r '.tool_input.file_path // empty')

# Only run on .rs files
if [[ "$file_path" == *.rs ]]; then
    cd "$CLAUDE_PROJECT_DIR" && cargo +nightly fmt --quiet 2>/dev/null
fi
