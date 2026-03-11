mod app;
mod cli;
mod error;
mod github;

use clap::Parser as _;
use error_stack::Report;

use crate::{app::App, cli::Cli, error::AppError};

#[tokio::main]
async fn main() -> Result<(), Report<AppError>> {
    let cli = Cli::parse();

    if cli.save_default_orgs {
        if cli.org.is_empty() {
            return Err(Report::new(AppError::InvalidInput)
                .attach("--save-default-orgs requires at least one --org value to persist."));
        }

        App::update_default_orgs(cli.org.clone())?;
    }

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
