// Configuration for gitomic, read from ~/.config/gitomic/gitomic.cfg. The format is a flat list of
// `key = value` lines; blank lines and lines beginning with '#' are ignored, as is inline text following a
// '#'. Unknown keys are ignored rather than rejected, so a newer configuration file remains usable by an
// older binary. Every key has a compiled-in default, so a missing file yields a fully valid configuration.

use std::fs;
use std::path::PathBuf;

use crate::Res;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    // Quiescence window in milliseconds. A batch of file-system events is committed once no further event has
    // arrived for this duration, coalescing an editor's multi-write save into a single atomic commit.
    pub debounce_ms: u64,
    // When true, finalize appends " [i/N]" to each commit message so that otherwise identical messages remain
    // distinguishable in `git log`. When false, every commit in the batch receives the identical message.
    pub finalize_numbering: bool,
    // When true, atomic commits stage untracked files as well as modifications (`git add -A`); when false,
    // only tracked-file changes are staged (`git add -u`).
    pub include_untracked: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            debounce_ms: 1000,
            finalize_numbering: false,
            include_untracked: true,
        }
    }
}

impl Config {
    // Standard configuration path: $XDG_CONFIG_HOME/gitomic/gitomic.cfg, falling back to
    // $HOME/.config/gitomic/gitomic.cfg.
    pub fn path() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("gitomic").join("gitomic.cfg"));
            }
        }
        std::env::var("HOME").ok().map(|h| {
            PathBuf::from(h)
                .join(".config")
                .join("gitomic")
                .join("gitomic.cfg")
        })
    }

    // Load configuration from the standard path. A missing file is not an error and yields defaults; a present
    // but malformed value is an error, so a typo surfaces immediately rather than being silently ignored.
    pub fn load() -> Res<Config> {
        match Self::path() {
            Some(p) if p.exists() => Self::parse(&fs::read_to_string(&p)?),
            _ => Ok(Config::default()),
        }
    }

    // Parse configuration text into a Config, starting from defaults and overriding each recognised key.
    pub fn parse(text: &str) -> Res<Config> {
        let mut cfg = Config::default();
        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                format!("gitomic.cfg line {}: expected 'key = value'", lineno + 1)
            })?;
            let key = key.trim();
            let value = value.trim();
            match key {
                "debounce_ms" => {
                    cfg.debounce_ms = value.parse().map_err(|_| {
                        format!(
                            "gitomic.cfg line {}: debounce_ms must be an integer",
                            lineno + 1
                        )
                    })?;
                }
                "finalize_numbering" => cfg.finalize_numbering = parse_bool(value, lineno + 1)?,
                "include_untracked" => cfg.include_untracked = parse_bool(value, lineno + 1)?,
                // Reserved for a future always-on multi-repository mode; accepted and ignored so an
                // aspirational configuration file does not break the current cwd-scoped tool.
                "watch_dir" => {}
                _ => {}
            }
        }
        Ok(cfg)
    }
}

// Remove an inline '#' comment and everything after it. A '#' only introduces a comment when preceded by
// whitespace or at the start of the line, so a '#' embedded in a value is preserved.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

// Parse a permissive boolean. Accepts true/false, yes/no, on/off, and 1/0, case-insensitively.
fn parse_bool(value: &str, lineno: usize) -> Res<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("gitomic.cfg line {}: expected a boolean", lineno).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable() {
        let c = Config::default();
        assert_eq!(c.debounce_ms, 1000);
        assert!(!c.finalize_numbering);
        assert!(c.include_untracked);
    }

    #[test]
    fn empty_text_yields_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn overrides_apply() {
        let text = "debounce_ms = 250\nfinalize_numbering = yes\ninclude_untracked = off\n";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.debounce_ms, 250);
        assert!(c.finalize_numbering);
        assert!(!c.include_untracked);
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let text = "# leading comment\n\n  debounce_ms = 500   # trailing comment\n";
        assert_eq!(Config::parse(text).unwrap().debounce_ms, 500);
    }

    #[test]
    fn unknown_and_reserved_keys_are_tolerated() {
        let text = "watch_dir = /home/x/code\nfuture_key = whatever\n";
        assert_eq!(Config::parse(text).unwrap(), Config::default());
    }

    #[test]
    fn malformed_line_is_rejected() {
        assert!(Config::parse("debounce_ms 500\n").is_err());
    }

    #[test]
    fn non_integer_debounce_is_rejected() {
        assert!(Config::parse("debounce_ms = soon\n").is_err());
    }
}
