//! Lightweight aero-eng CLI — engineering commands only (no DB dependencies).
//! Build: cargo build -p aero-cli
//! Binary: ./target/debug/aero-eng

use aero_eng::{Command, CommandRegistry, ExecutionContext, Outcome};
use async_trait::async_trait;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("aero-eng v{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let ctx = ExecutionContext::new(std::env::current_dir().unwrap_or_default());
    let mut reg = CommandRegistry::new();
    macro_rules! a { ($c:expr) => { reg = reg.add(Box::new($c)); }; }
    a!(Check_); a!(Gate_); a!(Test_); a!(Skill_); a!(Doctor_); a!(Completion_); a!(Network_); a!(Bench_); a!(Dashboard_);
    match reg.execute_with_ctx(cmd, &args, &ctx).await {
        Ok(r) => { if !r.outcome.message().is_empty() { println!("{}", r.outcome.message()); } std::process::exit(0); }
        Err(r) => { if !r.outcome.message().is_empty() { eprintln!("{}", r.outcome.message()); } std::process::exit(r.exit_code()); }
    }
}

macro_rules! c {
    ($s:ident, $n:expr, $d:expr, |$ctx:ident, $args:ident| $e:block) => {
        struct $s; #[async_trait] impl Command for $s {
            fn name(&self) -> &'static str { $n } fn description(&self) -> &'static str { $d }
            async fn execute(&self, $ctx: &ExecutionContext, $args: &[String]) -> Outcome $e
        }
    };
}

c!(Check_, "check", "Run cargo check + test + clippy + native gates", |ctx, _args| {
    use aero_eng::{run::{cargo_check, cargo_clippy, cargo_test_lib}, checks::{check_filesize, check_deps, check_workspace_members, check_todos, check_crate_metadata}, term};
    let s = std::time::Instant::now(); let mut sp = term::Spinner::new("checks"); sp.tick();
    let (co, to, clo) = tokio::join!(cargo_check(), cargo_test_lib(), cargo_clippy());
    let fs = check_filesize(&ctx.root, &ctx.eng_config); let deps = check_deps(&ctx.root);
    let ws = check_workspace_members(&ctx.root); let td = check_todos(&ctx.root); let md = check_crate_metadata(&ctx.root);
    println!("\n{}", term::header("Results"));
    for (l,o) in [("cargo check",&co),("cargo test",&to),("cargo clippy",&clo),
        ("filesize",&fs),("deps",&deps),("workspace",&ws),("todos",&td),("metadata",&md)] { println!("  {:<20} {}", l, o.message()); }
    println!("  {} {}", term::header("Total"), term::fmt_duration(s.elapsed().as_secs_f64()));
    sp.done("done"); Outcome::merge(&[co, to, clo, fs, deps, ws, td, md]).with_duration(s.elapsed())
});
c!(Test_, "test", "Run cargo test (lib)", |_ctx, _args| { aero_eng::run::cargo_test_lib().await });
c!(Gate_, "gate", "Run engineering gates", |ctx, args| {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list"); let sd = ctx.root.join("scripts");
    let f = |n: &str| -> String { sd.join(n).to_string_lossy().to_string() };
    async fn b(p: String, t: u64) -> Outcome { aero_eng::run::run_cmd("bash", &[&p], std::time::Duration::from_secs(t)).await }
    match sub {
        "list" => Outcome::ok("filesize truth web deps complexity filesize-native deps-native workspace-members all"),
        "filesize" => b(f("file-size-check.sh"),60).await, "truth" => b(f("truth-check.sh"),60).await,
        "web" => b(f("web-check.sh"),60).await, "deps" => b(f("dependency-check.sh"),60).await,
        "complexity" => b(f("complexity-check.sh"),60).await, "all" => b(f("file-size-check.sh"),120).await,
        "filesize-native" => { let o = aero_eng::checks::check_filesize(&ctx.root, &ctx.eng_config); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(serde_json::to_string_pretty(&d).unwrap()d).unwrap_or_default()); } o }
        "deps-native" => { let o = aero_eng::checks::check_deps(&ctx.root); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(serde_json::to_string_pretty(&d).unwrap()d).unwrap_or_default()); } o }
        "workspace-members" => { let o = aero_eng::checks::check_workspace_members(&ctx.root); if let Some(d) = o.detail() { println!("{}", serde_json::to_string_pretty(serde_json::to_string_pretty(&d).unwrap()d).unwrap_or_default()); } o }
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
    println!("  {} Config file     {}", term::info(""), "skip (lightweight binary)");
    let g = std::process::Command::new("git").args(["rev-parse","--git-dir"]).current_dir(&ctx.root).output().map(|o|o.status.success()).unwrap_or(false);
    if g { let d = std::process::Command::new("git").args(["status","--porcelain"]).current_dir(&ctx.root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().count()).unwrap_or(0);
        println!("  {} Git status      {}", term::info(""), if d==0 { term::ok("clean") } else { term::warn(&format!("{d} dirty")) }); }
    else { println!("  {} Git status      {}", term::info(""), term::warn("not a repo")); }
    if ok { println!("\n{}", term::ok("All healthy")); Outcome::ok("healthy") } else { Outcome::error("issues") }
});
c!(Completion_, "completion", "Shell completion bash|zsh|fish", |_ctx, args| {
    let cmds = "check gate test skill doctor network completion help";
    match args.get(2).map(|s| s.as_str()).unwrap_or("") {
        "bash" => { println!("complete -W '{}' aero-eng", cmds); Outcome::ok("") }
        "zsh" => { println!("#compdef aero-eng"); Outcome::ok("") }
        "fish" => { println!("complete -c aero-eng -f -a '{}'", cmds); Outcome::ok("") }
        _ => Outcome::error("usage: completion bash|zsh|fish")
    }
});

