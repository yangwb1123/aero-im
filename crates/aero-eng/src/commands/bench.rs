//! `bench` — run performance benchmarks.

use crate::outcome::Outcome;
use crate::register_command;

register_command!(
    Bench_,
    "bench",
    "Run performance benchmarks",
    |ctx, _args| {
        use crate::{
            checks::{
                check_crate_metadata, check_deps, check_filesize, check_todos,
                check_workspace_members,
            },
            outcome::Outcome as O,
            term,
        };
        use std::time::Instant;
        println!("{}", term::header("📊 Aero Benchmarks\n"));

        // 1. Outcome::merge benchmark
        let mut sp = term::Spinner::new("Outcome::merge x100000");
        sp.tick();
        let start = Instant::now();
        for _ in 0..100_000 {
            let outcomes = vec![O::ok("a"), O::error("b"), O::skip("c")];
            let _ = O::merge(&outcomes);
        }
        let t1 = start.elapsed();
        sp.done(&format!(
            "{:.0}μs/call",
            t1.as_secs_f64() * 1_000_000.0 / 100_000.0
        ));

        // 2. check_filesize benchmark
        let mut sp = term::Spinner::new("check_filesize");
        sp.tick();
        let start = Instant::now();
        for _ in 0..50 {
            let _ = check_filesize(&ctx.root, &ctx.eng_config);
        }
        let t2 = start.elapsed();
        sp.done(&format!("{:.1}ms/run", t2.as_secs_f64() * 1000.0 / 50.0));

        // 3. check_deps benchmark
        let mut sp = term::Spinner::new("check_deps");
        sp.tick();
        let start = Instant::now();
        for _ in 0..50 {
            let _ = check_deps(&ctx.root);
        }
        let t3 = start.elapsed();
        sp.done(&format!("{:.1}ms/run", t3.as_secs_f64() * 1000.0 / 50.0));

        // 4. check_workspace_members benchmark
        let mut sp = term::Spinner::new("check_workspace_members");
        sp.tick();
        let start = Instant::now();
        for _ in 0..100 {
            let _ = check_workspace_members(&ctx.root);
        }
        let t4 = start.elapsed();
        sp.done(&format!("{:.1}ms/run", t4.as_secs_f64() * 1000.0 / 100.0));

        // 5. check_todos benchmark
        let mut sp = term::Spinner::new("check_todos");
        sp.tick();
        let start = Instant::now();
        for _ in 0..50 {
            let _ = check_todos(&ctx.root);
        }
        let t5 = start.elapsed();
        sp.done(&format!("{:.1}ms/run", t5.as_secs_f64() * 1000.0 / 50.0));

        // 6. check_crate_metadata benchmark
        let mut sp = term::Spinner::new("check_crate_metadata");
        sp.tick();
        let start = Instant::now();
        for _ in 0..100 {
            let _ = check_crate_metadata(&ctx.root);
        }
        let t6 = start.elapsed();
        sp.done(&format!("{:.1}ms/run", t6.as_secs_f64() * 1000.0 / 100.0));

        println!(
            "\n{} {}",
            term::header("Results:"),
            term::fmt_duration((t1 + t2 + t3 + t4 + t5 + t6).as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "Outcome::merge (100k)",
            term::fmt_duration(t1.as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "check_filesize x50",
            term::fmt_duration(t2.as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "check_deps x50",
            term::fmt_duration(t3.as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "check_workspace_members x100",
            term::fmt_duration(t4.as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "check_todos x50",
            term::fmt_duration(t5.as_secs_f64())
        );
        println!(
            "  {:<30} {:>10}",
            "check_crate_metadata x100",
            term::fmt_duration(t6.as_secs_f64())
        );
        Outcome::ok("bench complete")
    }
);
