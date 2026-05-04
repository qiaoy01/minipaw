use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub exec: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn load(dir: &Path) -> Self {
        let Ok(entries) = fs::read_dir(dir) else {
            return Self::default();
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        paths.sort();
        let skills = paths
            .into_iter()
            .filter_map(|path| fs::read_to_string(&path).ok())
            .filter_map(|content| parse_skill_file(&content))
            .collect();
        Self { skills }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Compact index of skill names and descriptions for the LLM prompt.
    pub fn index_text(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("Available skills:\n");
        for skill in &self.skills {
            out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        out
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        let lower = name.trim().to_ascii_lowercase();
        self.skills
            .iter()
            .find(|s| s.name.to_ascii_lowercase() == lower)
    }

    /// Return the first executable skill whose name+description terms overlap
    /// with the input. Used to upgrade miniwhat classification to minihow when
    /// a registered skill can handle the request without LLM reasoning.
    pub fn match_for_input(&self, input: &str) -> Option<&Skill> {
        let input_terms = skill_terms(input);
        if input_terms.is_empty() {
            return None;
        }
        self.skills.iter().find(|skill| {
            if skill.exec.is_none() {
                return false;
            }
            let mut text = skill.name.replace('-', " ");
            text.push(' ');
            text.push_str(&skill.description);
            let skill_t = skill_terms(&text);
            input_terms.iter().any(|t| skill_t.contains(t))
        })
    }

    /// All exec program names defined by skills. Used to auto-trust skill
    /// commands in the tool policy so operator-curated skills run without
    /// requiring MINIPAW_ALLOW_EXEC in the environment.
    pub fn exec_programs(&self) -> impl Iterator<Item = &str> {
        self.skills.iter().filter_map(|s| {
            s.exec
                .as_deref()
                .and_then(|e| e.split_whitespace().next())
        })
    }
}

/// Extract meaningful terms for skill matching. Strips tokens that are too
/// short or too generic to distinguish one skill from another.
fn skill_terms(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3 && !is_generic_word(t))
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

fn is_generic_word(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "the" | "and" | "for" | "get" | "use" | "run" | "show" | "give" | "tell"
            | "what" | "how" | "who" | "why" | "when" | "where" | "its" | "that"
            | "this" | "with" | "from" | "into" | "about" | "are" | "was" | "were"
            | "have" | "has" | "had" | "been" | "can" | "will" | "would" | "should"
            | "may" | "might" | "make" | "does" | "just" | "now" | "not"
    )
}

fn parse_skill_file(content: &str) -> Option<Skill> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after = &trimmed["---".len()..];
    let end = after.find("\n---")?;
    let fields = parse_frontmatter(&after[..end]);
    let name = fields.get("name")?.trim().to_owned();
    let description = fields.get("description")?.trim().to_owned();
    if name.is_empty() || description.is_empty() {
        return None;
    }
    let exec = fields.get("exec").map(|v| v.trim().to_owned());
    Some(Skill { name, description, exec })
}

fn parse_frontmatter(text: &str) -> BTreeMap<&str, &str> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_with_exec() {
        let content = "---\nname: current-time\ndescription: Get the current date and time\nexec: date\n---\n";
        let skill = parse_skill_file(content).unwrap();
        assert_eq!(skill.name, "current-time");
        assert_eq!(skill.description, "Get the current date and time");
        assert_eq!(skill.exec.as_deref(), Some("date"));
    }

    #[test]
    fn parses_skill_without_exec() {
        let content = "---\nname: greet\ndescription: Greet the user warmly\n---\n";
        let skill = parse_skill_file(content).unwrap();
        assert_eq!(skill.name, "greet");
        assert!(skill.exec.is_none());
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(parse_skill_file("no frontmatter here").is_none());
        assert!(parse_skill_file("").is_none());
    }

    #[test]
    fn index_text_lists_all_skills() {
        let registry = SkillRegistry {
            skills: vec![
                Skill {
                    name: "current-time".into(),
                    description: "Get current date and time".into(),
                    exec: Some("date".into()),
                },
                Skill {
                    name: "greet".into(),
                    description: "Greet the user".into(),
                    exec: None,
                },
            ],
        };
        let text = registry.index_text();
        assert!(text.contains("current-time: Get current date and time"));
        assert!(text.contains("greet: Greet the user"));
    }

    #[test]
    fn find_is_case_insensitive() {
        let registry = SkillRegistry {
            skills: vec![Skill {
                name: "current-time".into(),
                description: "desc".into(),
                exec: Some("date".into()),
            }],
        };
        assert!(registry.find("current-time").is_some());
        assert!(registry.find("Current-Time").is_some());
        assert!(registry.find("unknown").is_none());
    }

    #[test]
    fn match_for_input_returns_executable_skill_on_term_overlap() {
        let registry = SkillRegistry {
            skills: vec![
                Skill {
                    name: "current-time".into(),
                    description: "Get the current date and time on the local machine".into(),
                    exec: Some("date".into()),
                },
                Skill {
                    name: "greet".into(),
                    description: "Greet the user warmly".into(),
                    exec: None,
                },
            ],
        };
        // Input shares "time" with current-time name/description.
        assert!(registry.match_for_input("what time is it").is_some());
        // Skill without exec is ignored even if terms match.
        assert!(registry.match_for_input("greet me").is_none());
        // No overlap at all.
        assert!(registry.match_for_input("list my files").is_none());
    }

    #[test]
    fn exec_programs_yields_program_names() {
        let registry = SkillRegistry {
            skills: vec![
                Skill {
                    name: "a".into(),
                    description: "d".into(),
                    exec: Some("date -u".into()),
                },
                Skill {
                    name: "b".into(),
                    description: "d".into(),
                    exec: None,
                },
            ],
        };
        let progs: Vec<&str> = registry.exec_programs().collect();
        assert_eq!(progs, vec!["date"]);
    }
}
