//! Lightweight aero-eng CLI — engineering commands only (no DB dependencies).
//! Build: cargo build -p aero-cli
//! Binary: ./target/debug/aero-eng
//!
//! All commands live in `aero-eng/src/commands/` (single source of truth);
//! this entry point only dispatches through the registry.

use aero_eng::{CommandRegistry, ExecutionContext};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("aero-eng v{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cmd = args.get(1).map_or("help", String::as_str);
    let ctx = ExecutionContext::new(std::env::current_dir().unwrap_or_default());
    let reg = CommandRegistry::collect();
    match reg.execute_with_ctx(cmd, &args, &ctx).await {
        Ok(r) => {
            if !r.outcome.message().is_empty() {
                println!("{}", r.outcome.message());
            }
            std::process::exit(0);
        }
        Err(r) => {
            if !r.outcome.message().is_empty() {
                eprintln!("{}", r.outcome.message());
            }
            std::process::exit(r.exit_code());
        }
    }
}
