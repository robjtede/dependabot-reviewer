mod app;
mod cli;
mod error;
mod github;

use clap::Parser as _;
use error_stack::Report;

use crate::app::App;
use crate::cli::Cli;
use crate::error::AppError;

#[tokio::main]
async fn main() -> Result<(), Report<AppError>> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
