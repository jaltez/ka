//! The `ka` binary. Phase 1 surface: `ka run` (headless NDJSON against real
//! models), `ka models` (catalog + local discovery), `ka config
// {schema,print}`. The TUI arrives in Phase 3.

use clap::{CommandFactory, Parser, Subcommand};
use ka_agent::config::Config;
use ka_agent::spawn_full;
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

    /// Continue the newest strand (with the terminal's waypoint preferred)
    #[arg(short = 'c', long)]
    continue_latest: bool,
    /// Open a specific strand file
    #[arg(long)]
    session: Option<std::path::PathBuf>,
    /// Model selector override (vendor/model@effort)
    #[arg(long)]
    model: Option<String>,
    /// Permission mode override (guarded|free)
    #[arg(long)]
    mode: Option<String>,
    /// Extra strict-TOML config layer (repeatable)
    #[arg(long = "config")]
    configs: Vec<std::path::PathBuf>,
    /// Extra dialect catalog overlay (strict TOML, repeatable)
    #[arg(long = "dialects")]
    dialects: Vec<std::path::PathBuf>,
    /// Skip local-endpoint discovery probes
    #[arg(long)]
    no_discovery: bool,
    /// Trust this directory's .ka/ka.toml (stores the decision)
    #[arg(long)]
    trust: bool,
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
        /// Continue the newest strand for this directory
        #[arg(short = 'c', long)]
        continue_latest: bool,
        /// Trust this directory's .ka/ka.toml (stores the decision)
        #[arg(long)]
        trust: bool,
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

