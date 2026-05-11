#!/usr/bin/env python3
"""Generate one .md skill file per tool registered in tools/paw.py.

The skill frontmatter is the format minipaw's SkillRegistry expects:

    ---
    name: <tool-name>
    description: <short description>
    exec: python3 <abs path to paw.py> <tool-name>
    ---

Skills are written to <pawbench>/workspace/skills/ — the same directory the
benchmark passes as MINIPAW_WORKSPACE so minipaw loads them at startup.
"""

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TOOLS = ROOT / "tools" / "paw.py"
DEST = ROOT / "workspace" / "skills"


def main() -> int:
    DEST.mkdir(parents=True, exist_ok=True)
    # Wipe stale skills so renames don't leave orphans.
    for existing in DEST.glob("*.md"):
        existing.unlink()

    out = subprocess.run(
        ["python3", str(TOOLS), "--list-skills"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    pairs = []
    name = desc = None
    for line in out.splitlines():
        if line.startswith("---NAME "):
            name = line[len("---NAME ") :].strip()
        elif line.startswith("---DESC "):
            desc = line[len("---DESC ") :].strip()
            if name and desc:
                pairs.append((name, desc))
                name = desc = None

    if not pairs:
        print("error: no skills emitted by paw.py", file=sys.stderr)
        return 2

    for name, desc in pairs:
        path = DEST / f"{name}.md"
        path.write_text(
            "---\n"
            f"name: {name}\n"
            f"description: {desc}\n"
            f"exec: python3 {TOOLS} {name}\n"
            "---\n"
            f"\nInvoke `python3 {TOOLS} {name}` to use this tool.\n"
        )

    print(f"wrote {len(pairs)} skills to {DEST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
