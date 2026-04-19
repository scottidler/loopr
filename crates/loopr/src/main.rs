use clap::Parser;

use loopr::Cli;

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    loopr::run(cli)?;
    Ok(())
}
