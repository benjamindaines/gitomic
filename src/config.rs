// Configuration for gitomic, read from ~/.config/gitomic/gitomic.cfg. The format is a flat list of
// `key = value` lines; blank lines and lines beginning with '#' are ignored, as is inline text following a
// '#'. Unknown keys are ignored rather than rejected, so a newer configuration file remains usable by an
// older binary. Every key has a compiled-in default, so a missing file yields a fully valid configuration.

use std::fs;
use std::path::PathBuf;

use crate::Res;

// Staging breadth for a watcher capture. See the `stage` field on Config for the meaning of each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageMode {
    // Stage only the paths the watcher observed changing this cycle, intersected with the paths git reports
    // as actually changed. New, renamed, and copy-over files are captured; untracked files the watcher never
    // saw are left alone.
    Observed,
    // Stage modifications to already-tracked paths only (`git add -u`).
    Tracked,
    // Stage every change including untracked files (`git add -A`).
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    // Quiescence window in milliseconds. A batch of file-system events is committed once no further event has
    // arrived for this duration, coalescing an editor's multi-write save into a single atomic commit.
    pub debounce_ms: u64,
    // When true, finalize appends " [i/N]" to each commit message so that otherwise identical messages remain
    // distinguishable in `git log`. When false, every commit in the batch receives the identical message.
    pub finalize_numbering: bool,
    // Staging breadth for an atomic capture. `Observed` (the default) stages only the paths the watcher saw
    // change this cycle, intersected with the paths git reports as actually changed, so new, renamed, and
    // copy-over files are captured while untracked files the watcher never saw are left alone. `Tracked`
    // stages modifications to already-tracked paths only (`git add -u`). `All` stages every change including
    // untracked files (`git add -A`).
    pub stage: StageMode,
    // When true, a watcher-triggered capture whose staged paths exactly match the paths recorded by the most
    // recent atomic commit in the active session is folded into that commit (`commit --amend`) instead of
    // starting a new one. This turns repeated debounce cycles that keep returning to the same file(s) into a
    // single commit, while a capture that touches a different file (or file set) still starts a fresh one.
    // Does not apply to `exec`, whose capture is a deliberate, explicitly requested result and always stands
    // on its own.
    pub coalesce_same_file: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            debounce_ms: 1000,
            finalize_numbering: false,
            stage: StageMode::Observed,
            coalesce_same_file: true,
        }
    }
}

impl Config {
    // Standard configuration directory: $XDG_CONFIG_HOME/gitomic, falling back to $HOME/.config/gitomic.
    // Shared with anything else that belongs beside the configuration file rather than under any one
    // repository's .git — the cross-repository session registry (proc::registry_path) is the first such case.
    pub fn dir() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("gitomic"));
            }
        }
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".config").join("gitomic"))
    }

    // Standard configuration path: $XDG_CONFIG_HOME/gitomic/gitomic.cfg, falling back to
    // $HOME/.config/gitomic/gitomic.cfg.
    pub fn path() -> Option<PathBuf> {
        Self::dir().map(|d| d.join("gitomic.cfg"))
    }

    // Load configuration from the standard path. A missing file is not an error and yields defaults; a present
    // but malformed value is an error, so a typo surfaces immediately rather than being silently ignored.
    pub fn load() -> Res<Config> {
        match Self::path() {
            Some(p) if p.exists() => Self::parse(&fs::read_to_string(p)?),
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
                "stage" => cfg.stage = parse_stage(value, lineno + 1)?,
                "coalesce_same_file" => cfg.coalesce_same_file = parse_bool(value, lineno + 1)?,
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

// Parse the staging mode. Accepts observed/tracked/all, case-insensitively.
fn parse_stage(value: &str, lineno: usize) -> Res<StageMode> {
    match value.to_ascii_lowercase().as_str() {
        "observed" => Ok(StageMode::Observed),
        "tracked" => Ok(StageMode::Tracked),
        "all" => Ok(StageMode::All),
        _ => Err(format!(
            "gitomic.cfg line {}: stage must be observed, tracked, or all",
            lineno
        )
        .into()),
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
        assert_eq!(c.stage, StageMode::Observed);
        assert!(c.coalesce_same_file);
    }

    #[test]
    fn empty_text_yields_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn overrides_apply() {
        let text = "debounce_ms = 250\nfinalize_numbering = yes\nstage = tracked\n\
                     coalesce_same_file = no\n";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.debounce_ms, 250);
        assert!(c.finalize_numbering);
        assert_eq!(c.stage, StageMode::Tracked);
        assert!(!c.coalesce_same_file);
    }

    #[test]
    fn stage_modes_parse() {
        assert_eq!(
            Config::parse("stage = observed\n").unwrap().stage,
            StageMode::Observed
        );
        assert_eq!(
            Config::parse("stage = tracked\n").unwrap().stage,
            StageMode::Tracked
        );
        assert_eq!(
            Config::parse("stage = ALL\n").unwrap().stage,
            StageMode::All
        );
    }

    #[test]
    fn invalid_stage_is_rejected() {
        assert!(Config::parse("stage = sometimes\n").is_err());
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
