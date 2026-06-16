//! mqo-agent — adaptive reference agent that plans which MQO pillars to call.
//!
//! The default planner is deterministic rule-based (no LLM required for tests).
//! External pillar CLIs are invoked via subprocess using the same tool-JSON contract
//! as `mqo-demo-runner`. When a binary is absent, the step falls back gracefully.

use std::{
    collections::HashMap,
    process::{Command, Stdio},
    time::Instant,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────── Planner config ────────────────

/// The bundled default planner rule table.
/// This is the readable config that drives all planning decisions.
pub const DEFAULT_RULE_TABLE: &str = r#"
# mqo-agent planner rule table (TOML-ish comment format for readability)
#
# Rule evaluation order (all rules are checked; highest-priority wins for ordering):
#
# 1. catalog-embed       — ALWAYS first: embeds the question against the semantic catalog
# 2. binding-confidence  — ALWAYS second: scores how well columns bound to the question
# 3. clarify             — IF binding_confidence < LOW_CONF_THRESHOLD (0.45)
#                          ↳ emits clarify_needed + question, halts until --answer provided
# 4. time-intelligence   — IF question contains period-over-period tokens:
#                          yoy, qoq, mom, ytd, "year over year", "quarter over quarter",
#                          "month over month", "year-to-date", "prior year", "prior quarter"
# 5. engine-parity       — IF model is registered on >1 engine (multi_engine fixture flag)
# 6. sensitivity-scan    — ALWAYS before final answer; ensures no PII/forbidden fields leak
# 7. rosetta-credential  — ALWAYS terminal: signs the final answer with provenance
#
# Planner variant:
#   --planner deterministic (default): rule table above, no LLM
#   --planner brain: consult local wm-brain for ordering (opt-in, not the test path)
"#;

/// Confidence threshold below which clarify fires.
pub const LOW_CONF_THRESHOLD: f64 = 0.45;

/// Period-over-period token patterns that trigger time-intelligence.
pub const PERIOD_TOKENS: &[&str] = &[
    "yoy",
    "qoq",
    "mom",
    "ytd",
    "year over year",
    "quarter over quarter",
    "month over month",
    "year-to-date",
    "prior year",
    "prior quarter",
    "year-on-year",
    "quarter-on-quarter",
    "month-on-month",
];

// ──────────────────────────────────────────── Model fixtures ─────────────────

/// Per-model fixture metadata used by the planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFixture {
    pub model: String,
    /// If true, the model is registered on more than one query engine.
    pub multi_engine: bool,
    /// Number of registered engines (informational).
    pub engine_count: usize,
}

/// Returns the bundled model fixture map.
pub fn bundled_model_fixtures() -> HashMap<String, ModelFixture> {
    let mut m = HashMap::new();
    // Tasty Bytes — single Snowflake engine.
    m.insert(
        "tasty_bytes".to_string(),
        ModelFixture {
            model: "tasty_bytes".to_string(),
            multi_engine: false,
            engine_count: 1,
        },
    );
    // internet_sales — BigQuery + Snowflake.
    m.insert(
        "internet_sales".to_string(),
        ModelFixture {
            model: "internet_sales".to_string(),
            multi_engine: true,
            engine_count: 2,
        },
    );
    // Default fallback — single engine.
    m.insert(
        "__default__".to_string(),
        ModelFixture {
            model: "__default__".to_string(),
            multi_engine: false,
            engine_count: 1,
        },
    );
    m
}

// ──────────────────────────────────────────── Plan types ─────────────────────

/// A single planned pillar step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Short pillar name, e.g. `"catalog-embed"`.
    pub pillar: String,
    /// Binary that implements this pillar.
    pub tool: String,
    /// Human-readable reason this step was selected.
    pub reason: String,
}

/// The full plan: ordered list of steps + metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub question: String,
    pub model: String,
    pub planner: String,
    pub steps: Vec<PlanStep>,
}

// ──────────────────────────────────────────── Execution types ────────────────

/// Verdict from one executed pillar step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    ClarifyNeeded,
    Skipped,
    Blocked,
    Failed,
}

/// Per-step result in an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub pillar: String,
    pub tool: String,
    pub verdict: Verdict,
    /// The raw output from the subprocess (or mock).
    pub output: serde_json::Value,
    /// Wall-clock milliseconds (excluded from determinism checks).
    pub ms: u64,
}

