//! ReAct training driver. Given a set of cases, for each Fail/Partial case:
//!   1. baseline = primary.run(case)
//!   2. for attempt in 0..max_attempts:
//!      - advisor_out = advisor.run(case)   (tool-equipped reference answer)
//!      - directive = advisor.propose(case, baseline, advisor_out, prior_attempts)
//!      - if NoChange: record + retry (advisor sees its own decline next round)
//!      - stage the directive into prompts/<class>.<subclass>.md
//!      - retest = primary.run(case)
//!      - if verdict improved AND no cross-category regression: PROMOTE, break
//!      - else: REVERT, record, retry
//!
//! All evaluation is in-process via `MiniCore::run_eval` so we don't shell
//! out to pawbench during the loop. Final report can be dumped to JSON for
//! pawbench-style summarization.

use crate::adjustments::{apply_training, AdjustmentDirective};
use crate::minicore::MiniCore;
use crate::rubric::{self, CaseSpec, Verdict};
use crate::types::{AgentChoice, MessageClass};

#[derive(Debug, Clone)]
pub struct TrainingConfig {
    pub max_attempts: usize,
    /// Number of cases sampled per OTHER subclass for cross-cat regression
    /// check before promoting. 0 = skip the check (per-case only).
    pub cross_check_per_subclass: usize,
    /// How many times to run baseline and retest (averaged) to smooth out
    /// qwen9b run-to-run variance. 1 = current behavior (single shot).
    /// Higher values trade wall time for more reliable promote decisions.
    pub retest_runs: usize,
    pub class: MessageClass,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            cross_check_per_subclass: 2,
            retest_runs: 1,
            class: MessageClass::MiniHow,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AttemptAction {
    NoChange,
    Promoted { summary: String },
    Reverted { reason: String },
    CrossCatRegressed { regressions: Vec<String> },
    Unparseable,
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub directive_summary: String,
    pub new_verdict: Verdict,
    pub delta_score: f32,
    pub action: AttemptAction,
}

#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub case_id: String,
    pub subclass: String,
    pub baseline_verdict: Verdict,
    pub baseline_reasons: Vec<String>,
    pub final_verdict: Verdict,
    pub attempts: Vec<AttemptRecord>,
    pub promoted_directive: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrainingReport {
    pub cases: Vec<CaseOutcome>,
    pub baseline_pass: usize,
    pub baseline_partial: usize,
    pub baseline_fail: usize,
    pub final_pass: usize,
    pub final_partial: usize,
    pub final_fail: usize,
    pub total_promoted: usize,
}

/// Pre-pass baseline: run primary on every case, return verdicts. Used as
/// the reference for cross-category regression checks during ReAct.
pub fn precompute_baseline(
    core: &mut MiniCore,
    cases: &[CaseSpec],
) -> Vec<(String, Verdict)> {
    let mut out = Vec::with_capacity(cases.len());
    for case in cases {
        let trace = core.run_eval(AgentChoice::Primary, &case.task, Some(&case.subclass));
        let (v, _) = rubric::evaluate(case, &trace);
        println!("baseline {} subclass={} verdict={}", case.id, case.subclass, v);
        out.push((case.id.clone(), v));
    }
    out
}

pub fn run_react_training(
    core: &mut MiniCore,
    cases: &[CaseSpec],
    cfg: &TrainingConfig,
) -> TrainingReport {
    if !core.has_advisor() {
        eprintln!("train: no advisor configured, training is a no-op");
    }

    // 1. Compute baseline verdicts for ALL cases (used for cross-cat checks).
    println!("=== baseline pass ({} cases) ===", cases.len());
    let baseline_map = precompute_baseline(core, cases);
    let baseline_set: std::collections::HashMap<String, Verdict> = baseline_map
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let baseline_pass = baseline_set.values().filter(|v| **v == Verdict::Pass).count();
    let baseline_partial = baseline_set.values().filter(|v| **v == Verdict::Partial).count();
    let baseline_fail = baseline_set.values().filter(|v| **v == Verdict::Fail).count();
    println!(
        "baseline: {} Pass, {} Partial, {} Fail",
        baseline_pass, baseline_partial, baseline_fail
    );

    let mut outcomes: Vec<CaseOutcome> = Vec::with_capacity(cases.len());
    let mut total_promoted = 0usize;

    // 2. For each case, ReAct if not already Pass.
    for case in cases {
        let baseline_verdict = baseline_set
            .get(&case.id)
            .cloned()
            .unwrap_or(Verdict::Fail);
        if baseline_verdict == Verdict::Pass {
            outcomes.push(CaseOutcome {
                case_id: case.id.clone(),
                subclass: case.subclass.clone(),
                baseline_verdict: baseline_verdict.clone(),
                baseline_reasons: Vec::new(),
                final_verdict: baseline_verdict,
                attempts: Vec::new(),
                promoted_directive: None,
            });
            continue;
        }

        println!(
            "\n=== train case {} subclass={} baseline={} ===",
            case.id, case.subclass, baseline_verdict
        );

        // Always do one trace for the adjust-meta prompt (advisor needs to
        // see one of primary's actual attempts). When retest_runs > 1, also
        // sample additional baseline runs to compute an averaged baseline
        // score — this is what the promote gate compares retest avg against.
        let baseline_trace = core.run_eval(AgentChoice::Primary, &case.task, Some(&case.subclass));
        let (_baseline_re_verdict, baseline_reasons) = rubric::evaluate(case, &baseline_trace);
        let baseline_score = if cfg.retest_runs > 1 {
            let (avg, runs) = sample_avg_score(core, case, cfg.retest_runs.saturating_sub(1));
            let combined =
                (rubric::score(&rubric::evaluate(case, &baseline_trace).0) + avg * runs as f32)
                    / (1 + runs) as f32;
            println!(
                "  baseline avg score over {} runs = {:.3}",
                1 + runs,
                combined
            );
            combined
        } else {
            rubric::score(&baseline_verdict)
        };

        let mut attempts: Vec<AttemptRecord> = Vec::new();
        let mut promoted_directive: Option<String> = None;
        // final_verdict stays at baseline unless a directive promotes successfully.
        let mut final_verdict = baseline_verdict.clone();

        for attempt_idx in 0..cfg.max_attempts {
            println!(
                "  attempt {}/{} (current verdict {})",
                attempt_idx + 1,
                cfg.max_attempts,
                final_verdict
            );

            // Get advisor's tool-equipped reference answer for this case.
            let advisor_trace =
                core.run_eval(AgentChoice::Advisor, &case.task, Some(&case.subclass));

            // Build prior_attempts summary for the adjust-meta prompt.
            let prior = format_prior_attempts(&attempts);
            let rubric_text = format_rubric(case, &baseline_reasons);

            let directive = core.ask_directive(
                &case.task,
                &baseline_trace.output,
                &advisor_trace.output,
                cfg.class,
                &case.subclass,
                &prior,
                &rubric_text,
            );

            let Some(directive) = directive else {
                println!("  attempt {} → unparseable directive", attempt_idx + 1);
                attempts.push(AttemptRecord {
                    directive_summary: "<unparseable>".into(),
                    new_verdict: final_verdict.clone(),
                    delta_score: 0.0,
                    action: AttemptAction::Unparseable,
                });
                continue;
            };

            if matches!(directive, AdjustmentDirective::NoChange) {
                println!("  attempt {} → NO_CHANGE", attempt_idx + 1);
                attempts.push(AttemptRecord {
                    directive_summary: "no-change".into(),
                    new_verdict: final_verdict.clone(),
                    delta_score: 0.0,
                    action: AttemptAction::NoChange,
                });
                continue;
            }

            let summary = directive.summary();
            println!("  attempt {} → propose: {}", attempt_idx + 1, summary);

            // Stage: snapshot subclass overlay. For SKILL_NEW also remember
            // the skill file path so we can delete it on revert — otherwise
            // the orphan skill persists and pollutes subsequent runs.
            let pre_snap = core
                .prompts()
                .snapshot_subclass(cfg.class, &case.subclass);
            let skill_to_cleanup: Option<std::path::PathBuf> = match &directive {
                AdjustmentDirective::SkillNew { name, .. } => Some(
                    core.prompts()
                        .workspace()
                        .join("skills")
                        .join(format!("{name}.md")),
                ),
                _ => None,
            };

            let workspace = core.prompts().workspace().to_owned();
            let apply_res = apply_training(
                &workspace,
                core.prompts(),
                &directive,
                Some(&case.subclass),
            );
            if let Err(err) = apply_res {
                eprintln!("  apply failed: {err}");
                attempts.push(AttemptRecord {
                    directive_summary: summary,
                    new_verdict: final_verdict.clone(),
                    delta_score: 0.0,
                    action: AttemptAction::Reverted {
                        reason: format!("apply error: {err}"),
                    },
                });
                continue;
            }

            // If a new skill was added, reload registry so primary sees it.
            if matches!(directive, AdjustmentDirective::SkillNew { .. }) {
                core.reload_skills();
            }

            // Re-test primary on this case. If retest_runs > 1, average
            // over multiple runs to smooth qwen9b's run-to-run variance.
            let test_trace = core.run_eval(AgentChoice::Primary, &case.task, Some(&case.subclass));
            let (first_verdict, test_reasons) = rubric::evaluate(case, &test_trace);
            let first_score = rubric::score(&first_verdict);
            let (test_verdict, test_score) = if cfg.retest_runs > 1 {
                let (extra_avg, extra_runs) =
                    sample_avg_score(core, case, cfg.retest_runs.saturating_sub(1));
                let avg = (first_score + extra_avg * extra_runs as f32)
                    / (1 + extra_runs) as f32;
                let agg_verdict = verdict_from_avg_score(avg);
                println!(
                    "    retest avg score over {} runs = {:.3} (first={} reasons={:?})",
                    1 + extra_runs,
                    avg,
                    first_verdict,
                    test_reasons
                );
                (agg_verdict, avg)
            } else {
                (first_verdict, first_score)
            };
            let delta = test_score - baseline_score;
            println!(
                "    retest: verdict={} score Δ{:+.3} reasons={:?}",
                test_verdict, delta, test_reasons
            );

            if delta <= 0.0 {
                // Revert: restore overlay snapshot AND delete any orphan
                // skill file we just wrote.
                let _ = core.prompts().restore_subclass(cfg.class, &case.subclass, &pre_snap);
                if let Some(p) = &skill_to_cleanup {
                    let _ = std::fs::remove_file(p);
                    core.reload_skills();
                }
                attempts.push(AttemptRecord {
                    directive_summary: summary,
                    new_verdict: test_verdict,
                    delta_score: delta,
                    action: AttemptAction::Reverted {
                        reason: format!("no improvement (Δ {:.3})", delta),
                    },
                });
                continue;
            }

            // Cross-category sanity check
            if cfg.cross_check_per_subclass > 0 {
                let regressions = cross_cat_check(
                    core,
                    cases,
                    &case.subclass,
                    cfg.cross_check_per_subclass,
                    &baseline_set,
                );
                if !regressions.is_empty() {
                    println!(
                        "    cross-cat regression on {} case(s): {:?} — REVERTING",
                        regressions.len(),
                        regressions
                    );
                    let _ = core
                        .prompts()
                        .restore_subclass(cfg.class, &case.subclass, &pre_snap);
                    if let Some(p) = &skill_to_cleanup {
                        let _ = std::fs::remove_file(p);
                        core.reload_skills();
                    }
                    attempts.push(AttemptRecord {
                        directive_summary: summary,
                        new_verdict: test_verdict,
                        delta_score: delta,
                        action: AttemptAction::CrossCatRegressed { regressions },
                    });
                    continue;
                }
            }

            // PROMOTE
            println!("    PROMOTED");
            attempts.push(AttemptRecord {
                directive_summary: summary.clone(),
                new_verdict: test_verdict.clone(),
                delta_score: delta,
                action: AttemptAction::Promoted {
                    summary: summary.clone(),
                },
            });
            promoted_directive = Some(summary);
            final_verdict = test_verdict;
            total_promoted += 1;
            break;
        }

        outcomes.push(CaseOutcome {
            case_id: case.id.clone(),
            subclass: case.subclass.clone(),
            baseline_verdict: baseline_verdict.clone(),
            baseline_reasons,
            final_verdict,
            attempts,
            promoted_directive,
        });
    }

    // Final tally
    let final_pass = outcomes.iter().filter(|o| o.final_verdict == Verdict::Pass).count();
    let final_partial = outcomes.iter().filter(|o| o.final_verdict == Verdict::Partial).count();
    let final_fail = outcomes.iter().filter(|o| o.final_verdict == Verdict::Fail).count();

    println!(
        "\n=== training complete: {} promoted; verdicts {}P/{}Par/{}F → {}P/{}Par/{}F ===",
        total_promoted,
        baseline_pass,
        baseline_partial,
        baseline_fail,
        final_pass,
        final_partial,
        final_fail
    );

    TrainingReport {
        cases: outcomes,
        baseline_pass,
        baseline_partial,
        baseline_fail,
        final_pass,
        final_partial,
        final_fail,
        total_promoted,
    }
}

fn format_prior_attempts(attempts: &[AttemptRecord]) -> String {
    if attempts.is_empty() {
        return "(none — this is your first attempt for this case)".to_owned();
    }
    let mut out = String::new();
    for (i, a) in attempts.iter().enumerate() {
        out.push_str(&format!(
            "Attempt {}: {} → verdict={} Δscore={:+.2} ({})\n",
            i + 1,
            a.directive_summary,
            a.new_verdict,
            a.delta_score,
            describe_action(&a.action)
        ));
    }
    out.push_str("\nYour next proposal MUST take a different angle than the prior attempts above.");
    out
}

/// Run primary on `case` `n` times and return (avg_score, n). Returns
/// (0.0, 0) when n=0 so the caller can fold it into a single-run baseline
/// without dividing by zero.
fn sample_avg_score(core: &mut MiniCore, case: &CaseSpec, n: usize) -> (f32, usize) {
    if n == 0 {
        return (0.0, 0);
    }
    let mut total = 0.0f32;
    for i in 0..n {
        let trace =
            core.run_eval(AgentChoice::Primary, &case.task, Some(&case.subclass));
        let (v, _) = rubric::evaluate(case, &trace);
        let s = rubric::score(&v);
        println!("      sample {}/{}: verdict={} score={:.2}", i + 1, n, v, s);
        total += s;
    }
    (total / n as f32, n)
}

/// Map an averaged numeric score back to a Verdict for display. Boundaries
/// are chosen so that a strict majority of Pass runs maps to Pass, a strict
/// majority of Fail to Fail, and the middle band shows as Partial.
fn verdict_from_avg_score(avg: f32) -> Verdict {
    if avg >= 0.75 {
        Verdict::Pass
    } else if avg >= 0.25 {
        Verdict::Partial
    } else {
        Verdict::Fail
    }
}

/// Build the rubric block that goes into adjust-meta.md so advisor knows
/// exactly what pawbench is grading on. `current_failures` is the list of
/// rubric reason strings (e.g. "tool_calls<8: got 7", "missing_in_output=contacts")
/// from the most recent baseline trace — these are the exact gaps the
/// directive must close.
fn format_rubric(case: &CaseSpec, current_failures: &[String]) -> String {
    let must_tools = if case.must_tools.is_empty() {
        "(none)".to_owned()
    } else {
        case.must_tools.join(", ")
    };
    let must_in_output = if case.must_in_output.is_empty() {
        "(none)".to_owned()
    } else {
        case.must_in_output.join(", ")
    };
    let must_in_exec = if case.must_in_exec.is_empty() {
        "(none)".to_owned()
    } else {
        case.must_in_exec.join(", ")
    };
    let current = if current_failures.is_empty() {
        "(none — primary already met the rubric on this run)".to_owned()
    } else {
        current_failures
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Pawbench grades this case on EXACT token/tool matches. Your rule MUST target the gaps below; semantic equivalents (e.g. \"threats\" vs \"contacts\") do NOT count.\n\
         min_tool_calls: {}\n\
         must_tools (each must appear at least once in EXEC): {}\n\
         must_in_output (each token must appear in primary's DONE answer): {}\n\
         must_in_exec (each token must appear in some EXEC result): {}\n\
         \n\
         current_failures (what primary missed on its last run):\n{}",
        case.min_tool_calls, must_tools, must_in_output, must_in_exec, current
    )
}

fn describe_action(action: &AttemptAction) -> &'static str {
    match action {
        AttemptAction::NoChange => "you declined to propose",
        AttemptAction::Promoted { .. } => "promoted",
        AttemptAction::Reverted { .. } => "reverted, did not help",
        AttemptAction::CrossCatRegressed { .. } => "reverted, hurt other categories",
        AttemptAction::Unparseable => "your response was unparseable",
    }
}

/// Pick K cases per OTHER subclass (not equal to `current_subclass`),
/// re-run primary on each, return ids that regressed vs baseline.
fn cross_cat_check(
    core: &mut MiniCore,
    cases: &[CaseSpec],
    current_subclass: &str,
    per_subclass: usize,
    baseline: &std::collections::HashMap<String, Verdict>,
) -> Vec<String> {
    // Bucket cases by subclass != current.
    let mut by_sub: std::collections::BTreeMap<String, Vec<&CaseSpec>> = Default::default();
    for c in cases {
        if c.subclass == current_subclass {
            continue;
        }
        by_sub.entry(c.subclass.clone()).or_default().push(c);
    }
    // Pick first `per_subclass` from each bucket (deterministic; manifest order).
    let mut sample: Vec<&CaseSpec> = Vec::new();
    for (_sub, list) in &by_sub {
        for c in list.iter().take(per_subclass) {
            sample.push(c);
        }
    }
    if sample.is_empty() {
        return Vec::new();
    }
    println!(
        "    cross-cat check: {} cases from {} other subclass(es)",
        sample.len(),
        by_sub.len()
    );
    let mut regressions = Vec::new();
    for c in sample {
        let trace = core.run_eval(AgentChoice::Primary, &c.task, Some(&c.subclass));
        let (v, _) = rubric::evaluate(c, &trace);
        let baseline_v = baseline.get(&c.id).cloned().unwrap_or(Verdict::Fail);
        if regressed(&baseline_v, &v) {
            regressions.push(format!("{}:{}→{}", c.id, baseline_v, v));
        }
    }
    regressions
}

fn regressed(baseline: &Verdict, new: &Verdict) -> bool {
    rubric::score(new) < rubric::score(baseline)
}

/// Serialize a TrainingReport as a human-readable summary plus jsonl
/// per-case records. Returns (summary_text, jsonl_text).
pub fn render_report(report: &TrainingReport) -> (String, String) {
    let mut summary = String::new();
    summary.push_str(&format!("# Training report\n\n"));
    summary.push_str(&format!(
        "- baseline: {}P / {}Par / {}F\n",
        report.baseline_pass, report.baseline_partial, report.baseline_fail
    ));
    summary.push_str(&format!(
        "- final:    {}P / {}Par / {}F\n",
        report.final_pass, report.final_partial, report.final_fail
    ));
    summary.push_str(&format!(
        "- promoted: {} directive(s)\n\n",
        report.total_promoted
    ));
    summary.push_str("| Case | Subclass | Baseline | Final | Attempts | Action |\n");
    summary.push_str("|---|---|---|---|---|---|\n");
    for c in &report.cases {
        let act = c
            .promoted_directive
            .as_deref()
            .unwrap_or(if c.attempts.is_empty() { "—" } else { "no promote" });
        summary.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            c.case_id,
            c.subclass,
            c.baseline_verdict,
            c.final_verdict,
            c.attempts.len(),
            act
        ));
    }

    let mut jsonl = String::new();
    for c in &report.cases {
        jsonl.push_str(&format!(
            r#"{{"id":"{}","subclass":"{}","baseline":"{}","final":"{}","attempts":{},"promoted":{}}}"#,
            c.case_id,
            c.subclass,
            c.baseline_verdict,
            c.final_verdict,
            c.attempts.len(),
            c.promoted_directive
                .as_ref()
                .map(|s| format!("\"{}\"", json_escape(s)))
                .unwrap_or_else(|| "null".to_owned())
        ));
        jsonl.push('\n');
    }
    (summary, jsonl)
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

