//! `modulex` — the human CLI over the routine engine.
//!
//! One-shot by design: load config, resolve the leash, run, print, exit.
//! The MCP server binary (`modulex-mcp`) is the long-lived surface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use modulex_core::{steps::builtin_registry, Config, Engine, GrantedCaveats, RunOptions};

#[derive(Parser)]
#[command(
    name = "modulex",
    version,
    about = "Deterministic, pluggable routine engine"
)]
struct Cli {
    /// Config path (overrides the $MODULEX_CONFIG / ./modulex.toml /
    /// ~/.modulex/config.toml search order).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a routine and print its report.
    Run {
        /// Routine name.
        routine: String,
        /// Run only these step names (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Skip these step names (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Describe what would run without side effects.
        #[arg(long)]
        dry_run: bool,
        /// Emit the report as compact JSON instead of markdown.
        #[arg(long)]
        json: bool,
    },
    /// Run a single step of a routine (debugging aid).
    Step {
        /// Routine name.
        routine: String,
        /// Step name within the routine.
        step: String,
        /// Describe instead of running.
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// List configured routines.
    List,
    /// List registered step types.
    Steps,
    /// Show config location, leash provenance, and tool availability.
    Doctor,
    /// Manage reminders in the agent state store.
    Remind {
        #[command(subcommand)]
        action: RemindAction,
    },
    /// Agent state store utilities.
    Store {
        #[command(subcommand)]
        action: StoreAction,
    },
}

#[derive(Subcommand)]
enum RemindAction {
    /// Register a reminder ("remind me of X").
    Add {
        /// The reminder text.
        text: String,
        /// Optional ISO due date (YYYY-MM-DD).
        #[arg(long)]
        due: Option<String>,
        /// Optional recurrence: daily | weekly | monthly.
        #[arg(long)]
        recur: Option<String>,
    },
    /// List open reminders.
    List,
    /// Mark a reminder done by id.
    Done {
        /// Reminder id (from `remind list`).
        id: i64,
    },
}

#[derive(Subcommand)]
enum StoreAction {
    /// Export the whole store as plain JSON (sovereignty).
    Export,
    /// Import a previous export (appends).
    Import {
        /// Path to a JSON export.
        file: PathBuf,
    },
}

fn load(config_path: Option<&PathBuf>) -> anyhow::Result<(Engine, PathBuf, String)> {
    let (config, path) = match config_path {
        Some(path) => (Config::from_path(path)?, path.clone()),
        None => Config::load()?,
    };
    let registry = builtin_registry();
    let declared = config.declared_programs(&registry);
    let granted = GrantedCaveats::load(config.caveats.as_ref(), declared)?;
    let banner = granted.banner();
    Ok((Engine::new(config, registry, granted.caveats), path, banner))
}

fn print_report(report: &modulex_core::Report, json: bool) {
    if json {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.to_text());
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(success) => {
            if success {
                ExitCode::SUCCESS
            } else {
                // Steps failed inside the report: exit 2 so scripts can
                // distinguish "ran with failures" from engine errors (1).
                ExitCode::from(2)
            }
        }
        Err(e) => {
            eprintln!("modulex: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<bool> {
    let (engine, config_path, banner) = load(cli.config.as_ref())?;

    match cli.command {
        Command::Run {
            routine,
            only,
            skip,
            dry_run,
            json,
        } => {
            eprintln!("{banner}");
            let report = engine
                .run_routine(
                    &routine,
                    RunOptions {
                        only,
                        skip,
                        dry_run,
                    },
                )
                .await?;
            print_report(&report, json);
            Ok(report.success)
        }
        Command::Step {
            routine,
            step,
            dry_run,
            json,
        } => {
            eprintln!("{banner}");
            let report = engine.run_step(&routine, &step, dry_run).await?;
            print_report(&report, json);
            Ok(report.success)
        }
        Command::List => {
            for (name, description, steps) in engine.list_routines() {
                if description.is_empty() {
                    println!("{name}  ({steps} steps)");
                } else {
                    println!("{name}  ({steps} steps) — {description}");
                }
            }
            Ok(true)
        }
        Command::Steps => {
            for name in engine.step_types() {
                println!("{name}");
            }
            Ok(true)
        }
        Command::Doctor => {
            println!("config: {}", config_path.display());
            println!("{banner}");
            let registry = builtin_registry();
            let declared = engine.config().declared_programs(&registry);
            if declared.is_empty() {
                println!("declared programs: (none — only pure steps configured)");
            } else {
                println!("declared programs:");
                for program in declared {
                    let status = if modulex_core::exec::program_available(&program) {
                        "ok"
                    } else {
                        "MISSING (its steps will soft-skip)"
                    };
                    println!("  {program}: {status}");
                }
            }
            println!("routines: {}", engine.list_routines().len());
            match engine.store() {
                Some(_) => println!("agent state store: ok"),
                None => println!("agent state store: UNAVAILABLE"),
            }
            Ok(true)
        }
        Command::Remind { action } => {
            let Some(store) = engine.store() else {
                anyhow::bail!("agent state store unavailable");
            };
            let generation = engine.current_generation();
            match action {
                RemindAction::Add { text, due, recur } => {
                    let id =
                        store.reminder_add(&text, due.as_deref(), recur.as_deref(), generation)?;
                    println!("reminder #{id} registered (after gen {generation})");
                }
                RemindAction::List => {
                    let reminders = store.reminders_open()?;
                    if reminders.is_empty() {
                        println!("(no open reminders)");
                    }
                    for r in reminders {
                        let due = r.due.map(|d| format!("  due {d}")).unwrap_or_default();
                        let recur = r
                            .recurrence
                            .map(|recurrence| format!("  [{recurrence}]"))
                            .unwrap_or_default();
                        println!("#{}  {}{due}{recur}", r.id, r.text);
                    }
                }
                RemindAction::Done { id } => {
                    if store.reminder_done(id, generation)? {
                        println!("reminder #{id} done");
                    } else {
                        anyhow::bail!("no open reminder #{id}");
                    }
                }
            }
            Ok(true)
        }
        Command::Store { action } => {
            let Some(store) = engine.store() else {
                anyhow::bail!("agent state store unavailable");
            };
            match action {
                StoreAction::Export => println!("{}", store.export_json()?),
                StoreAction::Import { file } => {
                    let json = std::fs::read_to_string(&file)?;
                    store.import_json(&json)?;
                    println!("imported {}", file.display());
                }
            }
            Ok(true)
        }
    }
}
