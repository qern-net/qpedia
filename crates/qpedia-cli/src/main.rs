use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "qpedia", about = "Qpedia admin CLI")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show runtime status
    Status,
    /// Run the linter once and print the report
    Lint,
    /// Re-embed all wiki pages
    Reembed,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Status  => println!("qpedia: not yet wired"),
        Cmd::Lint    => println!("lint: not yet wired"),
        Cmd::Reembed => println!("reembed: not yet wired"),
    }
    Ok(())
}
