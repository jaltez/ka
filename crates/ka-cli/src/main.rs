//! The `ka` binary. Phase 0 surface: `ka run` (headless NDJSON event stream)
//! and `ka config {schema,print}`. The TUI arrives in Phase 3.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use ka_agent::config::{Config, ConfigError};
use ka_protocol::{Command, Event, Stop, to_line};

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

async fn dispatch(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Some(CliCommand::Run {
            prompt,
            model,
            mode,
            configs,
        }) => run_headless(prompt, model, mode, &configs).await,
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
) -> Result<ExitCode, String> {
    let cfg = load_config(configs, model, mode)?;
    let prompt = match prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_string());
    }

    let mut handle = ka_agent::spawn(cfg);
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
                    .map_err(fmt_config_err)?;
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

fn fmt_config_err(e: ConfigError) -> String {
    e.to_string()
}
