//! upsidedown — hosted relay for `blit uplink` (docs/upsidedown.md).

mod certs;
mod config;
mod control;
mod jwt;
mod store;
mod worker;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "upsidedown", version, about = "hosted relay for blit uplink")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane (HTTPS API + ACME)
    ControlPlane,
    /// Run a relay worker (one external TCP+UDP port)
    Worker {
        /// Worker name used for store registration
        #[arg(long)]
        name: String,
        /// External port this worker owns
        #[arg(long)]
        port: u16,
    },
    /// Run control plane + workers in one process (development)
    Dev {
        /// Number of workers (ports 4441…)
        #[arg(long, default_value_t = 2)]
        workers: u16,
    },
    /// Generate an Ed25519 signing keypair (base64url)
    Keygen,
    /// Mint a token (development / operations tool; keys stay elsewhere in production)
    Mint {
        /// base64url Ed25519 secret key (from `upsidedown keygen`)
        #[arg(long)]
        secret_key: String,
        /// blit session ID (`sid` claim)
        #[arg(long)]
        sid: String,
        /// Token role: "server" or "client"
        #[arg(long)]
        role: String,
        /// Lifetime in seconds (docs recommend ~1 year for server tokens)
        #[arg(long, default_value_t = 3600)]
        ttl_secs: u64,
    },
}

async fn run_dev(workers: u16) -> Result<(), String> {
    let env = config::load().await?;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..workers {
        let env = env.clone();
        let (name, port) = (format!("w{}", i + 1), 4441 + i);
        tasks.spawn(async move { worker::run_with(env, name, port).await });
    }
    let cp_env = env.clone();
    tasks.spawn(async move { control::run_with(cp_env).await });
    while let Some(result) = tasks.join_next().await {
        result.map_err(|e| format!("task panicked: {e}"))??;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::ControlPlane => control::run().await,
        Command::Worker { name, port } => worker::run(name, port).await,
        Command::Dev { workers } => run_dev(workers).await,
        Command::Keygen => {
            jwt::keygen();
            Ok(())
        }
        Command::Mint {
            secret_key,
            sid,
            role,
            ttl_secs,
        } => jwt::mint_cli(&secret_key, &sid, &role, ttl_secs),
    };
    if let Err(e) = result {
        eprintln!("upsidedown: {e}");
        std::process::exit(1);
    }
}
