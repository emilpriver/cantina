use clap::{Parser, Subcommand};

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

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Install { name } => {
            println!("Installing task: {}", name);
        }
    }
}

