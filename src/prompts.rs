use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::types::MessageClass;

const MINIHOW_DEFAULT: &str = include_str!("../prompts/minihow.md");
const MINIWHY_DEFAULT: &str = include_str!("../prompts/miniwhy.md");
const MINIWHAT_DEFAULT: &str = include_str!("../prompts/miniwhat.md");
const ADJUST_META_DEFAULT: &str = include_str!("../prompts/adjust-meta.md");

/// On-disk prompt templates with `{{var}}` substitution. Defaults are
/// materialized to `<workspace>/prompts/*.md` on first run; subsequent reads
/// always go through disk so operator (or advisor) edits take effect without
/// recompiling.
#[derive(Debug, Clone)]
pub struct PromptStore {
    workspace: PathBuf,
}

impl PromptStore {
    pub fn install(workspace: &Path) -> io::Result<Self> {
        let dir = workspace.join("prompts");
        fs::create_dir_all(&dir)?;
        for (file, default) in default_files() {
            let path = dir.join(file);
            if !path.exists() {
                fs::write(&path, default)?;
            }
        }
        Ok(Self {
            workspace: workspace.to_owned(),
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn class_path(&self, class: MessageClass) -> PathBuf {
        self.workspace
            .join("prompts")
            .join(format!("{}.md", class))
    }

    /// Path of the per-subclass overlay file (e.g.
    /// `prompts/minihow.navigation.md`). Caller is responsible for
    /// validating the subclass name.
    fn subclass_path(&self, class: MessageClass, subclass: &str) -> PathBuf {
        self.workspace
            .join("prompts")
            .join(format!("{}.{}.md", class, subclass))
    }

    pub fn read_class(&self, class: MessageClass) -> String {
        let path = self.class_path(class);
        fs::read_to_string(&path).unwrap_or_else(|_| default_for(class).to_owned())
    }

    /// Read a per-subclass overlay if it exists and contains
    /// non-whitespace content. Returns None for missing or empty files —
    /// callers should treat both as "no overlay".
    pub fn read_subclass(&self, class: MessageClass, subclass: &str) -> Option<String> {
        let path = self.subclass_path(class, subclass);
        let content = fs::read_to_string(&path).ok()?;
        if content.trim().is_empty() {
            None
        } else {
            Some(content)
        }
    }

    pub fn render(&self, class: MessageClass, vars: &[(&str, &str)]) -> String {
        substitute(&self.read_class(class), vars)
    }

    /// Render the main class template, then append the per-subclass overlay
    /// (if any). Both go through `{{var}}` substitution. Used during ReAct
    /// training so primary sees rules scoped to the current task subclass.
    pub fn render_with_subclass(
        &self,
        class: MessageClass,
        subclass: Option<&str>,
        vars: &[(&str, &str)],
    ) -> String {
        let mut out = substitute(&self.read_class(class), vars);
        if let Some(sub) = subclass {
            if let Some(overlay) = self.read_subclass(class, sub) {
                let overlay_rendered = substitute(&overlay, vars);
                out.push_str("\n\nSubclass rules (");
                out.push_str(sub);
                out.push_str("):\n");
                out.push_str(overlay_rendered.trim_end());
                out.push('\n');
            }
        }
        out
    }

    pub fn read_adjust_meta(&self) -> String {
        let path = self.workspace.join("prompts").join("adjust-meta.md");
        fs::read_to_string(&path).unwrap_or_else(|_| ADJUST_META_DEFAULT.to_owned())
    }

    pub fn render_adjust_meta(&self, vars: &[(&str, &str)]) -> String {
        substitute(&self.read_adjust_meta(), vars)
    }

    /// Append a new numbered rule to a class prompt. Looks for the highest
    /// existing leading-digit list item and uses the next integer; falls back
    /// to plain append when no numbered list is detected.
    pub fn append_rule(&self, class: MessageClass, rule: &str) -> io::Result<usize> {
        let path = self.class_path(class);
        let text = fs::read_to_string(&path).unwrap_or_else(|_| default_for(class).to_owned());
        let next = next_rule_number(&text);
        let trimmed = rule.trim();
        let mut updated = text.trim_end().to_owned();
        updated.push('\n');
        updated.push_str(&format!("{next}. {trimmed}\n"));
        fs::write(&path, &updated)?;
        Ok(next)
    }

    /// Append a numbered rule to the per-subclass overlay. Creates the file
    /// on first write. Numbering is independent per subclass (each overlay
    /// has its own rule sequence starting at 1).
    pub fn append_rule_to_subclass(
        &self,
        class: MessageClass,
        subclass: &str,
        rule: &str,
    ) -> io::Result<usize> {
        let path = self.subclass_path(class, subclass);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let next = next_rule_number(&text);
        let trimmed = rule.trim();
        let mut updated = if text.trim().is_empty() {
            String::new()
        } else {
            let mut t = text.trim_end().to_owned();
            t.push('\n');
            t
        };
        updated.push_str(&format!("{next}. {trimmed}\n"));
        fs::write(&path, &updated)?;
        Ok(next)
    }

    /// Read raw bytes of the subclass overlay file (or empty string if
    /// missing). Used as the snapshot value for ReAct stage/revert.
    pub fn snapshot_subclass(&self, class: MessageClass, subclass: &str) -> String {
        let path = self.subclass_path(class, subclass);
        fs::read_to_string(&path).unwrap_or_default()
    }

    /// Write the subclass overlay file (creating if needed, deleting if the
    /// content is empty). Used to revert a staged ReAct attempt.
    pub fn restore_subclass(
        &self,
        class: MessageClass,
        subclass: &str,
        content: &str,
    ) -> io::Result<()> {
        let path = self.subclass_path(class, subclass);
        if content.is_empty() {
            if path.exists() {
                fs::remove_file(&path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)
    }
}

fn default_files() -> &'static [(&'static str, &'static str)] {
    &[
        ("minihow.md", MINIHOW_DEFAULT),
        ("miniwhy.md", MINIWHY_DEFAULT),
        ("miniwhat.md", MINIWHAT_DEFAULT),
        ("adjust-meta.md", ADJUST_META_DEFAULT),
    ]
}

fn default_for(class: MessageClass) -> &'static str {
    match class {
        MessageClass::MiniHow => MINIHOW_DEFAULT,
        MessageClass::MiniWhy => MINIWHY_DEFAULT,
        MessageClass::MiniWhat => MINIWHAT_DEFAULT,
    }
}

fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (key, value) in vars {
        let needle = format!("{{{{{key}}}}}");
        out = out.replace(&needle, value);
    }
    out
}

fn next_rule_number(text: &str) -> usize {
    let mut max_seen = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(end) = trimmed.find(|c: char| !c.is_ascii_digit()) else {
            continue;
        };
        if end == 0 {
            continue;
        }
        // Require the digit run to be followed by ". " to count as a numbered rule.
        if !trimmed[end..].starts_with(". ") {
            continue;
        }
        if let Ok(n) = trimmed[..end].parse::<usize>() {
            max_seen = max_seen.max(n);
        }
    }
    max_seen + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_template_variables() {
        let out = substitute("hello {{name}} on {{os}}", &[("name", "world"), ("os", "linux")]);
        assert_eq!(out, "hello world on linux");
    }

    #[test]
    fn next_rule_number_finds_highest() {
        let text = "Header\n1. first\n2. second\n9. ninth\nfooter";
        assert_eq!(next_rule_number(text), 10);
    }

    #[test]
    fn next_rule_number_starts_at_one_when_no_rules() {
        assert_eq!(next_rule_number("no rules here"), 1);
    }

    #[test]
    fn install_writes_defaults_then_persists_edits() {
        let dir = std::env::temp_dir().join(format!("minipaw-prompts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = PromptStore::install(&dir).unwrap();
        let original = store.read_class(MessageClass::MiniWhy);
        assert!(original.contains("analysis advisor"));

        // Operator edit survives across reads.
        fs::write(dir.join("prompts").join("miniwhy.md"), "custom\n").unwrap();
        assert_eq!(store.read_class(MessageClass::MiniWhy), "custom\n");

        // append_rule increments the rule number.
        fs::write(
            dir.join("prompts").join("minihow.md"),
            "intro\n1. first\n2. second\n",
        )
        .unwrap();
        let n = store.append_rule(MessageClass::MiniHow, "the new rule").unwrap();
        assert_eq!(n, 3);
        let updated = store.read_class(MessageClass::MiniHow);
        assert!(updated.contains("3. the new rule"));

        let _ = fs::remove_dir_all(&dir);
    }
}
