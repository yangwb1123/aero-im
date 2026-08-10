//! `skill` — list / view / run skills.

use crate::outcome::Outcome;
use crate::register_command;

register_command!(Skill_, "skill", "List/view/run skills", |ctx, args| {
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
                    return crate::run::run_cmd(r, &[&ps], std::time::Duration::from_secs(120))
                        .await;
                }
            }
            Outcome::error("nf")
        }
        _ => Outcome::error("use list/view/run"),
    }
});
