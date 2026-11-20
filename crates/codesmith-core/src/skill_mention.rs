//! Skill mentions in a prompt, as OpenCode's prompts carry them: `@id`
//! naming an available skill loads it with the prompt. The first mention
//! of each skill brings its content (what the `skill` tool returns) into
//! the prompt's message, before the prompt's text; later mentions of the
//! same skill only refer to it. A skill the rules deny (`skill` action)
//! refuses the whole prompt before anything is written, as OpenCode's
//! `SkillNotFoundError` does.
//!
//! `@` followed by a path that exists in the working directory is a file
//! mention, not a skill's. A skill can also be activated on its own
//! ([`crate::session::Session::activate_skill`]): its raw content joins
//! the session, without a model call.

use crate::skill::Skill;
use std::path::Path;

/// A skill that is not available (or that the rules deny) to this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    pub skill: String,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "skill {:?} is not available", self.skill)
    }
}

impl std::error::Error for NotFound {}

/// One `@id` in a prompt: the skill and where (byte offsets) it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub skill: String,
    pub start: usize,
    pub end: usize,
}

fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// The skill mentions in `text`: `@id` at the start or after whitespace,
/// naming one of `skills`, that is not a path in `cwd`.
pub fn mentions(text: &str, skills: &[Skill], cwd: &Path) -> Vec<Mention> {
    let mut out = Vec::new();
    for (i, c) in text.char_indices() {
        if c != '@' || text[..i].chars().last().is_some_and(|p| !p.is_whitespace()) {
            continue;
        }
        let rest = &text[i + 1..];
        let len = rest.find(|c: char| !id_char(c)).unwrap_or(rest.len());
        // A trailing dot ends the sentence, not the id.
        let id = rest[..len].trim_end_matches('.');
        if id.is_empty() || !skills.iter().any(|s| s.id == id) || cwd.join(id).exists() {
            continue;
        }
        out.push(Mention {
            skill: id.to_string(),
            start: i,
            end: i + 1 + id.len(),
        });
    }
    out
}

/// What the prompt's message carries before its text: the content of each
/// mentioned skill, once, in the order first mentioned.
pub fn contents(mentioned: &[Mention], skills: &[Skill]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for m in mentioned {
        if seen.contains(&m.skill.as_str()) {
            continue;
        }
        seen.push(&m.skill);
        if let Some(s) = skills.iter().find(|s| s.id == m.skill) {
            out.push(crate::skill::content(s));
        }
    }
    out
}

/// Whether user text is a mentioned skill's content (the engine's).
pub fn is_content(text: &str) -> bool {
    text.starts_with("<skill_content name=\"") && text.trim_end().ends_with("</skill_content>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(id: &str) -> Skill {
        Skill {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            path: format!("/skills/{id}/SKILL.md").into(),
            content: format!("Use {id}"),
        }
    }

    #[test]
    fn finds_mentions_of_known_skills_only() {
        let skills = [skill("effect"), skill("api-design")];
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("api-design"), "a file").unwrap();
        let m = mentions(
            "Use @effect, @api-design, @unknown, a@effect and @effect.",
            &skills,
            cwd.path(),
        );
        let found: Vec<(&str, usize, usize)> = m
            .iter()
            .map(|m| (m.skill.as_str(), m.start, m.end))
            .collect();
        assert_eq!(found, [("effect", 4, 11), ("effect", 49, 56)]);
        let c = contents(&m, &skills);
        assert_eq!(c.len(), 1);
        assert!(is_content(&c[0]));
    }
}
