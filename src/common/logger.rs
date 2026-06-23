use chrono::Local;
use colored::*;

const LOG_LEVEL: &str = "LOG";

/// Strip URLs from text before it is printed. RPC / landing endpoints embed API keys in
/// the host or path (e.g. wss://…core.chainstack.com/<key>), and connection errors print
/// the URL — which is exactly how secrets end up in shared terminal output. Each URL is
/// replaced with its scheme + "[redacted]" so the log stays readable without leaking keys.
pub fn redact_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(abs) = rest.find("://") {
        out.push_str(&rest[..abs]); // text up to and including the scheme
        out.push_str("://[redacted]");
        let after = &rest[abs + 3..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | ']' | '}' | ','))
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[derive(Clone)]
pub struct Logger {
    prefix: String,
    date_format: String,
}

impl Logger {
    // Constructor function to create a new Logger instance
    pub fn new(prefix: String) -> Self {
        Logger {
            prefix,
            date_format: String::from("%Y-%m-%d %H:%M:%S"),
        }
    }

    // Method to log a message with a prefix
    pub fn log(&self, message: String) -> String {
        let log = redact_secrets(&format!("{} {}", self.prefix_with_date(), message));
        println!("{}", log);
        log
    }

    pub fn debug(&self, message: String) -> String {
        let log = redact_secrets(&format!("{} [{}] {}", self.prefix_with_date(), "DEBUG", message));
        if LogLevel::new().is_debug() {
            println!("{}", log);
        }
        log
    }
    pub fn error(&self, message: String) -> String {
        let log = redact_secrets(&format!("{} [{}] {}", self.prefix_with_date(), "ERROR", message));
        println!("{}", log);

        log
    }

    // Add success method to fix compilation errors in monitor.rs
    pub fn success(&self, message: String) -> String {
        let log = redact_secrets(&format!("{} [{}] {}", self.prefix_with_date(), "SUCCESS".green().bold(), message));
        println!("{}", log);
        log
    }

    // Add a new method for performance-critical paths
    pub fn log_critical(&self, message: String) -> String {
        // Only log if not in a performance-critical section
        let log = format!("{} {}", self.prefix_with_date(), message);
        // Skip println for critical paths
        log
    }

    fn prefix_with_date(&self) -> String {
        let date = Local::now();
        format!(
            "[{}] {}",
            date.format(&self.date_format.as_str().blue().bold()),
            self.prefix
        )
    }
}

struct LogLevel<'a> {
    level: &'a str,
}
impl LogLevel<'_> {
    fn new() -> Self {
        let level = LOG_LEVEL;
        LogLevel { level }
    }
    fn is_debug(&self) -> bool {
        self.level.to_lowercase().eq("debug")
    }
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn redacts_rpc_url_with_embedded_key() {
        let s = "ws: connect failed: wss://solana-mainnet.core.chainstack.com/bb31ea6386a3a3b5 (retry in 5s)";
        let out = redact_secrets(s);
        assert!(!out.contains("bb31ea6386a3a3b5"), "API key leaked: {out}");
        assert!(!out.contains("chainstack.com"), "host leaked: {out}");
        assert!(out.contains("wss://[redacted]"));
        assert!(out.contains("retry in 5s"), "non-secret context should survive: {out}");
    }

    #[test]
    fn redacts_multiple_urls() {
        let s = "primary https://a.com/key1 backup https://b.com/key2 end";
        let out = redact_secrets(s);
        assert!(!out.contains("key1") && !out.contains("key2"), "{out}");
        assert!(out.contains("primary") && out.contains("backup") && out.contains("end"));
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let s = "🔴 SELL 50% of MINT at price 1234 | pnl +42%";
        assert_eq!(redact_secrets(s), s);
    }
}
