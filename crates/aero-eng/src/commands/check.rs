//! `check` / `test` / `integration` — build, test, and harness commands.

use crate::outcome::Outcome;
use crate::register_command;

register_command!(
    Check_,
    "check",
    "Run cargo check + test + clippy + native gates",
    |ctx, _args| {
        use crate::{
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
register_command!(Test_, "test", "Run cargo test (lib)", |_ctx, _args| {
    crate::run::cargo_test_lib().await
});
register_command!(
    Integration_,
    "integration",
    "Run the integration harness (scripts/test-integration.sh)",
    |_ctx, _args| { crate::run::test_integration().await }
);
