//! `gate` — run engineering gates (shell scripts + native checks).

use crate::outcome::Outcome;
use crate::register_command;

register_command!(Gate_, "gate", "Run engineering gates", |ctx, args| {
    async fn b(p: String, t: u64) -> Outcome {
        crate::run::run_cmd("bash", &[&p], std::time::Duration::from_secs(t)).await
    }
    let sub = args.get(2).map_or("list", std::string::String::as_str);
    let sd = ctx.root.join("scripts");
    let f = |n: &str| -> String { sd.join(n).to_string_lossy().to_string() };
    match sub {
        "list" => Outcome::ok(
            "filesize truth web deps complexity filesize-native deps-native workspace-members todos metadata readme b5 all",
        ),
        "filesize" => b(f("file-size-check.sh"), 60).await,
        "truth" => b(f("truth-check.sh"), 60).await,
        "web" => b(f("web-check.sh"), 60).await,
        "deps" => b(f("dependency-check.sh"), 60).await,
        "complexity" => b(f("complexity-check.sh"), 60).await,
        "all" => {
            let (filesize, truth, web, deps, complexity) = tokio::join!(
                b(f("file-size-check.sh"), 120),
                b(f("truth-check.sh"), 120),
                b(f("web-check.sh"), 120),
                b(f("dependency-check.sh"), 120),
                b(f("complexity-check.sh"), 120),
            );
            let native_filesize =
                crate::checks::check_filesize(&ctx.root, &ctx.eng_config);
            let native_deps = crate::checks::check_deps(&ctx.root);
            let workspace = crate::checks::check_workspace_members(&ctx.root);
            let todos = crate::checks::check_todos(&ctx.root);
            let metadata = crate::checks::check_crate_metadata(&ctx.root);
            let readme = crate::checks::check_readme(&ctx.root);
            Outcome::merge(&[
                filesize,
                truth,
                web,
                deps,
                complexity,
                native_filesize,
                native_deps,
                workspace,
                todos,
                metadata,
                readme,
            ])
        }
        "filesize-native" => {
            let o = crate::checks::check_filesize(&ctx.root, &ctx.eng_config);
            if let Some(d) = o.detail() {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            o
        }
        "deps-native" => {
            let o = crate::checks::check_deps(&ctx.root);
            if let Some(d) = o.detail() {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            o
        }
        "workspace-members" => {
            let o = crate::checks::check_workspace_members(&ctx.root);
            if let Some(d) = o.detail() {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            o
        }
        "todos" => {
            let o = crate::checks::check_todos(&ctx.root);
            if let Some(d) = o.detail() {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            o
        }
        "metadata" => {
            let o = crate::checks::check_crate_metadata(&ctx.root);
            println!("{}", o.message());
            o
        }
        "b5" => b(f("test-integration.sh"), 1800).await,
        "readme" => {
            let o = crate::checks::check_readme(&ctx.root);
            println!("{}", o.message());
            o
        }
        _ => Outcome::error("unknown"),
    }
});
