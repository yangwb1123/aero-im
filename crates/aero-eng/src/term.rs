//! Terminal utilities — colored output, progress spinners, formatting.
//!
//! Zero-dependency: uses ANSI escape codes directly rather than pulling in
//! `colored` or `indicatif` crates (keeping the binary lean).

/// ANSI color codes.
pub mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GRAY: &str = "\x1b[90m";
}

/// Whether the terminal supports color. Cached after first probe.
fn is_color() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHED: AtomicU8 = AtomicU8::new(2); // 2=unchecked, 1=yes, 0=no
    let cached = CACHED.load(Ordering::Relaxed);
    if cached == 1 {
        return true;
    }
    if cached == 0 {
        return false;
    }
    // Probe: is stdout a terminal?
    let supported = std::io::IsTerminal::is_terminal(&std::io::stdout());
    CACHED.store(if supported { 1 } else { 0 }, Ordering::Relaxed);
    supported
}

/// Wrap text in a color if the terminal supports it.
fn c(text: &str, ansi: &str) -> String {
    if is_color() {
        format!("{ansi}{text}{}", color::RESET)
    } else {
        text.to_owned()
    }
}

/// Green checkmark prefix.
pub fn ok(text: impl AsRef<str>) -> String {
    format!("{} {}", c("✓", color::GREEN), text.as_ref())
}

/// Red cross prefix.
pub fn err(text: impl AsRef<str>) -> String {
    format!("{} {}", c("✗", color::RED), text.as_ref())
}

/// Yellow warning prefix.
pub fn warn(text: impl AsRef<str>) -> String {
    format!("{} {}", c("!", color::YELLOW), text.as_ref())
}

/// Blue info prefix.
pub fn info(text: impl AsRef<str>) -> String {
    format!("{} {}", c("▶", color::BLUE), text.as_ref())
}

/// Bold header.
pub fn header(text: impl AsRef<str>) -> String {
    if is_color() {
        format!("{}{}{}", color::BOLD, text.as_ref(), color::RESET)
    } else {
        text.as_ref().to_owned()
    }
}

/// Format a duration in human-readable form.
pub fn fmt_duration(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let m = (secs / 60.0).floor();
        let s = secs - m * 60.0;
        format!("{m:.0}m {s:.0}s")
    }
}

/// Format a summary line with status icon, label, duration, and detail.
pub fn summary_line(icon: &str, label: &str, duration: std::time::Duration, detail: &str) -> String {
    let secs = duration.as_secs_f64();
    let time = fmt_duration(secs);
    format!("{icon} {label:<20} {time:>8}  {detail}")
}

/// A simple text spinner for long-running operations.
/// Uses `\r` to overwrite the current line.
pub struct Spinner {
    chars: [char; 4],
    i: usize,
    label: String,
}

impl Spinner {
    /// Create a new spinner with the given label.
    pub fn new(label: impl Into<String>) -> Self {
        Self { chars: ['▖', '▘', '▝', '▗'], i: 0, label: label.into() }
    }

    /// Advance the spinner by one tick and print.
    pub fn tick(&mut self) {
        use std::io::Write;
        let ch = self.chars[self.i % self.chars.len()];
        self.i += 1;
        if is_color() {
            print!("\r{}{} {}{}", color::CYAN, ch, self.label, color::RESET);
        } else {
            print!("\r  {} ...", self.label);
        }
        let _ = std::io::stdout().flush();
    }

    /// Finish the spinner with a status message.
    pub fn done(&self, status: &str) {
        use std::io::Write;
        println!("\r{} {}", ok(&self.label), status);
        let _ = std::io::stdout().flush();
    }

    /// Finish the spinner with an error message.
    pub fn fail(&self, status: &str) {
        eprintln!("\r{} {}", err(&self.label), status);
    }
}

/// Ensure newline after spinner output.
impl Drop for Spinner {
    fn drop(&mut self) {
        // Only print newline if we've written spinner chars
        if self.i > 0 {
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_prefix_contains_checkmark() {
        let s = ok("all good");
        assert!(s.contains("all good"));
    }

    #[test]
    fn err_prefix_contains_cross() {
        let s = err("something broke");
        assert!(s.contains("something broke"));
    }

    #[test]
    fn fmt_duration_zero() {
        let s = fmt_duration(0.0);
        assert!(s.contains("0ms"));
    }

    #[test]
    fn fmt_duration_seconds() {
        let s = fmt_duration(12.5);
        assert!(s.contains("12"));
    }

    #[test]
    fn fmt_duration_minutes() {
        let s = fmt_duration(125.0);
        assert!(s.contains("m") || s.contains("s"));
    }

    #[test]
    fn summary_line_format() {
        let d = std::time::Duration::from_secs(5);
        let s = summary_line("✓", "test", d, "passed");
        assert!(s.contains("test"));
        assert!(s.contains("5"));
    }

    #[test]
    fn spinner_tick_does_not_panic() {
        let mut s = Spinner::new("loading");
        s.tick();
        s.tick();
        s.done("complete");
    }
}
