//! Aero Engineering CLI — 15 commands, trait-based dispatch.

use aero_eng::{Command, CommandRegistry, ExecutionContext, Outcome};
use async_trait::async_trait;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("aero-cli v{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cmd = args.get(1).map_or("help", String::as_str);
    let ctx = ExecutionContext::new(std::env::current_dir().unwrap_or_default());
    let mut reg = CommandRegistry::new();
    macro_rules! a {
        ($c:expr) => {
            reg = reg.with_command(Box::new($c));
        };
    }
    a!(Migrate);
    a!(Health);
    a!(AiTest);
    a!(Streams);
    a!(WsPing);
    a!(Check_);
    a!(Smoke_);
    a!(Gate_);
    a!(Test_);
    a!(Skill_);
    a!(Doctor_);
    a!(Dev_);
    a!(Integration_);
    a!(Completion_);
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

fn ws_ping_url(raw_host: &str) -> Result<url::Url, &'static str> {
    let mut url = url::Url::parse(raw_host).map_err(|_| "invalid AERO_HOST websocket URL")?;
    let raw_authority = raw_host
        .split_once("://")
        .map(|(_, remainder)| {
            let end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
            &remainder[..end]
        })
        .unwrap_or_default();
    if !matches!(url.scheme(), "ws" | "wss") || url.host().is_none() || raw_authority.is_empty() {
        return Err("AERO_HOST must use ws:// or wss:// and include a host");
    }

    // `Url::username()` cannot distinguish an absent username from `ws://@host`.
    // Check the original authority for the delimiter as well as the decoded
    // fields so even empty or percent-encoded userinfo is rejected.
    let authority_has_userinfo = raw_authority.bytes().any(|byte| byte == b'@');
    if authority_has_userinfo || !url.username().is_empty() || url.password().is_some() {
        return Err("AERO_HOST must not contain userinfo");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("AERO_HOST must not contain a query or fragment");
    }

    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if url.scheme() == "ws" && !loopback {
        return Err("cleartext ws:// is allowed only for loopback hosts");
    }

    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| "AERO_HOST cannot be used as a websocket base URL")?;
        segments.pop_if_empty().push("ws");
    }
    Ok(url)
}

fn ws_ping_request(
    raw_host: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, &'static str> {
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest,
        http::{header, HeaderValue},
    };

    if token.is_empty() || token != token.trim() || token.chars().any(char::is_whitespace) {
        return Err("AERO_TOKEN is not a valid bearer value");
    }
    let url = ws_ping_url(raw_host)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| "invalid AERO_HOST websocket URL")?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "AERO_TOKEN is not a valid bearer value")?;
    authorization.set_sensitive(true);
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, authorization);
    Ok(request)
}

