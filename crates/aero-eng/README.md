# aero-eng — Aero Engineering CLI Framework

Trait-based command framework for building engineering CLI tools in Rust. Inspired by snaplink's `cli.py`, adapted to Rust's type system.

## Quick Start

```rust
use aero_eng::{Command, CommandRegistry, ExecutionContext, Outcome};
use async_trait::async_trait;

struct HelloCmd;
#[async_trait]
impl Command for HelloCmd {
    fn name(&self) -> &'static str { "hello" }
    fn description(&self) -> &'static str { "Say hello" }
    async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
        Outcome::ok("Hello, world!")
    }
}

#[tokio::main]
async fn main() {
    let reg = CommandRegistry::new().add(Box::new(HelloCmd));
    reg.execute("hello", &["hello".into()]).await.unwrap();
}
```

## Modules

| Module | Description | Tests |
|---|---|---|
| `Command` trait | `name()`, `description()`, `execute()` | — |
| `CommandRegistry` | Command registration and dispatch | 1 |
| `Outcome` | Structured result with severity + merge | 17 |
| `ExecutionContext` | Project root + config | — |
| `checks` | Native Rust engineering gates | 10 |
| `config` | `engineering.toml` loader | 2 |
| `run` | Async external process runner | 2 |
| `term` | Colored output + spinners | 7 |

## Outcome

```rust
let o = Outcome::ok("all passed");           // exit_code = 0
let o = Outcome::error("something broke");   // exit_code = 1
let o = Outcome::skip("not applicable");     // exit_code = 2
let o = Outcome::warning(3, "caution");      // exit_code = 3

// Merge multiple outcomes (worst severity wins)
let merged = Outcome::merge(&[ok, err, skip]);
assert!(merged.is_error());

// Add timing
let o = Outcome::ok("done").with_duration(duration);

// Add machine-readable detail
let o = Outcome::ok("done").with_detail(json!({"key": "value"}));
```

## Checks

```rust
use aero_eng::checks::{check_filesize, check_deps};
use aero_eng::config::EngineeringConfig;

let cfg = EngineeringConfig::load(&root);
let result = check_filesize(&root, &cfg);
println!("{} files checked", result.detail()["checked"]);

let deps = check_deps(&root);
println!("{} violations", deps.detail()["violations"]);
```

## Binary

Built as two binaries:

- `aero-eng` (14 MB) — lightweight, only aero-eng + tokio deps
- `aero-cli` (100 MB) — full, includes aero-server + DB deps

## Features

- No Python runtime required — single Rust binary
- Parallel execution via `tokio::join!`
- Structured error reporting (not exit code sums)
- Colored terminal output (ANSI, zero deps)
- Shell completion (bash/zsh/fish)
- Config-driven via `engineering.toml`
