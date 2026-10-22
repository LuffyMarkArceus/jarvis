use clap::{Parser, Subcommand};
use jarvis_core::{init_tracing, orchestrator::Orchestrator};
use tracing::info;

#[derive(Parser)]
#[command(name = "jarvis")]
#[command(about = "Jarvis - Personal AI Assistant", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ask Jarvis something
    Ask { prompt: String },
    /// Start the daemon
    Daemon,
    /// Show status
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let orch = Orchestrator::new();

    match cli.command {
        Commands::Ask { prompt } => {
            info!("Asking: {}", prompt);
            let response = orch.run(&prompt).await?;
            println!("{}", response);
        }
        Commands::Daemon => {
            info!("Starting Jarvis daemon...");
            println!("Daemon mode not yet implemented");
        }
        Commands::Status => {
            println!("Jarvis CLI v0.1.0 - Core under construction");
            println!("Ollama: http://127.0.0.1:11434");
        }
    }

    Ok(())
}