c!(
    Migrate,
    "migrate",
    "Apply pending DB migrations",
    |_ctx, _args| {
        let Some(c) = load_cfg().await else {
            return Outcome::error("config required");
        };
        let p = match aero_storage::connect_pg(&c.database.url, 4).await {
            Ok(p) => p,
            Err(e) => return Outcome::error(format!("db: {e}")),
        };
        match aero_storage::migrate(&p).await {
            Ok(()) => Outcome::ok("✓ ok"),
            Err(e) => Outcome::error(format!("{e}")),
        }
    }
);
c!(Health, "health", "Probe PG/Redis/NATS", |_ctx, _args| {
    let Some(c) = load_cfg().await else {
        return Outcome::error("config required");
    };
    let mut ok = true;
    let mut v = vec![];
    let pg = aero_storage::connect_pg(&c.database.url, 1).await;
    let rd = aero_storage::RedisCache::connect(&c.redis.url).await;
    let nt = async_nats::connect(&c.nats.url).await;
    match pg {
        Ok(p) => match sqlx::query_as::<_, (i32,)>("SELECT 1").fetch_one(&p).await {
            Ok(_) => v.push("✓ pg".into()),
            Err(e) => {
                v.push(format!("✗ {e}"));
                ok = false;
            }
        },
        Err(e) => {
            v.push(format!("✗ {e}"));
            ok = false;
        }
    }
    match rd {
        Ok(_) => v.push("✓ redis".into()),
        Err(e) => {
            v.push(format!("✗ {e}"));
            ok = false;
        }
    }
    match nt {
        Ok(_) => v.push("✓ nats".into()),
        Err(e) => {
            v.push(format!("✗ {e}"));
            ok = false;
        }
    }
    if ok {
        Outcome::ok(v.join("\n"))
    } else {
        Outcome::error(v.join("\n"))
    }
});
c!(AiTest, "ai-test", "Exercise AI backends", |_ctx, _args| {
    // Explicit operator connectivity probe, not a production/tenant business
    // path: it intentionally bypasses `AiService` and the usage ledger so a
    // broken database cannot hide whether the provider credentials/network work.
    use aero_ai::{default_embedder, AnthropicClient, ChatMsg};
    match default_embedder().embed_one("hi").await {
        Ok(v) => println!("embedder: dim={}", v.len()),
        Err(e) => println!("embedder: {e}"),
    }
    match AnthropicClient::from_env() {
        Some(c) => match c.complete("s", &[ChatMsg::user("Hi")], 32).await {
            Ok(r) => println!("llm: {}", r.trim()),
            Err(e) => println!("llm: {e}"),
        },
        None => println!("no key"),
    }
    Outcome::ok("done")
});
c!(Streams, "streams", "List live streams", |_ctx, _args| {
    let Some(c) = load_cfg().await else {
        return Outcome::error("config required");
    };
    let p = match aero_storage::connect_pg(&c.database.url, 2).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(format!("db: {e}")),
    };
    let list = aero_storage::StreamRepo::new(p).list_live().await;
    match list {
        Ok(l) => {
            for s in l {
                println!("{} {:?}", s.id, s.protocol);
            }
            Outcome::ok("")
        }
        Err(e) => Outcome::error(format!("{e}")),
    }
});
c!(Test_, "test", "Run cargo test", |_ctx, _args| {
    aero_eng::run::cargo_test_lib().await
});
c!(WsPing, "ws-ping", "WS ping", |_ctx, _args| {
    use futures::{SinkExt, StreamExt};
    let t = std::env::var("AERO_TOKEN").unwrap_or_default();
    if t.is_empty() {
        return Outcome::error("set AERO_TOKEN");
    }
    let h = std::env::var("AERO_HOST").unwrap_or_else(|_| "ws://localhost:3030".into());
    let request = match ws_ping_request(&h, &t) {
        Ok(request) => request,
        Err(error) => return Outcome::error(error),
    };
    let (mut ws, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(w) => w,
        Err(e) => return Outcome::error(format!("{e}")),
    };
    match ws.next().await {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(m))) => println!("  {m}"),
        _ => return Outcome::error("bad"),
    }
    let _ = ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"ping"}"#.into(),
        ))
        .await;
    match ws.next().await {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(m))) => println!("  pong: {m}"),
        _ => return Outcome::error("no"),
    }
    let _ = ws.close(None).await;
    Outcome::ok("✓")
});
c!(
    Check_,
    "check",
    "Run cargo check + test + clippy + native gates",
    |ctx, _args| {
        use aero_eng::{
            checks::{
                check_crate_metadata, check_deps, check_filesize, check_todos,
                check_workspace_members,
            },
            run::{cargo_check, cargo_clippy, cargo_test_lib},
            term,
        };
        let s = std::time::Instant::now();
        let mut sp = term::Spinner::new("checks");
        sp.tick();
        // cargo checks are async; native checks are sync (fast)
        let (co, to, clo) = tokio::join!(cargo_check(), cargo_test_lib(), cargo_clippy());
        let fs = check_filesize(&ctx.root, &ctx.eng_config);
        let deps = check_deps(&ctx.root);
        let ws = check_workspace_members(&ctx.root);
        let td = check_todos(&ctx.root);
        let md = check_crate_metadata(&ctx.root);
        println!("\n{}", term::header("Results"));
        for (l, o) in [
            ("cargo check", &co),
            ("cargo test", &to),
            ("cargo clippy", &clo),
            ("filesize", &fs),
            ("deps", &deps),
            ("workspace", &ws),
            ("todos", &td),
            ("metadata", &md),
        ] {
            println!("  {:<20} {}", l, o.message());
        }
        println!(
            "  {} {}",
            term::header("Total"),
            term::fmt_duration(s.elapsed().as_secs_f64())
        );
        sp.done("done");
        Outcome::merge(&[co, to, clo, fs, deps, ws, td, md]).with_duration(s.elapsed())
    }
);

#[cfg(test)]
mod tests {
    use super::{ws_ping_request, ws_ping_url};
    use tokio_tungstenite::tungstenite::http::header;

    #[test]
    fn ws_ping_url_accepts_secure_remote_and_loopback_cleartext() {
        assert_eq!(
            ws_ping_url("wss://im.example.test/gateway/")
                .unwrap()
                .as_str(),
            "wss://im.example.test/gateway/ws"
        );
        assert_eq!(
            ws_ping_url("ws://127.0.0.1:3030").unwrap().as_str(),
            "ws://127.0.0.1:3030/ws"
        );
        assert_eq!(
            ws_ping_url("ws://[::1]:3030/base").unwrap().as_str(),
            "ws://[::1]:3030/base/ws"
        );
        assert!(ws_ping_url("ws://LOCALHOST:3030").is_ok());
    }

