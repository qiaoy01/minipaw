#!/usr/bin/env python3
"""pawbench + Trajectory-Critic Retry (Tree's advisor design).

Each case is run TWICE:
  Pass 1: primary alone (deepseek-chat) executes the original input.
  Critic: deepseek-reasoner reads the transcript + case rubric, emits a
          short hint focused on missing tools / lost values.
  Pass 2: primary re-runs with hint appended to the input.
We take Pass 2 as the advisor-augmented result.

This is independent of the upstream advisor branch's shadow-run + Jaccard +
adjust-meta design. The only thing shared with the advisor branch is the
pawbench harness itself (cases, scoring, skill workspace).
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import run as bench  # noqa: E402

SJTU_BASE = os.environ.get("SJTU_API_BASE", "https://models.sjtu.edu.cn/api/v1")
SJTU_KEY = os.environ.get("SJTU_API_KEY", "").strip()
CRITIC_MODEL = os.environ.get("CRITIC_MODEL", "deepseek-chat")
CRITIC_TIMEOUT = int(os.environ.get("CRITIC_TIMEOUT", "180"))


def call_critic(system: str, user: str) -> str:
    """Call SJTU OpenAI-compatible /chat/completions, return content text.

    Uses no proxy (SJTU API requires direct connection on campus / SJTU VPN).
    """
    if not SJTU_KEY:
        raise RuntimeError("SJTU_API_KEY not set in environment")
    body = json.dumps({
        "model": CRITIC_MODEL,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "max_tokens": 400,
        "temperature": 0.2,
    }).encode("utf-8")
    req = urllib.request.Request(
        f"{SJTU_BASE}/chat/completions",
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {SJTU_KEY}",
            "Content-Type": "application/json",
        },
    )
    # Bypass any global proxy: build an opener with no proxy handler.
    proxy_handler = urllib.request.ProxyHandler({})
    opener = urllib.request.build_opener(proxy_handler)
    with opener.open(req, timeout=CRITIC_TIMEOUT) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    msg = payload["choices"][0]["message"]
    content = (msg.get("content") or "").strip()
    if not content:
        content = (msg.get("reasoning_content") or "").strip()
    return content or "<critic-empty-response>"


def trim_transcript(stdout: str, max_chars: int = 6000) -> str:
    """Keep step lines + final output, drop verbose llm-raw chatter."""
    keep = []
    for line in stdout.splitlines():
        if line.startswith("minihow step=") or line.startswith("t1 ") or \
           line.startswith("DONE:") or line.startswith("llm error"):
            keep.append(line)
    final = "\n".join(keep)
    # also append the last 1500 chars of stdout (final output area)
    final = final + "\n---tail---\n" + stdout[-1500:]
    return final[:max_chars]


CRITIC_SYSTEM = (
    "You are a brief critic for a small agent benchmark. Read the agent's "
    "trajectory on a multi-step robot task and the rubric it was supposed to "
    "satisfy, then output a SINGLE concise hint (<= 80 words, plain text) "
    "telling the agent what it missed. Focus on: (a) required tools not "
    "called, (b) values that were lost between steps (e.g. scene_id, "
    "inventory items), (c) information the final DONE answer must mention. "
    "Do NOT solve the task. Do NOT list every tool. One hint, action-oriented."
)


def build_critic_user(case: dict, transcript: str, metrics: dict) -> str:
    parts = [f"TASK INPUT: {case['input']}\n"]
    if case.get("must_tools"):
        parts.append("REQUIRED TOOLS: " + ", ".join(case["must_tools"]))
    if case.get("must_in_output"):
        parts.append("FINAL ANSWER MUST MENTION: " + ", ".join(case["must_in_output"]))
    if case.get("must_in_exec"):
        parts.append("MUST APPEAR IN EXEC: " + ", ".join(case["must_in_exec"]))
    parts.append("")
    parts.append("--- AGENT TRAJECTORY ---")
    parts.append(transcript)
    parts.append("--- END TRAJECTORY ---")
    parts.append("")
    missing = []
    if metrics.get("missing_tools"):
        missing.append("tools_missing=" + ",".join(metrics["missing_tools"]))
    if metrics.get("missing_in_output"):
        missing.append("output_missing=" + ",".join(metrics["missing_in_output"]))
    if metrics.get("missing_in_exec"):
        missing.append("exec_missing=" + ",".join(metrics["missing_in_exec"]))
    if missing:
        parts.append("AUTO-DETECTED GAPS: " + " | ".join(missing))
    parts.append("")
    parts.append("Give the agent ONE hint (<= 80 words) so it can fix the gaps on a retry.")
    return "\n".join(parts)


def run_case_with_critic(case: dict, timeout: int) -> dict:
    # Pass 1
    p1 = bench.run_case(case, timeout=timeout)
    m1 = bench.extract_metrics(p1, case)
    v1, r1 = bench.verdict(case, m1, p1)
    fm1 = bench.classify_failure_mode(case, m1, p1) if v1 != "Pass" else "—"

    if v1 == "Pass":
        return {
            "pass1": {"verdict": v1, "reasons": r1, "failure_mode": fm1, "metrics": m1, "elapsed_s": p1["elapsed_s"]},
            "critic_hint": "",
            "pass2": None,
            "final_verdict": v1,
            "final_reasons": r1,
            "final_metrics": m1,
            "final_failure_mode": "—",
            "improvement": "kept_pass",
        }

    # Critic
    transcript = trim_transcript(p1["stdout"] + "\n" + p1["stderr"])
    critic_user = build_critic_user(case, transcript, m1)
    try:
        hint = call_critic(CRITIC_SYSTEM, critic_user)
    except Exception as exc:
        hint = f"<critic-error: {exc}>"

    # Pass 2 with hint appended
    augmented = dict(case)
    augmented["input"] = case["input"] + "\n\nADVISOR HINT: " + hint
    p2 = bench.run_case(augmented, timeout=timeout)
    m2 = bench.extract_metrics(p2, case)
    v2, r2 = bench.verdict(case, m2, p2)
    fm2 = bench.classify_failure_mode(case, m2, p2) if v2 != "Pass" else "—"

    rank = {"Pass": 2, "Partial": 1, "Fail": 0}
    if rank[v2] > rank[v1]:
        improvement = f"{v1}->{v2}"
    elif rank[v2] == rank[v1]:
        improvement = f"same_{v2}"
    else:
        improvement = f"regressed_{v1}->{v2}"

    return {
        "pass1": {"verdict": v1, "reasons": r1, "failure_mode": fm1, "metrics": m1, "elapsed_s": p1["elapsed_s"]},
        "critic_hint": hint,
        "pass2": {"verdict": v2, "reasons": r2, "failure_mode": fm2, "metrics": m2, "elapsed_s": p2["elapsed_s"]},
        "final_verdict": v2,
        "final_reasons": r2,
        "final_metrics": m2,
        "final_failure_mode": fm2,
        "improvement": improvement,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--limit", type=int)
    parser.add_argument("--ids", type=lambda s: s.split(","))
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--name", default=None)
    args = parser.parse_args()

    cases = bench.load_cases(args.limit, args.ids)
    if not cases:
        print("no cases", file=sys.stderr)
        return 2

    name = args.name or datetime.now().strftime("critic_%Y%m%d_%H%M%S")
    out_dir = bench.RESULTS_ROOT / name
    out_dir.mkdir(parents=True, exist_ok=True)
    log_dir = out_dir / "transcripts"
    log_dir.mkdir(exist_ok=True)
    results_path = out_dir / "results.jsonl"

    print(f"running {len(cases)} cases with critic-retry -> {out_dir}")
    records: list[dict] = []
    with results_path.open("w") as out:
        for i, case in enumerate(cases, 1):
            cid = case["id"]
            t0 = time.time()
            try:
                outcome = run_case_with_critic(case, timeout=args.timeout)
            except Exception as exc:
                outcome = {
                    "pass1": None,
                    "critic_hint": "",
                    "pass2": None,
                    "final_verdict": "Error",
                    "final_reasons": [f"runner_exception: {exc}"],
                    "final_metrics": {},
                    "final_failure_mode": "runner_error",
                    "improvement": "error",
                }
            elapsed = round(time.time() - t0, 1)
            record = {
                "id": cid,
                "category": case.get("category", ""),
                "elapsed_s": elapsed,
                "verdict": outcome["final_verdict"],
                "improvement": outcome["improvement"],
                "reasons": outcome["final_reasons"],
                "failure_mode": outcome["final_failure_mode"],
                "metrics": outcome["final_metrics"],
                "critic_hint": outcome["critic_hint"],
                "pass1_verdict": outcome["pass1"]["verdict"] if outcome["pass1"] else None,
                "pass2_verdict": outcome["pass2"]["verdict"] if outcome["pass2"] else None,
            }
            out.write(json.dumps(record, ensure_ascii=False) + "\n")
            out.flush()
            records.append(record)
            print(f"[{i:>3}/{len(cases)}] {cid} ... {outcome['final_verdict']:7s}  "
                  f"({outcome['improvement']}, {elapsed}s)")

    # short summary
    n = len(records)
    p = sum(1 for r in records if r["verdict"] == "Pass")
    pa = sum(1 for r in records if r["verdict"] == "Partial")
    f = sum(1 for r in records if r["verdict"] == "Fail")
    err = sum(1 for r in records if r["verdict"] == "Error")
    score = round(100.0 * (p + 0.5 * pa) / max(1, n), 1)
    imp = {}
    for r in records:
        imp[r["improvement"]] = imp.get(r["improvement"], 0) + 1
    summary = (out_dir / "summary.md")
    summary.write_text(
        f"# critic-retry result (Tree's advisor design)\n\n"
        f"- Runs: {n}  Pass: {p}  Partial: {pa}  Fail: {f}  Error: {err}\n"
        f"- Score (Pass + 0.5*Partial): **{score}/100**\n"
        f"- Critic model: {CRITIC_MODEL}\n"
        f"- Improvement breakdown: {imp}\n\n"
        f"## Per-case\n\n"
        f"| ID | Verdict | Pass1→Pass2 | Improvement | Reasons |\n"
        f"|---|---|---|---|---|\n"
        + "\n".join(
            f"| {r['id']} | {r['verdict']} | {r['pass1_verdict']}→{r['pass2_verdict']} | {r['improvement']} | {';'.join(r['reasons']) or '—'} |"
            for r in records
        )
        + "\n"
    )
    print(f"\nsummary: {summary}")
    print(f"score: {score}/100  ({p} pass, {pa} partial, {f} fail, {err} error)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
