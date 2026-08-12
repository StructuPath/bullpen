//! `bullpen run --json` — the run as newline-delimited JSON.
//!
//! The event kinds below are hand-written string constants, not names derived
//! from the `Event` variants they came from. `bullpen_agent::Event` is
//! internal state: renaming a variant is a refactor, and it must not silently
//! change the wire. Same argument `sessions_json` makes for field names — the
//! moment anything reads this stream, the strings are a compatibility surface
//! and the CLI is the crate that owns it.
//!
//! Every builder here is pure. The one impure function, [`emit`], is the only
//! place the stream touches stdout, so ordering is whatever order the caller
//! calls it in.

use std::io::Write;

use bullpen_agent::{Event, MAX_TOOL_RESULT_BYTES};
use bullpen_llm::Usage;
use serde_json::{Value, json};

pub const KIND_ASSISTANT_TEXT: &str = "assistant_text";
pub const KIND_TOOL_START: &str = "tool_start";
pub const KIND_TOOL_END: &str = "tool_end";
pub const KIND_TURN_DONE: &str = "turn_done";
pub const KIND_RESULT: &str = "result";
pub const KIND_DISPATCHED: &str = "dispatched";

/// One event as one line of the stream. Exhaustive on purpose: a fifth
/// `Event` variant must fail to compile here rather than vanish from the wire.
/// Pure so it can be tested without a terminal.
pub fn event_json(event: &Event) -> Value {
    match event {
        Event::AssistantText { text } => json!({
            "kind": KIND_ASSISTANT_TEXT,
            "text": text,
        }),
        Event::ToolStart { id, name, input } => {
            let (input, truncated) = cap_input(input);
            json!({
                "kind": KIND_TOOL_START,
                "id": id,
                "name": name,
                "input": input,
                "input_truncated": truncated,
            })
        }
        Event::ToolEnd {
            id,
            name,
            output,
            is_error,
        } => {
            let (output, truncated) = cap_text(output);
            json!({
                "kind": KIND_TOOL_END,
                "id": id,
                "name": name,
                "output": output,
                "output_truncated": truncated,
                "is_error": is_error,
            })
        }
        Event::TurnDone { usage } => json!({
            "kind": KIND_TURN_DONE,
            "usage": usage_json(*usage),
        }),
    }
}

/// The terminal object. Carries the outcome outright so a consumer never has
/// to infer completion from the stream closing.
/// Pure so it can be tested without a terminal.
pub fn result_json(session_id: &str, text: &str, usage: Usage, error: Option<&str>) -> Value {
    json!({
        "kind": KIND_RESULT,
        "session_id": session_id,
        "text": text,
        "usage": usage_json(usage),
        "error": error.is_some(),
        "message": error,
    })
}

/// The whole stream for a `--bg` dispatch: the run itself happens in another
/// process, so there is nothing to stream but the handle to it.
/// Pure so it can be tested without a terminal.
pub fn dispatched_json(session_id: &str, input_id: &str, pid: u32) -> Value {
    json!({
        "kind": KIND_DISPATCHED,
        "session_id": session_id,
        "input_id": input_id,
        "pid": pid,
    })
}

fn usage_json(usage: Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    })
}