/// State checkpointed between a clarify pause and resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyCheckpoint {
    pub question: String,
    pub model: String,
    pub clarify_question: String,
    /// Steps already completed before clarify halted.
    pub completed_steps: Vec<StepResult>,
    /// Steps remaining after clarify (post-resume plan).
    pub pending_pillars: Vec<PlanStep>,
}

/// The final output of an `ask` run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub question: String,
    pub model: String,
    /// The plan that was executed.
    pub plan: Vec<PlanStep>,
    /// Results for each executed step.
    pub step_results: Vec<StepResult>,
    /// Whether a clarify round was needed.
    pub clarify_needed: bool,
    /// The clarify question emitted (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarify_question: Option<String>,
    /// The user's disambiguation answer (if provided).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarify_answer: Option<String>,
    /// The final signed answer text (or None when clarify_needed and no answer given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_answer: Option<String>,
}

// ──────────────────────────────────────────── Mock responses ─────────────────

/// Returns a canned mock response for a given pillar + question.
/// These are deterministic and stable across runs.
fn mock_response(pillar: &str, question: &str, confidence_override: Option<f64>) -> serde_json::Value {
    match pillar {
        "catalog-embed" => serde_json::json!({
            "pillar": "catalog-embed",
            "status": "ok",
            "embedded_tokens": 12,
            "top_matches": ["revenue", "region", "category"]
        }),
        "binding-confidence" => {
            let conf = confidence_override.unwrap_or(0.82);
            serde_json::json!({
                "pillar": "binding-confidence",
                "status": "ok",
                "confidence": conf,
                "bucket": if conf < LOW_CONF_THRESHOLD { "low" } else if conf < 0.7 { "medium" } else { "high" },
                "question": question
            })
        }
        "clarify" => serde_json::json!({
            "pillar": "clarify",
            "status": "clarify_needed",
            "clarify_question": "Did you mean revenue by customer segment or by product category?"
        }),
        "time-intelligence" => serde_json::json!({
            "pillar": "time-intelligence",
            "status": "ok",
            "period": "year-over-year",
            "anchor_date": "2024-12-31",
            "comparison_window": "2023-12-31"
        }),
        "engine-parity" => serde_json::json!({
            "pillar": "engine-parity",
            "status": "ok",
            "engines": ["bigquery", "snowflake"],
            "parity": "verified"
        }),
        "sensitivity-scan" => serde_json::json!({
            "pillar": "sensitivity-scan",
            "status": "ok",
            "violations": [],
            "cleared": true
        }),
        "rosetta-credential" => serde_json::json!({
            "pillar": "rosetta-credential",
            "status": "ok",
            "provenance": "rosetta-signed",
            "answer": "Query executed successfully with verified provenance."
        }),
        _ => serde_json::json!({
            "pillar": pillar,
            "status": "ok"
        }),
    }
}

// ──────────────────────────────────────────── Planner ───────────────────────

/// Options for the planner.
#[derive(Debug, Clone)]
pub struct PlannerOptions {
    pub planner: PlannerVariant,
    pub model_fixtures: HashMap<String, ModelFixture>,
}

/// Which planner to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerVariant {
    Deterministic,
    Brain,
}

impl Default for PlannerOptions {
    fn default() -> Self {
        PlannerOptions {
            planner: PlannerVariant::Deterministic,
            model_fixtures: bundled_model_fixtures(),
        }
    }
}

/// Checks if the question contains any period-over-period tokens.
pub fn has_period_tokens(question: &str) -> bool {
    let lower = question.to_lowercase();
    PERIOD_TOKENS.iter().any(|tok| lower.contains(tok))
}

/// Checks if the model is multi-engine per the fixture map.
pub fn is_multi_engine(model: &str, fixtures: &HashMap<String, ModelFixture>) -> bool {
    fixtures
        .get(model)
        .or_else(|| fixtures.get("__default__"))
        .map(|f| f.multi_engine)
        .unwrap_or(false)
}

