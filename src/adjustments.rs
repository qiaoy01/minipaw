use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::prompts::PromptStore;
use crate::types::{now_epoch_secs, MessageClass, TaskId};

/// What the advisor proposes after seeing primary's diverged output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdjustmentDirective {
    NoChange,
    /// Append a new numbered rule to the system prompt for `class`.
    RuleAppend { class: MessageClass, rule: String },
    /// Register a new executable skill primary can invoke.
    SkillNew {
        name: String,
        description: String,
        exec: String,
    },
}

impl AdjustmentDirective {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoChange => "no-change",
            Self::RuleAppend { .. } => "rule-append",
            Self::SkillNew { .. } => "skill-new",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::NoChange => "no change suggested".to_owned(),
            Self::RuleAppend { class, rule } => {
                format!("rule-append [{class}]: {}", cap_oneline(rule, 120))
            }
            Self::SkillNew { name, description, .. } => {
                format!("skill-new [{name}]: {}", cap_oneline(description, 120))
            }
        }
    }
}

/// Parse the advisor's response. The advisor is instructed to put the
/// directive on line 1; we also tolerate a leading blank line or a Markdown
/// quote prefix before the directive.
pub fn parse_directive(class: MessageClass, response: &str) -> Option<AdjustmentDirective> {
    let line = response
        .lines()
        .map(|l| l.trim_start_matches(|c: char| c == '>' || c.is_whitespace()))
        .find(|line| !line.is_empty())?;
    if line.starts_with("NO_CHANGE") {
        return Some(AdjustmentDirective::NoChange);
    }
    if let Some(rest) = line.strip_prefix("PROMPT_RULE_APPEND:") {
        let rule = rest.trim();
        if rule.is_empty() || rule.len() > 400 {
            return None;
        }
        return Some(AdjustmentDirective::RuleAppend {
            class,
            rule: rule.to_owned(),
        });
    }
    if let Some(rest) = line.strip_prefix("SKILL_NEW:") {
        let parts: Vec<&str> = rest.split('|').map(str::trim).collect();
        if parts.len() != 3 {
            return None;
        }
        let (name, description, exec) = (parts[0], parts[1], parts[2]);
        if !is_valid_skill_name(name) || description.is_empty() || exec.is_empty() {
            return None;
        }
        return Some(AdjustmentDirective::SkillNew {
            name: name.to_owned(),
            description: description.to_owned(),
            exec: exec.to_owned(),
        });
    }
    None
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Apply directly to live prompts/skills. Used in training mode.
/// When `subclass` is Some, RULE_APPEND directives write to
/// `<class>.<subclass>.md` (creating the file as needed). When None,
/// they fall back to the global class file (legacy behavior).
pub fn apply_training(
    workspace: &Path,
    prompts: &PromptStore,
    directive: &AdjustmentDirective,
    subclass: Option<&str>,
) -> io::Result<String> {
    match directive {
        AdjustmentDirective::NoChange => Ok("no change".to_owned()),
        AdjustmentDirective::RuleAppend { class, rule } => match subclass {
            Some(sub) => {
                let n = prompts.append_rule_to_subclass(*class, sub, rule)?;
                Ok(format!("appended rule {n} to {class}.{sub}.md"))
            }
            None => {
                let n = prompts.append_rule(*class, rule)?;
                Ok(format!("appended rule {n} to {class}.md"))
            }
        },
        AdjustmentDirective::SkillNew {
            name,
            description,
            exec,
        } => {
            let path = write_skill_file(workspace, name, description, exec)?;
            Ok(format!("wrote skill {}", path.display()))
        }
    }
}

/// Stash the directive under `<workspace>/proposals/` for operator review.
/// Used in trial mode.
pub fn write_proposal(
    workspace: &Path,
    task_id: TaskId,
    directive: &AdjustmentDirective,
) -> io::Result<PathBuf> {
    let dir = workspace.join("proposals");
    fs::create_dir_all(&dir)?;
    let timestamp = now_epoch_secs();
    let id = format!("{timestamp}-{}-t{}", directive.kind(), task_id.0);
    let path = dir.join(format!("{id}.md"));
    let body = render_proposal(task_id, timestamp, directive);
    fs::write(&path, body)?;
    Ok(path)
}

fn render_proposal(task_id: TaskId, timestamp: u64, directive: &AdjustmentDirective) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("task: {task_id}\n"));
    out.push_str(&format!("created_at: {timestamp}\n"));
    out.push_str(&format!("kind: {}\n", directive.kind()));
    match directive {
        AdjustmentDirective::NoChange => {
            out.push_str("---\n\nNo change suggested.\n");
        }
        AdjustmentDirective::RuleAppend { class, rule } => {
            out.push_str(&format!("class: {class}\n"));
            out.push_str("---\n\n");
            out.push_str(rule);
            out.push('\n');
        }
        AdjustmentDirective::SkillNew {
            name,
            description,
            exec,
        } => {
            out.push_str(&format!("skill_name: {name}\n"));
            out.push_str(&format!("skill_description: {description}\n"));
            out.push_str(&format!("skill_exec: {exec}\n"));
            out.push_str("---\n\n");
            out.push_str("Generated skill file body:\n\n");
            out.push_str(&render_skill_file(name, description, exec));
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct ProposalEntry {
    pub id: String,
    pub path: PathBuf,
    pub kind: String,
    pub class: Option<MessageClass>,
    pub task_id: Option<TaskId>,
    pub created_at: u64,
    pub body: String,
}

pub fn list_proposals(workspace: &Path) -> Vec<ProposalEntry> {
    let dir = workspace.join("proposals");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| read_proposal(&path))
        .collect()
}

pub fn find_proposal(workspace: &Path, id: &str) -> Option<ProposalEntry> {
    let path = workspace.join("proposals").join(format!("{id}.md"));
    read_proposal(&path)
}

fn read_proposal(path: &Path) -> Option<ProposalEntry> {
    let content = fs::read_to_string(path).ok()?;
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let (front, body) = split_frontmatter(&content)?;
    let mut kind = String::new();
    let mut class = None;
    let mut task_id = None;
    let mut created_at = 0u64;
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "kind" => kind = value.to_owned(),
            "class" => class = MessageClass::parse(value),
            "task" => {
                task_id = value
                    .strip_prefix('t')
                    .unwrap_or(value)
                    .parse::<u64>()
                    .ok()
                    .map(TaskId);
            }
            "created_at" => created_at = value.parse::<u64>().unwrap_or(0),
            _ => {}
        }
    }
    Some(ProposalEntry {
        id,
        path: path.to_owned(),
        kind,
        class,
        task_id,
        created_at,
        body: body.to_owned(),
    })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let stripped = content.strip_prefix("---\n")?;
    let end = stripped.find("\n---")?;
    let front = &stripped[..end];
    let body = stripped[end + "\n---".len()..]
        .trim_start_matches('\n')
        .trim_start_matches('\n');
    Some((front, body))
}

