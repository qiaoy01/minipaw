#!/usr/bin/env python3
"""Run the pawbench benchmark against a primary-only minipaw instance.

Usage:
    python3 pawbench/run.py                 # run all cases in cases/cases.jsonl
    python3 pawbench/run.py --limit 5       # run first 5 cases
    python3 pawbench/run.py --ids A01,B03   # run specific cases by id
    python3 pawbench/run.py --timeout 240   # per-case wall timeout in seconds

Outputs are written under pawbench/results/<timestamp>/:
    results.jsonl      one line per case with verdict + parsed metrics
    transcripts/*.log  full minipaw stdout/stderr per case
    summary.md         the test_report_robot.md style report
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
WORKSPACE = ROOT / "workspace"
STATE_DIR = WORKSPACE / "robot_state"
MEMORY_DIR = WORKSPACE / "memory"
BIN = REPO / "target" / "release" / "minipaw"
CASES = ROOT / "cases" / "cases.jsonl"
RESULTS_ROOT = ROOT / "results"

EXEC_LINE_RE = re.compile(r'^minihow step=(\d+) exec="(.+)" ok=(true|false)', re.MULTILINE)

# Tool equivalence groups: any one tool in the group satisfies the
# rubric requirement for any other tool in the same group. This keeps the
# benchmark fair when the LLM picks a semantically-equivalent skill instead
# of the exact one named in must_tools. Equivalence is by *information
# carried in the output*, not by intent or category.
TOOL_ALIASES: list[set[str]] = [
    # Battery level is exposed by both the BMS reading and the charging-status report.
    {"self_battery_level", "robot_charge_status"},
    # Position information available from the kinematics report or from the map locator.
    {"robot_move_status", "robot_map_locate"},
    # Memory used percentage / load: diagnostics_summary aggregates these.
    {"self_load_avg", "self_diagnostics_summary"},
]


def _expand_alias_set(name: str) -> set[str]:
    for group in TOOL_ALIASES:
        if name in group:
            return group
    return {name}
# Match `paw.py` followed by any non-identifier characters (whitespace, quotes,
# commas — covers both `paw.py robot_camera_capture` and the subprocess form
# `paw.py', 'robot_move_status'`), then capture the tool identifier.
PAW_TOOL_RE = re.compile(r"paw\.py[^a-zA-Z_]+([a-zA-Z_]\w+)")
FINAL_HEADER_RE = re.compile(r"^t\d+ \[\w+\] steps=(\d+)\s*$", re.MULTILINE)


def load_cases(limit: int | None, ids: list[str] | None) -> list[dict]:
    if not CASES.exists():
        sys.exit(f"missing {CASES} — run pawbench/cases/generate.py first")
    out = []
    with CASES.open() as f:
        for line in f:
            line = line.strip()
            if line:
                out.append(json.loads(line))
    if ids:
        wanted = set(ids)
        out = [c for c in out if c["id"] in wanted]
    if limit:
        out = out[:limit]
    return out


def reset_workspace() -> None:
    # Keep skills/, prompts/, minipaw.json; wipe per-run state.
    if STATE_DIR.exists():
        shutil.rmtree(STATE_DIR)
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    if MEMORY_DIR.exists():
        for entry in MEMORY_DIR.iterdir():
            try:
                if entry.is_dir():
                    shutil.rmtree(entry)
                else:
                    entry.unlink()
            except OSError:
                pass
    MEMORY_DIR.mkdir(parents=True, exist_ok=True)


def env_for_run() -> dict:
    env = os.environ.copy()
    env["MINIPAW_WORKSPACE"] = str(WORKSPACE)
    env["PAWBENCH_STATE_DIR"] = str(STATE_DIR)
    env["MINIPAW_MAX_SESSION_STEPS"] = env.get("MINIPAW_MAX_SESSION_STEPS", "32")
    env["MINIPAW_ALLOW_EXEC"] = "1"
    # Keep advisor unconfigured: minipaw.json only contains "primary".
    return env


def run_case(case: dict, timeout: int) -> dict:
    reset_workspace()
    started = time.time()
    proc = subprocess.run(
        [str(BIN), "task", "new", case["input"]],
        capture_output=True,
        text=True,
        timeout=timeout,
        env=env_for_run(),
    )
    elapsed = time.time() - started
    stdout = proc.stdout
    stderr = proc.stderr
    return {
        "elapsed_s": round(elapsed, 1),
        "rc": proc.returncode,
        "stdout": stdout,
        "stderr": stderr,
    }


def extract_metrics(run: dict, case: dict) -> dict:
    stdout = run["stdout"] + "\n" + run["stderr"]

    # Total minihow EXEC steps and the program lines.
    exec_lines = EXEC_LINE_RE.findall(stdout)

    # Distinct tool invocations: every "paw.py <toolname>" occurrence in EXEC commands
    # OR in EXEC results gets counted, deduped per-occurrence (chained `; ` counts each).
    tool_calls: list[str] = []
    for _step, cmd, _ok in exec_lines:
        tool_calls.extend(PAW_TOOL_RE.findall(cmd))

    must_tools = case.get("must_tools", [])
    used_tools = set(tool_calls)
    missing_tools = [
        t for t in must_tools if not (_expand_alias_set(t) & used_tools)
    ]

    # Final answer block: everything after the last "tN [class] steps=N" header
    final_header = list(FINAL_HEADER_RE.finditer(stdout))
    final_output = ""
    minipaw_steps = 0
    if final_header:
        m = final_header[-1]
        minipaw_steps = int(m.group(1))
        final_output = stdout[m.end():].strip()

    # Substring rubric.
    must_in_output = case.get("must_in_output", [])
    must_in_exec = case.get("must_in_exec", [])
    lo_out = final_output.lower()
    lo_full = stdout.lower()
    missing_in_output = [s for s in must_in_output if s.lower() not in lo_out]
    missing_in_exec = [s for s in must_in_exec if s.lower() not in lo_full]

    return {
        "tool_calls_total": len(tool_calls),
        "tool_calls_distinct": len(used_tools),
        "tool_call_sequence": tool_calls,
        "missing_tools": missing_tools,
        "missing_in_output": missing_in_output,
        "missing_in_exec": missing_in_exec,
        "minipaw_steps": minipaw_steps,
        "final_output": final_output,
        "exec_step_count": len(exec_lines),
    }


def verdict(case: dict, metrics: dict, run: dict) -> tuple[str, list[str]]:
    """Return (Pass/Partial/Fail, list of reasons)."""
    reasons: list[str] = []
    min_tc = case.get("min_tool_calls", 8)
    if metrics["tool_calls_total"] < min_tc:
        reasons.append(
            f"tool_calls<{min_tc}: got {metrics['tool_calls_total']}"
        )
    if metrics["missing_tools"]:
        reasons.append("missing_tools=" + ",".join(metrics["missing_tools"]))
    if metrics["missing_in_output"]:
        reasons.append("missing_in_output=" + ",".join(metrics["missing_in_output"]))
    if metrics["missing_in_exec"]:
        reasons.append("missing_in_exec=" + ",".join(metrics["missing_in_exec"]))
    if run["rc"] != 0:
        reasons.append(f"rc={run['rc']}")
    if "Session reached step limit" in metrics["final_output"]:
        reasons.append("step_limit_reached")

    if not reasons:
        return "Pass", reasons
    # If at least 70% of must_tools are satisfied AND tool_calls meets minimum,
    # consider it Partial rather than full Fail.
    sat_tools = len(case.get("must_tools", [])) - len(metrics["missing_tools"])
    total_tools = max(1, len(case.get("must_tools", [])))
    if sat_tools / total_tools >= 0.7 and metrics["tool_calls_total"] >= min_tc:
        return "Partial", reasons
    return "Fail", reasons


def classify_failure_mode(case: dict, metrics: dict, run: dict) -> str:
    """Map a failure to one of the cognitive-component buckets from ref.md."""
    if "step_limit_reached" in (run.get("stdout", "") + metrics["final_output"]):
        return "planning_or_step_budget"
    if metrics["missing_tools"]:
        # Did the model decline to use available tools, or substitute wrong ones?
        used = set(metrics["tool_call_sequence"])
        if not used:
            return "initiation_no_tool_use"
        return "tool_selection_skipped_required_tool"
    if metrics["missing_in_output"]:
        # Tools ran but final answer dropped values.
        return "working_memory_value_loss_H1_or_H2"
    if metrics["missing_in_exec"]:
        return "wrong_arguments_or_skipped_subtools"
    if run["rc"] != 0:
        return "runtime_error"
    return "other"


def write_summary(run_dir: Path, all_records: list[dict]) -> None:
    n = len(all_records)
    passes = sum(1 for r in all_records if r["verdict"] == "Pass")
    partials = sum(1 for r in all_records if r["verdict"] == "Partial")
    fails = sum(1 for r in all_records if r["verdict"] == "Fail")
    score = round(100.0 * (passes + 0.5 * partials) / max(1, n), 1)

    by_category: dict[str, dict] = {}
    for r in all_records:
        cat = r["category"]
        d = by_category.setdefault(cat, {"n": 0, "pass": 0, "partial": 0, "fail": 0})
        d["n"] += 1
        d[r["verdict"].lower()] += 1

    by_mode: dict[str, int] = {}
    for r in all_records:
        if r["verdict"] != "Pass":
            by_mode[r["failure_mode"]] = by_mode.get(r["failure_mode"], 0) + 1

    avg_tools_pass = (
        sum(r["metrics"]["tool_calls_total"] for r in all_records if r["verdict"] == "Pass")
        / max(1, passes)
    )
    avg_tools_all = sum(r["metrics"]["tool_calls_total"] for r in all_records) / max(1, n)

    lines: list[str] = []
    lines.append(f"# pawbench: minipaw + qwen9b multi-step tool-use benchmark")
    lines.append("")
    lines.append(f"- Run timestamp: {run_dir.name}")
    lines.append(f"- Endpoint: http://<endpoint-ip>:14416/v1 (qwen9b)")
    lines.append(f"- Cases: {n}")
    lines.append(f"- Pass: {passes}")
    lines.append(f"- Partial: {partials}")
    lines.append(f"- Fail: {fails}")
    lines.append(f"- Score (Pass + 0.5*Partial): **{score}/100**")
    lines.append(f"- Avg tool calls (pass): {avg_tools_pass:.1f}")
    lines.append(f"- Avg tool calls (all): {avg_tools_all:.1f}")
    lines.append("")
    lines.append("## Per-category breakdown")
    lines.append("")
    lines.append("| Category | N | Pass | Partial | Fail |")
    lines.append("|---|---|---|---|---|")
    for cat, d in sorted(by_category.items()):
        lines.append(f"| {cat} | {d['n']} | {d['pass']} | {d['partial']} | {d['fail']} |")
    lines.append("")
    lines.append("## Failure mode distribution (cognitive-component mapping)")
    lines.append("")
    lines.append("Failure modes are labeled per ref.md §5 components. A single failed run")
    lines.append("is mapped to the deepest mode it triggers (initiation > tool selection >")
    lines.append("working memory > arguments > runtime).")
    lines.append("")
    for mode, n_mode in sorted(by_mode.items(), key=lambda kv: -kv[1]):
        lines.append(f"- **{mode}**: {n_mode}")
    lines.append("")

    lines.append("## Per-case results")
    lines.append("")
    lines.append("| ID | Category | Verdict | Tool calls | Reasons |")
    lines.append("|---|---|---|---|---|")
    for r in all_records:
        reasons = "; ".join(r["reasons"]) or "—"
        lines.append(
            f"| {r['id']} | {r['category']} | {r['verdict']} | {r['metrics']['tool_calls_total']} | {reasons} |"
        )
    lines.append("")

    # Failure deep-dives — full input + analysis for every Fail and Partial.
    failures = [r for r in all_records if r["verdict"] != "Pass"]
    if failures:
        lines.append("## Failure analyses")
        lines.append("")
        for r in failures:
            lines.append(f"### {r['id']} — {r['verdict']} ({r['failure_mode']})")
            lines.append("")
            lines.append(f"**Input**: {r['input']}")
            lines.append("")
            lines.append(f"**Expected tools** ({len(r['must_tools'])}): {', '.join(r['must_tools'])}")
            lines.append("")
            lines.append(
                f"**Actual tool calls** ({r['metrics']['tool_calls_total']} total, "
                f"{r['metrics']['tool_calls_distinct']} distinct): "
                f"{', '.join(r['metrics']['tool_call_sequence']) or '(none)'}"
            )
            lines.append("")
            lines.append(f"**Reasons**: {'; '.join(r['reasons'])}")
            lines.append("")
            lines.append(f"**Final answer**:")
            lines.append("")
            for ln in (r["metrics"]["final_output"] or "(empty)").splitlines():
                lines.append("> " + ln)
            lines.append("")
            lines.append(f"**Author note**: {r['notes']}")
            lines.append("")

    (run_dir / "summary.md").write_text("\n".join(lines))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--limit", type=int, default=None)
    p.add_argument("--ids", type=str, default="")
    p.add_argument("--timeout", type=int, default=300)
    p.add_argument("--name", type=str, default=None, help="Custom run subdir name.")
    args = p.parse_args()

    if not BIN.exists():
        sys.exit(f"missing binary {BIN} — run `cargo build --release` first")

    ids = [s.strip() for s in args.ids.split(",") if s.strip()] if args.ids else None
    cases = load_cases(args.limit, ids)
    if not cases:
        sys.exit("no cases selected")

    stamp = args.name or datetime.now().strftime("run_%Y%m%d_%H%M%S")
    run_dir = RESULTS_ROOT / stamp
    (run_dir / "transcripts").mkdir(parents=True, exist_ok=True)

    print(f"running {len(cases)} cases -> {run_dir}")
    results = []
    for i, case in enumerate(cases, 1):
        cid = case["id"]
        print(f"[{i:3d}/{len(cases)}] {cid} ... ", end="", flush=True)
        try:
            run = run_case(case, args.timeout)
        except subprocess.TimeoutExpired:
            print("TIMEOUT")
            run = {"elapsed_s": args.timeout, "rc": -1, "stdout": "", "stderr": f"timeout after {args.timeout}s"}
        (run_dir / "transcripts" / f"{cid}.log").write_text(
            f"=== STDOUT ===\n{run['stdout']}\n=== STDERR ===\n{run['stderr']}"
        )
        metrics = extract_metrics(run, case)
        v, reasons = verdict(case, metrics, run)
        mode = classify_failure_mode(case, metrics, run) if v != "Pass" else "—"
        record = {
            "id": cid,
            "category": case["category"],
            "input": case["input"],
            "must_tools": case["must_tools"],
            "notes": case["notes"],
            "elapsed_s": run["elapsed_s"],
            "verdict": v,
            "reasons": reasons,
            "failure_mode": mode,
            "metrics": metrics,
        }
        results.append(record)
        print(f"{v}  ({metrics['tool_calls_total']} tool calls, {run['elapsed_s']}s)  reasons={reasons or '—'}")

        with (run_dir / "results.jsonl").open("a") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    write_summary(run_dir, results)
    print(f"\ndone. summary: {run_dir / 'summary.md'}")


if __name__ == "__main__":
    main()