    #[test]
    fn ws_ping_url_rejects_unsafe_schemes_authorities_and_metadata() {
        for host in [
            "http://localhost:3030",
            "https://im.example.test",
            "ws://im.example.test",
            "ws://user:password@localhost:3030",
            "ws://@localhost:3030",
            "wss://im.example.test?debug=true",
            "wss://im.example.test/#fragment",
            "wss:///missing-host",
        ] {
            assert!(ws_ping_url(host).is_err(), "unexpectedly accepted {host}");
        }
    }

    #[test]
    fn ws_ping_request_keeps_token_only_in_sensitive_authorization() {
        let token = "test-token-not-a-real-credential";
        let request = ws_ping_request("wss://im.example.test", token).unwrap();
        assert_eq!(request.uri().to_string(), "wss://im.example.test/ws");
        assert!(!request.uri().to_string().contains(token));

        let authorization = request.headers().get(header::AUTHORIZATION).unwrap();
        assert_eq!(authorization, &format!("Bearer {token}"));
        assert!(authorization.is_sensitive());
        assert!(!format!("{request:?}").contains(token));
        assert_eq!(
            request
                .headers()
                .values()
                .filter(|value| value
                    .as_bytes()
                    .windows(token.len())
                    .any(|part| part == token.as_bytes()))
                .count(),
            1
        );
    }

