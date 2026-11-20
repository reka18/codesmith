//! The `question` tool: the agent asks the user structured questions.
//! Ported from OpenCode's question tool.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.
Usage notes:
- A \"Type your own answer\" option is added automatically; don't include a separate option for free form answers
- Set `multiple: true` to allow selecting more than one option
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<Choice>,
    #[serde(default)]
    pub multiple: bool,
}

/// Asks the user; `None` means the user dismissed the questions.
pub trait Questioner: Send + Sync {
    fn ask(&self, prompts: &[Prompt]) -> Option<Vec<Vec<String>>>;

    /// Ask a whole form, for a front end that shows forms itself: `None`
    /// when it does not (the form is asked as questions instead),
    /// `Some(None)` when the user dismissed it.
    fn ask_form(&self, _form: &crate::form::Info) -> Option<Option<crate::form::Answer>> {
        None
    }
}

pub fn spec() -> (String, Value) {
    (
        DESCRIPTION.into(),
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Questions to ask",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "Complete question"},
                            "header": {"type": "string", "description": "Very short label (max 30 chars)"},
                            "options": {
                                "type": "array",
                                "description": "Available choices",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Display text (1-5 words, concise)"},
                                        "description": {"type": "string", "description": "Explanation of choice"}
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiple": {"type": "boolean", "description": "Allow selecting multiple choices"}
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    )
}

pub fn parse(input: &Value) -> Result<Vec<Prompt>, String> {
    let q = input
        .get("questions")
        .ok_or("Missing required parameter `questions`")?;
    let prompts: Vec<Prompt> =
        serde_json::from_value(q.clone()).map_err(|e| format!("Invalid questions: {e}"))?;
    if prompts.is_empty() {
        return Err("Ask at least one question".into());
    }
    Ok(prompts)
}

/// OpenCode's `toModelContent`.
pub fn to_model_content(prompts: &[Prompt], answers: &[Vec<String>]) -> String {
    let formatted: Vec<String> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let a = answers
                .get(i)
                .filter(|a| !a.is_empty())
                .map(|a| a.join(", "))
                .unwrap_or_else(|| "Unanswered".into());
            format!("\"{}\"=\"{a}\"", p.question)
        })
        .collect();
    format!("User has answered your questions: {}. You can now continue with the user's answers in mind.", formatted.join(", "))
}

/// Turn a typed reply into answers: option numbers (comma or space
/// separated) pick options; anything else is a free-form answer.
pub fn interpret(reply: &str, prompt: &Prompt) -> Vec<String> {
    let r = reply.trim();
    if r.is_empty() {
        return vec![];
    }
    let nums: Option<Vec<usize>> = r
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<usize>().ok())
        .collect();
    match nums {
        Some(ns) if !ns.is_empty() && ns.iter().all(|n| *n >= 1 && *n <= prompt.options.len()) => {
            let picked: Vec<String> = ns
                .iter()
                .map(|n| prompt.options[n - 1].label.clone())
                .collect();
            if prompt.multiple {
                let mut uniq = Vec::new();
                for p in picked {
                    if !uniq.contains(&p) {
                        uniq.push(p);
                    }
                }
                uniq
            } else {
                picked.into_iter().take(1).collect()
            }
        }
        _ => vec![r.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(multiple: bool) -> Prompt {
        Prompt {
            question: "Which database?".into(),
            header: "DB".into(),
            options: vec![
                Choice {
                    label: "SQLite (Recommended)".into(),
                    description: "embedded".into(),
                },
                Choice {
                    label: "Postgres".into(),
                    description: "server".into(),
                },
            ],
            multiple,
        }
    }

    #[test]
    fn interpreting_replies() {
        assert_eq!(interpret("1", &p(false)), vec!["SQLite (Recommended)"]);
        assert_eq!(
            interpret("2, 1", &p(false)),
            vec!["Postgres"],
            "single choice takes the first pick"
        );
        assert_eq!(
            interpret("2,1 2", &p(true)),
            vec!["Postgres", "SQLite (Recommended)"]
        );
        assert_eq!(
            interpret("3", &p(false)),
            vec!["3"],
            "out of range is free text"
        );
        assert_eq!(interpret("use DuckDB", &p(false)), vec!["use DuckDB"]);
        assert!(interpret("  ", &p(false)).is_empty());
    }

    #[test]
    fn model_content() {
        let qs = vec![
            p(false),
            Prompt {
                question: "Name?".into(),
                header: "".into(),
                options: vec![],
                multiple: false,
            },
        ];
        assert_eq!(
            to_model_content(&qs, &[vec!["Postgres".into()], vec![]]),
            "User has answered your questions: \"Which database?\"=\"Postgres\", \"Name?\"=\"Unanswered\". You can now continue with the user's answers in mind."
        );
    }

    #[test]
    fn parsing() {
        let v = json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "A", "description": "a"}]}]});
        assert_eq!(parse(&v).unwrap()[0].options[0].label, "A");
        assert!(parse(&json!({"questions": []})).is_err());
        assert!(parse(&json!({})).is_err());
    }
}
