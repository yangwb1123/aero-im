//! Operational CLI: migrations, key generation, smoke checks.

use aero_common::{config::AppConfig, telemetry};
use aero_storage::{connect_pg, migrate};
use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    let cfg = AppConfig::load().context("load config")?;
    let _g = telemetry::init(&cfg.telemetry, "aero-cli");

    match cmd {
        "migrate" => {
            let pg = connect_pg(&cfg.database.url, 4).await?;
            migrate(&pg).await?;
            println!("migrations applied");
        }
        "health" => {
            let pg = connect_pg(&cfg.database.url, 1).await?;
            let row: (i32,) = sqlx::query_as("SELECT 1").fetch_one(&pg).await?;
            println!("postgres ok: {}", row.0);
        }
        _ => {
            eprintln!("usage: aero-cli <migrate|health>");
            std::process::exit(2);
        }
    }
    Ok(())
}