c!(Network_, "network", "Network diagnostics: ping, dns, port", |_ctx, args| {
    use aero_eng::term;
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("help");
    match sub {
        "help" => { println!("network commands:"); println!("  ping <host>    - ICMP-style connectivity check (TCP dial)"); println!("  dns <domain>   - DNS resolution check"); println!("  port <host>:<p> - TCP port connectivity check"); Outcome::ok("") }
        "ping" => {
            let host = args.get(3).map(|s| s.as_str()).unwrap_or("");
            if host.is_empty() { return Outcome::error("Usage: network ping <host>"); }
            let port = if host.contains(':') { host.rsplit(':').next().unwrap_or("80").parse().unwrap_or(80) } else { 80 };
            let host_clean = host.split(':').next().unwrap_or(host);
            println!("  ▶ TCP connect to {host_clean}:{port}...");
            let start = std::time::Instant::now();
            match tokio::net::TcpStream::connect((host_clean, port)).await {
                Ok(_) => { let ms = start.elapsed().as_secs_f64() * 1000.0; println!("  {} {:.0}ms", term::ok("connected"), ms); Outcome::ok("") }
                Err(e) => Outcome::error(format!("connection failed: {e}"))
            }
        }
        "dns" => {
            let domain = args.get(3).map(|s| s.as_str()).unwrap_or("");
            if domain.is_empty() { return Outcome::error("Usage: network dns <domain>"); }
            println!("  ▶ Resolving {domain}...");
            let start = std::time::Instant::now();
            match tokio::net::lookup_host((domain, 0)).await {
                Ok(addrs) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    let addrs: Vec<_> = addrs.collect();
                    println!("  {} {:.0}ms, {} addresses:", term::ok("resolved"), ms, addrs.len());
                    for addr in &addrs { println!("    {addr}"); }
                    Outcome::ok("")
                }
                Err(e) => Outcome::error(format!("dns failed: {e}"))
            }
        }
        "port" => {
            let target = args.get(3).map(|s| s.as_str()).unwrap_or("");
            if target.is_empty() { return Outcome::error("Usage: network port <host:port>"); }
            let host_port: Vec<&str> = target.splitn(2, ':').collect();
            if host_port.len() != 2 { return Outcome::error("format: host:port"); }
            let port: u16 = match host_port[1].parse() { Ok(p) => p, Err(_) => return Outcome::error("invalid port") };
            let host = host_port[0];
            println!("  ▶ Checking {host}:{port}...");
            let start = std::time::Instant::now();
            match tokio::net::TcpStream::connect((host, port)).await {
                Ok(_) => { let ms = start.elapsed().as_secs_f64() * 1000.0; println!("  {} {:.0}ms - port OPEN", term::ok(""), ms); Outcome::ok("") }
                Err(_) => { let ms = start.elapsed().as_secs_f64() * 1000.0; println!("  {} {:.0}ms - port CLOSED/filtered", term::err(""), ms); Outcome::error("port closed") }
            }
        }
        _ => Outcome::error("use: network ping|dns|port|help")
    }
});

