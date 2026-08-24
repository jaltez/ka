//! The `ka` binary. Phase 1 surface: `ka run` (headless NDJSON against real
//! models), `ka models` (catalog + local discovery), `ka config
// {schema,print}`. The TUI arrives in Phase 3.

use clap::{CommandFactory, Parser, Subcommand};
use ka_agent::config::Config;
use ka_agent::spawn_with;
use ka_dialect::Catalog;
use ka_protocol::{Command, Event, Stop, to_line};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "ka",
    version,
    about = "ka — model-agnostic, low-footprint coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand, Clone)]
enum CliCommand {
    /// Stream one headless turn as NDJSON events
    Run {
        /// Prompt text (omitted: read from stdin)
        prompt: Option<String>,
        /// Model selector override (vendor/model:effort)
        #[arg(long)]
        model: Option<String>,
        /// Permission mode override (guarded|free)
        #[arg(long)]
        mode: Option<String>,
        /// Extra strict-TOML config layer (repeatable, highest file wins)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
        /// Extra dialect catalog overlay (strict TOML, repeatable)
        #[arg(long = "dialects")]
        dialects: Vec<PathBuf>,
        /// Skip local-endpoint discovery probes
        #[arg(long)]
        no_discovery: bool,
    },
    /// List known models (embedded catalog + local discovery)
    Models {
        /// Skip local discovery probes
        #[arg(long)]
        no_discovery: bool,
        /// Extra dialect catalog overlay (strict TOML, repeatable)
        #[arg(long = "dialects")]
        dialects: Vec<PathBuf>,
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },
}

#[derive(Subcommand, Clone)]
enum ConfigCommand {
    /// Print the JSON schema for ka.toml layers
    Schema,
    /// Print the resolved configuration
    Print {
        /// Extra strict-TOML config layer (repeatable)
        #[arg(long = "config")]
        configs: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("ka: failed to start runtime: {e}");
            std::process::exit(1);
        });
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ka: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Defaults < user < project < extra files < env < flags, every layer strict.
fn load_config(
    configs: &[PathBuf],
    flag_model: Option<String>,
    flag_mode: Option<String>,
) -> Result<Config, String> {
    let mut cfg = Config::default();

    let user = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/ka/ka.toml"));
    let project = PathBuf::from(".ka/ka.toml");

    for path in user
        .iter()
        .chain(std::iter::once(&project))
        .chain(configs.iter())
    {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let layer = Config::parse_layer(&text, &path.display().to_string())
                    .map_err(|e| e.to_string())?;
                cfg.overlay(layer);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }

    if let Ok(model) = std::env::var("KA_MODEL") {
        if !model.is_empty() {
            cfg.model = Some(model);
        }
    }
    if let Ok(mode) = std::env::var("KA_MODE") {
        if !mode.is_empty() {
            cfg.mode = Some(parse_mode(&mode)?);
        }
    }
    if let Some(model) = flag_model {
        cfg.model = Some(model);
    }
    if let Some(mode) = flag_mode {
        cfg.mode = Some(parse_mode(&mode)?);
    }
    Ok(cfg)
}

fn parse_mode(s: &str) -> Result<ka_protocol::Mode, String> {
    match s {
        "guarded" => Ok(ka_protocol::Mode::Guarded),
        "free" => Ok(ka_protocol::Mode::Free),
        other => Err(format!("unknown mode {other:?} (expected guarded|free)")),
    }
}

fn wire_str(w: ka_dialect::Wire) -> String {
    match w {
        ka_dialect::Wire::OpenaiChat => "openai_chat".to_string(),
        ka_dialect::Wire::AnthropicMessages => "anthropic_messages".to_string(),
    }
}

fn load_catalog(overlays: &[PathBuf]) -> Result<Catalog, String> {
    let mut catalog = Catalog::embedded();
    for path in overlays {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let over = Catalog::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        catalog.overlay(over);
    }
    Ok(catalog)
}

async fn build_catalog(overlays: &[PathBuf], with_discovery: bool) -> Result<Catalog, String> {
    let mut catalog = load_catalog(overlays)?;
    if with_discovery {
        ka_dialect::discovery::overlay_discovered(&mut catalog).await;
    }
    Ok(catalog)
}

async fn dispatch(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Some(CliCommand::Run {
            prompt,
            model,
            mode,
            configs,
            dialects,
            no_discovery,
        }) => run_headless(prompt, model, mode, &configs, &dialects, !no_discovery).await,
        Some(CliCommand::Models {
            no_discovery,
            dialects,
        }) => {
            let mut catalog = load_catalog(&dialects)?;
            if !no_discovery {
                ka_dialect::discovery::overlay_discovered(&mut catalog).await;
            }
            let header = format!(
                "{:<34} {:<20} {:>9} {:>7}  {}",
                "model", "wire", "context", "$in/M", "auth"
            );
            println!("{header}");
            for (id, d) in &catalog.dialects {
                println!(
                    "{:<34} {:<20} {:>9} {:>7}  {}",
                    id,
                    wire_str(d.wire),
                    d.context,
                    d.price.input_per_mtok,
                    d.api_key_env.as_deref().unwrap_or("-")
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(CliCommand::Config { cmd }) => match cmd {
            ConfigCommand::Schema => {
                println!(
                    "{}",
                    Config::schema_json().map_err(|e| format!("schema: {e}"))?
                );
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Print { configs } => {
                let cfg = load_config(&configs, None, None)?;
                let text =
                    toml::to_string_pretty(&cfg).map_err(|e| format!("serialize config: {e}"))?;
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
        },
        None => {
            Cli::command()
                .print_help()
                .map_err(|e| format!("help: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_headless(
    prompt: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    configs: &[PathBuf],
    dialects: &[PathBuf],
    with_discovery: bool,
) -> Result<ExitCode, String> {
    let cfg = load_config(configs, model, mode)?;
    let prompt = match prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_string());
    }

    let catalog = build_catalog(dialects, with_discovery).await?;
    let mut handle = spawn_with(cfg, catalog);
    handle
        .commands
        .send(Command::Prompt {
            text: prompt,
            attachments: vec![],
        })
        .await
        .map_err(|_| "engine closed before prompt".to_string())?;

    let mut stdout = std::io::stdout().lock();
    let mut final_stop: Option<Stop> = None;
    while let Some(event) = handle.events.recv().await {
        let line = to_line(&event).map_err(|e| format!("serialize event: {e}"))?;
        std::io::Write::write_all(&mut stdout, line.as_bytes())
            .map_err(|e| format!("stdout: {e}"))?;
        if let Event::TurnFinished { stop, .. } = &event {
            final_stop = Some(*stop);
            break;
        }
    }
    std::io::Write::flush(&mut stdout).map_err(|e| format!("stdout: {e}"))?;
    match final_stop {
        Some(Stop::Aborted) => Ok(ExitCode::from(2)),
        Some(Stop::Error) => Ok(ExitCode::from(1)),
        _ => Ok(ExitCode::SUCCESS),
    }
}

fn read_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin: {e}"))?;
    Ok(buf)
}
