//! Aero Engineering CLI — 15 commands, trait-based dispatch.

use aero_eng::{Command, CommandRegistry, ExecutionContext, Outcome};
use async_trait::async_trait;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let ctx = ExecutionContext::new(std::env::current_dir().unwrap_or_default());
    let mut reg = CommandRegistry::new();
    macro_rules! a { ($c:expr) => { reg = reg.add(Box::new($c)); }; }
    a!(Migrate); a!(Health); a!(AiTest); a!(Streams); a!(WsPing);
    a!(Check_); a!(Smoke_); a!(Gate_); a!(Test_); a!(Skill_);
    a!(Doctor_); a!(Dev_); a!(Integration_); a!(Completion_);
    match reg.execute_with_ctx(cmd, &args, &ctx).await {
        Ok(r) => { if !r.outcome.message().is_empty() { println!("{}", r.outcome.message()); } std::process::exit(0); }
        Err(r) => { if !r.outcome.message().is_empty() { eprintln!("{}", r.outcome.message()); } std::process::exit(r.exit_code()); }
    }
}

macro_rules! c {
    ($s:ident, $n:expr, $d:expr, |$ctx:ident, $args:ident| $e:block) => {
        struct $s;
        #[async_trait] impl Command for $s {
            fn name(&self) -> &'static str { $n } fn description(&self) -> &'static str { $d }
            async fn execute(&self, $ctx: &ExecutionContext, $args: &[String]) -> Outcome $e
        }
    };
}

/// Load config with graceful error message instead of panic.
/// Returns None on failure with error already printed to stderr.
async fn load_cfg() -> Option<aero_common::config::AppConfig> {
    match aero_common::config::AppConfig::load() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("Error loading config.toml: {e}");
            eprintln!("Hint: copy config.example.toml to config.toml and edit it.");
            None
        }
    }
}