/// Truncate to the shared 256 KiB cap on a char boundary, reporting whether it
/// had to. The flag replaces `cap_result`'s inline marker: a consumer parsing
/// JSON should not have to scan the payload for a sentinel.
fn cap_text(s: &str) -> (String, bool) {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        return (s.to_string(), false);
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Tool input stays a JSON value while it fits. Over the cap it becomes the
/// truncated serialization as a string — a clipped object would not parse.
fn cap_input(input: &Value) -> (Value, bool) {
    let text = input.to_string();
    if text.len() <= MAX_TOOL_RESULT_BYTES {
        return (input.clone(), false);
    }
    let (capped, _) = cap_text(&text);
    (Value::String(capped), true)
}

/// Write one line and flush it, so a consumer reads the run mid-flight.
/// Failures are dropped for the same reason the delta streamer drops them: a
/// closed stdout must not take the run down with it.
pub fn emit(value: &Value) {
    let mut out = std::io::stdout();
    let _ = out.write_all(value.to_string().as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
        }
    }

    #[test]
    fn event_kinds_are_stable_wire_strings() {
        assert_eq!(KIND_ASSISTANT_TEXT, "assistant_text");
        assert_eq!(KIND_TOOL_START, "tool_start");
        assert_eq!(KIND_TOOL_END, "tool_end");
        assert_eq!(KIND_TURN_DONE, "turn_done");
        assert_eq!(KIND_RESULT, "result");
        assert_eq!(KIND_DISPATCHED, "dispatched");
    }

    #[test]
    fn assistant_text_carries_the_turn_text() {
        assert_eq!(
            event_json(&Event::AssistantText {
                text: "hello".into()
            }),
            json!({ "kind": "assistant_text", "text": "hello" })
        );
    }

    #[test]
    fn tool_start_emits_its_input_as_a_json_value_when_small() {
        assert_eq!(
            event_json(&Event::ToolStart {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({ "command": "ls" }),
            }),
            json!({
                "kind": "tool_start",
                "id": "t1",
                "name": "bash",
                "input": { "command": "ls" },
                "input_truncated": false,
            })
        );
    }

    #[test]
    fn tool_start_input_over_the_cap_becomes_a_truncated_string() {
        let event = Event::ToolStart {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({ "command": "x".repeat(MAX_TOOL_RESULT_BYTES) }),
        };
        let value = event_json(&event);
        assert_eq!(value["input_truncated"], json!(true));
        let input = value["input"].as_str().unwrap();
        assert!(input.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(input.starts_with(r#"{"command":"xxx"#));
    }

    #[test]
    fn tool_end_output_is_truncated_on_a_char_boundary() {
        let event = Event::ToolEnd {
            id: "t1".into(),
            name: "bash".into(),
            // Three-byte chars: the cap lands mid-character.
            output: "☃".repeat(MAX_TOOL_RESULT_BYTES),
            is_error: false,
        };
        let value = event_json(&event);
        assert_eq!(value["output_truncated"], json!(true));
        let output = value["output"].as_str().unwrap();
        assert!(output.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(output.chars().all(|c| c == '☃'));
    }

    #[test]
    fn tool_end_flags_provider_errors() {
        assert_eq!(
            event_json(&Event::ToolEnd {
                id: "t1".into(),
                name: "bash".into(),
                output: "no such file".into(),
                is_error: true,
            }),
            json!({
                "kind": "tool_end",
                "id": "t1",
                "name": "bash",
                "output": "no such file",
                "output_truncated": false,
                "is_error": true,
            })
        );
    }

    #[test]
    fn turn_done_carries_cumulative_usage() {
        assert_eq!(
            event_json(&Event::TurnDone {
                usage: usage(120, 34)
            }),
            json!({
                "kind": "turn_done",
                "usage": { "input_tokens": 120, "output_tokens": 34 },
            })
        );
    }

    #[test]
    fn result_carries_the_full_session_id_not_the_display_prefix() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            result_json(id, "the answer", usage(9, 4), None),
            json!({
                "kind": "result",
                "session_id": id,
                "text": "the answer",
                "usage": { "input_tokens": 9, "output_tokens": 4 },
                "error": false,
                "message": null,
            })
        );
    }

    #[test]
    fn result_on_error_keeps_the_partial_text_and_sets_the_error_flag() {
        assert_eq!(
            result_json("abc", "half an ans", usage(9, 4), Some("provider stopped")),
            json!({
                "kind": "result",
                "session_id": "abc",
                "text": "half an ans",
                "usage": { "input_tokens": 9, "output_tokens": 4 },
                "error": true,
                "message": "provider stopped",
            })
        );
    }

    #[test]
    fn dispatched_carries_the_full_ids_and_pid() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let input_id = "fedcba9876543210fedcba9876543210";
        assert_eq!(
            dispatched_json(session_id, input_id, 4242),
            json!({
                "kind": "dispatched",
                "session_id": session_id,
                "input_id": input_id,
                "pid": 4242,
            })
        );
    }
}
