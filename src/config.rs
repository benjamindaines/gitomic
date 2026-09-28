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
    // Glob patterns (see `glob_match`) matched against a changed path's basename; a match is dropped at the
    // watcher's event source before it can arm the debounce timer or enter the observed-path set (issue #8).
    // Seeded from `DEFAULT_IGNORE_PATTERNS` and extended, never replaced, by the `ignore_patterns` config key,
    // so a configuration file that adds one project-specific pattern is not obliged to re-list the defaults.
    // Applies only under `StageMode::Observed`, the only mode that consults the observed-path set at all;
    // `Tracked` and `All` stage by `git add -u`/`-A` directly and never route through this list.
    pub ignore_patterns: Vec<String>,
    // Size in MiB above which the interactive cherry-pick screen does not read a changed file to show
    // its diff. Such a file is listed with its size, and its commit is loaded in full only after the
    // operator asks for it (`D`). Reading a multi-gigabyte image to draw a preview is what makes the
    // screen unusable on slow hardware. 0 turns the limit off.
    pub cherry_size_limit_mb: u64,
    // When true, `init` installs a `pre-push` hook that refuses to push a branch while that branch has an open
    // session holding placeholder commits, and `finish` removes it once the session is closed. Off by default:
    // a hook is per-clone state that gitomic writes into the repository, it has to stand aside for an existing
    // `pre-push` hook, and `git push --no-verify` bypasses it, so it is a guard the operator opts into rather
    // than one imposed on every repository. The unconditional protection is in `finish`, which refuses to
    // rewrite a commit a remote already holds whether or not this is set.
    pub push_guard: bool,
}

// Editor swap, lock, and backup file conventions excluded from watcher observation by default. These files
// are transient artifacts of the editing process, not of the edit itself: including them in the observed set
// does two kinds of damage. Their own churn arms the debounce timer independently of the file actually being
// edited, and — because `coalesce_same_file` requires the staged path set to match the prior commit's
// exactly — a swap file's create/remove cycle interleaved with the real file's changes means the two path
// sets never repeat, so `coalesce_same_file` never fires and every debounce cycle becomes its own commit.
// Covers vim (`.swp`/`.swo`/`.swx`/`.un~`), Kate (`.kate-swp`, matched with or without the leading dot since
// `glob_match`'s `*` is not dot-excluding), Emacs (`#*#` auto-save, `.#*` lock symlink), and the generic `~`
// backup suffix several editors share (gedit and others).
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "*.swp",
    "*.swo",
    "*.swx",
    "*.un~",
    "*.kate-swp",
    "#*#",
    ".#*",
    "*~",
];

impl Default for Config {
    fn default() -> Self {
        Config {
            debounce_ms: 1000,
            finalize_numbering: false,
            stage: StageMode::Observed,
            coalesce_same_file: true,
            ignore_patterns: DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            cherry_size_limit_mb: 32,
            push_guard: false,
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
                "cherry_size_limit_mb" => {
                    cfg.cherry_size_limit_mb = value.parse().map_err(|_| {
                        format!(
                            "gitomic.cfg line {}: cherry_size_limit_mb must be an integer",
                            lineno + 1
                        )
                    })?;
                }
                "finalize_numbering" => cfg.finalize_numbering = parse_bool(value, lineno + 1)?,
                "stage" => cfg.stage = parse_stage(value, lineno + 1)?,
                "coalesce_same_file" => cfg.coalesce_same_file = parse_bool(value, lineno + 1)?,
                "push_guard" => cfg.push_guard = parse_bool(value, lineno + 1)?,
                // Comma-separated glob patterns, extending (never replacing) DEFAULT_IGNORE_PATTERNS. A blank
                // entry from stray comma placement (",," or a trailing comma) is dropped rather than becoming
                // a pattern that matches everything.
                "ignore_patterns" => cfg.ignore_patterns.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                ),
                // Reserved for a future always-on multi-repository mode; accepted and ignored so an
                // aspirational configuration file does not break the current cwd-scoped tool.
                "watch_dir" => {}
                _ => {}
            }
        }
        Ok(cfg)
    }
}

// Matches `name` against a glob pattern restricted to the `*` wildcard (matches any sequence of characters,
// including none). This is the entire vocabulary `ignore_patterns` needs — editor swap/backup conventions are
// prefix/suffix shapes, never character classes or `?` — so a hand-rolled matcher keeps the dependency budget
// at `libc` + `notify` rather than pulling in a glob crate for one wildcard. `*` is not dot-excluding here,
// unlike a shell glob: "*.kate-swp" matches both "file.kate-swp" and ".file.kate-swp", which is the desired
// behaviour since Kate's swap file for a dotfile would otherwise need its own pattern.
//
// Standard two-pointer wildcard match: `star` records the most recent `*` seen in the pattern and `star_ni`
// the text position it last matched from, so a mismatch can backtrack by re-trying that `*` against one more
// character of text instead of full recursion.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ni = 0usize;
    while ni < n.len() {
        if pi < p.len() && p[pi] == n[ni] {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
        assert_eq!(
            c.ignore_patterns,
            DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_cherry_size_limit_defaults_to_32_and_can_be_set_or_turned_off() {
        assert_eq!(Config::default().cherry_size_limit_mb, 32);
        assert_eq!(
            Config::parse("cherry_size_limit_mb = 8")
                .unwrap()
                .cherry_size_limit_mb,
            8
        );
        assert_eq!(
            Config::parse("cherry_size_limit_mb = 0")
                .unwrap()
                .cherry_size_limit_mb,
            0
        );
        assert!(Config::parse("cherry_size_limit_mb = big").is_err());
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

    #[test]
    fn ignore_patterns_extend_rather_than_replace_defaults() {
        let c = Config::parse("ignore_patterns = *.bak, build/*.o\n").unwrap();
        assert_eq!(c.ignore_patterns.len(), DEFAULT_IGNORE_PATTERNS.len() + 2);
        assert!(c.ignore_patterns.contains(&"*.kate-swp".to_string()));
        assert!(c.ignore_patterns.contains(&"*.bak".to_string()));
        assert!(c.ignore_patterns.contains(&"build/*.o".to_string()));
    }

    #[test]
    fn ignore_patterns_drops_blank_entries() {
        let c = Config::parse("ignore_patterns = *.bak,,  ,\n").unwrap();
        assert_eq!(c.ignore_patterns.len(), DEFAULT_IGNORE_PATTERNS.len() + 1);
    }

    #[test]
    fn glob_match_handles_leading_and_trailing_star() {
        assert!(glob_match("*.kate-swp", "install.sh.kate-swp"));
        assert!(glob_match("*.kate-swp", ".install.sh.kate-swp"));
        assert!(!glob_match("*.kate-swp", "install.sh.kate-swpx"));
        assert!(glob_match("#*#", "#scratch.el#"));
        assert!(glob_match(".#*", ".#scratch.el"));
        assert!(glob_match("*~", "notes.txt~"));
        assert!(!glob_match("*~", "notes.txt"));
        assert!(glob_match("*", "anything.at.all"));
    }
}
