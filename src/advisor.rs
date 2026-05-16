use std::collections::BTreeSet;

use crate::types::{AgentChoice, MessageClass};

/// Outcome of comparing the primary and advisor responses for the same task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceVerdict {
    /// Outputs share enough surface text to be treated as equivalent.
    Match,
    /// Outputs overlap meaningfully but disagree on details.
    Close,
    /// Outputs diverge enough that the advisor's answer should be preferred.
    Divergent,
}

#[derive(Debug, Clone)]
pub struct DivergenceRecord {
    pub class: MessageClass,
    pub served_by: AgentChoice,
    pub primary: String,
    pub advisor: String,
    pub similarity: f32,
    pub verdict: DivergenceVerdict,
}

impl DivergenceRecord {
    pub fn render(&self) -> String {
        format!(
            "[advisor] class={} served_by={} similarity={:.2} verdict={:?}\n--- primary ---\n{}\n--- advisor ---\n{}",
            self.class,
            self.served_by,
            self.similarity,
            self.verdict,
            cap(&self.primary, 600),
            cap(&self.advisor, 600),
        )
    }
}

pub fn compare(
    class: MessageClass,
    served_by: AgentChoice,
    primary: &str,
    advisor: &str,
) -> DivergenceRecord {
    let similarity = jaccard_similarity(primary, advisor);
    let verdict = if similarity >= 0.75 {
        DivergenceVerdict::Match
    } else if similarity >= 0.35 {
        DivergenceVerdict::Close
    } else {
        DivergenceVerdict::Divergent
    };
    DivergenceRecord {
        class,
        served_by,
        primary: primary.to_owned(),
        advisor: advisor.to_owned(),
        similarity,
        verdict,
    }
}

/// Token-set Jaccard similarity over alphanumeric tokens of length >= 3.
/// Cheap, deterministic, and good enough to flag obvious disagreements
/// without dragging in an embedding model.
fn jaccard_similarity(a: &str, b: &str) -> f32 {
    let left = tokenize(a);
    let right = tokenize(b);
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    let union: usize = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    let intersection: usize = left.intersection(&right).count();
    intersection as f32 / union as f32
}

fn tokenize(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn cap(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_outputs_match() {
        let record = compare(
            MessageClass::MiniWhat,
            AgentChoice::Primary,
            "the time is 12:34",
            "the time is 12:34",
        );
        assert_eq!(record.verdict, DivergenceVerdict::Match);
        assert!(record.similarity > 0.99);
    }

    #[test]
    fn unrelated_outputs_diverge() {
        let record = compare(
            MessageClass::MiniHow,
            AgentChoice::Primary,
            "fifteen apples are stored in the basket",
            "the kernel version reported by uname is 6.12",
        );
        assert_eq!(record.verdict, DivergenceVerdict::Divergent);
        assert!(record.similarity < 0.35);
    }

    #[test]
    fn partial_overlap_is_close() {
        let record = compare(
            MessageClass::MiniWhy,
            AgentChoice::Advisor,
            "the agent failed because the token expired",
            "the agent failed when the api token rotated",
        );
        assert_eq!(record.verdict, DivergenceVerdict::Close);
    }
}
