use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mqo_agent::{
    build_plan, execute_plan, serve_mcp, PlannerOptions, PlannerVariant, RunOptions,
    DEFAULT_RULE_TABLE,
};

/// Which planner variant to use.
#[derive(Debug, Clone, ValueEnum)]
enum PlannerArg {
    /// Deterministic rule-based planner (default, used in CI).
    Deterministic,
    /// Consult local wm-brain for plan ordering (opt-in, requires brain daemon).
    Brain,
}

#[derive(Parser)]
#[command(
    name = "mqo-agent",
    about = "Adaptive reference agent that plans which MQO pillars to call for any NL question",
    long_about = "Given a natural-language question, mqo-agent builds a deterministic plan of \
                  which pillar tools to invoke (catalog-embed, binding-confidence, clarify, \
                  time-intelligence, engine-parity, sensitivity-scan, rosetta-credential) and \
                  runs them in order, emitting answer.json.\n\n\
                  Use --mock to run without any sibling binaries installed (safe for CI).\n\n\
                  The planner rule table is bundled and readable via the `rules` subcommand.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ask a natural-language question and get a grounded answer.
    ///
    /// Builds a plan, invokes each pillar step in order, and emits answer.json.
    /// Use --mock for offline/CI testing (no sibling binaries needed).
    Ask {
        /// The natural-language question to answer.
        question: String,

        /// The semantic model to query against.
        #[arg(long, default_value = "__default__")]
        model: String,

        /// Use canned mock responses instead of real subprocess calls.
        #[arg(long)]
        mock: bool,

        /// Planner variant to use.
        #[arg(long, value_enum, default_value = "deterministic")]
        planner: PlannerArg,

        /// Disambiguation answer for a previous clarify_needed response.
        ///
        /// When a previous ask returned clarify_needed, re-run with the same
        /// question and --answer <your-answer> to resume deterministically.
        #[arg(long)]
        answer: Option<String>,

        /// Override mock binding-confidence score (for testing; range 0.0–1.0).
        #[arg(long, hide = true)]
        mock_confidence: Option<f64>,

        /// Write answer JSON to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// Print the plan (selected pillars + reasons) without executing.
    ///
    /// Shows which pillars would be invoked and why, given the question
    /// and model, so planning decisions are auditable before any query fires.
    Plan {
        /// The natural-language question to plan for.
        question: String,

        /// The semantic model to plan against.
        #[arg(long, default_value = "__default__")]
        model: String,

        /// Planner variant to use.
        #[arg(long, value_enum, default_value = "deterministic")]
        planner: PlannerArg,

        /// Emit JSON instead of human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Print the planner rule table.
    ///
    /// Shows the full documented rule table that drives all planning decisions.
    Rules,

    /// Run as an MCP subprocess server over stdin/stdout.
    ///
    /// Reads newline-delimited JSON tool calls; supports `ask` and `plan` tools:
    ///   {"tool": "ask", "question": "<q>", "model": "<m>", "mock": false}
    ///   {"tool": "plan", "question": "<q>", "model": "<m>"}
    Serve,
}

fn make_planner_opts(planner: &PlannerArg) -> PlannerOptions {
    PlannerOptions {
        planner: match planner {
            PlannerArg::Deterministic => PlannerVariant::Deterministic,
            PlannerArg::Brain => PlannerVariant::Brain,
        },
        model_fixtures: mqo_agent::bundled_model_fixtures(),
    }
}

fn main() -> Result<()> {
    sigpipe::reset();

    let cli = Cli::parse();

    match cli.command {
        Commands::Ask {
            question,
            model,
            mock,
            planner,
            answer,
            mock_confidence,
            out,
        } => {
            let opts = make_planner_opts(&planner);
            let plan = build_plan(&question, &model, &opts);
            let run_opts = RunOptions {
                mock,
                clarify_answer: answer,
                mock_confidence,
            };
            let ans = execute_plan(&plan, &run_opts);
            let content = serde_json::to_string_pretty(&ans).context("serialize answer")?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &content)
                        .with_context(|| format!("write answer to {}", path.display()))?;
                    eprintln!("answer written to {}", path.display());
                }
                None => println!("{}", content),
            }
            if ans.clarify_needed {
                eprintln!(
                    "clarify_needed: {}",
                    ans.clarify_question
                        .as_deref()
                        .unwrap_or("(no question emitted)")
                );
                eprintln!("Re-run with: mqo-agent ask \"{}\" --model {} --answer \"<your answer>\"", question, model);
                if !mock {
                    std::process::exit(2);
                }
            }
        }

        Commands::Plan {
            question,
            model,
            planner,
            json,
        } => {
            let opts = make_planner_opts(&planner);
            let plan = build_plan(&question, &model, &opts);
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                println!("Plan for: {}", plan.question);
                println!("Model:    {}", plan.model);
                println!("Planner:  {}", plan.planner);
                println!();
                for (i, step) in plan.steps.iter().enumerate() {
                    println!("  {:2}. {:20}  {}", i + 1, step.pillar, step.tool);
                    println!("      Reason: {}", step.reason);
                    println!();
                }
            }
        }

        Commands::Rules => {
            println!("{}", DEFAULT_RULE_TABLE);
        }

        Commands::Serve => {
            serve_mcp()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use mqo_agent::{build_plan, execute_plan, PlannerOptions, RunOptions};

    /// Smoke: plan subcommand logic works end-to-end.
    #[test]
    fn main_plan_returns_non_empty_steps() {
        let opts = PlannerOptions::default();
        let plan = build_plan("total revenue by region", "__default__", &opts);
        assert!(!plan.steps.is_empty(), "plan must have steps");
    }

    /// Smoke: ask mock runs without error.
    #[test]
    fn main_ask_mock_does_not_panic() {
        let opts = PlannerOptions::default();
        let plan = build_plan("revenue by region", "tasty_bytes", &opts);
        let run_opts = RunOptions {
            mock: true,
            clarify_answer: None,
            mock_confidence: None,
        };
        let answer = execute_plan(&plan, &run_opts);
        let _ = serde_json::to_string_pretty(&answer).unwrap();
    }
}
