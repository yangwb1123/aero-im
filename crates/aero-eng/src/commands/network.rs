//! `network` — network diagnostics: ping, dns, port, relay-probe.

use crate::outcome::Outcome;
use crate::register_command;
use crate::term;

register_command!(
    Network_,
    "network",
    "Network diagnostics: ping, dns, port",
    |_ctx, args| {
        let sub = args.get(2).map_or("help", std::string::String::as_str);
        match sub {
            "help" => {
                println!("network commands:");
                println!("  ping <host>    - ICMP-style connectivity check (TCP dial)");
                println!("  dns <domain>   - DNS resolution check");
                println!("  port <host>:<p> - TCP port connectivity check");
                println!("  relay-probe [mock-url] - run the audit relay mock-sink probe suite");
                Outcome::ok("")
            }
            "ping" => {
                let host = args.get(3).map_or("", std::string::String::as_str);
                if host.is_empty() {
                    return Outcome::error("Usage: network ping <host>");
                }
                let port = if host.contains(':') {
                    host.rsplit(':')
                        .next()
                        .unwrap_or("80")
                        .parse()
                        .unwrap_or(80)
                } else {
                    80
                };
                let host_clean = host.split(':').next().unwrap_or(host);
                println!("  ▶ TCP connect to {host_clean}:{port}...");
                let start = std::time::Instant::now();
                match tokio::net::TcpStream::connect((host_clean, port)).await {
                    Ok(_) => {
                        let ms = start.elapsed().as_secs_f64() * 1000.0;
                        println!("  {} {:.0}ms", term::ok("connected"), ms);
                        Outcome::ok("")
                    }
                    Err(e) => Outcome::error(format!("connection failed: {e}")),
                }
            }
            "dns" => {
                let domain = args.get(3).map_or("", std::string::String::as_str);
                if domain.is_empty() {
                    return Outcome::error("Usage: network dns <domain>");
                }
                println!("  ▶ Resolving {domain}...");
                let start = std::time::Instant::now();
                match tokio::net::lookup_host((domain, 0)).await {
                    Ok(addrs) => {
                        let ms = start.elapsed().as_secs_f64() * 1000.0;
                        let addrs: Vec<_> = addrs.collect();
                        println!(
                            "  {} {:.0}ms, {} addresses:",
                            term::ok("resolved"),
                            ms,
                            addrs.len()
                        );
                        for addr in &addrs {
                            println!("    {addr}");
                        }
                        Outcome::ok("")
                    }
                    Err(e) => Outcome::error(format!("dns failed: {e}")),
                }
            }
            "port" => {
                let target = args.get(3).map_or("", std::string::String::as_str);
                if target.is_empty() {
                    return Outcome::error("Usage: network port <host:port>");
                }
                let host_port: Vec<&str> = target.splitn(2, ':').collect();
                if host_port.len() != 2 {
                    return Outcome::error("format: host:port");
                }
                let port: u16 = match host_port[1].parse() {
                    Ok(p) => p,
                    Err(_) => return Outcome::error("invalid port"),
                };
                let host = host_port[0];
                println!("  ▶ Checking {host}:{port}...");
                let start = std::time::Instant::now();
                if tokio::net::TcpStream::connect((host, port)).await.is_ok() {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    println!("  {} {:.0}ms - port OPEN", term::ok(""), ms);
                    Outcome::ok("")
                } else {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    println!("  {} {:.0}ms - port CLOSED/filtered", term::err(""), ms);
                    Outcome::error("port closed")
                }
            }
            "relay-probe" => {
                // B5-2 test vehicle: DB-free mock-sink probe suite over the
                // audit connector state machine. Direct spawn (run_cmd would
                // flatten distinct exit codes); stdout/stderr inherited so the
                // harness's named `probe: <name>: PASS` greps see the probe's
                // own output verbatim. Contract (aero-cli-b5-2-relay-probe
                // design §2.1/§2.2): exit 0 = all PASS, 1 = any FAIL,
                // 2 = usage; contract-outside codes → error (exit 1). The
                // probe bin is B5-2's file-gate; this arm is pre-wired so the
                // harness `relay-mock-probe` leg goes green the moment the
                // file lands.
                let mut cmd = if let Ok(bin) = std::env::var("AERO_RELAY_PROBE_BIN") {
                    tokio::process::Command::new(bin)
                } else {
                    let mut c = tokio::process::Command::new("cargo");
                    c.args([
                        "run",
                        "--quiet",
                        "-p",
                        "aero-audit-connector",
                        "--bin",
                        "aero-audit-relay-probe",
                    ]);
                    c
                };
                if let Some(url) = args.get(3) {
                    cmd.arg(url);
                }
                cmd.stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => return Outcome::error(format!("cannot launch relay probe: {e}")),
                };
                match tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await
                {
                    Ok(Ok(status)) => match status.code() {
                        Some(0) => Outcome::ok(""),
                        Some(1) => Outcome::error("relay probe: one or more scenarios FAILED"),
                        Some(2) => Outcome::warning(2, "relay probe usage error"),
                        _ => Outcome::error(format!("relay probe exited abnormally: {status}")),
                    },
                    Ok(Err(e)) => Outcome::error(format!("relay probe wait failed: {e}")),
                    Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        Outcome::error("relay probe timed out after 120s")
                    }
                }
            }
            _ => Outcome::error("use: network ping|dns|port|relay-probe|help"),
        }
    }
);
