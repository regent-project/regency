//! regencyctl (D15): the swiss-army knife of regency's distributed
//! architecture. For now: `init`. Further commands (admin through backend,
//! direct RabbitMQ/CouchDB operations, per-service interaction) are not
//! designed yet.

mod config;
mod init;
mod permissions;

use clap::{Parser, Subcommand};
use tracing::error;

#[derive(Parser)]
#[command(name = "regencyctl", version, about = "regency admin CLI")]
struct Args {
    #[arg(short, long, default_value_t = regencylib::config::default_path("regencyctl"))]
    config: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create (idempotently) the RabbitMQ vhost, users, permissions and
    /// topology, and the CouchDB databases, users, settings and bootstrap admin.
    Init {
        #[arg(long)]
        skip_rabbitmq: bool,
        #[arg(long)]
        skip_couchdb: bool,
    },
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config: config::Config = match regencylib::config::load(&args.config) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("regencyctl: {e}");
            std::process::exit(2);
        }
    };
    regencylib::logging::init(&config.log, "regencyctl", env!("CARGO_PKG_VERSION"), "cli");
    regencylib::security::enforce_at_startup(&config.security, [init::check(&config)]);
    let result = match args.command {
        Commands::Init {
            skip_rabbitmq,
            skip_couchdb,
        } => {
            async {
                let init = init::Init {
                    config: &config,
                    secrets: config.secrets.pool()?,
                };
                if !skip_couchdb {
                    init.couchdb().await?;
                }
                if !skip_rabbitmq {
                    init.rabbitmq().await?;
                }
                anyhow::Ok(())
            }
            .await
        }
    };
    if let Err(e) = result {
        error!(error = %format!("{e:#}"), "regencyctl failed");
        std::process::exit(1);
    }
}