/// Defaults < user < project (if trusted) < extra files < env < flags.
fn load_config(
    configs: &[PathBuf],
    flag_model: Option<String>,
    flag_mode: Option<String>,
    trust_project: bool,
) -> Result<Config, String> {
    let mut cfg = Config::default();

    let user = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/ka/ka.toml"));
    let project = if trust_project {
        Some(PathBuf::from(".ka/ka.toml"))
    } else {
        None
    };

    for path in user.iter().chain(project.iter()).chain(configs.iter()) {
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

fn trust_for_cwd(force: bool) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    project_config_trusted(&cwd, force)
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
    if cli.command.is_none() {
        return run_tui(cli).await;
    }
    let cli = Cli {
        command: cli.command,
        continue_latest: cli.continue_latest,
        session: cli.session,
        model: cli.model,
        mode: cli.mode,
        configs: cli.configs,
        dialects: cli.dialects,
        no_discovery: cli.no_discovery,
        trust: cli.trust,
    };
    match cli.command {
        Some(CliCommand::Run {
            prompt,
            model,
            mode,
            configs,
            dialects,
            no_discovery,
            continue_latest,
            trust,
        }) => {
            run_headless(
                prompt,
                model,
                mode,
                &configs,
                &dialects,
                !no_discovery,
                continue_latest,
                trust,
            )
            .await
        }
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
                let trust = trust_for_cwd(false);
                let cfg = load_config(&configs, None, None, trust)?;
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

#[allow(clippy::too_many_arguments)]
async fn run_headless(
    prompt: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    configs: &[PathBuf],
    dialects: &[PathBuf],
    with_discovery: bool,
    continue_latest: bool,
    force_trust: bool,
) -> Result<ExitCode, String> {
    let trust = trust_for_cwd(force_trust);
    let cfg = load_config(configs, model, mode, trust)?;
    let prompt = match prompt {
        Some(p) => p,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err("empty prompt".to_string());
    }

    let catalog = build_catalog(dialects, with_discovery).await?;
    let choice = if continue_latest {
        ka_agent::StrandChoice::Latest
    } else {
        ka_agent::StrandChoice::New
    };
    let mut handle = spawn_full(cfg, catalog, choice);
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
        // headless policy: permission asks auto-deny (last option = deny)
        if let Event::Ask { id, .. } = &event {
            handle
                .commands
                .send(Command::Answer {
                    question: id.clone(),
                    choice: 2,
                })
                .await
                .map_err(|_| "engine closed during ask")?;
        }
        match &event {
            Event::TurnFinished { stop, .. } => final_stop = Some(*stop),
            Event::Idle => break,
            _ => {}
        }
    }
    std::io::Write::flush(&mut stdout).map_err(|e| format!("stdout: {e}"))?;
    match final_stop {
        Some(Stop::Aborted) => Ok(ExitCode::from(2)),
        Some(Stop::Error) => Ok(ExitCode::from(1)),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// The interactive surface: picker (unless -c/--session) then the TUI.
async fn run_tui(cli: Cli) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let choice = if let Some(path) = cli.session.clone() {
        ka_agent::StrandChoice::Path(path)
    } else if cli.continue_latest {
        // waypoint first, else newest
        match ka_agent::read_waypoint() {
            Some((way_cwd, path)) if way_cwd == cwd && path.exists() => {
                ka_agent::StrandChoice::Path(path)
            }
            _ => ka_agent::StrandChoice::Latest,
        }
    } else {
        pick_strand(&cwd)?
    };

    let trust = trust_for_cwd(cli.trust);
    let cfg = load_config(&cli.configs, cli.model.clone(), cli.mode.clone(), trust)?;
    let catalog = build_catalog(&cli.dialects, !cli.no_discovery).await?;
    let model_label = cfg.model.clone().unwrap_or_else(|| "(canned)".to_string());
    let handle = ka_agent::spawn_full(cfg, catalog, choice);
    let ka_agent::EngineHandle { commands, events } = handle;
    let exit = ka_term::tui::run(commands, events, &model_label)
        .await
        .map_err(|e| format!("tui: {e}"))?;
    match exit {
        ka_term::tui::Exit::Quit | ka_term::tui::Exit::EngineEnded => Ok(ExitCode::SUCCESS),
    }
}

/// Text session picker: newest strands for this cwd + new-session default.
fn pick_strand(cwd: &std::path::Path) -> Result<ka_agent::StrandChoice, String> {
    use std::io::Write;

    let strands = ka_strand::list(cwd).map_err(|e| format!("listing strands: {e}"))?;
    if strands.is_empty() {
        return Ok(ka_agent::StrandChoice::New);
    }
    println!("recent strands for {}:", cwd.display());
    println!("  0) new session");
    for (i, s) in strands.iter().take(9).enumerate() {
        println!("  {}) [{}] {} ({} msgs)", i + 1, s.ts, s.title, s.messages);
    }
    print!("choice [0]: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {e}"))?;
    let n: usize = line.trim().parse().unwrap_or(0);
    if n == 0 || n > strands.len() {
        Ok(ka_agent::StrandChoice::New)
    } else {
        Ok(ka_agent::StrandChoice::Path(strands[n - 1].path.clone()))
    }
}

/// Trust store: directories whose `.ka/` local config ka will load.
fn trust_path() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("ka/trust.json")
}

fn load_trust() -> Vec<PathBuf> {
    std::fs::read_to_string(trust_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_trust(dirs: &[PathBuf]) {
    let path = trust_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(dirs) {
        let _ = std::fs::write(path, json);
    }
}

/// Whether the project config layer for `cwd` may load. Prompts on a TTY
/// (first sighting), skips with a warning otherwise. `--trust` forces.
fn project_config_trusted(cwd: &std::path::Path, force_trust: bool) -> bool {
    let project = cwd.join(".ka/ka.toml");
    if !project.exists() {
        return true; // nothing to gate
    }
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut trusted = load_trust();
    if trusted.contains(&canonical) {
        return true;
    }
    if force_trust {
        trusted.push(canonical);
        save_trust(&trusted);
        return true;
    }
    // prompt only when interactive
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "ka: this directory has a .ka/ka.toml project config.\n     {}\n   Trust it (loads its rules/model settings)? [y/N]",
            canonical.display()
        );
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let ans = line.trim().to_lowercase();
            if ans == "y" || ans == "yes" {
                trusted.push(canonical);
                save_trust(&trusted);
                return true;
            }
        }
    }
    eprintln!("ka: project config NOT trusted; skipping .ka/ka.toml (pass --trust to trust it)");
    false
}

fn read_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin: {e}"))?;
    Ok(buf)
}
