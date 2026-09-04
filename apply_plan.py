#!/usr/bin/env python3
import os
import re
import sys
from pathlib import Path

def main():
    if len(sys.argv) != 2:
        print("Usage: python3 apply_plan.py <plan_file>")
        sys.exit(1)

    plan_path = Path(sys.argv[1])
    if not plan_path.exists():
        print(f"Plan file not found: {plan_path}")
        sys.exit(1)

    with open(plan_path, 'r', encoding='utf-8') as f:
        lines = f.readlines()

    i = 0
    while i < len(lines):
        line = lines[i]
        match = re.match(r'^\s*-\s+(?:Create|Modify):\s+(.+)$', line)
        if match:
            file_path = match.group(1).strip()
            i += 1
            while i < len(lines) and not lines[i].strip().startswith('```'):
                i += 1
            if i >= len(lines):
                print(f"Warning: No code fence found for {file_path}")
                i += 1
                continue
            i += 1
            content_lines = []
            while i < len(lines) and not lines[i].strip().startswith('```'):
                content_lines.append(lines[i])
                i += 1
            target = Path(file_path)
            target.parent.mkdir(parents=True, exist_ok=True)
            with open(target, 'w', encoding='utf-8') as out:
                out.writelines(content_lines)
            print(f"Written: {file_path}")
            i += 1
        else:
            i += 1

if __name__ == "__main__":
    main()
