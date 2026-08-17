---
type: concept
title: Event and streaming model — Events out, assistant text on a separate TextSink
description: The loop emits Event values on an optional channel that rendering consumes outside the loop; assistant text streams live on a separate TextSink so the CLI prints tokens to stdout while tool activity goes to stderr. A gone subscriber never stops the loop.
tags: [events, streaming, textsink, rendering, wire-names]
---

# Event and streaming model

The [agent loop](../crates/agent.md) emits `Event` values (assistant text,
tool start/end, turn done) on an optional channel; rendering lives entirely
outside the loop. Assistant text streams live on a separate `TextSink`. A
gone subscriber never stops the loop — `emit` and the delta sends all drop
send errors.

## `Event`

```rust
pub enum Event {
    AssistantText { text: String },
    ToolStart { id, name, input: serde_json::Value },
    ToolEnd { id, name, output: String, is_error: bool },
    TurnDone { usage: Usage },
}
```

One subscriber; attach via `.with_events(tx)` before `send`. The CLI's printer
task owns stdout under `--json` (so event order is the order the loop produced
them) and routes verbose tool activity to stderr.

## `TextSink`

```rust
pub type TextSink = tokio::sync::mpsc::UnboundedSender<String>;
```

[`Provider::complete_streaming`](../crates/llm.md) sends text fragments as
they arrive; tool calls are still assembled and returned in the final
`Response`. The [Anthropic adapter](../crates/anthropic-adapter.md) forwards
`text_delta` events live and accumulates `input_json_delta` per content-block
index, parsing at `content_block_stop` — never persisting a partial stream.
The [Codex](../crates/codex-adapter.md) and
[chat-completions](../crates/chatcompletions-adapter.md) adapters buffer
(the trait default emits the final text as one delta) until their SSE delta
parsers land. The CLI's delta streamer writes to stdout as text arrives
(dropped under `--json`, but the sink stays attached so the agent still takes
its streaming path).

## Wire names are owned by the CLI

The wire names for the event kinds (`assistant_text`, `tool_start`,
`tool_end`, `turn_done`, `result`, `dispatched`) are owned by the
[CLI's JSON module](../crates/cli-run-json.md), not derived from the `Event`
variants — renaming a variant is a refactor and must not silently change the
wire.

See [the agent loop](../crates/agent.md) for the emit points and
[durable execution](../architecture/durable-execution.md) for the
"never persist partial provider streams" rule that shapes streaming retries.
