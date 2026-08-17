---
type: crate
title: bullpen run --json — the NDJSON event stream
description: The hand-written wire kinds for assistant_text/tool_start/tool_end/turn_done/result/dispatched, the 256 KiB input/output caps with truncation flags, and the pure emit over stdout.
tags: [cli, json, ndjson, event-stream, wire-format]
---

# `bullpen run --json` (`crates/cli/src/json.rs`)

The run as newline-delimited JSON on stdout. The event kinds here are
hand-written string constants, **not** names derived from the
[`bullpen_agent::Event`](agent.md) variants they came from. `Event` is
internal state: renaming a variant is a refactor and must not silently change
the wire. Same argument `sessions_json` makes for field names — the moment
anything reads this stream, the strings are a compatibility surface and the
CLI is the crate that owns it.

## Wire kinds

```rust
pub const KIND_ASSISTANT_TEXT: &str = "assistant_text";
pub const KIND_TOOL_START: &str = "tool_start";
pub const KIND_TOOL_END: &str = "tool_end";
pub const KIND_TURN_DONE: &str = "turn_done";
pub const KIND_RESULT: &str = "result";
pub const KIND_DISPATCHED: &str = "dispatched";
```

## Builders (pure, testable without a terminal)

- `event_json(event) -> Value` — exhaustive `match` on `Event` (a fifth
  variant must fail to compile here rather than vanish from the wire).
  - `AssistantText { text }` → `{kind, text}`.
  - `ToolStart { id, name, input }` → `{kind, id, name, input, input_truncated}`.
    Tool input stays a JSON value while it fits; over the cap it becomes the
    truncated serialization as a string (a clipped object would not parse).
  - `ToolEnd { id, name, output, is_error }` → `{kind, id, name, output, output_truncated, is_error}`.
  - `TurnDone { usage }` → `{kind, usage: {input_tokens, output_tokens}}`.
- `result_json(session_id, text, usage, error) -> Value` — the terminal
  object. `{kind: "result", session_id, text, usage, error: error.is_some(), message: error}`.
  Carries the outcome outright so a consumer never has to infer completion
  from the stream closing.
- `dispatched_json(session_id, pid) -> Value` — the whole stream for a `--bg`
  dispatch (the run happens in another process); `{kind: "dispatched", session_id, pid}`.

## Caps

`cap_text(s)` truncates to `MAX_TOOL_RESULT_BYTES` (256 KiB, the shared cap
from [agent](agent.md)) on a char boundary, returning `(String, bool)` where
the flag replaces the inline marker — a JSON consumer should not have to scan
the payload for a sentinel. `cap_input(input)` keeps it a JSON value while it
fits, else the truncated serialization as a string.

`emit(value)` writes one line and flushes it so a consumer reads the run
mid-flight. Failures are dropped for the same reason the delta streamer drops
them: a closed stdout must not take the run down with it.

## Focused tests

- `event_kinds_are_stable_wire_strings` — the constants are the documented
  strings.
- `assistant_text_carries_the_turn_text`.
- `tool_start_emits_its_input_as_a_json_value_when_small` /
  `tool_start_input_over_the_cap_becomes_a_truncated_string` — input stays a
  value under the cap; over the cap it becomes a truncated string with
  `input_truncated: true`.
- `tool_end_output_is_truncated_on_a_char_boundary` — three-byte chars cap
  mid-character and the result is still valid UTF-8.
- `tool_end_flags_provider_errors` — `is_error` is emitted.
- `result_*` / `dispatched_*` tests assert the terminal/dispatch shapes.

The [CLI](cli.md) `run` path calls `emit(&event_json(&event))` in the printer
task (one task owns stdout under `--json`) and `emit(&result_json(...))` as
the provably last line after the consumer tasks join.
