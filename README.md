# mqo-agent

Adaptive reference agent that plans which MQO pillars to call for any NL question.

## TL;DR

`mqo-demo-runner` proves the pillars compose — but only for a fixed, scripted question walking a hard-coded pillar order. A real AI agent over the semantic layer is handed an *arbitrary* NL question and must **decide**: does this need a clarify round? does it mention YoY so time-intelligence fires? is the bind confident enough to skip disambiguation? `mqo-agent` is that decision-maker — a deterministic planner that, given one NL question, selects which pillar tools to invoke and in what order, loops on clarify, and stops with a defensible answer.

## Usage

```sh
# Plan which pillars would fire (audit without executing)
mqo-agent plan "Show me revenue YoY by EMEA region" --model internet_sales

# Ask a question (mock mode — no sibling binaries needed)
mqo-agent ask "What are total sales by category?" --model tasty_bytes --mock

# Ask with disambiguation
mqo-agent ask "Revenue stuff" --model tasty_bytes --mock
# If clarify_needed, re-run with:
mqo-agent ask "Revenue stuff" --model tasty_bytes --mock --answer "revenue by product category"

# Show the planner rule table
mqo-agent rules

# Run as MCP subprocess server (stdin/stdout JSON)
mqo-agent serve
```

## Acceptance Criteria

1. `plan "<question with YoY>"` includes a time-intelligence step with a recorded reason; `plan "<question without period tokens>"` omits it.
2. `plan` against a multi-engine model includes an engine-parity step; against a single-engine model it does not.
3. `ask … --mock` executes exactly the planned steps in planned order and emits `answer.json` with per-step verdicts and a final answer; no unplanned pillar runs.
4. A low-confidence bind yields `clarify_needed` + a question and does NOT fabricate a final answer; `ask … --answer <x>` resumes and completes deterministically.
5. sensitivity-scan always appears before the final answer in any non-clarify plan; rosetta-credential is always the terminal step of a completed answer.
6. The planner rule table is a readable bundled config; `--help` documents every subcommand/flag.
7. Determinism: identical `plan` and `--mock ask` output across runs (timings excluded from equality).
8. `serve` answers `mqo-mcp-server` `ask` and `plan` tool calls over stdin/stdout; the full mock test runs with no network and no sibling binaries installed.

## Install

```sh
cargo install --path .
```

Requires Rust 1.85+. External pillar CLIs (`mqo-catalog-embed`, `mqo-binding-confidence`, etc.) are resolved from PATH — use `--mock` for offline/CI testing when they are not installed.

## Planner Rules

The planner is fully deterministic and documented. Run `mqo-agent rules` to see the full rule table. In brief:

| Step | When |
|------|------|
| catalog-embed | Always first |
| binding-confidence | Always second |
| clarify | If confidence < 0.45 (and no `--answer` already given) |
| time-intelligence | If question contains YoY/QoQ/MoM/YTD/etc. tokens |
| engine-parity | If the model is registered on >1 query engine |
| sensitivity-scan | Always penultimate |
| rosetta-credential | Always terminal |

## License

MIT — Joe Yen <jyen.tech@gmail.com>
