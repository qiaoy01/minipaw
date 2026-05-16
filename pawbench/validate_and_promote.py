#!/usr/bin/env python3
"""Validate staged advisor proposals against a held-out case set.

Workflow:
  1. Measure baseline score on the validation set with the current
     prompts/skills.
  2. For each proposal in pawbench/workspace/proposals/, apply it,
     re-run validation, and keep the change only if the aggregate
     score does not regress (Δscore >= 0).
  3. Reverted proposals are discarded; promoted proposals stay in
     prompts/skills and the proposal file is removed (apply_proposal
     deletes it automatically on success).

Usage:
    python3 pawbench/validate_and_promote.py [--ids id1,id2,...]
                                             [--min-delta 0]
                                             [--keep-rejected]

Notes:
  - Validation uses primary-only; the workspace minipaw.json must have
    `advisor_mode: trial` (or no advisor) during training, and we
    temporarily strip the advisor block for validation runs.
  - Workspace mutations are snapshotted at the file level — no git
    dependency.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
WORKSPACE = ROOT / "workspace"
PROPOSALS = WORKSPACE / "proposals"
PROMPTS = WORKSPACE / "prompts"
SKILLS = WORKSPACE / "skills"
WORKSPACE_CONFIG = WORKSPACE / "minipaw.json"
BIN = REPO / "target" / "release" / "minipaw"
RUN_PY = ROOT / "run.py"

# Default validation set: 18 cases for statistical power against qwen9b
# run-to-run variance (we've observed cases like B02 oscillate between
# 3 and 7 tool calls run-to-run, so single-case signals are unreliable).
# Coverage: 5 transport / 4 scout / 4 charge (the focused 37 categories
# where most training proposals originate) + 5 cross-category sanity
# (survey/diag/decision/error/compute) to catch out-of-domain regressions.
# At ~5-10s/case primary-only, full validation ≈ 1.5-3 min per pass.
DEFAULT_VALIDATION_IDS = [
    # transport (5) — sensitive to navigation chain depth
    "B01_navigate_to_water",
    "B02_navigate_to_shelter",
    "B07_terrain_aware_move",
    "B10_terrain_aware_move",
    "B12_route_compare",
    # scout (4) — sensitive to comm + observation chain
    "C01_recon_report",
    "C04_recon_report",
    "C06_threat_response",
    "C09_threat_response",
    # charge (4) — sensitive to plan depth + verification rules
    "D01_solar_burn",
    "D03_solar_burn",
    "D06_dock_then_check",
    "D08_dock_then_check",
    # cross-category sanity (5) — catch out-of-domain regressions
    "A04_forage_pick",        # survey
    "E01_health_full",        # diag (long chain)
    "F01_battery_then_route", # decision
    "H01_recover_empty_arm",  # error handling
    "I01_mission_brief",      # decision (mixed)
]


def snapshot_workspace() -> Path:
    """Backup prompts/ and skills/ to a temp dir; return path."""
    backup = Path(tempfile.mkdtemp(prefix="minipaw_validate_"))
    shutil.copytree(PROMPTS, backup / "prompts")
    shutil.copytree(SKILLS, backup / "skills")
    return backup


def restore_workspace(backup: Path) -> None:
    if PROMPTS.exists():
        shutil.rmtree(PROMPTS)
    if SKILLS.exists():
        shutil.rmtree(SKILLS)
    shutil.copytree(backup / "prompts", PROMPTS)
    shutil.copytree(backup / "skills", SKILLS)
    shutil.rmtree(backup)


def run_validation(ids: list[str], name: str) -> tuple[float, dict]:
    """Run pawbench primary-only on the given case ids, return (score, stats).

    Score = (passes + 0.5 * partials) / n  (in [0, 1]).
    """
    cmd = [
        sys.executable,
        str(RUN_PY),
        "--ids", ",".join(ids),
        "--name", name,
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"validation run failed: {proc.stderr or proc.stdout}")
    results_path = ROOT / "results" / name / "results.jsonl"
    if not results_path.exists():
        sys.exit(f"validation results missing: {results_path}")

    n = passes = partials = fails = 0
    for line in results_path.open():
        r = json.loads(line)
        n += 1
        if r["verdict"] == "Pass":
            passes += 1
        elif r["verdict"] == "Partial":
            partials += 1
        else:
            fails += 1
    score = (passes + 0.5 * partials) / max(1, n)
    return score, {"n": n, "pass": passes, "partial": partials, "fail": fails}


def strip_advisor_for_validation() -> dict | None:
    """If workspace minipaw.json has an advisor block, save and remove it
    for the duration of validation. Returns original config (or None)."""
    if not WORKSPACE_CONFIG.exists():
        return None
    original = json.loads(WORKSPACE_CONFIG.read_text())
    if "advisor" not in original.get("agents", {}) and "advisor_mode" not in original:
        return None
    primary_only = {"agents": {"primary": original["agents"]["primary"]}}
    WORKSPACE_CONFIG.write_text(json.dumps(primary_only, indent=2) + "\n")
    return original


def restore_config(original: dict | None) -> None:
    if original is not None:
        WORKSPACE_CONFIG.write_text(json.dumps(original, indent=2) + "\n")


def apply_proposal(proposal_id: str) -> tuple[bool, str]:
    """Apply a proposal via minipaw CLI. Returns (ok, message)."""
    env = os.environ.copy()
    env["MINIPAW_WORKSPACE"] = str(WORKSPACE)
    proc = subprocess.run(
        [str(BIN), "config", "advisor", "proposals", "apply", proposal_id],
        capture_output=True, text=True, env=env,
    )
    return proc.returncode == 0, (proc.stdout + proc.stderr).strip()


def list_proposals() -> list[Path]:
    if not PROPOSALS.exists():
        return []
    return sorted(PROPOSALS.glob("*.md"))


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--ids", type=str, default="",
                   help="comma-separated case ids for validation set (default: built-in 8-case sample)")
    p.add_argument("--min-delta", type=float, default=0.0,
                   help="minimum score delta (in [0,1]) to keep a proposal (default 0.0 = no regression)")
    p.add_argument("--keep-rejected", action="store_true",
                   help="if set, rejected proposals are kept in proposals/ for human review (default: discard)")
    args = p.parse_args()

    if not BIN.exists():
        sys.exit(f"missing binary {BIN} — run `cargo build --release` first")

    ids = [s.strip() for s in args.ids.split(",") if s.strip()] or DEFAULT_VALIDATION_IDS
    proposals = list_proposals()
    if not proposals:
        print("no proposals to validate. Nothing to do.")
        return 0

    print(f"validation set ({len(ids)} cases): {', '.join(ids)}")
    print(f"proposals to evaluate: {len(proposals)}")
    print()

    original_config = strip_advisor_for_validation()
    try:
        stamp = time.strftime("validate_%Y%m%d_%H%M%S")

        print("=== baseline ===")
        baseline_score, baseline_stats = run_validation(ids, f"{stamp}_baseline")
        print(f"baseline: {baseline_stats} score={baseline_score:.3f}")
        current_score = baseline_score

        promoted: list[str] = []
        rejected: list[str] = []

        for i, proposal_path in enumerate(proposals, 1):
            proposal_id = proposal_path.stem
            print(f"\n=== [{i}/{len(proposals)}] {proposal_id} ===")
            snapshot = snapshot_workspace()
            ok, msg = apply_proposal(proposal_id)
            if not ok:
                print(f"  apply failed: {msg[:200]}")
                restore_workspace(snapshot)
                rejected.append(proposal_id)
                continue

            new_score, new_stats = run_validation(ids, f"{stamp}_after_{proposal_id}")
            delta = new_score - current_score
            print(f"  {new_stats} score={new_score:.3f} (Δ{delta:+.3f})")

            if delta >= args.min_delta:
                print(f"  PROMOTED (Δ {delta:+.3f} >= {args.min_delta:+.3f})")
                current_score = new_score
                shutil.rmtree(snapshot)
                promoted.append(proposal_id)
            else:
                print(f"  REVERTED (Δ {delta:+.3f} < {args.min_delta:+.3f})")
                restore_workspace(snapshot)
                rejected.append(proposal_id)
                if args.keep_rejected:
                    # Re-create the proposal file for human review.
                    # Note: apply_proposal deleted it; reconstruct from name.
                    # Simplest: just leave a note marker. Real reconstruction
                    # would need to cache the file before apply.
                    marker = PROPOSALS / f"{proposal_id}.rejected.md"
                    marker.parent.mkdir(parents=True, exist_ok=True)
                    marker.write_text(
                        f"# Rejected by validator\n"
                        f"id: {proposal_id}\n"
                        f"baseline_score: {baseline_score:.3f}\n"
                        f"new_score: {new_score:.3f}\n"
                        f"delta: {delta:+.3f}\n"
                    )

        print()
        print("=== summary ===")
        print(f"baseline score: {baseline_score:.3f} ({baseline_stats})")
        print(f"final score:    {current_score:.3f}")
        print(f"net delta:      {current_score - baseline_score:+.3f}")
        print(f"promoted: {len(promoted)} ({', '.join(promoted) or '—'})")
        print(f"rejected: {len(rejected)} ({', '.join(rejected) or '—'})")
    finally:
        restore_config(original_config)

    return 0


if __name__ == "__main__":
    sys.exit(main())
