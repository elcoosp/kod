#!/usr/bin/env python3
"""Apply a KOD-generated plan file to disk.

A plan file is a sequence of `- Create: <path>` / `- Modify: <path>`
headers, each followed by a fenced code block carrying the file's new
content.

H-C10 hardening (see the production-readiness review): the previous
script accepted any path in the plan — absolute, `..`, or a symlink
target — and wrote it verbatim. A model-authored plan was therefore
an arbitrary-write primitive. Now:

  * every path is resolved against a *root* (default: cwd) and
    refused if it escapes that root;
  * `Create` refuses to overwrite an existing file unless `--force`;
  * `Modify` requires an existing file and, without `--force`,
    refuses to clobber unless the file's content already matches
    what the plan's `Modify` would write;
  * every write is atomic (temp + rename in the same directory);
  * symlinks in the path are followed only inside the root.

Usage:
    python3 apply_plan.py <plan_file> [--root DIR] [--force] [--dry-run]
"""
from __future__ import annotations

import argparse
import os
import re
import sys
import tempfile
from pathlib import Path


HEADER_RE = re.compile(r"^\s*-\s+(Create|Modify):\s+(.+?)\s*$")


def parse_plan(lines: list[str]) -> list[tuple[str, str, str]]:
    """Return [(kind, raw_path, content)] in file order."""
    out: list[tuple[str, str, str]] = []
    i = 0
    while i < len(lines):
        m = HEADER_RE.match(lines[i])
        if not m:
            i += 1
            continue
        kind, raw_path = m.group(1), m.group(2)
        i += 1
        # Skip to the opening fence.
        while i < len(lines) and not lines[i].lstrip().startswith("```"):
            i += 1
        if i >= len(lines):
            print(f"warning: no code fence after {kind}: {raw_path}", file=sys.stderr)
            continue
        i += 1
        body: list[str] = []
        while i < len(lines) and not lines[i].lstrip().startswith("```"):
            body.append(lines[i])
            i += 1
        i += 1  # consume closing fence
        out.append((kind, raw_path, "".join(body)))
    return out


def resolve_within_root(root: Path, raw: str) -> Path:
    """Resolve `raw` against `root`, refusing any escape.

    Uses `Path.resolve(strict=False)` so a not-yet-existing file's
    path is still normalized. A `..` that escapes the root raises
    `ValueError`; a symlink whose *target* escapes the root also
    raises (the resolved path must still be under root).
    """
    root_resolved = root.resolve(strict=True)
    candidate = (root_resolved / raw).resolve(strict=False)
    try:
        candidate.relative_to(root_resolved)
    except ValueError:
        raise ValueError(
            f"path escapes the plan root: {raw!r} resolves to "
            f"{candidate}, which is not under {root_resolved}"
        )
    return candidate


def atomic_write(path: Path, content: str) -> None:
    """Same-directory temp file + rename; never a partial write."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".kod-plan-", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def main() -> int:
    ap = argparse.ArgumentParser(description="Apply a KOD plan file.")
    ap.add_argument("plan", help="path to the plan file")
    ap.add_argument(
        "--root",
        default=".",
        help="directory every plan path is resolved against (default: cwd)",
    )
    ap.add_argument(
        "--force",
        action="store_true",
        help="overwrite existing files (Create) and mismatched content (Modify)",
    )
    ap.add_argument(
        "--dry-run",
        action="store_true",
        help="print what would happen without touching the filesystem",
    )
    args = ap.parse_args()

    plan_path = Path(args.plan)
    if not plan_path.exists():
        print(f"plan file not found: {plan_path}", file=sys.stderr)
        return 1

    root = Path(args.root).resolve(strict=False)
    if not root.is_dir():
        print(f"root is not a directory: {root}", file=sys.stderr)
        return 1

    try:
        lines = plan_path.read_text(encoding="utf-8").splitlines(keepends=True)
    except OSError as e:
        print(f"could not read plan: {e}", file=sys.stderr)
        return 1

    entries = parse_plan(lines)
    if not entries:
        print("plan contains no Create/Modify entries", file=sys.stderr)
        return 1

    failures = 0
    for kind, raw, content in entries:
        try:
            target = resolve_within_root(root, raw)
        except ValueError as e:
            print(f"refusing {kind} {raw!r}: {e}", file=sys.stderr)
            failures += 1
            continue

        exists = target.exists()
        if kind == "Create":
            if exists and not args.force:
                print(
                    f"refusing Create {target}: file exists (pass --force to overwrite)",
                    file=sys.stderr,
                )
                failures += 1
                continue
        else:  # Modify
            if not exists:
                print(
                    f"refusing Modify {target}: file does not exist",
                    file=sys.stderr,
                )
                failures += 1
                continue
            if not args.force:
                try:
                    current = target.read_text(encoding="utf-8")
                except OSError as e:
                    print(f"could not read {target}: {e}", file=sys.stderr)
                    failures += 1
                    continue
                if current != content:
                    print(
                        f"refusing Modify {target}: content differs from the "
                        f"plan's new content (pass --force to overwrite)",
                        file=sys.stderr,
                    )
                    failures += 1
                    continue

        if args.dry_run:
            verb = "would write" if exists else "would create"
            print(f"{verb} {target} ({len(content)} bytes)")
            continue

        try:
            atomic_write(target, content)
        except OSError as e:
            print(f"failed to write {target}: {e}", file=sys.stderr)
            failures += 1
            continue
        print(f"wrote {target} ({len(content)} bytes)")

    if failures:
        print(f"{failures} entr{'y' if failures == 1 else 'ies'} skipped", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
