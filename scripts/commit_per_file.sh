#!/usr/bin/env bash
set -uo pipefail
cd /Users/adm/Documents/Repos/kod

# Modified files (M)
modified=$(git diff --name-only --diff-filter=M)

# Deleted files (D)
deleted=$(git diff --name-only --diff-filter=D)

# Untracked files
untracked=$(git ls-files --others --exclude-standard)

count=0

# Commit each modified file
while IFS= read -r f; do
  [ -z "$f" ] && continue
  git add -- "$f" && git commit -m "Update $f" -- "$f"
  count=$((count + 1))
done <<< "$modified"

# Commit each deleted file (stage deletion with -u since file is gone from worktree)
while IFS= read -r f; do
  [ -z "$f" ] && continue
  git add -u -- "$f" && git commit -m "Remove $f" -- "$f"
  count=$((count + 1))
done <<< "$deleted"

# Commit each untracked file
while IFS= read -r f; do
  [ -z "$f" ] && continue
  git add -- "$f" && git commit -m "Add $f" -- "$f"
  count=$((count + 1))
done <<< "$untracked"

echo "Total commits: $count"
