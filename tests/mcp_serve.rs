//! AC8: serve answers `ask` and `plan` tool calls over stdin/stdout.
//! Full mock test runs with no network and no sibling binaries.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run mqo-agent serve with an input line and capture one output line.
fn serve_one(input: &str) -> serde_json::Value {
    let binary = env!("CARGO_BIN_EXE_mqo-agent");
    let mut child = Command::new(binary)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mqo-agent serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", input).expect("write");
    }

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("{}");
    serde_json::from_str(line).unwrap_or(serde_json::json!({"error": "parse", "raw": line}))
}

#[test]
fn ac8_serve_plan_tool_returns_steps() {
    let input = serde_json::json!({
        "tool": "plan",
        "question": "Show me revenue YoY by region",
        "model": "internet_sales"
    })
    .to_string();

    let resp = serve_one(&input);
    let result = &resp["result"];

    // Must return a plan with steps
    let steps = result["steps"].as_array().expect("steps must be array");
    assert!(!steps.is_empty(), "plan must have steps");

    let pillars: Vec<&str> = steps
        .iter()
        .filter_map(|s| s["pillar"].as_str())
        .collect();
    assert!(
        pillars.contains(&"time-intelligence"),
        "YoY question must include time-intelligence in plan; got {:?}",
        pillars
    );
    assert!(
        pillars.contains(&"engine-parity"),
        "multi-engine model must include engine-parity; got {:?}",
        pillars
    );
}

#[test]
fn ac8_serve_ask_tool_mock_returns_answer() {
    let input = serde_json::json!({
        "tool": "ask",
        "question": "What are total sales by region?",
        "model": "tasty_bytes",
        "mock": true
    })
    .to_string();

    let resp = serve_one(&input);
    let result = &resp["result"];

    assert!(
        result["final_answer"].is_string(),
        "serve ask must return final_answer; got: {:?}",
        result
    );
    assert_eq!(
        result["clarify_needed"].as_bool().unwrap_or(true),
        false,
        "clarify_needed must be false for high-confidence mock"
    );
}

#[test]
fn ac8_serve_unknown_tool_returns_error() {
    let input = serde_json::json!({
        "tool": "frobnicate",
        "question": "test"
    })
    .to_string();

    let resp = serve_one(&input);
    let result = &resp["result"];
    assert!(
        result["error"].is_string(),
        "unknown tool must return error field"
    );
}