    #[test]
    fn ws_ping_request_rejects_whitespace_in_bearer() {
        for token in ["", " token", "token ", "token\nvalue", "token value"] {
            assert!(ws_ping_request("wss://im.example.test", token).is_err());
        }
    }
}
c!(Smoke_, "smoke", "List/run smoke tests", |ctx, args| {
    let sub = args.get(2).map_or("list", std::string::String::as_str);
    let sd = ctx.root.join("scripts");
    let mut sm: Vec<String> = vec![];
    if let Ok(e) = std::fs::read_dir(&sd) {
        for en in e.flatten() {
            let n = en.file_name().to_string_lossy().to_string();
            if n.starts_with("smoke_") {
                let ext = std::path::Path::new(&n).extension();
                if ext.is_some_and(|e| e.eq_ignore_ascii_case("py"))
                    || ext.is_some_and(|e| e.eq_ignore_ascii_case("sh"))
                {
                    sm.push(
                        n.strip_prefix("smoke_")
                            .unwrap_or(&n)
                            .strip_suffix(".py")
                            .or_else(|| n.strip_suffix(".sh"))
                            .unwrap_or(&n)
                            .to_owned(),
                    );
                }
            }
        }
    }
    sm.sort();
    match sub {
        "list" => {
            for s in &sm {
                println!("  {s}");
            }
            Outcome::ok("")
        }
        "run" => {
            let name = args.get(3).map_or("", std::string::String::as_str);
            if name.is_empty() {
                return Outcome::error("need name");
            }
            let p = sd.join(format!("smoke_{name}.py"));
            let s = sd.join(format!("smoke_{name}.sh"));
            let sc = if p.exists() { p } else { s };
            if !sc.exists() {
                return Outcome::error("nf");
            }
            let ps = sc.to_string_lossy().to_string();
            aero_eng::run::run_cmd("python3", &[&ps], std::time::Duration::from_secs(120)).await
        }
        _ => Outcome::error("use list/run"),
    }
});
c!(Gate_, "gate", "Run gates (shell + native)", |ctx, args| {
    aero_eng::run_gate(ctx, args).await
});
c!(Skill_, "skill", "List/view/run skills", |ctx, args| {
    let sub = args.get(2).map_or("list", std::string::String::as_str);
    let sk = ctx.root.join("skills");
    match sub {
        "list" => {
            if let Ok(e) = std::fs::read_dir(&sk) {
                for en in e.flatten() {
                    let n = en.file_name().to_string_lossy().to_string();
                    if std::path::Path::new(&n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                    {
                        println!("  {} [doc]", &n[..n.len() - 3]);
                    } else if en.path().is_dir() {
                        println!("  {n} [exec]");
                    }
                }
            }
            Outcome::ok("")
        }
        "view" => {
            let n = args.get(3).map_or("", std::string::String::as_str);
            let p = sk.join(format!("{n}.md"));
            if p.exists() {
                println!("{}", std::fs::read_to_string(&p).unwrap_or_default());
                Outcome::ok("")
            } else {
                Outcome::error("nf")
            }
        }
        "run" => {
            let n = args.get(3).map_or("", std::string::String::as_str);
            if n.is_empty() {
                return Outcome::error("need name");
            }
            for (r, e) in [("bash", "sh"), ("python3", "py")] {
                let p = sk.join(n).join(format!("run.{e}"));
                if p.exists() {
                    let ps = p.to_string_lossy().to_string();
                    return aero_eng::run::run_cmd(r, &[&ps], std::time::Duration::from_secs(120))
                        .await;
                }
            }
            Outcome::error("nf")
        }
        _ => Outcome::error("use list/view/run"),
    }
});
c!(
    Doctor_,
    "doctor",
    "Environment diagnostics",
    |ctx, _args| {
        use aero_eng::term;
        let mut ok = true;
        println!("{}", term::header("🔍 Aero IM Doctor\n"));
        let r = std::process::Command::new("rustc")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!(
            "  {} Rust toolchain  {}",
            term::info(""),
            if r {
                term::ok("ok")
            } else {
                ok = false;
                term::err("miss")
            }
        );
        println!(
            "  {} Config file     {}",
            term::info(""),
            if aero_common::config::AppConfig::load().is_ok() {
                term::ok("valid")
            } else {
                ok = false;
                term::err("bad")
            }
        );
        // Git check: verify it's actually a git repo
        let git_root = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&ctx.root)
            .output();
        let is_git = git_root
            .as_ref()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if is_git {
            let d = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&ctx.root)
                .output()
                .ok()
                .map_or(0, |o| String::from_utf8_lossy(&o.stdout).lines().count());
            println!(
                "  {} Git status      {}",
                term::info(""),
                if d == 0 {
                    term::ok("clean")
                } else {
                    term::warn(format!("{d} dirty"))
                }
            );
        } else {
            println!(
                "  {} Git status      {}",
                term::info(""),
                term::warn("not a git repo")
            );
        }
        let m = std::fs::read_dir(ctx.root.join("migrations"));
        println!(
            "  {} Migrations      {}",
            term::info(""),
            if let Ok(e) = m {
                term::ok(format!("{} files", e.count()))
            } else {
                ok = false;
                term::err("not found")
            }
        );
        if ok {
            println!("\n{}", term::ok("All healthy"));
            Outcome::ok("healthy")
        } else {
            Outcome::error("issues")
        }
    }
);
c!(Dev_, "dev", "Start dev environment", |ctx, args| {
    use aero_eng::term;
    let skip = args.contains(&"--skip-services".to_string());
    println!("{}", term::header("🚀 Aero Dev Environment\n"));
    let mut sp = term::Spinner::new("prereq");
    sp.tick();
    let d = std::process::Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let c = std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !c {
        sp.fail("rust missing");
        return Outcome::error("rustup");
    }
    sp.done(&format!("rust ✓ docker {}", if d { "✓" } else { "✗" }));
    if !skip && d {
        let mut sp = term::Spinner::new("services");
        sp.tick();
        let _ = std::process::Command::new("docker")
            .args(["compose", "up", "-d"])
            .current_dir(&ctx.root)
            .output();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        sp.done("up");
    }
    let mut sp = term::Spinner::new("migrate");
    sp.tick();
    let Some(c) = load_cfg().await else {
        return Outcome::error("config required");
    };
    let p = match aero_storage::connect_pg(&c.database.url, 4).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(format!("db: {e}")),
    };
    match aero_storage::migrate(&p).await {
        Ok(()) => sp.done("ok"),
        Err(e) => return Outcome::error(format!("{e}")),
    }
    println!("\n{}", term::ok("ready"));
    Outcome::ok("dev ready")
});
c!(
    Integration_,
    "integration",
    "Integration tests (needs PG)",
    |ctx, _args| {
        let p = ctx.root.join("scripts/test-integration.sh");
        if !p.exists() {
            return Outcome::error("nf");
        }
        let ps = p.to_string_lossy().to_string();
        aero_eng::run::run_cmd("bash", &[&ps], std::time::Duration::from_secs(600)).await
    }
);
c!(
    Completion_,
    "completion",
    "Shell completion bash|zsh|fish",
    |_ctx, args| {
        let cmds = "migrate health ai-test streams ws-ping check smoke gate test skill doctor completion dev integration help";
        match args.get(2).map_or("", std::string::String::as_str) {
            "bash" => {
                println!("complete -W '{cmds}' aero-cli");
                Outcome::ok("")
            }
            "zsh" => {
                println!("#compdef aero-cli");
                Outcome::ok("")
            }
            "fish" => {
                println!("complete -c aero-cli -f -a '{cmds}'");
                Outcome::ok("")
            }
            _ => Outcome::error("usage: completion bash|zsh|fish"),
        }
    }
);
