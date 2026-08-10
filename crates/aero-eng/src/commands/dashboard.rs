//! `dashboard` — project health dashboard.

use crate::outcome::Outcome;
use crate::register_command;
use crate::term;

register_command!(
    Dashboard_,
    "dashboard",
    "Project health dashboard",
    |ctx, _args| {
        println!("{}", term::header("📊 Aero IM Dashboard"));
        println!("  {}\n", "=".repeat(50));

        // Rust toolchain
        let rc = std::process::Command::new("rustc")
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        let cc = std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        println!("  {} Rust:      {}", term::ok(""), rc);
        println!("  {} Cargo:     {}", term::ok(""), cc);

        // Project stats
        let crate_count = std::fs::read_dir(ctx.root.join("crates"))
            .map(std::iter::Iterator::count)
            .unwrap_or(0);
        let rs_files = count_rs(&ctx.root.join("crates"));
        let mig_count = std::fs::read_dir(ctx.root.join("migrations"))
            .map(std::iter::Iterator::count)
            .unwrap_or(0);
        let js_files = std::fs::read_dir(ctx.root.join("web"))
            .map(|e| {
                e.filter(|e| {
                    e.as_ref()
                        .map(|e| e.path().extension().is_some_and(|x| x == "js"))
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        println!();
        println!("  {} Crates:    {}", term::info(""), crate_count);
        println!("  {} Rust src:  {} files", term::info(""), rs_files);
        println!("  {} Migrations: {} files", term::info(""), mig_count);
        println!("  {} Web:       {} JS files", term::info(""), js_files);

        // Test stats (from last run)
        println!();
        println!(
            "  {} Tests (run 'aero-eng test' to refresh)",
            term::header("")
        );
        println!("  {} Run: aero-eng test --workspace --lib", term::info(""));
        println!("  {} Integration: aero-eng integration", term::info(""));

        // Git
        println!();
        let g = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&ctx.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if g {
            let branch = std::process::Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&ctx.root)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default();
            let last = std::process::Command::new("git")
                .args(["log", "--oneline", "-1"])
                .current_dir(&ctx.root)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default();
            let dirty = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&ctx.root)
                .output()
                .ok()
                .map_or(0, |o| String::from_utf8_lossy(&o.stdout).lines().count());
            println!("  {} Branch:    {}", term::info(""), branch);
            println!("  {} Last:      {}", term::info(""), last);
            println!(
                "  {} Status:    {} uncommitted",
                term::info(""),
                if dirty == 0 {
                    term::ok("clean")
                } else {
                    term::warn(dirty.to_string())
                }
            );
        }

        println!();
        println!("  {}", "=".repeat(50));
        Outcome::ok("dashboard")
    }
);

fn count_rs(dir: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(e) = std::fs::read_dir(dir) {
        for en in e.flatten() {
            let p = en.path();
            if p.is_dir() {
                if p.file_name().unwrap_or_default() != "target"
                    && !p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with('.')
                {
                    n += count_rs(&p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                n += 1;
            }
        }
    }
    n
}
