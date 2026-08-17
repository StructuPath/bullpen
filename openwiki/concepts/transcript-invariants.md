---
type: concept
title: Transcript invariants — every tool_use gets one paired tool_result
description: The four invariants the agent loop enforces on the transcript so it is always structurally valid for the next provider call, including after the max-turns fuse trips and after recovery synthesizes interrupted results.
tags: [transcript, invariants, tool-use-pairing, structural-validity]
---

# Transcript invariants

<!-- openwiki: broken internal link [agent.md] file "agent.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Enforced in [`bullpen-agent`](agent.md), tested in its unit suite and the
<!-- openwiki: broken internal link [harness.md] file "harness.md" does not exist. Fix the href or restore the target, then delete this comment. -->
[harness](harness.md) end-to-end tests. The transcript is the conversation
the model sees; these invariants make it always structurally valid for the
next provider call.

1. **Every `tool_use` gets exactly one `tool_result`**, in the model's
   request order — including unknown tools, failed tools, and future
   denied/canceled/skipped calls, which produce error-shaped results. An
   unknown tool yields `("unknown tool: {name}", true)`; a failed tool yields
   `(e.to_string(), true)`. Tested by `unknown_tool_yields_error_result_and_loop_continues`
   and `tool_roundtrip_pairs_result_with_use`.

2. **An assistant message and its tool results are appended together, never
   separately.** In `drive`, the assistant message and the results message
   are both pushed only after `journal.tool_results` succeeds, so a crash
   before that point leaves neither dangling. The transcript is always
   structurally valid for the next provider call, even after the max-turns
   fuse trips. Tested by `max_turns_fuse_trips` (every assistant tool_use
   message is followed by an all-tool_results message).

3. **A `max_tokens` stop returns the partial text as a distinct error; it is
   never silently continued.** `StopReason::MaxTokens` →
   `AgentError::Truncated { partial }`, distinct from a clean end. Tested by
   `max_tokens_stop_is_distinct_error_with_partial`.

4. **Tool results are capped (256 KiB) before entering the transcript.** The
<!-- openwiki: broken internal link [cli-run-json.md] file "cli-run-json.md" does not exist. Fix the href or restore the target, then delete this comment. -->
   same cap bounds tool payloads on the [CLI's JSON event stream](cli-run-json.md).
   `cap_result` truncates on a char boundary with an inline marker.

## After recovery

<!-- openwiki: broken internal link [recovery.md] file "recovery.md" does not exist. Fix the href or restore the target, then delete this comment. -->
[Recovery](recovery.md) maintains these invariants for a crashed run: it
appends synthetic `interrupted` `ToolResult`s at the provisioned
`results_entry_id` (so every `tool_use` from the interrupted batch still
gets a paired result), and appends a closing assistant note if the transcript
would otherwise end on a user message (so the next run never produces two
consecutive user turns). The `recovers_crashed_batch_with_synthetic_results`
test asserts the 4-message transcript (user, assistant tool_use, synthetic
tool_result, closing assistant note).

See [durable execution](../architecture/durable-execution.md) for the
protocol that makes these appends safe, and the [agent loop](../crates/agent.md)
for the enforcement points.
