//! `completion` — shell completion scripts (bash | zsh | fish).
//!
//! The word list is derived from the registry (`completion_words()`) — the
//! single source of truth — instead of a hand-maintained literal that
//! historically drifted (bench/dashboard were missing).

use crate::outcome::Outcome;
use crate::register_command;
use crate::registry::CommandRegistry;

register_command!(
    Completion_,
    "completion",
    "Shell completion bash|zsh|fish",
    |_ctx, args| {
        let words = CommandRegistry::collect().completion_words();
        match args.get(2).map_or("", std::string::String::as_str) {
            "bash" => {
                println!("complete -W '{words}' aero-eng");
                Outcome::ok("")
            }
            "zsh" => {
                println!("#compdef aero-eng");
                Outcome::ok("")
            }
            "fish" => {
                println!("complete -c aero-eng -f -a '{words}'");
                Outcome::ok("")
            }
            _ => Outcome::error("usage: completion bash|zsh|fish"),
        }
    }
);
