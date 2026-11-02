//! `stalwart-operator`: gives every `MailSender` and `ClusterMailSender` a
//! Stalwart account and writes its SMTP credentials into Secrets.

mod cluster;
mod config;
mod crd;
mod reconcile;
mod stalwart;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controller.
    Run {
        /// The operator's config (Stalwart URL, SMTP host, namespace policy).
        #[arg(long, env = "OPERATOR_CONFIG_PATH", default_value = "operator.toml")]
        config: PathBuf,
        /// A Stalwart administrator able to manage accounts and to log in as them
        /// with `<address>%<user>`.
        #[arg(long, env = "STALWART_USER")]
        user: String,
        #[arg(long, env = "STALWART_PASSWORD", hide_env_values = true)]
        password: String,
    },
    /// Print the `MailSender` and `ClusterMailSender` CustomResourceDefinitions as
    /// YAML.
    Crd,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Args::parse().command {
        Command::Crd => match crd::crds_yaml() {
            Ok(yaml) => {
                print!("{yaml}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Command::Run {
            config,
            user,
            password,
        } => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();

            let config = match config::Config::load(&config) {
                Ok(config) => config,
                Err(error) => {
                    tracing::error!(%error, "loading the config");
                    return ExitCode::FAILURE;
                }
            };
            let stalwart =
                match stalwart::Stalwart::new(config.stalwart.url.clone(), user, password) {
                    Ok(stalwart) => stalwart,
                    Err(error) => {
                        tracing::error!(%error, "building the Stalwart client");
                        return ExitCode::FAILURE;
                    }
                };
            let kube = match kube::Client::try_default().await {
                Ok(kube) => kube,
                Err(error) => {
                    tracing::error!(%error, "connecting to Kubernetes");
                    return ExitCode::FAILURE;
                }
            };
            reconcile::run(kube, config, stalwart).await;
            ExitCode::SUCCESS
        }
    }
}