c!(Migrate, "migrate", "Apply pending DB migrations", |_ctx, _args| {
    let c = match load_cfg().await { Some(c) => c, None => return Outcome::error("config required") };
    let p = match aero_storage::connect_pg(&c.database.url, 4).await { Ok(p) => p, Err(e) => return Outcome::error(format!("db: {e}")) };
    match aero_storage::migrate(&p).await { Ok(_) => Outcome::ok("✓ ok"), Err(e) => Outcome::error(format!("{e}")) }
});
c!(Health, "health", "Probe PG/Redis/NATS", |_ctx, _args| {
    let c = match load_cfg().await { Some(c) => c, None => return Outcome::error("config required") }; let mut ok = true; let mut v = vec![];
    let pg = aero_storage::connect_pg(&c.database.url,1).await; let rd = aero_storage::RedisCache::connect(&c.redis.url).await; let nt = async_nats::connect(&c.nats.url).await;
    match pg { Ok(p) => match sqlx::query_as::<_,(i32,)>("SELECT 1").fetch_one(&p).await { Ok(_) => v.push("✓ pg".into()), Err(e) => { v.push(format!("✗ {e}")); ok=false; } }, Err(e) => { v.push(format!("✗ {e}")); ok=false; } }
    match rd { Ok(_) => v.push("✓ redis".into()), Err(e) => { v.push(format!("✗ {e}")); ok=false; } }
    match nt { Ok(_) => v.push("✓ nats".into()), Err(e) => { v.push(format!("✗ {e}")); ok=false; } }
    if ok { Outcome::ok(v.join("\n")) } else { Outcome::error(v.join("\n")) }
});
c!(AiTest, "ai-test", "Exercise AI backends", |_ctx, _args| {
    use aero_ai::{default_embedder, AnthropicClient, ChatMsg};
    match default_embedder().embed_one("hi").await { Ok(v) => println!("embedder: dim={}", v.len()), Err(e) => println!("embedder: {e}") }
    match AnthropicClient::from_env() { Some(c) => match c.complete("s",&[ChatMsg::user("Hi")],32).await { Ok(r) => println!("llm: {}", r.trim()), Err(e) => println!("llm: {e}") }, None => println!("no key") }
    Outcome::ok("done")
});
c!(Streams, "streams", "List live streams", |_ctx, _args| {
    let c = match load_cfg().await { Some(c) => c, None => return Outcome::error("config required") };
    let p = match aero_storage::connect_pg(&c.database.url,2).await { Ok(p) => p, Err(e) => return Outcome::error(format!("db: {e}")) };
    let list = aero_storage::StreamRepo::new(p).list_live().await;
    match list {
        Ok(l) => { for s in l { println!("{} {:?}", s.id, s.protocol); } Outcome::ok("") }
        Err(e) => Outcome::error(format!("{e}"))
    }
});
c!(Test_, "test", "Run cargo test", |_ctx, _args| { aero_eng::run::cargo_test_lib().await });
c!(WsPing, "ws-ping", "WS ping", |_ctx, _args| {
    let t = std::env::var("AERO_TOKEN").unwrap_or_default(); if t.is_empty() { return Outcome::error("set AERO_TOKEN"); }
    let h = std::env::var("AERO_HOST").unwrap_or_else(|_|"ws://localhost:3030".into());
    use futures::{SinkExt, StreamExt};
    let (mut ws, _) = match tokio_tungstenite::connect_async(&format!("{h}/ws?token={t}")).await { Ok(w) => w, Err(e) => return Outcome::error(format!("{e}")) };
    match ws.next().await { Some(Ok(tokio_tungstenite::tungstenite::Message::Text(m))) => println!("  {m}"), _ => return Outcome::error("bad") }
    let _ = ws.send(tokio_tungstenite::tungstenite::Message::Text(r#"{"type":"ping"}"#.into())).await;
    match ws.next().await { Some(Ok(tokio_tungstenite::tungstenite::Message::Text(m))) => println!("  pong: {m}"), _ => return Outcome::error("no") }
    let _ = ws.close(None).await; Outcome::ok("✓")
});
c!(Check_, "check", "Run cargo check + test + clippy", |_ctx, _args| {
    use aero_eng::{run::{cargo_check, cargo_clippy, cargo_test_lib}, term};
    let s = std::time::Instant::now(); let mut sp = term::Spinner::new("checks"); sp.tick();
    let (co, to, clo) = tokio::join!(cargo_check(), cargo_test_lib(), cargo_clippy());
    println!("\n{}", term::header("Results"));
    for (l,o) in [("cargo check",&co),("cargo test",&to),("cargo clippy",&clo)] { println!("  {:<20} {}", l, o.message()); }
    println!("  {} {}", term::header("Total"), term::fmt_duration(s.elapsed().as_secs_f64()));
    sp.done("done"); Outcome::merge(&[co, to, clo]).with_duration(s.elapsed())
});
c!(Smoke_, "smoke", "List/run smoke tests", |ctx, args| {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list"); let sd = ctx.root.join("scripts");
    let mut sm: Vec<String> = vec![];
    if let Ok(e) = std::fs::read_dir(&sd) { for en in e.flatten() { let n = en.file_name().to_string_lossy().to_string();
        if n.starts_with("smoke_") && (n.ends_with(".py")||n.ends_with(".sh")) { sm.push(n.strip_prefix("smoke_").unwrap_or(&n).strip_suffix(".py").or_else(|| n.strip_suffix(".sh")).unwrap_or(&n).to_owned()); } } }
    sm.sort();
    match sub {
        "list" => { for s in &sm { println!("  {s}"); } Outcome::ok("") }
        "run" => { let name = args.get(3).map(|s| s.as_str()).unwrap_or(""); if name.is_empty() { return Outcome::error("need name"); }
            let p = sd.join(format!("smoke_{name}.py")); let s = sd.join(format!("smoke_{name}.sh")); let sc = if p.exists() { p } else { s };
            if !sc.exists() { return Outcome::error("nf"); } let ps = sc.to_string_lossy().to_string();
            aero_eng::run::run_cmd("python3", &[&ps], std::time::Duration::from_secs(120)).await
        }
        _ => Outcome::error("use list/run")
    }
});
c!(Gate_, "gate", "Run gates (shell + native)", |ctx, args| {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list"); let sd = ctx.root.join("scripts");
    let file = |n: &str| -> String { sd.join(n).to_string_lossy().to_string() };
    async fn bash(p: String, t: u64) -> Outcome { aero_eng::run::run_cmd("bash", &[&p], std::time::Duration::from_secs(t)).await }
    match sub {
        "list" => Outcome::ok("filesize truth web deps complexity filesize-native deps-native workspace-members todos all"),
        "filesize" => bash(file("file-size-check.sh"),60).await, "truth" => bash(file("truth-check.sh"),60).await,
        "web" => bash(file("web-check.sh"),60).await, "deps" => bash(file("dependency-check.sh"),60).await,
        "complexity" => bash(file("complexity-check.sh"),60).await, "all" => bash(file("file-size-check.sh"),120).await,
        "filesize-native" => { let o = aero_eng::checks::check_filesize(&ctx.root, &ctx.eng_config); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(&d).unwrap()); } o }
        "deps-native" => { let o = aero_eng::checks::check_deps(&ctx.root); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(&d).unwrap()); } o }
        "workspace-members" => { let o = aero_eng::checks::check_workspace_members(&ctx.root); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(&d).unwrap()); } o }
        "todos" => { let o = aero_eng::checks::check_todos(&ctx.root); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(&d).unwrap()); } o }
        _ => Outcome::error("unknown")
    }
});
c!(Skill_, "skill", "List/view/run skills", |ctx, args| {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list"); let sk = ctx.root.join("skills");
    match sub {
        "list" => { if let Ok(e) = std::fs::read_dir(&sk) { for en in e.flatten() { let n = en.file_name().to_string_lossy().to_string(); if n.ends_with(".md") { println!("  {} [doc]", &n[..n.len()-3]); } else if en.path().is_dir() { println!("  {} [exec]", n); } } } Outcome::ok("") }
        "view" => { let n = args.get(3).map(|s| s.as_str()).unwrap_or(""); let p = sk.join(format!("{n}.md")); if p.exists() { println!("{}", std::fs::read_to_string(&p).unwrap_or_default()); Outcome::ok("") } else { Outcome::error("nf") } }
        "run" => { let n = args.get(3).map(|s| s.as_str()).unwrap_or(""); if n.is_empty() { return Outcome::error("need name"); }
            for (r,e) in [("bash","sh"),("python3","py")] { let p = sk.join(n).join(format!("run.{e}")); if p.exists() { let ps = p.to_string_lossy().to_string(); return aero_eng::run::run_cmd(r, &[&ps], std::time::Duration::from_secs(120)).await; } }
            Outcome::error("nf") }
        _ => Outcome::error("use list/view/run")
    }
});
c!(Doctor_, "doctor", "Environment diagnostics", |ctx, _args| {
    use aero_eng::term; let mut ok = true;
    println!("{}", term::header("🔍 Aero IM Doctor\n"));
    let r = std::process::Command::new("rustc").arg("--version").output().map(|o|o.status.success()).unwrap_or(false);
    println!("  {} Rust toolchain  {}", term::info(""), if r { term::ok("ok") } else { ok=false; term::err("miss") });
    println!("  {} Config file     {}", term::info(""), if aero_common::config::AppConfig::load().is_ok() { term::ok("valid") } else { ok=false; term::err("bad") });
    // Git check: verify it's actually a git repo
    let git_root = std::process::Command::new("git").args(["rev-parse","--git-dir"]).current_dir(&ctx.root).output();
    let is_git = git_root.as_ref().map(|o| o.status.success()).unwrap_or(false);
    if is_git {
        let d = std::process::Command::new("git").args(["status","--porcelain"]).current_dir(&ctx.root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().count()).unwrap_or(0);
        println!("  {} Git status      {}", term::info(""), if d==0 { term::ok("clean") } else { term::warn(&format!("{d} dirty")) });
    } else {
        println!("  {} Git status      {}", term::info(""), term::warn("not a git repo"));
    }
    let m = std::fs::read_dir(ctx.root.join("migrations"));
    println!("  {} Migrations      {}", term::info(""), match m { Ok(e) => term::ok(&format!("{} files", e.count())), Err(_) => { ok=false; term::err("not found") } });
    if ok { println!("\n{}", term::ok("All healthy")); Outcome::ok("healthy") } else { Outcome::error("issues") }
});
c!(Dev_, "dev", "Start dev environment", |ctx, args| {
    use aero_eng::term; let skip = args.contains(&"--skip-services".to_string());
    println!("{}", term::header("🚀 Aero Dev Environment\n")); let mut sp = term::Spinner::new("prereq"); sp.tick();
    let d = std::process::Command::new("docker").arg("--version").output().map(|o|o.status.success()).unwrap_or(false);
    let c = std::process::Command::new("cargo").arg("--version").output().map(|o|o.status.success()).unwrap_or(false);
    if !c { sp.fail("rust missing"); return Outcome::error("rustup"); }
    sp.done(&format!("rust ✓ docker {}", if d{"✓"}else{"✗"}));
    if !skip && d { let mut sp = term::Spinner::new("services"); sp.tick(); let _ = std::process::Command::new("docker").args(["compose","up","-d"]).current_dir(&ctx.root).output(); tokio::time::sleep(std::time::Duration::from_secs(3)).await; sp.done("up"); }
    let mut sp = term::Spinner::new("migrate"); sp.tick();
    let c = match load_cfg().await { Some(c) => c, None => return Outcome::error("config required") };
    let p = match aero_storage::connect_pg(&c.database.url,4).await { Ok(p) => p, Err(e) => return Outcome::error(format!("db: {e}")) };
    match aero_storage::migrate(&p).await { Ok(_) => sp.done("ok"), Err(e) => return Outcome::error(format!("{e}")) }
    println!("\n{}", term::ok("ready")); Outcome::ok("dev ready")
});
c!(Integration_, "integration", "Integration tests (needs PG)", |ctx, _args| {
    let p = ctx.root.join("scripts/test-integration.sh"); if !p.exists() { return Outcome::error("nf"); }
    let ps = p.to_string_lossy().to_string(); aero_eng::run::run_cmd("bash", &[&ps], std::time::Duration::from_secs(600)).await
});
c!(Completion_, "completion", "Shell completion bash|zsh|fish", |_ctx, args| {
    let cmds = "migrate health ai-test streams ws-ping check smoke gate test skill doctor completion dev integration help";
    match args.get(2).map(|s| s.as_str()).unwrap_or("") {
        "bash" => { println!("complete -W '{}' aero-cli", cmds); Outcome::ok("") }
        "zsh" => { println!("#compdef aero-cli"); Outcome::ok("") }
        "fish" => { println!("complete -c aero-cli -f -a '{}'", cmds); Outcome::ok("") }
        _ => Outcome::error("usage: completion bash|zsh|fish")
    }
});