pub fn apply_proposal(
    workspace: &Path,
    prompts: &PromptStore,
    proposal: &ProposalEntry,
) -> io::Result<String> {
    let directive = directive_from_proposal(proposal).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proposal {} is malformed", proposal.id),
        )
    })?;
    let outcome = apply_training(workspace, prompts, &directive, None)?;
    fs::remove_file(&proposal.path)?;
    Ok(outcome)
}

pub fn reject_proposal(proposal: &ProposalEntry) -> io::Result<()> {
    fs::remove_file(&proposal.path)
}

fn directive_from_proposal(p: &ProposalEntry) -> Option<AdjustmentDirective> {
    match p.kind.as_str() {
        "no-change" => Some(AdjustmentDirective::NoChange),
        "rule-append" => Some(AdjustmentDirective::RuleAppend {
            class: p.class?,
            rule: p.body.trim().to_owned(),
        }),
        "skill-new" => {
            // Frontmatter holds the canonical skill_* fields; re-parse them.
            let front = fs::read_to_string(&p.path).ok()?;
            let (front, _) = split_frontmatter(&front)?;
            let mut name = None;
            let mut description = None;
            let mut exec = None;
            for line in front.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    match k.trim() {
                        "skill_name" => name = Some(v.trim().to_owned()),
                        "skill_description" => description = Some(v.trim().to_owned()),
                        "skill_exec" => exec = Some(v.trim().to_owned()),
                        _ => {}
                    }
                }
            }
            Some(AdjustmentDirective::SkillNew {
                name: name?,
                description: description?,
                exec: exec?,
            })
        }
        _ => None,
    }
}

fn write_skill_file(
    workspace: &Path,
    name: &str,
    description: &str,
    exec: &str,
) -> io::Result<PathBuf> {
    let dir = workspace.join("skills");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.md"));
    fs::write(&path, render_skill_file(name, description, exec))?;
    Ok(path)
}

fn render_skill_file(name: &str, description: &str, exec: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nexec: {exec}\n---\n\nAuto-generated by advisor adjustment.\n"
    )
}

fn cap_oneline(text: &str, max: usize) -> String {
    let single = text.replace('\n', " ");
    if single.len() <= max {
        return single;
    }
    let mut end = max;
    while !single.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &single[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_change() {
        assert_eq!(
            parse_directive(MessageClass::MiniHow, "NO_CHANGE\n"),
            Some(AdjustmentDirective::NoChange)
        );
    }

    #[test]
    fn parses_rule_append() {
        let d = parse_directive(
            MessageClass::MiniHow,
            "PROMPT_RULE_APPEND: always print intermediates labeled\n",
        );
        assert_eq!(
            d,
            Some(AdjustmentDirective::RuleAppend {
                class: MessageClass::MiniHow,
                rule: "always print intermediates labeled".into(),
            })
        );
    }

    #[test]
    fn parses_skill_new() {
        let d = parse_directive(
            MessageClass::MiniWhat,
            "SKILL_NEW: disk-free | Show free disk space | df -h\n",
        );
        assert!(matches!(
            d,
            Some(AdjustmentDirective::SkillNew { .. })
        ));
    }

    #[test]
    fn rejects_skill_with_invalid_name() {
        let d = parse_directive(
            MessageClass::MiniWhat,
            "SKILL_NEW: bad name! | desc | echo\n",
        );
        assert!(d.is_none());
    }

    #[test]
    fn ignores_unrecognized_directive() {
        assert!(parse_directive(MessageClass::MiniHow, "explain it differently").is_none());
    }

    #[test]
    fn apply_training_appends_rule() {
        let dir = std::env::temp_dir().join(format!("minipaw-adjust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = PromptStore::install(&dir).unwrap();
        let outcome = apply_training(
            &dir,
            &store,
            &AdjustmentDirective::RuleAppend {
                class: MessageClass::MiniHow,
                rule: "test rule".into(),
            },
            None,
        )
        .unwrap();
        assert!(outcome.contains("rule"));
        let updated = store.read_class(MessageClass::MiniHow);
        assert!(updated.contains("test rule"));
        let _ = fs::remove_dir_all(&dir);
    }
}
