//! bullpen CLI — the composition root.
//!
//! This is the only place that knows the full product: it wires the provider,
//! tool registry, store, and agent loop together. Core crates never reach up
//! into here.

use std::sync::Arc;

use anyhow::{Context, bail};
use bullpen_agent::{Agent, AgentConfig, Event};
use bullpen_llm::anthropic::{Anthropic, DEFAULT_MODEL};
use bullpen_store::Store;
use bullpen_tools::{Registry, ToolCtx};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bullpen", version, about = "A durable agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt headlessly and print the final answer.
    Run {
        /// The task for the agent.
        prompt: String,
        /// Model id to use.
        #[arg(long, default_value = DEFAULT_MODEL)]
        model: String,
        /// Resume an existing session by id (or unique id prefix).
        #[arg(short, long)]
        resume: Option<String>,
        /// Show tool activity on stderr while running.
        #[arg(short, long)]
        verbose: bool,
    },
    /// List stored sessions.
    Sessions,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Run {
            prompt,
            model,
            resume,
            verbose,
        } => run(prompt, model, resume, verbose).await,
        Command::Sessions => sessions(),
    }
}

async fn run(
    prompt: String,
    model: String,
    resume: Option<String>,
    verbose: bool,
) -> anyhow::Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY is not set; export it to talk to the model")?;
    let cwd = std::env::current_dir()?;

    let mut store = Store::open(&Store::default_path())?;
    let (session, transcript, usage, model) = match &resume {
        Some(prefix) => {
            let session = store.resolve_session(prefix)?;
            let transcript = store.load_transcript(&session.id)?;
            let usage = session.usage;
            let model = session.model.clone();
            (session, transcript, usage, model)
        }
        None => {
            let session = store.create_session(&cwd.display().to_string(), &model)?;
            (session, Vec::new(), Default::default(), model)
        }
    };

    let provider = Arc::new(Anthropic::new(api_key));
    let tool_ctx = ToolCtx {
        workspace: cwd.clone(),
    };
    let config = AgentConfig {
        model,
        system: system_prompt(&cwd),
        ..Default::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if !verbose {
                continue;
            }
            match event {
                Event::AssistantText { text } => eprintln!("· {text}"),
                Event::ToolStart { name, input, .. } => eprintln!("→ {name} {input}"),
                Event::ToolEnd { name, is_error, .. } if is_error => {
                    eprintln!("✗ {name} failed")
                }
                Event::ToolEnd { .. } | Event::TurnDone { .. } => {}
            }
        }
    });

    let mut agent = Agent::new(provider, Registry::standard(), tool_ctx, config)
        .with_transcript(transcript, usage)
        .with_events(tx);

    let result = agent.send(&prompt).await;

    // Persist whatever happened — including a transcript cut short by an
    // error — so the session is always resumable.
    store.save_transcript(&session.id, agent.messages(), agent.usage())?;
    let _ = printer.await;

    match result {
        Ok(answer) => {
            println!("{answer}");
            eprintln!(
                "\n[session {} · {} in / {} out tokens]",
                &session.id[..8],
                agent.usage().input_tokens,
                agent.usage().output_tokens
            );
            Ok(())
        }
        Err(e) => bail!("agent error (session {} saved): {e}", &session.id[..8]),
    }
}

fn sessions() -> anyhow::Result<()> {
    let store = Store::open(&Store::default_path())?;
    let sessions = store.list_sessions()?;
    if sessions.is_empty() {
        println!("no sessions yet — start one with `bullpen run \"...\"`");
        return Ok(());
    }
    for s in sessions {
        println!(
            "{}  {}  {:>6}/{:<6}  {}",
            &s.id[..8],
            s.updated_at,
            s.usage.input_tokens,
            s.usage.output_tokens,
            if s.title.is_empty() { "(untitled)" } else { &s.title },
        );
    }
    Ok(())
}

fn system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are bullpen, a coding agent operating in a repository.\n\
         Working directory: {}\n\n\
         Use the available tools to inspect and change the workspace. Prefer \
         reading files before editing them. Report what you actually did — \
         if a command fails, say so with its output rather than working \
         around it silently.",
        cwd.display()
    )
}
