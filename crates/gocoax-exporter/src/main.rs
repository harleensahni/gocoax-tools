use axum::{routing::get, Router};
use clap::Parser;
use gocoax_exporter::scrape::{scrape, AppState};
use std::sync::Arc;

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.config)?;
    let state = Arc::new(AppState::from_config_text(&text)?);
    let listen = state.listen().to_string();

    let app = Router::new().route(
        "/metrics",
        get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move { scrape(state).await }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    println!("gocoax-exporter listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}
