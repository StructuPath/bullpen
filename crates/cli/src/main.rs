//! bullpen CLI — the composition root.
//!
//! This is the only place that knows the full product: it wires providers,
//! credentials, the tool registry, the store, and the agent loop together.
//! Core crates never reach up into here.

use std::sync::Arc;

use anyhow::{Context, bail};
use bullpen_agent::{Agent, AgentConfig, Event};
use bullpen_auth::codex::{CodexAuth, CodexCliBorrow, StoredCodex};
use bullpen_auth::{AuthFile, Credential, openrouter, pkce::Pkce};
use bullpen_llm::Provider;
use bullpen_llm::anthropic::{Anthropic, DEFAULT_MODEL as ANTHROPIC_DEFAULT_MODEL};
use bullpen_llm::chatcompletions::{ChatCompletions, OPENROUTER_DEFAULT_MODEL};
use bullpen_llm::codex::{Codex, DEFAULT_CODEX_MODEL};
use bullpen_store::Store;
use bullpen_tools::{Registry, ToolCtx};
use clap::{Parser, Subcommand, ValueEnum};

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
        /// Which provider to use for a new session. Resumed sessions keep
        /// the provider and model they were created with.
        #[arg(short, long, value_enum, default_value_t = ProviderKind::Anthropic)]
        provider: ProviderKind,
        /// Model id; defaults to the provider's default model.
        #[arg(short, long)]
        model: Option<String>,
        /// Resume an existing session by id (or unique id prefix).
        #[arg(short, long)]
        resume: Option<String>,
        /// Show tool activity on stderr while running.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Connect a provider account (stores credentials in ~/.bullpen).
    Login {
        #[arg(value_enum)]
        provider: LoginProvider,
        /// Print the URL and paste the code manually instead of running a
        /// localhost callback (for SSH sessions etc.). OpenRouter only.
        #[arg(long)]
        headless: bool,
    },
    /// List stored sessions.
    Sessions,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ProviderKind {
    Anthropic,
    Openrouter,
    Codex,
}

impl ProviderKind {
    fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openrouter => "openrouter",
            Self::Codex => "codex",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" => Some(Self::Anthropic),
            "openrouter" => Some(Self::Openrouter),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    fn default_model(self) -> String {
        match self {
            Self::Anthropic => ANTHROPIC_DEFAULT_MODEL.to_string(),
            Self::Openrouter => OPENROUTER_DEFAULT_MODEL.to_string(),
            // Prefer the model the user's Codex CLI is configured for — when
            // borrowing its credentials, its model choice is the one known to
            // work with that account.
            Self::Codex => {
                codex_cli_configured_model().unwrap_or_else(|| DEFAULT_CODEX_MODEL.to_string())
            }
        }
    }
}

/// The `model = "..."` value from `~/.codex/config.toml`, if present.
fn codex_cli_configured_model() -> Option<String> {
    let path = std::env::home_dir()?.join(".codex").join("config.toml");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("model") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

#[derive(Copy, Clone, ValueEnum)]
enum LoginProvider {
    Openrouter,
    Codex,
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
            provider,
            model,
            resume,
            verbose,
        } => run(prompt, provider, model, resume, verbose).await,
        Command::Login { provider, headless } => match provider {
            LoginProvider::Openrouter => login_openrouter(headless).await,
            LoginProvider::Codex => login_codex().await,
        },
        Command::Sessions => sessions(),
    }
}

// ── Provider construction ───────────────────────────────────────────────────

fn build_provider(kind: ProviderKind) -> anyhow::Result<Arc<dyn Provider>> {
    let auth_path = AuthFile::default_path();
    match kind {
        ProviderKind::Anthropic => {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .context("ANTHROPIC_API_KEY is not set; export it to use the anthropic provider")?;
            Ok(Arc::new(Anthropic::new(api_key)))
        }
        ProviderKind::Openrouter => {
            let key = match std::env::var("OPENROUTER_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => match AuthFile::load(&auth_path)?.get(openrouter::PROVIDER) {
                    Some(Credential::ApiKey { key }) => key.clone(),
                    _ => bail!(
                        "no OpenRouter credentials — run `bullpen login openrouter` \
                         or export OPENROUTER_API_KEY"
                    ),
                },
            };
            Ok(Arc::new(ChatCompletions::openrouter(key)))
        }
        ProviderKind::Codex => {
            let stored = AuthFile::load(&auth_path)?
                .get(bullpen_auth::codex::PROVIDER)
                .is_some();
            if stored {
                return Ok(Arc::new(Codex::new(Arc::new(StoredCodex::new(auth_path)))));
            }
            let borrow = CodexCliBorrow::new(CodexCliBorrow::default_path());
            if borrow.available() {
                eprintln!("[using Codex CLI credentials from ~/.codex/auth.json]");
                return Ok(Arc::new(Codex::new(Arc::new(borrow))));
            }
            bail!(
                "no Codex credentials — run `bullpen login codex`, or log in once \
                 with the Codex CLI so bullpen can borrow its session"
            );
        }
    }
}