c!(Bench_, "bench", "Run performance benchmarks", |ctx, _args| {
    use aero_eng::{term, checks::{check_filesize, check_deps, check_workspace_members, check_todos, check_crate_metadata}, outcome::Outcome as O};
    use std::time::Instant;
    println!("{}", term::header("📊 Aero Benchmarks\n"));
    
    // 1. Outcome::merge benchmark
    let mut sp = term::Spinner::new("Outcome::merge x100000"); sp.tick();
    let start = Instant::now();
    for _ in 0..100000 {
        let outcomes = vec![O::ok("a"), O::error("b"), O::skip("c")];
        let _ = O::merge(&outcomes);
    }
    let t1 = start.elapsed();
    sp.done(&format!("{:.0}μs/call", t1.as_secs_f64() * 1_000_000.0 / 100_000.0));

    // 2. check_filesize benchmark
    let mut sp = term::Spinner::new("check_filesize"); sp.tick();
    let start = Instant::now();
    for _ in 0..50 {
        let _ = check_filesize(&ctx.root, &ctx.eng_config);
    }
    let t2 = start.elapsed();
    sp.done(&format!("{:.1}ms/run", t2.as_secs_f64() * 1000.0 / 50.0));

    // 3. check_deps benchmark
    let mut sp = term::Spinner::new("check_deps"); sp.tick();
    let start = Instant::now();
    for _ in 0..50 {
        let _ = check_deps(&ctx.root);
    }
    let t3 = start.elapsed();
    sp.done(&format!("{:.1}ms/run", t3.as_secs_f64() * 1000.0 / 50.0));

    // 4. check_workspace_members benchmark
    let mut sp = term::Spinner::new("check_workspace_members"); sp.tick();
    let start = Instant::now();
    for _ in 0..100 { let _ = check_workspace_members(&ctx.root); }
    let t4 = start.elapsed();
    sp.done(&format!("{:.1}ms/run", t4.as_secs_f64() * 1000.0 / 100.0));

    // 5. check_todos benchmark
    let mut sp = term::Spinner::new("check_todos"); sp.tick();
    let start = Instant::now();
    for _ in 0..50 { let _ = check_todos(&ctx.root); }
    let t5 = start.elapsed();
    sp.done(&format!("{:.1}ms/run", t5.as_secs_f64() * 1000.0 / 50.0));

    // 6. check_crate_metadata benchmark
    let mut sp = term::Spinner::new("check_crate_metadata"); sp.tick();
    let start = Instant::now();
    for _ in 0..100 { let _ = check_crate_metadata(&ctx.root); }
    let t6 = start.elapsed();
    sp.done(&format!("{:.1}ms/run", t6.as_secs_f64() * 1000.0 / 100.0));

    println!("\n{} {}", term::header("Results:"), term::fmt_duration((t1 + t2 + t3 + t4 + t5 + t6).as_secs_f64()));
    println!("  {:<30} {:>10}", "Outcome::merge (100k)", term::fmt_duration(t1.as_secs_f64()));
    println!("  {:<30} {:>10}", "check_filesize x50", term::fmt_duration(t2.as_secs_f64()));
    println!("  {:<30} {:>10}", "check_deps x50", term::fmt_duration(t3.as_secs_f64()));
    println!("  {:<30} {:>10}", "check_workspace_members x100", term::fmt_duration(t4.as_secs_f64()));
    println!("  {:<30} {:>10}", "check_todos x50", term::fmt_duration(t5.as_secs_f64()));
    println!("  {:<30} {:>10}", "check_crate_metadata x100", term::fmt_duration(t6.as_secs_f64()));
    Outcome::ok("bench complete")
});

