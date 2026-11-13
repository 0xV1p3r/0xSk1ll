use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use sk1ll_daemon::{Config, JoinOptions};

/// The 0xSk1ll daemon. `sk1ll daemon` runs the same thing.
#[derive(Parser)]
#[command(name = "sk1ll-daemon", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground (the default)
    Run,
    /// Join a fleet (same as `sk1ll join`)
    Join {
        server_url: String,
        code: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        copy_from: Option<String>,
        /// Local clone (SK1LL_REPO, nearest skills repo, else ~/skills)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env()?;
    // SAFETY: single-threaded here, before the runtime starts. Git (run by
    // core and the daemon) must fail instead of prompting, and gets the
    // machine's credentials from the environment.
    unsafe { sk1ll_daemon::prepare_process(&cfg)? };
    sk1ll_daemon::init_logging();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => rt.block_on(sk1ll_daemon::run(cfg)),
        Cmd::Join { server_url, code, id, copy_from, repo } => {
            let repo = sk1ll_daemon::locate_repo(repo.as_deref(), false)?;
            let opts = JoinOptions { server_url, code, machine_id: id, copy_from, repo };
            let (joined, via_daemon) = rt.block_on(sk1ll_daemon::join_or_rejoin(&cfg, opts))?;
            println!("joined as {} with repo {}", joined.machine_id, joined.repo.display());
            if via_daemon {
                println!("the running daemon restarts with the new credentials");
            }
            Ok(())
        }
    }
}