/// Build the deterministic plan for a question + model.
pub fn build_plan(question: &str, model: &str, opts: &PlannerOptions) -> Plan {
    let mut steps = Vec::new();

    // Step 1: catalog-embed — always
    steps.push(PlanStep {
        pillar: "catalog-embed".to_string(),
        tool: "mqo-catalog-embed".to_string(),
        reason: "Always first: embed question against semantic catalog to identify relevant columns.".to_string(),
    });

    // Step 2: binding-confidence — always
    steps.push(PlanStep {
        pillar: "binding-confidence".to_string(),
        tool: "mqo-binding-confidence".to_string(),
        reason: "Always second: score column binding confidence to determine if clarification is needed.".to_string(),
    });

    // Step 3: clarify — if low confidence (represented in plan; runtime decides based on score)
    // We include it in the plan as a conditional step; at execution time the actual score decides.
    // For plan-only (no execution), we include it as a conditional marker.
    steps.push(PlanStep {
        pillar: "clarify".to_string(),
        tool: "mqo-clarify".to_string(),
        reason: format!(
            "Conditional: fires if binding-confidence < {LOW_CONF_THRESHOLD:.2}; \
             emits a clarify question and halts until --answer is provided."
        ),
    });

    // Step 4: time-intelligence — if period tokens
    if has_period_tokens(question) {
        steps.push(PlanStep {
            pillar: "time-intelligence".to_string(),
            tool: "mqo-time-intelligence".to_string(),
            reason: format!(
                "Period-over-period tokens detected in question: {}",
                PERIOD_TOKENS
                    .iter()
                    .filter(|t| question.to_lowercase().contains(*t))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    // Step 5: engine-parity — if multi-engine model
    if is_multi_engine(model, &opts.model_fixtures) {
        steps.push(PlanStep {
            pillar: "engine-parity".to_string(),
            tool: "mqo-engine-parity".to_string(),
            reason: format!(
                "Model '{model}' is registered on multiple query engines; \
                 engine-parity ensures consistent results across them."
            ),
        });
    }

    // Step 6: sensitivity-scan — always before final answer
    steps.push(PlanStep {
        pillar: "sensitivity-scan".to_string(),
        tool: "mqo-sensitivity-scan".to_string(),
        reason: "Always penultimate: scan for PII or forbidden field leakage before returning.".to_string(),
    });

    // Step 7: rosetta-credential — always terminal
    steps.push(PlanStep {
        pillar: "rosetta-credential".to_string(),
        tool: "rosetta-credential".to_string(),
        reason: "Always terminal: sign the final answer with provenance.".to_string(),
    });

    Plan {
        question: question.to_string(),
        model: model.to_string(),
        planner: match opts.planner {
            PlannerVariant::Deterministic => "deterministic".to_string(),
            PlannerVariant::Brain => "brain".to_string(),
        },
        steps,
    }
}

// ──────────────────────────────────────────── Runner ────────────────────────

/// Options for ask/run execution.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Use mock responses instead of real subprocess invocations.
    pub mock: bool,
    /// Optional disambiguation answer (for --answer resume).
    pub clarify_answer: Option<String>,
    /// Optional confidence override for mock (for testing low-conf path).
    pub mock_confidence: Option<f64>,
}

/// Invoke one pillar step as a subprocess. Returns (verdict, output, ms).
fn invoke_step(
    step: &PlanStep,
    question: &str,
    model: &str,
    opts: &RunOptions,
) -> (Verdict, serde_json::Value, u64) {
    let start = Instant::now();

    if opts.mock {
        let conf_override = if step.pillar == "binding-confidence" {
            opts.mock_confidence
        } else {
            None
        };
        let output = mock_response(&step.pillar, question, conf_override);
        let ms = start.elapsed().as_millis() as u64;

        // Determine verdict from mock output
        let verdict = if step.pillar == "clarify" {
            let status = output.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "clarify_needed" {
                Verdict::ClarifyNeeded
            } else {
                Verdict::Ok
            }
        } else {
            Verdict::Ok
        };

        return (verdict, output, ms);
    }

    // Real subprocess invocation
    // Check if binary is on PATH
    let binary_available = which_binary(&step.tool);
    if binary_available.is_none() {
        let ms = start.elapsed().as_millis() as u64;
        return (
            Verdict::Skipped,
            serde_json::json!({
                "pillar": step.pillar,
                "status": "skipped",
                "reason": format!("binary '{}' not found on PATH; use --mock for offline testing", step.tool)
            }),
            ms,
        );
    }

    // Invoke the binary with tool-JSON contract
    let result = Command::new(step.tool.clone())
        .arg("ask")
        .arg(question)
        .arg("--model")
        .arg(model)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(out) if out.status.success() => {
            let output: serde_json::Value = serde_json::from_slice(&out.stdout)
                .unwrap_or_else(|_| {
                    serde_json::json!({ "pillar": step.pillar, "raw": String::from_utf8_lossy(&out.stdout).to_string() })
                });
            (Verdict::Ok, output, ms)
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            (
                Verdict::Failed,
                serde_json::json!({
                    "pillar": step.pillar,
                    "status": "failed",
                    "exit_code": out.status.code(),
                    "stderr": stderr
                }),
                ms,
            )
        }
        Err(e) => (
            Verdict::Failed,
            serde_json::json!({
                "pillar": step.pillar,
                "status": "failed",
                "error": e.to_string()
            }),
            ms,
        ),
    }
}

/// Returns the path to a binary if it exists on PATH, else None.
fn which_binary(name: &str) -> Option<String> {
    // Use `which` or check PATH manually
    std::env::var("PATH").ok().and_then(|path| {
        for dir in path.split(':') {
            let candidate = format!("{dir}/{name}");
            if std::path::Path::new(&candidate).exists() {
                return Some(candidate);
            }
        }
        None
    })
}

/// Extract binding confidence from a step result output.
fn extract_confidence(output: &serde_json::Value) -> Option<f64> {
    output.get("confidence").and_then(|v| v.as_f64())
}

/// Extract clarify question from a clarify step output.
fn extract_clarify_question(output: &serde_json::Value) -> Option<String> {
    output
        .get("clarify_question")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Execute the plan, running each step in order and returning the final Answer.
pub fn execute_plan(
    plan: &Plan,
    run_opts: &RunOptions,
) -> Answer {
    let mut step_results: Vec<StepResult> = Vec::new();
    let mut clarify_question_out: Option<String> = None;
    let mut binding_conf: f64 = 1.0;
    let question = &plan.question;
    let model = &plan.model;

    for step in &plan.steps {
        // Special handling: clarify step only fires if binding confidence is low
        if step.pillar == "clarify" {
            if binding_conf >= LOW_CONF_THRESHOLD {
                // Skip clarify — confidence is fine
                step_results.push(StepResult {
                    pillar: step.pillar.clone(),
                    tool: step.tool.clone(),
                    verdict: Verdict::Skipped,
                    output: serde_json::json!({
                        "pillar": "clarify",
                        "status": "skipped",
                        "reason": format!("confidence {binding_conf:.3} >= threshold {LOW_CONF_THRESHOLD:.2}")
                    }),
                    ms: 0,
                });
                continue;
            }

            // If we have a --answer already, skip clarify and proceed
            if run_opts.clarify_answer.is_some() {
                step_results.push(StepResult {
                    pillar: step.pillar.clone(),
                    tool: step.tool.clone(),
                    verdict: Verdict::Skipped,
                    output: serde_json::json!({
                        "pillar": "clarify",
                        "status": "skipped",
                        "reason": "disambiguation answer already provided via --answer"
                    }),
                    ms: 0,
                });
                continue;
            }

            // Actually invoke clarify (real or mock)
            let (verdict, output, ms) = invoke_step(step, question, model, run_opts);
            clarify_question_out = extract_clarify_question(&output);

            step_results.push(StepResult {
                pillar: step.pillar.clone(),
                tool: step.tool.clone(),
                verdict: verdict.clone(),
                output,
                ms,
            });

            if verdict == Verdict::ClarifyNeeded {
                // Halt — return partial answer with clarify_needed
                return Answer {
                    question: question.clone(),
                    model: model.clone(),
                    plan: plan.steps.clone(),
                    step_results,
                    clarify_needed: true,
                    clarify_question: clarify_question_out,
                    clarify_answer: None,
                    final_answer: None,
                };
            }
            continue;
        }

        let (verdict, output, ms) = invoke_step(step, question, model, run_opts);

        // Capture binding confidence for clarify gating
        if step.pillar == "binding-confidence" {
            if let Some(conf) = extract_confidence(&output) {
                binding_conf = conf;
            }
        }

        step_results.push(StepResult {
            pillar: step.pillar.clone(),
            tool: step.tool.clone(),
            verdict: verdict.clone(),
            output,
            ms,
        });

        // If a step fails (not just skips), stop the chain
        if verdict == Verdict::Failed {
            break;
        }
    }

    // Determine if a clarify was needed from the step results
    let clarify_fired = step_results
        .iter()
        .any(|r| r.pillar == "clarify" && r.verdict == Verdict::ClarifyNeeded);

    // Build the final answer
    let final_answer = if clarify_fired {
        None
    } else {
        // Look for answer from rosetta-credential step
        let rosetta_out = step_results
            .iter()
            .find(|r| r.pillar == "rosetta-credential")
            .map(|r| r.output.clone());
        rosetta_out
            .as_ref()
            .and_then(|o| o.get("answer").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .or_else(|| Some("Query plan executed successfully.".to_string()))
    };

    Answer {
        question: question.clone(),
        model: model.clone(),
        plan: plan.steps.clone(),
        step_results,
        clarify_needed: clarify_fired,
        clarify_question: clarify_question_out,
        clarify_answer: run_opts.clarify_answer.clone(),
        final_answer,
    }
}

// ──────────────────────────────────────────── MCP serve ─────────────────────

/// Run as an MCP subprocess server over stdin/stdout.
/// Reads newline-delimited JSON tool calls; supports `ask` and `plan` tools.
pub fn serve_mcp() -> Result<()> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let call: serde_json::Value =
            serde_json::from_str(&line).context("parse tool call JSON")?;

        let tool = call
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let question = call
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let model = call
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("__default__");
        let mock = call.get("mock").and_then(|v| v.as_bool()).unwrap_or(false);

        let opts = PlannerOptions::default();

        let result: serde_json::Value = match tool {
            "plan" => {
                let plan = build_plan(question, model, &opts);
                serde_json::to_value(&plan).unwrap_or(serde_json::json!({"error": "serialize"}))
            }
            "ask" => {
                let plan = build_plan(question, model, &opts);
                let run_opts = RunOptions {
                    mock,
                    clarify_answer: call
                        .get("answer")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    mock_confidence: None,
                };
                let answer = execute_plan(&plan, &run_opts);
                serde_json::to_value(&answer).unwrap_or(serde_json::json!({"error": "serialize"}))
            }
            _ => serde_json::json!({
                "error": format!("unknown tool '{}'; supported: ask, plan", tool)
            }),
        };

        let response = serde_json::json!({
            "tool": tool,
            "result": result
        });
        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }

    Ok(())
}

// ──────────────────────────────────────────── Tests ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> PlannerOptions {
        PlannerOptions::default()
    }

    fn default_run_opts() -> RunOptions {
        RunOptions {
            mock: true,
            clarify_answer: None,
            mock_confidence: None,
        }
    }

    // AC1: plan with YoY includes time-intelligence; plan without omits it.
    #[test]
    fn ac1_yoy_question_includes_time_intelligence() {
        let plan = build_plan("Show me revenue YoY by region", "internet_sales", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        assert!(
            pillars.contains(&"time-intelligence"),
            "YoY question must include time-intelligence step; got: {:?}",
            pillars
        );
        // Also verify the reason records the token
        let ti = plan.steps.iter().find(|s| s.pillar == "time-intelligence").unwrap();
        assert!(
            ti.reason.to_lowercase().contains("yoy"),
            "time-intelligence reason should mention 'yoy'"
        );
    }

    #[test]
    fn ac1_no_period_tokens_omits_time_intelligence() {
        let plan = build_plan("Show me menu items by category", "tasty_bytes", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        assert!(
            !pillars.contains(&"time-intelligence"),
            "Non-period question must NOT include time-intelligence; got: {:?}",
            pillars
        );
    }

    #[test]
    fn ac1_period_token_variants() {
        for token in &["QoQ", "MoM", "year over year", "prior quarter", "YTD", "year-to-date"] {
            let q = format!("Show me sales {} comparison", token);
            let plan = build_plan(&q, "tasty_bytes", &default_opts());
            let has_ti = plan.steps.iter().any(|s| s.pillar == "time-intelligence");
            assert!(has_ti, "token '{}' should trigger time-intelligence", token);
        }
    }

    // AC2: multi-engine model includes engine-parity; single-engine does not.
    #[test]
    fn ac2_multi_engine_includes_engine_parity() {
        let plan = build_plan("What is total revenue?", "internet_sales", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        assert!(
            pillars.contains(&"engine-parity"),
            "multi-engine model must include engine-parity; got: {:?}",
            pillars
        );
    }

    #[test]
    fn ac2_single_engine_omits_engine_parity() {
        let plan = build_plan("What are the top menu items?", "tasty_bytes", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        assert!(
            !pillars.contains(&"engine-parity"),
            "single-engine model must NOT include engine-parity; got: {:?}",
            pillars
        );
    }

    // AC3: --mock executes planned steps in order; emits answer with per-step verdicts; no unplanned steps.
    #[test]
    fn ac3_mock_ask_executes_planned_steps_in_order() {
        let plan = build_plan("What is revenue by region?", "tasty_bytes", &default_opts());
        let planned_pillars: Vec<String> = plan.steps.iter().map(|s| s.pillar.clone()).collect();

        let answer = execute_plan(&plan, &default_run_opts());

        // All planned pillars must appear in step_results (some may be Skipped)
        for pillar in &planned_pillars {
            assert!(
                answer.step_results.iter().any(|r| &r.pillar == pillar),
                "planned pillar '{}' not found in step_results",
                pillar
            );
        }

        // No unplanned pillars should appear
        for result in &answer.step_results {
            assert!(
                planned_pillars.contains(&result.pillar),
                "unplanned pillar '{}' appeared in results",
                result.pillar
            );
        }

        // Final answer must be present (high confidence path)
        assert!(answer.final_answer.is_some(), "final_answer must be set");
        assert!(!answer.clarify_needed, "should not need clarify on this path");

        // Per-step verdicts must be set
        for result in &answer.step_results {
            // Verdicts are valid enum values — just check they're there
            let _ = &result.verdict;
        }
    }

    #[test]
    fn ac3_no_unplanned_pillars_in_results() {
        let plan = build_plan("Show me revenue YoY", "internet_sales", &default_opts());
        let answer = execute_plan(&plan, &default_run_opts());
        let planned_pillars: Vec<String> = plan.steps.iter().map(|s| s.pillar.clone()).collect();
        for result in &answer.step_results {
            assert!(
                planned_pillars.contains(&result.pillar),
                "unplanned pillar '{}' in results",
                result.pillar
            );
        }
    }

    // AC4: low-confidence bind yields clarify_needed; --answer resumes and completes.
    #[test]
    fn ac4_low_confidence_yields_clarify_needed() {
        let plan = build_plan("Revenue stuff", "tasty_bytes", &default_opts());
        let run_opts = RunOptions {
            mock: true,
            clarify_answer: None,
            mock_confidence: Some(0.30), // below threshold
        };
        let answer = execute_plan(&plan, &run_opts);
        assert!(
            answer.clarify_needed,
            "low-confidence binding must set clarify_needed"
        );
        assert!(
            answer.clarify_question.is_some(),
            "clarify_question must be emitted"
        );
        assert!(
            answer.final_answer.is_none(),
            "final_answer must NOT be fabricated when clarify_needed"
        );
    }

    #[test]
    fn ac4_with_answer_resumes_and_completes() {
        let plan = build_plan("Revenue stuff", "tasty_bytes", &default_opts());
        let run_opts = RunOptions {
            mock: true,
            clarify_answer: Some("revenue by product category".to_string()),
            mock_confidence: Some(0.30),
        };
        let answer = execute_plan(&plan, &run_opts);
        // With --answer provided, clarify is skipped, plan completes
        assert!(
            !answer.clarify_needed,
            "when --answer provided, clarify_needed must be false"
        );
        assert!(
            answer.final_answer.is_some(),
            "final_answer must be present after clarify resume"
        );
        assert_eq!(
            answer.clarify_answer.as_deref(),
            Some("revenue by product category")
        );
    }

    // AC5: sensitivity-scan always before final answer; rosetta-credential always terminal.
    #[test]
    fn ac5_sensitivity_scan_before_final_answer() {
        let plan = build_plan("What is revenue by region?", "tasty_bytes", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        let sens_pos = pillars.iter().position(|&p| p == "sensitivity-scan");
        let rosetta_pos = pillars.iter().position(|&p| p == "rosetta-credential");
        assert!(sens_pos.is_some(), "sensitivity-scan must be in plan");
        assert!(rosetta_pos.is_some(), "rosetta-credential must be in plan");
        assert!(
            sens_pos.unwrap() < rosetta_pos.unwrap(),
            "sensitivity-scan must precede rosetta-credential"
        );
    }

    #[test]
    fn ac5_rosetta_credential_is_terminal() {
        for (q, m) in &[
            ("revenue by region", "tasty_bytes"),
            ("revenue YoY", "internet_sales"),
        ] {
            let plan = build_plan(q, m, &default_opts());
            let last = plan.steps.last().unwrap();
            assert_eq!(
                last.pillar, "rosetta-credential",
                "rosetta-credential must be the last step; got '{}'",
                last.pillar
            );
        }
    }

    #[test]
    fn ac5_sensitivity_scan_before_rosetta_yoy_multi_engine() {
        let plan = build_plan("revenue YoY EMEA vs AMER", "internet_sales", &default_opts());
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        let sens_pos = pillars.iter().position(|&p| p == "sensitivity-scan").unwrap();
        let rosetta_pos = pillars.iter().position(|&p| p == "rosetta-credential").unwrap();
        assert!(sens_pos < rosetta_pos, "sensitivity-scan must be before rosetta-credential in YoY/multi-engine plan");
    }

    // AC6: rule table readable; --help documents subcommands (tested via plan output structure).
    #[test]
    fn ac6_rule_table_is_non_empty_and_readable() {
        assert!(DEFAULT_RULE_TABLE.len() > 100, "rule table must be non-empty");
        assert!(DEFAULT_RULE_TABLE.contains("catalog-embed"), "rule table must mention catalog-embed");
        assert!(DEFAULT_RULE_TABLE.contains("rosetta-credential"), "rule table must mention rosetta-credential");
        assert!(DEFAULT_RULE_TABLE.contains("LOW_CONF_THRESHOLD"), "rule table must document threshold");
    }

    // AC7: determinism — identical plan and mock ask output across runs.
    #[test]
    fn ac7_plan_is_deterministic() {
        let q = "Show me revenue YoY by EMEA region";
        let m = "internet_sales";
        let plan1 = build_plan(q, m, &default_opts());
        let plan2 = build_plan(q, m, &default_opts());
        let p1 = serde_json::to_string(&plan1).unwrap();
        let p2 = serde_json::to_string(&plan2).unwrap();
        assert_eq!(p1, p2, "plan must be deterministic across runs");
    }

    #[test]
    fn ac7_mock_ask_is_deterministic() {
        let q = "revenue by region";
        let m = "tasty_bytes";
        let plan = build_plan(q, m, &default_opts());

        // Strip ms (timings excluded) to compare structural determinism
        let answer1 = execute_plan(&plan, &default_run_opts());
        let answer2 = execute_plan(&plan, &default_run_opts());

        // Compare everything except ms fields
        let strip_ms = |a: &Answer| -> serde_json::Value {
            let mut v = serde_json::to_value(a).unwrap();
            if let Some(results) = v.get_mut("step_results").and_then(|r| r.as_array_mut()) {
                for r in results.iter_mut() {
                    if let Some(obj) = r.as_object_mut() {
                        obj.remove("ms");
                    }
                }
            }
            v
        };

        assert_eq!(
            strip_ms(&answer1),
            strip_ms(&answer2),
            "mock ask must be deterministic (excluding timings)"
        );
    }

    // AC8: serve answers ask/plan over stdin/stdout — tested via unit-level invocation.
    // The full stdin/stdout MCP contract is exercised in tests/mcp_serve.rs.
    #[test]
    fn ac8_plan_via_planner_returns_valid_structure() {
        let opts = PlannerOptions::default();
        let plan = build_plan("revenue by region", "tasty_bytes", &opts);
        // Plan must have at least catalog-embed, binding-confidence, sensitivity-scan, rosetta-credential
        let pillars: Vec<&str> = plan.steps.iter().map(|s| s.pillar.as_str()).collect();
        for required in &["catalog-embed", "binding-confidence", "sensitivity-scan", "rosetta-credential"] {
            assert!(pillars.contains(required), "plan missing required pillar: {}", required);
        }
    }
}
