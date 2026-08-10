//! `doctor` — environment diagnostics.

use crate::outcome::Outcome;
use crate::register_command;
use crate::term;

register_command!(
    Doctor_,
    "doctor",
    "Environment diagnostics",
    |ctx, _args| {
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
            "  {} Config file     skip (lightweight binary)",
            term::info("")
        );
        let g = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&ctx.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if g {
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
                term::warn("not a repo")
            );
        }
        if ok {
            println!("\n{}", term::ok("All healthy"));
            Outcome::ok("healthy")
        } else {
            Outcome::error("issues")
        }
    }
);
