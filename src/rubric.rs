//! Case-level rubric: parse pawbench-style case manifests and score a
//! SessionTrace into Pass/Partial/Fail. Mirrors the judgment logic in
//! pawbench/run.py so Rust-side training (src/train.rs) can decide whether
//! a trial primary run improved on baseline without shelling out.

use std::collections::HashSet;

use crate::minicore::SessionTrace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Partial,
    Fail,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => f.write_str("Pass"),
            Self::Partial => f.write_str("Partial"),
            Self::Fail => f.write_str("Fail"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaseSpec {
    pub id: String,
    pub task: String,
    pub subclass: String,
    pub must_tools: Vec<String>,
    pub must_in_output: Vec<String>,
    pub must_in_exec: Vec<String>,
    pub min_tool_calls: usize,
}

/// Pawbench-style tool aliases. Tools in the same set are interchangeable
/// for must_tools satisfaction.
fn tool_aliases() -> &'static [&'static [&'static str]] {
    &[
        &["self_battery_level", "robot_charge_status"],
        &["robot_move_status", "robot_map_locate"],
        &["self_load_avg", "self_diagnostics_summary"],
    ]
}

fn expand_alias(name: &str) -> HashSet<String> {
    for group in tool_aliases() {
        if group.contains(&name) {
            return group.iter().map(|s| (*s).to_owned()).collect();
        }
    }
    let mut set = HashSet::new();
    set.insert(name.to_owned());
    set
}

#[derive(Debug, Clone)]
pub struct CaseMetrics {
    pub tool_calls_total: usize,
    pub tool_calls_distinct: usize,
    pub tool_call_sequence: Vec<String>,
    pub missing_tools: Vec<String>,
    pub missing_in_output: Vec<String>,
    pub missing_in_exec: Vec<String>,
    pub exec_step_count: usize,
}

/// Extract every `paw.py <tool>` invocation from an EXEC command string.
/// Matches pawbench/run.py PAW_TOOL_RE: `paw\.py[^a-zA-Z_]+([a-zA-Z_]\w+)`.
fn extract_paw_tools(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = cmd.as_bytes();
    let pat = b"paw.py";
    let mut i = 0;
    while i + pat.len() <= bytes.len() {
        if &bytes[i..i + pat.len()] == pat {
            // Skip any non-alpha/underscore separators after paw.py
            let mut j = i + pat.len();
            while j < bytes.len()
                && !(bytes[j].is_ascii_alphabetic() || bytes[j] == b'_')
            {
                j += 1;
            }
            // Capture identifier: [A-Za-z_][A-Za-z0-9_]*
            let start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > start {
                out.push(std::str::from_utf8(&bytes[start..j]).unwrap().to_owned());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

pub fn compute_metrics(case: &CaseSpec, trace: &SessionTrace) -> CaseMetrics {
    // Collect all paw.py tool invocations across all EXEC cmds.
    let mut tool_calls = Vec::new();
    for ex in &trace.execs {
        tool_calls.extend(extract_paw_tools(&ex.cmd));
    }
    let used_tools: HashSet<String> = tool_calls.iter().cloned().collect();

    let missing_tools: Vec<String> = case
        .must_tools
        .iter()
        .filter(|t| {
            let aliases = expand_alias(t);
            aliases.intersection(&used_tools).next().is_none()
        })
        .cloned()
        .collect();

    let lo_out = trace.output.to_ascii_lowercase();
    // For must_in_exec, scan the concatenation of all exec results.
    let mut full_exec = String::new();
    for ex in &trace.execs {
        full_exec.push_str(&ex.result);
        full_exec.push('\n');
    }
    // Also include the final output in the exec-substring scan, matching
    // pawbench which scans the full stdout (which includes final answer).
    full_exec.push_str(&trace.output);
    let lo_full = full_exec.to_ascii_lowercase();

    let missing_in_output: Vec<String> = case
        .must_in_output
        .iter()
        .filter(|s| !lo_out.contains(&s.to_ascii_lowercase()))
        .cloned()
        .collect();
    let missing_in_exec: Vec<String> = case
        .must_in_exec
        .iter()
        .filter(|s| !lo_full.contains(&s.to_ascii_lowercase()))
        .cloned()
        .collect();

    CaseMetrics {
        tool_calls_total: tool_calls.len(),
        tool_calls_distinct: used_tools.len(),
        tool_call_sequence: tool_calls,
        missing_tools,
        missing_in_output,
        missing_in_exec,
        exec_step_count: trace.execs.len(),
    }
}

pub fn evaluate(case: &CaseSpec, trace: &SessionTrace) -> (Verdict, Vec<String>) {
    let metrics = compute_metrics(case, trace);
    let mut reasons: Vec<String> = Vec::new();
    let min_tc = case.min_tool_calls.max(1);
    if metrics.tool_calls_total < min_tc {
        reasons.push(format!(
            "tool_calls<{min_tc}: got {}",
            metrics.tool_calls_total
        ));
    }
    if !metrics.missing_tools.is_empty() {
        reasons.push(format!("missing_tools={}", metrics.missing_tools.join(",")));
    }
    if !metrics.missing_in_output.is_empty() {
        reasons.push(format!(
            "missing_in_output={}",
            metrics.missing_in_output.join(",")
        ));
    }
    if !metrics.missing_in_exec.is_empty() {
        reasons.push(format!(
            "missing_in_exec={}",
            metrics.missing_in_exec.join(",")
        ));
    }
    if trace.output.contains("Session reached step limit") {
        reasons.push("step_limit_reached".to_owned());
    }

    if reasons.is_empty() {
        return (Verdict::Pass, reasons);
    }
    // Partial: ≥70% of must_tools satisfied AND min_tool_calls met.
    let total_tools = case.must_tools.len().max(1);
    let sat_tools = case.must_tools.len().saturating_sub(metrics.missing_tools.len());
    let ratio = sat_tools as f32 / total_tools as f32;
    if ratio >= 0.7 && metrics.tool_calls_total >= min_tc {
        return (Verdict::Partial, reasons);
    }
    (Verdict::Fail, reasons)
}

pub fn score(verdict: &Verdict) -> f32 {
    match verdict {
        Verdict::Pass => 1.0,
        Verdict::Partial => 0.5,
        Verdict::Fail => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Manifest parsing
// ---------------------------------------------------------------------------

/// Parse a jsonl manifest where each line is a case record. Reuses
/// pawbench cases.jsonl format. Maps the pawbench `category` field to
/// `subclass` so per-subclass prompts can be addressed.
pub fn parse_manifest(text: &str) -> Result<Vec<CaseSpec>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_case_line(trimmed) {
            Some(c) => out.push(c),
            None => return Err(format!("manifest line {}: failed to parse", i + 1)),
        }
    }
    Ok(out)
}

fn parse_case_line(line: &str) -> Option<CaseSpec> {
    let id = json_string(line, "id")?;
    let task = json_string(line, "input")?;
    // Prefer explicit "subclass" if present; fall back to "category".
    let subclass = json_string(line, "subclass")
        .or_else(|| json_string(line, "category"))
        .unwrap_or_else(|| "general".to_owned());
    let must_tools = json_string_array(line, "must_tools");
    let must_in_output = json_string_array(line, "must_in_output");
    let must_in_exec = json_string_array(line, "must_in_exec");
    let min_tool_calls = json_usize(line, "min_tool_calls").unwrap_or(8);
    Some(CaseSpec {
        id,
        task,
        subclass,
        must_tools,
        must_in_output,
        must_in_exec,
        min_tool_calls,
    })
}

fn json_string(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let kp = text.find(&marker)?;
    let after_key = &text[kp + marker.len()..];
    let colon = after_key.find(':')?;
    let mut chars = after_key[colon + 1..].char_indices();
    let quote_pos = loop {
        let (i, c) = chars.next()?;
        if c == '"' {
            break i;
        }
        if !c.is_whitespace() {
            return None;
        }
    };
    let body_start = colon + 1 + quote_pos + 1;
    let body = &after_key[body_start..];
    let mut out = String::new();
    let mut esc = false;
    for c in body.chars() {
        if esc {
            out.push(match c {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'b' => '\u{0008}',
                'f' => '\u{000C}',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

fn json_usize(text: &str, key: &str) -> Option<usize> {
    let marker = format!("\"{key}\"");
    let kp = text.find(&marker)?;
    let after_key = &text[kp + marker.len()..];
    let colon = after_key.find(':')?;
    let value_str: String = after_key[colon + 1..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    value_str.parse().ok()
}

fn json_string_array(text: &str, key: &str) -> Vec<String> {
    let marker = format!("\"{key}\"");
    let Some(kp) = text.find(&marker) else { return Vec::new() };
    let after_key = &text[kp + marker.len()..];
    let Some(colon) = after_key.find(':') else { return Vec::new() };
    let mut chars = after_key[colon + 1..].char_indices();
    // Find opening '['
    loop {
        match chars.next() {
            Some((_, c)) if c == '[' => break,
            Some((_, c)) if c.is_whitespace() => continue,
            _ => return Vec::new(),
        }
    }
    // Now extract string elements until ']'
    let mut out = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    let mut current = String::new();
    for (_, c) in chars {
        if in_str {
            if esc {
                current.push(match c {
                    '"' => '"',
                    '\\' => '\\',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                out.push(std::mem::take(&mut current));
                in_str = false;
            } else {
                current.push(c);
            }
        } else if c == '"' {
            in_str = true;
        } else if c == ']' {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minicore::{ExecRecord, SessionTrace};

    #[test]
    fn extract_tools_finds_chained_paw() {
        let cmd = "python3 -c \"subprocess.run(['python3','../tools/paw.py','robot_move_forward','3']); subprocess.run(['python3','../tools/paw.py','robot_map_locate'])\"";
        let tools = extract_paw_tools(cmd);
        assert_eq!(tools, vec!["robot_move_forward", "robot_map_locate"]);
    }

    #[test]
    fn parse_pawbench_style_line() {
        let line = r#"{"id": "A01", "category": "survey", "input": "test task", "must_tools": ["a","b"], "must_in_output": ["x"], "must_in_exec": [], "min_tool_calls": 5}"#;
        let c = parse_case_line(line).unwrap();
        assert_eq!(c.id, "A01");
        assert_eq!(c.subclass, "survey");
        assert_eq!(c.must_tools, vec!["a", "b"]);
        assert_eq!(c.must_in_output, vec!["x"]);
        assert_eq!(c.min_tool_calls, 5);
    }

    #[test]
    fn evaluate_pass_when_all_satisfied() {
        let case = CaseSpec {
            id: "T".into(),
            task: "".into(),
            subclass: "general".into(),
            must_tools: vec!["robot_camera_capture".into()],
            must_in_output: vec!["ok".into()],
            must_in_exec: vec![],
            min_tool_calls: 1,
        };
        let trace = SessionTrace {
            output: "task is ok".into(),
            steps: 1,
            execs: vec![ExecRecord {
                cmd: "python3 ../tools/paw.py robot_camera_capture".into(),
                ok: true,
                result: "camera=ok".into(),
            }],
        };
        let (v, _) = evaluate(&case, &trace);
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn evaluate_partial_when_count_short() {
        let case = CaseSpec {
            id: "T".into(),
            task: "".into(),
            subclass: "general".into(),
            must_tools: vec!["a".into(), "b".into(), "c".into()],
            must_in_output: vec![],
            must_in_exec: vec![],
            min_tool_calls: 2,
        };
        // 2 of 3 must_tools satisfied (67% < 70%) → Fail
        let trace = SessionTrace {
            output: "done".into(),
            steps: 1,
            execs: vec![
                ExecRecord { cmd: "python3 ../tools/paw.py a".into(), ok: true, result: "".into() },
                ExecRecord { cmd: "python3 ../tools/paw.py b".into(), ok: true, result: "".into() },
            ],
        };
        let (v, _) = evaluate(&case, &trace);
        assert_eq!(v, Verdict::Fail);
    }

    #[test]
    fn alias_satisfies_must_tool() {
        let case = CaseSpec {
            id: "T".into(),
            task: "".into(),
            subclass: "general".into(),
            must_tools: vec!["self_battery_level".into()],
            must_in_output: vec![],
            must_in_exec: vec![],
            min_tool_calls: 1,
        };
        let trace = SessionTrace {
            output: "done".into(),
            steps: 1,
            execs: vec![ExecRecord {
                cmd: "python3 ../tools/paw.py robot_charge_status".into(),
                ok: true,
                result: "".into(),
            }],
        };
        let (v, _) = evaluate(&case, &trace);
        assert_eq!(v, Verdict::Pass);
    }
}