// ── Login flows ─────────────────────────────────────────────────────────────

async fn login_openrouter(headless: bool) -> anyhow::Result<()> {
    let pkce = Pkce::generate();
    let http = reqwest::Client::new();

    let code = if headless {
        let url = openrouter::authorize_url(&pkce.challenge, None);
        println!("Open this URL, approve access, and paste the code shown:\n\n  {url}\n");
        eprint!("Code: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let code = line.trim().to_string();
        if code.is_empty() {
            bail!("no code entered");
        }
        code
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let callback = format!("http://localhost:{port}/callback");
        let url = openrouter::authorize_url(&pkce.challenge, Some(&callback));

        println!("Opening your browser to authorize bullpen with OpenRouter…");
        println!("If nothing opens, visit:\n\n  {url}\n");
        openrouter::open_browser(&url);

        tokio::time::timeout(
            std::time::Duration::from_secs(300),
            openrouter::capture_code(listener),
        )
        .await
        .context("timed out waiting for the browser authorization (5 minutes)")??
    };

    let key = openrouter::exchange(&http, None, &code, &pkce.verifier).await?;
    let mut auth = AuthFile::load(&AuthFile::default_path())?;
    auth.set(openrouter::PROVIDER, Credential::ApiKey { key })?;
    println!("OpenRouter connected. Try: bullpen run -p openrouter \"hello\"");
    Ok(())
}

async fn login_codex() -> anyhow::Result<()> {
    let credential = CodexAuth::default()
        .login(|prompt| {
            println!("To authorize bullpen with your ChatGPT account, visit:\n");
            println!("  {}\n", prompt.verification_url);
            println!("and enter code: {}\n", prompt.user_code);
            openrouter::open_browser(&prompt.verification_url);
            println!("Waiting for approval…");
        })
        .await?;
    let mut auth = AuthFile::load(&AuthFile::default_path())?;
    auth.set(bullpen_auth::codex::PROVIDER, credential)?;
    println!("Codex connected. Try: bullpen run -p codex \"hello\"");
    Ok(())
}

// ── Run ─────────────────────────────────────────────────────────────────────

async fn run(
    prompt: String,
    provider_kind: ProviderKind,
    model: Option<String>,
    resume: Option<String>,
    verbose: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let mut store = Store::open(&Store::default_path())?;

    let (session, provider_kind, model) = match &resume {
        Some(prefix) => {
            let session = store.resolve_session(prefix)?;
            let kind = ProviderKind::from_name(&session.provider).with_context(|| {
                format!("session uses unknown provider `{}`", session.provider)
            })?;
            let model = session.model.clone();
            (session, kind, model)
        }
        None => {
            let model = model.unwrap_or_else(|| provider_kind.default_model());
            let session = store.create_session(
                &cwd.display().to_string(),
                provider_kind.name(),
                &model,
            )?;
            (session, provider_kind, model)
        }
    };

    // Recover any run a previous process left open, then rebuild the
    // transcript from the durable entry tree.
    let (transcript, recovery) = bullpen_harness::prepare_session(&mut store, &session.id)?;
    if let Some(r) = &recovery {
        eprintln!(
            "[recovered interrupted run {}: {} tool call(s) marked interrupted]",
            &r.run_id[..8],
            r.interrupted_tools
        );
    }
    let usage = store.get_session(&session.id)?.usage;
    drop(store);

    let provider = build_provider(provider_kind)?;
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

    let tool_ctx = ToolCtx {
        workspace: cwd.clone(),
    };
    // The journal persists every step of the run as it happens (its own
    // store handle; WAL makes the two connections safe). If the process
    // dies mid-run, the next invocation recovers from the durable state.
    let journal = bullpen_harness::StoreJournal::new(
        Store::open(&Store::default_path())?,
        &session.id,
    );
    let mut agent = Agent::new(provider, Registry::standard(), tool_ctx, config)
        .with_transcript(transcript, usage)
        .with_events(tx)
        .with_journal(Box::new(journal));

    let result = agent.send(&prompt).await;
    let usage = agent.usage();
    // The agent owns the event sender; it must drop before the printer's
    // receive loop can end, or this await never returns.
    drop(agent);
    let _ = printer.await;

    match result {
        Ok(answer) => {
            println!("{answer}");
            eprintln!(
                "\n[session {} · {} · {} in / {} out tokens]",
                &session.id[..8],
                provider_kind.name(),
                usage.input_tokens,
                usage.output_tokens
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
            "{}  {}  {:<10}  {:>6}/{:<6}  {}",
            &s.id[..8],
            s.updated_at,
            s.provider,
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
