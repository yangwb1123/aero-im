//! Operational CLI: migrations, key generation, smoke checks, AI test, stream
//! roster, WS keep-alive ping.

use aero_ai::{default_embedder, default_transcriber, AnthropicClient};
use aero_common::{config::AppConfig, telemetry};
use aero_storage::{connect_pg, migrate, StreamRepo};
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
            println!("✓ migrations applied");
        }
        "health" => {
            let pg = connect_pg(&cfg.database.url, 1).await?;
            let row: (i32,) = sqlx::query_as("SELECT 1").fetch_one(&pg).await?;
            println!("✓ postgres ok: {}", row.0);
            print!("redis: ");
            match aero_storage::RedisCache::connect(&cfg.redis.url).await {
                Ok(_) => println!("ok"),
                Err(e) => println!("FAIL — {e}"),
            }
            print!("nats : ");
            match async_nats::connect(&cfg.nats.url).await {
                Ok(_) => println!("ok"),
                Err(e) => println!("FAIL — {e}"),
            }
        }
        "ai-test" => {
            println!("▶ embedder");
            let emb = default_embedder();
            let v = emb.embed_one("hello world").await?;
            println!("  dim={} sample={:.4} {:.4} {:.4}", v.len(), v[0], v[1], v[2]);

            println!("▶ transcriber");
            let t = default_transcriber();
            println!("  using: {}", t.name());

            println!("▶ anthropic");
            match AnthropicClient::from_env() {
                Some(c) => match c.complete("speak in one short phrase", &[aero_ai::ChatMsg::user("Say hi")], 32).await {
                    Ok(reply) => println!("  ok: {}", reply.trim()),
                    Err(e) => println!("  reachable but call failed: {e}"),
                },
                None => println!("  not configured (set ANTHROPIC_API_KEY)"),
            }
        }
        "streams" => {
            let pg = connect_pg(&cfg.database.url, 2).await?;
            let repo = StreamRepo::new(pg);
            let live = repo.list_live().await?;
            if live.is_empty() {
                println!("(no live streams)");
            } else {
                for s in live {
                    println!(
                        "{}  {:?}  {}  hls={}  started_at={:?}",
                        s.id,
                        s.protocol,
                        s.title,
                        s.hls_path.as_deref().unwrap_or("-"),
                        s.started_at
                    );
                }
            }
        }
        "ws-ping" => {
            // Requires AERO_TOKEN env (a valid JWT). Connects to the server and round-trips one ping.
            let token = std::env::var("AERO_TOKEN")
                .map_err(|_| anyhow::anyhow!("set AERO_TOKEN to a JWT obtained via /api/auth/login"))?;
            let host = std::env::var("AERO_HOST").unwrap_or_else(|_| "ws://localhost:3030".into());
            let url = format!("{host}/ws?token={token}");
            println!("▶ connecting {url}");
            let (mut ws, _) = tokio_tungstenite::connect_async(url).await?;
            use futures::{SinkExt, StreamExt};
            let welcome = match ws.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t,
                other => anyhow::bail!("unexpected first frame: {other:?}"),
            };
            println!("  welcome: {welcome}");
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"type":"ping"}"#.into(),
            ))
            .await?;
            let pong = match ws.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t,
                other => anyhow::bail!("expected pong: {other:?}"),
            };
            println!("  pong:    {pong}");
            ws.close(None).await?;
            println!("✓ ws round-trip ok");
        }
        "help" | "--help" | "-h" => print_help(),
        _ => {
            eprintln!("unknown command: {cmd}");
            print_help();
            std::process::exit(2);
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        r#"aero-cli — operational utilities

Usage: aero-cli <command>

Commands:
  migrate      Apply pending DB migrations
  health       Probe Postgres, Redis, NATS reachability
  ai-test      Exercise the embedder, transcriber, and Anthropic round-trip
  streams      List currently live streams
  ws-ping      Round-trip a WS ping against AERO_HOST using AERO_TOKEN
  help         This message
"#
    );
}
