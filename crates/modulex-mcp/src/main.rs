//! `modulex-mcp` — the stdio MCP server binary.
//!
//! ```text
//! modulex-mcp [--config PATH]          # serve MCP on stdio
//! modulex-mcp --probe [--config PATH]  # dry-run the first routine, print, exit
//! modulex-mcp --tools                  # print tool specs and exit
//! ```
//!
//! Register with an MCP client, e.g.:
//! `claude mcp add modulex -- modulex-mcp`

use std::path::PathBuf;

use clap::Parser;
use modulex_core::{steps::builtin_registry, Config, Engine, GrantedCaveats, RunOptions};
use modulex_mcp::Server;

#[derive(Parser)]
#[command(
    name = "modulex-mcp",
    version,
    about = "Stdio MCP server for modulex routines"
)]
struct Cli {
    /// Config path (overrides the $MODULEX_CONFIG / ./modulex.toml /
    /// ~/.modulex/config.toml search order).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Dry-run the first configured routine and print its report (sanity
    /// check), then exit.
    #[arg(long)]
    probe: bool,

    /// Print the MCP tool specs as JSON and exit.
    #[arg(long)]
    tools: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.tools {
        // The full registry view (every facet) — introspection, not the
        // budgeted index a client sees.
        let everything = modulex_mcp::FacetPolicy::resolve(
            Some("core,store,store-classic"),
            &modulex_core::config::McpConfig::default(),
        );
        println!("{}", modulex_mcp::tools::registry().specs_json(&everything));
        return Ok(());
    }

    let config = match &cli.config {
        Some(path) => Config::from_path(path)?,
        None => Config::load()?.0,
    };
    let registry = builtin_registry();
    let declared = config.declared_programs(&registry);
    let granted = GrantedCaveats::load(config.caveats.as_ref(), declared)?;
    // Provenance to stderr — stdout belongs to the protocol.
    eprintln!("{}", granted.banner());

    let server = Server::new(Engine::new(config, registry, granted.caveats));
    eprintln!("{}", server.policy().banner());

    if cli.probe {
        let Some((routine, _, _)) = server.engine().list_routines().into_iter().next() else {
            anyhow::bail!("no routines configured — nothing to probe");
        };
        let report = server
            .engine()
            .run_routine(
                &routine,
                RunOptions {
                    dry_run: true,
                    ..RunOptions::default()
                },
            )
            .await?;
        println!("{}", report.to_text());
        return Ok(());
    }

    server.run_stdio().await
}
