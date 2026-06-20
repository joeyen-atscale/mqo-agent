# mqo-agent

A deterministic planner that, given one natural-language question, decides which MQO pillar tools to call and in what order — then runs them and emits a signed answer.

## Why it exists

`mqo-demo-runner` proves the MQO pillars compose, but only for a fixed, scripted question walking a hard-coded pillar order. A real agent over the semantic layer is handed an *arbitrary* question and has to decide for itself: does this need a clarify round? Does it mention year-over-year, so time-intelligence has to fire? Is the model on more than one engine, so the answer needs a parity check? Is the column binding confident enough to skip disambiguation entirely?

`mqo-agent` is that decision-maker. The same question always produces the same plan, the same plan always runs the same way, and every selection carries a recorded reason — so the routing is auditable before a single query fires. It is the reference for how an agent should reason about the pillars, not a finished production agent.

## Install

```sh
cargo install --path .
```

Requires Rust 1.85+.

## Quickstart

The fastest way to see what it does is to plan a question without running anything:

```sh
mqo-agent plan "Show me revenue YoY by EMEA region" --model internet_sales
```

```
Plan for: Show me revenue YoY by EMEA region
Model:    internet_sales
Planner:  deterministic

   1. catalog-embed         mqo-catalog-embed
      Reason: Always first: embed question against semantic catalog to identify relevant columns.
   2. binding-confidence    mqo-binding-confidence
      Reason: Always second: score column binding confidence to determine if clarification is needed.
   3. clarify               mqo-clarify
      Reason: Conditional: fires if binding-confidence < 0.45; emits a clarify question and halts until --answer is provided.
   4. time-intelligence     mqo-time-intelligence
      Reason: Period-over-period tokens detected in question: yoy
   5. engine-parity         mqo-engine-parity
      Reason: Model 'internet_sales' is registered on multiple query engines; engine-parity ensures consistent results across them.
   6. sensitivity-scan      mqo-sensitivity-scan
      Reason: Always penultimate: scan for PII or forbidden field leakage before returning.
   7. rosetta-credential    rosetta-credential
      Reason: Always terminal: sign the final answer with provenance.
```

Drop `time-intelligence` by asking a question with no period tokens; drop `engine-parity` by pointing at a single-engine model like `tasty_bytes`.

To execute a plan, use `ask`. The pillar steps shell out to sibling binaries on `PATH`; when those aren't installed, `--mock` runs the whole chain against canned fixtures with no network and no siblings — the mode used in CI:

```sh
mqo-agent ask "What are total sales by category?" --model tasty_bytes --mock
```

This emits `answer.json` to stdout (or to `--out FILE`): the plan, a per-step verdict (`ok` / `skipped` / `clarify_needed` / `failed`), each step's raw output, and a final signed answer.

When the column binding is low-confidence, the agent stops rather than guess:

```sh
mqo-agent ask "Revenue stuff" --model tasty_bytes --mock
# → clarify_needed, with a question; no final answer is fabricated.

# Resume deterministically with the disambiguation:
mqo-agent ask "Revenue stuff" --model tasty_bytes --mock --answer "revenue by product category"
```

## How it works

The planner walks one rule table. Two steps are unconditional bookends; the middle is gated on the question and the model:

| Step | When |
|------|------|
| catalog-embed | Always first — embed the question against the semantic catalog |
| binding-confidence | Always second — score how well columns bind to the question |
| clarify | If binding-confidence < 0.45 — halt and ask, unless `--answer` was already supplied |
| time-intelligence | If the question contains period tokens (yoy, qoq, mom, ytd, "year over year", "prior quarter", …) |
| engine-parity | If the model is registered on more than one query engine |
| sensitivity-scan | Always penultimate — scan for PII or forbidden-field leakage |
| rosetta-credential | Always terminal — sign the answer with provenance |

`plan` shows which steps would fire and why without running them. `ask` runs them in that order — and only those; the clarify gate is the one place a confidence score, not the question text, decides the path. The rule table is bundled and printable:

```sh
mqo-agent rules
```

`mqo-agent serve` runs the same logic as a subprocess MCP server, reading newline-delimited JSON tool calls (`{"tool": "plan", ...}` / `{"tool": "ask", ...}`) on stdin and writing JSON responses on stdout.

## Where it fits

Part of the MQO line. `mqo-mcp` serves the semantic layer; the individual pillars (`mqo-catalog-embed`, `mqo-binding-confidence`, `mqo-time-intelligence`, and the rest) are the tools this agent orchestrates; `mqo-demo-runner` is the scripted predecessor that this generalizes from a fixed path to an arbitrary question.

## Status

The planner and the `--mock` execution path are complete and covered by tests against the acceptance criteria (period-token routing, engine-parity gating, the clarify halt-and-resume, step ordering, determinism, and the `serve` contract). Live execution depends on the pillar binaries being present on `PATH`; absent ones are reported as `skipped` rather than failing the run, so end-to-end results are only as real as the siblings you have installed. The `--planner brain` variant is exposed but not yet wired — it currently produces the same deterministic plan as the default.

## License

MIT — Joe Yen <jyen.tech@gmail.com>
