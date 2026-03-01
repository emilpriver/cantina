use clap::{Parser, Subcommand};
use tokio;

mod errors;
mod install;

#[derive(Parser)]
#[command(name = "cantina")]
#[command(about = "MacOS package manager", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Install { name: String },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Install { name } => install::install(name).await,
    };
}