c!(Dashboard_, "dashboard", "Project health dashboard", |ctx, _args| {
    use aero_eng::term;
    println!("{}", term::header("📊 Aero IM Dashboard"));
    println!("  {}\n", "=".repeat(50));
    
    // Rust toolchain
    let rc = std::process::Command::new("rustc").arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    let cc = std::process::Command::new("cargo").arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    println!("  {} Rust:      {}", term::ok(""), rc);
    println!("  {} Cargo:     {}", term::ok(""), cc);
    
    // Project stats
    let crate_count = std::fs::read_dir(ctx.root.join("crates")).map(|e| e.count()).unwrap_or(0);
    let rs_files = count_rs(&ctx.root.join("crates"));
    let mig_count = std::fs::read_dir(ctx.root.join("migrations")).map(|e| e.count()).unwrap_or(0);
    let js_files = std::fs::read_dir(ctx.root.join("web")).map(|e| e.filter(|e| e.as_ref().map(|e| e.path().extension().map_or(false, |x| x == "js")).unwrap_or(false)).count()).unwrap_or(0);
    println!("");
    println!("  {} Crates:    {}", term::info(""), crate_count);
    println!("  {} Rust src:  {} files", term::info(""), rs_files);
    println!("  {} Migrations: {} files", term::info(""), mig_count);
    println!("  {} Web:       {} JS files", term::info(""), js_files);
    
    // Test stats (from last run)
    println!("");
    println!("  {} Tests {}", term::header(""), "(run 'aero-cli test' to refresh)");
    println!("  {} Run: aero-cli test --workspace --lib", term::info(""));
    println!("  {} Integration: aero-cli integration", term::info(""));
    
    // Git
    println!("");
    let g = std::process::Command::new("git").args(["rev-parse","--git-dir"]).current_dir(&ctx.root).output().map(|o|o.status.success()).unwrap_or(false);
    if g {
        let branch = std::process::Command::new("git").args(["rev-parse","--abbrev-ref","HEAD"]).current_dir(&ctx.root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
        let last = std::process::Command::new("git").args(["log","--oneline","-1"]).current_dir(&ctx.root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
        let dirty = std::process::Command::new("git").args(["status","--porcelain"]).current_dir(&ctx.root).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().count()).unwrap_or(0);
        println!("  {} Branch:    {}", term::info(""), branch);
        println!("  {} Last:      {}", term::info(""), last);
        println!("  {} Status:    {} uncommitted", term::info(""), if dirty == 0 { term::ok("clean") } else { term::warn(&dirty.to_string()) });
    }
    
    println!("");
    println!("  {}", "=".repeat(50));
    Outcome::ok("dashboard")
});

fn count_rs(dir: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(e) = std::fs::read_dir(dir) { for en in e.flatten() {
        let p = en.path();
        if p.is_dir() { if p.file_name().unwrap_or_default() != "target" && !p.file_name().unwrap_or_default().to_string_lossy().starts_with('.') { n += count_rs(&p); } }
        else if p.extension().map_or(false, |x| x == "rs") { n += 1; }
    } }
    n
}
