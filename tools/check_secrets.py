#!/usr/bin/env python3
"""Secret scanner for ecology-thor.

Refuses to let credentials ride in commits. Scans every *tracked* file plus
the working tree (excluding .git and target/), looking for the token shapes
this project's owners actually use (GitHub PATs, typesafe.ai keys, DeepInfra
keys) plus generic high-signal patterns (AWS, private keys, bearer tokens).

Exit 0 = clean; exit 1 = findings (printed with file:line).

This is a tripwire, not a vault: if it fires, rotate the key *first*, then
clean the history. See docs/SAFETY.md.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("github-pat", re.compile(r"gh[pousr]_[A-Za-z0-9]{36,}")),
    ("typesafe-key", re.compile(r"apikey_[0-9a-f]{16,}_[0-9a-f]{32,}")),
    ("deepinfra-key", re.compile(r"sk-di-[A-Za-z0-9]{16,}")),
    ("openai-key", re.compile(r"sk-[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20}")),
    ("anthropic-key", re.compile(r"sk-ant-[A-Za-z0-9_\-]{24,}")),
    ("aws-access-key", re.compile(r"AKIA[0-9A-Z]{16}")),
    ("private-key-block", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")),
    ("bearer-token", re.compile(r"Bearer\s+[A-Za-z0-9_\-\.]{32,}")),
    ("generic-env-secret", re.compile(r"(?i)(api[_-]?key|secret|token|password)\s*[=:]\s*['\"][A-Za-z0-9+/]{24,}['\"]")),
]

# Files/dirs we never scan (vendored blobs, build output).
SKIP_DIRS = {".git", "target", "node_modules", "data", "build", ".venv", "__pycache__"}
SKIP_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".pdf", ".ptx", ".cubin", ".whl", ".lock"}


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    findings: list[str] = []

    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        rel = path.relative_to(root)
        if any(part in SKIP_DIRS for part in rel.parts):
            continue
        if path.suffix in SKIP_SUFFIXES:
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            for name, pattern in PATTERNS:
                if pattern.search(line):
                    findings.append(f"{rel}:{lineno}: [{name}] {line.strip()[:100]}")

    if findings:
        print("SECRET SCAN FAILED — rotate these keys immediately, then clean history:")
        for f in findings:
            print(f"  {f}")
        return 1
    print("secret scan: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
