//! Structured follow-up questions for interactive runs.
//!
//! The tool is transport-agnostic: the application injects an [`Asker`]
//! (the CLI's reads one line from the controlling terminal), and a run
//! with nobody on the other end — background, JSON-streamed, piped —
//! registers the *detached* variant, which fails the call with a clear
//! reason instead of blocking on input nobody will ever type. The model
//! always sees the same tool; only the answerer changes.

use std::sync::Arc;

use bullpen_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Tool, ToolCtx, ToolError, required_str};

/// Answers questions on behalf of whoever is driving this run.
#[async_trait::async_trait]
pub trait Asker: Send + Sync {
    /// Put the rendered `prompt` to the human and return their raw reply.
    async fn ask(&self, prompt: &str) -> Result<String, ToolError>;
}

pub struct Ask {
    asker: Option<Arc<dyn Asker>>,
}

impl Ask {
    /// Someone is on the terminal; questions reach them.
    pub fn interactive(asker: Arc<dyn Asker>) -> Self {
        Self { asker: Some(asker) }
    }

    /// Nobody is listening (background, `--json`, piped stdin). The tool
    /// still registers so the model gets a reason, not an unknown-tool
    /// error.
    pub fn detached() -> Self {
        Self { asker: None }
    }
}

/// The question as the human sees it. Options are numbered so a reply can
/// be just the number.
fn render(question: &str, options: &[&str]) -> String {
    let mut out = question.to_string();
    for (index, option) in options.iter().enumerate() {
        out.push_str(&format!("\n  {}. {option}", index + 1));
    }
    if !options.is_empty() {
        out.push_str("\nReply with a number or free text.");
    }
    out
}

/// A reply of `2` against options means the second option; anything else
/// is taken verbatim. The tool owns this mapping so every transport gets
/// it, and gets it tested.
fn resolve<'a>(answer: &'a str, options: &[&'a str]) -> &'a str {
    match answer.parse::<usize>() {
        Ok(n) if (1..=options.len()).contains(&n) => options[n - 1],
        _ => answer,
    }
}

#[async_trait::async_trait]
impl Tool for Ask {
    fn name(&self) -> &'static str {
        "ask"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask".into(),
            description: "Ask the human driving this run one question and \
                          get their answer. Optional `options` render as a \
                          numbered choice list; the reply is the chosen \
                          option's text, or free text. Use it only when \
                          genuinely blocked on a decision the task cannot \
                          settle — in a detached run there is no one to \
                          answer and the call fails with that reason."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question, complete enough to answer without reading the transcript"
                    },
                    "options": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Choices to offer (optional)"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn run(&self, _ctx: &ToolCtx, _call_id: &str, input: Value) -> Result<String, ToolError> {
        let question = required_str(&input, "question")?;
        let options: Vec<&str> = input
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let Some(asker) = &self.asker else {
            return Err(ToolError::Failed(
                "this run is detached — no one is listening to answer. \
                 Decide with your best judgment and note the assumption in \
                 your report."
                    .into(),
            ));
        };

        let reply = asker.ask(&render(question, &options)).await?;
        let answer = reply.trim();
        if answer.is_empty() {
            return Err(ToolError::Failed(
                "no answer was given (empty reply)".into(),
            ));
        }
        Ok(resolve(answer, &options).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Replies with a fixed line and records the prompt it was shown.
    struct FakeAsker {
        reply: &'static str,
        seen: Mutex<String>,
    }

    impl FakeAsker {
        fn new(reply: &'static str) -> Arc<Self> {
            Arc::new(Self {
                reply,
                seen: Mutex::new(String::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl Asker for FakeAsker {
        async fn ask(&self, prompt: &str) -> Result<String, ToolError> {
            *self.seen.lock().unwrap() = prompt.to_string();
            Ok(format!("{}\n", self.reply))
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn detached_runs_fail_with_a_reason_not_a_hang() {
        let err = Ask::detached()
            .run(&ctx(), "t", json!({"question": "which?"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("detached"), "{err}");
    }

    #[tokio::test]
    async fn a_numbered_reply_selects_the_option() {
        let asker = FakeAsker::new("2");
        let out = Ask::interactive(asker.clone())
            .run(
                &ctx(),
                "t",
                json!({"question": "which db?", "options": ["sqlite", "postgres"]}),
            )
            .await
            .unwrap();
        assert_eq!(out, "postgres");
        let seen = asker.seen.lock().unwrap().clone();
        assert!(seen.contains("1. sqlite"), "{seen}");
        assert!(seen.contains("2. postgres"), "{seen}");
        assert!(seen.contains("number or free text"), "{seen}");
    }

    #[tokio::test]
    async fn free_text_and_out_of_range_numbers_pass_through() {
        let out = Ask::interactive(FakeAsker::new("use mysql actually"))
            .run(
                &ctx(),
                "t",
                json!({"question": "which?", "options": ["a", "b"]}),
            )
            .await
            .unwrap();
        assert_eq!(out, "use mysql actually");

        let out = Ask::interactive(FakeAsker::new("7"))
            .run(
                &ctx(),
                "t",
                json!({"question": "which?", "options": ["a", "b"]}),
            )
            .await
            .unwrap();
        assert_eq!(out, "7");
    }

    #[tokio::test]
    async fn empty_reply_and_missing_question_are_errors() {
        let err = Ask::interactive(FakeAsker::new("   "))
            .run(&ctx(), "t", json!({"question": "q"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no answer"), "{err}");

        let err = Ask::interactive(FakeAsker::new("x"))
            .run(&ctx(), "t", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn asking_is_neither_parallel_nor_replay_safe() {
        let ask = Ask::detached();
        assert!(!ask.parallel_safe(&json!({"question": "q"})));
        assert!(!ask.replay_safe());
    }
}
