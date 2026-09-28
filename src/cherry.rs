// `gitomic cherry-pick` (issue #13): bring commits from another branch onto the checked-out branch
// by way of a patch file.
//
// The chosen commits are replayed onto the current HEAD in the object database (see patch.rs), the
// result is written as a patch file, and that file is applied to the work tree. History is never
// rewritten, which is what keeps the operation compatible with a live session and with commits that
// are not yet pushed: HEAD only ever moves forward, and only through gitomic's own capture.
//
// Outcome by situation:
//   - No session on the branch: the change is left in the work tree, uncommitted (the effect of
//     `git cherry-pick -n`). `gitomic init` afterwards, or an ordinary commit, records it.
//   - A session (live or stopped): the change is recorded as one atomic commit with the usual empty
//     placeholder message, so it is stamped by `finish` like any other step. A live watcher is
// stopped
//     for the duration, exactly as `drop` does, and restarted afterwards.
//
// If HEAD moved between choosing the commits and applying them (the watcher committing in the
// meantime, or a commit made by hand), the patch is recomputed against the new HEAD with the
// decisions already taken; a conflict that has not been decided stops the operation before anything
// is changed.
//
// Interactive use is `gitomic cherry-pick` with no commit (cherry_ui.rs). With commits named, the
// same pipeline runs without a terminal and refuses any conflict.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::commands::{live_watcher, report_flush, short, terminate_watcher};
use crate::config::Config;
use crate::patch::{self, Built, Merged, Spec};
use crate::{commands, git, Res};

// Cap on the commits offered from one branch, so a branch with a long history does not make the
// listing slow. The picker states when the cap was reached.
pub const MAX_CANDIDATES: usize = 500;

pub struct Opts {
    // Restrict every named commit to this one path (see `patch::Spec::only`).
    pub only: Option<String>,
    pub from: Option<String>,
    pub hashes: Vec<String>,
    pub dry_run: bool,
    pub patch_only: bool,
}

// Branches a commit can be picked from: local branches, then remote-tracking ones, most recently
// updated first, without the checked-out branch and without remote HEAD aliases.
pub fn branches(cwd: &Path) -> Res<Vec<String>> {
    let root = git::work_tree(cwd)?;
    let current = git::current_branch(&root).ok();
    let mut out = Vec::new();
    for (prefix, strip) in [("refs/heads", 2), ("refs/remotes", 2)] {
        let list = git::run(
            &root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                &format!("--format=%(refname:lstrip={strip})"),
                prefix,
            ],
        )?;
        for name in list.lines().filter(|l| !l.is_empty()) {
            if name.ends_with("/HEAD") || Some(name) == current.as_deref() {
                continue;
            }
            out.push(name.to_string());
        }
    }
    Ok(out)
}

// Commits on `source` that HEAD lacks, newest first, as (full id, subject). Merge commits are left
// out, since replaying one needs a chosen parent, and so are commits whose change HEAD already
// holds under another id (`--cherry-pick`), which is what a repeated pick would otherwise offer
// again.
pub fn candidates(cwd: &Path, source: &str) -> Res<Vec<(String, String)>> {
    let root = git::work_tree(cwd)?;
    let source = git::rev_parse(&root, &format!("{source}^{{commit}}"))
        .map_err(|_| format!("cherry-pick: '{source}' does not name a commit"))?;
    let out = git::run(
        &root,
        &[
            "log",
            "--cherry-pick",
            "--right-only",
            "--no-merges",
            &format!("--max-count={MAX_CANDIDATES}"),
            "--format=%H%x00%s",
            &format!("HEAD...{source}"),
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\0'))
        .map(|(id, subject)| (id.to_string(), subject.trim().to_string()))
        .collect())
}

// Paths changed by `commit` (against its first parent), sorted as git lists them.
pub fn changed_paths(root: &Path, commit: &str) -> Res<Vec<String>> {
    let out = git::run(
        root,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-z",
            commit,
        ],
    )?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect())
}

// The commits of `source` that HEAD lacks and that change `path`, as full ids (unordered). The
// same range as `candidates`, so every id returned is a row of the picker's list. History is not
// simplified, and renames are not followed: a file is followed under the name it has at each
// commit.
pub fn touching(cwd: &Path, source: &str, path: &str) -> Res<Vec<String>> {
    let root = git::work_tree(cwd)?;
    let out = git::run(
        &root,
        &[
            "rev-list",
            "--cherry-pick",
            "--right-only",
            "--no-merges",
            "--full-history",
            &format!("--max-count={MAX_CANDIDATES}"),
            &format!("HEAD...{source}"),
            "--",
            &format!(":(literal){path}"),
        ],
    )?;
    Ok(out.lines().map(str::to_string).collect())
}

// Whether replaying `pick` onto `base` would change anything. A commit whose change the branch
// already holds in another form (the same edit made by hand, or a later commit that supersedes it)
// merges to exactly the tree of `base` and reports false. A conflict, or a conflict the picker
// cannot present, reports true: those commits need the user's attention and stay selectable.
// Kept for the tests, which check the picker's `Reader` against it.
#[cfg(test)]
pub fn applies(root: &Path, base: &str, pick: &str) -> Res<bool> {
    match patch::merge(root, base, pick)? {
        Merged::Clean(tree) => {
            let current = git::rev_parse(root, &format!("{base}^{{tree}}"))?;
            Ok(tree != current)
        }
        Merged::Conflicted { .. } | Merged::Unsupported(_) => Ok(true),
    }
}

// Cap on the text of one preview, so that a commit touching a huge generated file is stopped while
// git is still producing it. The cut is announced in the text.
pub const MAX_PREVIEW_BYTES: usize = 8 * 1024 * 1024;

// Cap on the commit header shown above a preview.
const MAX_HEADER_BYTES: usize = 1024 * 1024;

// A changed file that a preview did not read because it exceeds the size limit.
#[derive(Clone, PartialEq, Debug)]
pub struct Gated {
    pub path: String,
    // The larger of the file's size before and after the commit.
    pub bytes: u64,
}

// What the picker shows for one commit, and which of its files were left unread.
pub struct Preview {
    pub text: String,
    pub gated: Vec<Gated>,
}

// A size in bytes as a short human-readable string, in powers of 1024.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// The files `pick` changes that exceed `limit` bytes (the larger of the size before and after),
// found without reading any of them: `diff-tree` names the blobs and `cat-file` gives their sizes
// from the object headers.
pub fn oversized(
    root: &Path,
    pick: &str,
    limit: u64,
    cancel: Option<&git::Cancel>,
) -> Res<Vec<Gated>> {
    let raw = git::run_capped(
        root,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--no-renames",
            "--no-commit-id",
            "--no-abbrev",
            "--raw",
            "-z",
            pick,
        ],
        MAX_HEADER_BYTES,
        cancel,
    )?;
    // Entries alternate: ":<mode> <mode> <old id> <new id> <status>" and then the path.
    let mut changes: Vec<(String, String, String)> = Vec::new();
    let mut fields = raw.text.split('\0');
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let parts: Vec<&str> = meta.trim_start_matches(':').split_whitespace().collect();
        if let [old_mode, new_mode, old, new, _status] = parts[..] {
            // A submodule entry names a commit of another repository, not a blob here.
            if old_mode == "160000" || new_mode == "160000" {
                continue;
            }
            changes.push((path.to_string(), old.to_string(), new.to_string()));
        }
    }
    let mut ids: Vec<String> = Vec::new();
    for (_, old, new) in &changes {
        for id in [old, new] {
            if id.bytes().any(|b| b != b'0') && !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    let sizes = git::blob_sizes(root, &ids)?;
    let size_of = |id: &str| -> u64 {
        ids.iter()
            .position(|i| i == id)
            .and_then(|at| sizes[at])
            .unwrap_or(0)
    };
    Ok(changes
        .into_iter()
        .filter_map(|(path, old, new)| {
            let bytes = size_of(&old).max(size_of(&new));
            (bytes > limit).then_some(Gated { path, bytes })
        })
        .collect())
}

// The text the picker shows for one commit: its header and message, then what applying it to `base`
// would change. A conflict is announced ahead of the diff, whose conflicted files show the markers
// that the decision screen later resolves.
#[cfg(test)]
pub fn preview(root: &Path, base: &str, pick: &str, only: Option<&str>) -> Res<String> {
    preview_with(root, base, pick, only, &Hints::default()).map(|p| p.text)
}

// What `preview_with` may use beyond the commit itself.
#[derive(Default)]
pub struct Hints<'a> {
    // A size in bytes above which a changed file is not read. Such files are named, with their
    // sizes, in the text and in `Preview::gated`; None reads everything.
    pub limit: Option<u64>,
    // The tree already obtained by merging `pick` onto `base`, which saves repeating the merge.
    // Ignored for a pick restricted to one file, whose merge is a different one.
    pub known_tree: Option<&'a str>,
    // The files of `pick` over `limit`, when they are already known.
    pub oversized: Option<Vec<Gated>>,
    // Lets another thread abandon the work between and during its git commands.
    pub cancel: Option<&'a git::Cancel>,
}

// `preview` with the refinements the interactive screen needs (see `Hints`).
//
// A commit that changes a file over the limit is not merged onto `base`: merging reads the large
// file on both sides, which is the cost the limit exists to avoid. It is shown as the commit made
// it, with the large files named, and reading them (`D`) shows the change relative to `base`.
pub fn preview_with(
    root: &Path,
    base: &str,
    pick: &str,
    only: Option<&str>,
    hints: &Hints,
) -> Res<Preview> {
    let cancel = hints.cancel;
    let header = git::run_capped(
        root,
        &[
            "show",
            "-s",
            "--no-color",
            "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
            pick,
        ],
        MAX_HEADER_BYTES,
        cancel,
    )?;
    let mut text = header.text;
    text.push_str("\n\n");
    // A pick restricted to one file is previewed as its restricted form, which is what would apply.
    let effective = match only {
        Some(path) => {
            text.push_str(&format!(
                "Applied for {path} only; other files of this commit are left out.\n\n"
            ));
            patch::scoped_commit(root, pick, path)?
        }
        None => pick.to_string(),
    };
    if cancel.is_some_and(git::Cancel::is_cancelled) {
        return Err(git::CANCELLED.into());
    }
    if let Some(limit) = hints.limit {
        let big = match &hints.oversized {
            Some(known) if only.is_none() => known.clone(),
            _ => oversized(root, &effective, limit, cancel)?,
        };
        if !big.is_empty() {
            text.push_str(&format!(
                "Not read (over {}); press D to load this commit in full:\n",
                human(limit)
            ));
            for g in &big {
                text.push_str(&format!("  {}  {}\n", g.path, human(g.bytes)));
            }
            text.push_str(
                "\nBecause of them, what this commit would change relative to HEAD is not\n\
                 worked out: it is shown as it was made. D reads the files and shows\n\
                 the change to HEAD.\n\n",
            );
            let threshold = format!("core.bigFileThreshold={limit}");
            let own = git::run_capped(
                root,
                &[
                    "-c",
                    &threshold,
                    "show",
                    "--stat",
                    "--patch",
                    "--format=",
                    "--no-color",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    &effective,
                ],
                MAX_PREVIEW_BYTES,
                cancel,
            )?;
            push_diff(&mut text, &own);
            return Ok(Preview { text, gated: big });
        }
    }
    let tree = match (only, hints.known_tree) {
        (None, Some(tree)) => tree.to_string(),
        _ => match patch::merge_cancellable(root, base, &effective, cancel)? {
            Merged::Clean(tree) => tree,
            Merged::Conflicted { tree, files } => {
                text.push_str("CONFLICT: applying this commit needs a decision in\n");
                for f in &files {
                    text.push_str(&format!("  {} ({} to decide)\n", f.path, f.units()));
                }
                text.push_str(
                    "Enter on a marked selection opens the decision screen for each.\n\n",
                );
                tree
            }
            Merged::Unsupported(list) => {
                text.push_str("CANNOT BE PICKED here: the conflict has no A/B decision.\n");
                for l in list {
                    text.push_str(&format!("  {l}\n"));
                }
                return Ok(Preview {
                    text,
                    gated: Vec::new(),
                });
            }
        },
    };
    let diff = git::run_capped(
        root,
        &[
            "diff",
            "--stat",
            "--patch",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base,
            &tree,
        ],
        MAX_PREVIEW_BYTES,
        cancel,
    )?;
    if diff.text.is_empty() {
        text.push_str("(this commit changes nothing relative to the current HEAD)\n");
    } else {
        push_diff(&mut text, &diff);
    }
    Ok(Preview {
        text,
        gated: Vec::new(),
    })
}

// Append git's diff output, and the notice that it was cut when it was.
fn push_diff(text: &mut String, diff: &git::Capped) {
    text.push_str(&diff.text);
    text.push('\n');
    if diff.truncated {
        text.push_str(&format!(
            "... output cut after {} of diff text\n",
            human(MAX_PREVIEW_BYTES as u64)
        ));
    }
}

// Everything the interactive screen reads from git, gathered so that it can be cloned and used from
// several threads. Whatever one thread learns is shared: a merge is computed once per commit, and
// the base tree once.
#[derive(Clone)]
pub struct Reader {
    root: PathBuf,
    base: String,
    // Size limit in bytes for `preview`; 0 disables it.
    limit: u64,
    shared: Arc<Mutex<Known>>,
}

#[derive(Default)]
struct Known {
    base_tree: Option<String>,
    // Tree produced by merging a commit onto the base, for commits that merge cleanly. A conflicted
    // merge is not kept: its files carry whole file bodies, and those commits are seldom revisited.
    clean: HashMap<String, String>,
    // The files over the size limit that a commit changes.
    oversized: HashMap<String, Vec<Gated>>,
}

impl Reader {
    pub fn new(root: PathBuf, base: String, limit_bytes: u64) -> Reader {
        Reader {
            root,
            base,
            limit: limit_bytes,
            shared: Arc::new(Mutex::new(Known::default())),
        }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, Known> {
        // A panic in another thread must not turn every later lookup into a panic as well.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn base_tree(&self) -> Res<String> {
        let cached = self.known().base_tree.clone();
        if let Some(t) = cached {
            return Ok(t);
        }
        let tree = git::rev_parse(&self.root, &format!("{}^{{tree}}", self.base))?;
        self.known().base_tree = Some(tree.clone());
        Ok(tree)
    }

    // The files `pick` changes that are over the size limit; empty when the limit is off.
    fn big(&self, pick: &str, cancel: Option<&git::Cancel>) -> Res<Vec<Gated>> {
        if self.limit == 0 {
            return Ok(Vec::new());
        }
        let cached = self.known().oversized.get(pick).cloned();
        if let Some(found) = cached {
            return Ok(found);
        }
        let found = oversized(&self.root, pick, self.limit, cancel)?;
        self.known()
            .oversized
            .insert(pick.to_string(), found.clone());
        Ok(found)
    }

    // Same answer as the free function `applies`, with the merge remembered for `preview`. A commit
    // that changes a file over the size limit is not merged, since that reads the file; it is
    // reported as applying, which keeps it reachable.
    pub fn applies(&self, pick: &str) -> Res<bool> {
        self.applies_with(pick, None)
    }

    // `applies`, abandonable through `cancel` (the error is then `git::CANCELLED`).
    pub fn applies_with(&self, pick: &str, cancel: Option<&git::Cancel>) -> Res<bool> {
        if !self.big(pick, cancel)?.is_empty() {
            return Ok(true);
        }
        // The lock is released before `base_tree` takes it again; the guard of an `if let`
        // scrutinee would otherwise live through the whole body.
        let cached = self.known().clean.get(pick).cloned();
        if let Some(tree) = cached {
            return Ok(tree != self.base_tree()?);
        }
        match patch::merge_cancellable(&self.root, &self.base, pick, cancel)? {
            Merged::Clean(tree) => {
                let changes = tree != self.base_tree()?;
                self.known().clean.insert(pick.to_string(), tree);
                Ok(changes)
            }
            Merged::Conflicted { .. } | Merged::Unsupported(_) => Ok(true),
        }
    }

    // `preview_with` for this reader's base and size limit. `full` reads every file regardless of
    // the limit.
    pub fn preview(
        &self,
        pick: &str,
        only: Option<&str>,
        full: bool,
        cancel: Option<&git::Cancel>,
    ) -> Res<Preview> {
        let limit = (!full && self.limit > 0).then_some(self.limit);
        let known = self.known().clean.get(pick).cloned();
        let oversized = match (limit, only) {
            (Some(_), None) => Some(self.big(pick, cancel)?),
            _ => None,
        };
        preview_with(
            &self.root,
            &self.base,
            pick,
            only,
            &Hints {
                limit,
                known_tree: known.as_deref(),
                oversized,
                cancel,
            },
        )
    }
}

// Entry point for the command.
pub fn run(cwd: &Path, opts: Opts) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    if git::operation_in_progress(&git_dir) {
        return Err(
            "cherry-pick: a merge, rebase, cherry-pick, revert, or bisect is in progress".into(),
        );
    }
    let spec = if opts.hashes.is_empty() {
        match crate::cherry_ui::run(cwd, opts.from.as_deref())? {
            Some(spec) => spec,
            None => {
                println!("gitomic: nothing picked");
                return Ok(());
            }
        }
    } else {
        spec_from_hashes(&root, &opts.hashes, opts.only.as_deref())?
    };
    apply_spec(cwd, spec, opts.dry_run, opts.patch_only)
}

// Resolve command-line selectors into a spec against the current HEAD. The order given is the order
// of replay. A commit already contained in HEAD, a merge commit, or a root commit is refused.
fn spec_from_hashes(root: &Path, hashes: &[String], only: Option<&str>) -> Res<Spec> {
    let base = git::rev_parse(root, "HEAD")?;
    let mut picks: Vec<(String, String)> = Vec::new();
    for sel in hashes {
        let id = git::rev_parse(root, &format!("{sel}^{{commit}}"))
            .map_err(|_| format!("cherry-pick: '{sel}' does not name a single commit"))?;
        if picks.iter().any(|(p, _)| *p == id) {
            continue;
        }
        if git::succeeds(root, &["merge-base", "--is-ancestor", &id, "HEAD"])? {
            return Err(format!("cherry-pick: {} is already contained in HEAD", short(&id)).into());
        }
        let parents = git::run(root, &["show", "-s", "--format=%P", &id])?;
        match parents.split_whitespace().count() {
            0 => return Err(format!("cherry-pick: {} is a root commit", short(&id)).into()),
            1 => {}
            _ => return Err(format!("cherry-pick: {} is a merge commit", short(&id)).into()),
        }
        let subject = git::run(root, &["show", "-s", "--format=%s", &id])?;
        picks.push((id, subject));
    }
    let only = match only {
        Some(path) => {
            for (id, _) in &picks {
                if !changed_paths(root, id)?.iter().any(|p| p == path) {
                    return Err(format!("cherry-pick: {} does not change {path}", short(id)).into());
                }
            }
            picks
                .iter()
                .map(|(id, _)| (id.clone(), path.to_string()))
                .collect()
        }
        None => Vec::new(),
    };
    Ok(Spec {
        base,
        picks,
        only,
        ..Spec::default()
    })
}

// Apply `spec` to the checked-out branch: stop a live watcher, build (or rebuild) the patch against
// the HEAD that exists once the watcher is quiet, write and apply it, record it when a session is
// open, and restart the watcher.
pub fn apply_spec(cwd: &Path, spec: Spec, dry_run: bool, patch_only: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root).ok();
    if git::operation_in_progress(&git_dir) {
        return Err(
            "cherry-pick: a merge, rebase, cherry-pick, revert, or bisect is in progress".into(),
        );
    }

    let writes = !dry_run && !patch_only;
    let was_live = writes
        && branch
            .as_deref()
            .is_some_and(|b| live_watcher(&git_dir, b).is_some());
    if was_live {
        let branch = branch.as_deref().unwrap_or_default();
        let cfg = Config::load()?;
        terminate_watcher(&git_dir, branch)?;
        report_flush(&root, &git_dir, branch, &cfg);
    }

    let result = apply_locked(
        &root,
        &git_dir,
        branch.as_deref(),
        spec,
        dry_run,
        patch_only,
    );

    if was_live {
        if let Err(e) = commands::init(cwd, false, false) {
            eprintln!("gitomic: cherry-pick: watcher could not be restarted: {e}");
        }
    }
    result
}

fn apply_locked(
    root: &Path,
    git_dir: &Path,
    branch: Option<&str>,
    spec: Spec,
    dry_run: bool,
    patch_only: bool,
) -> Res<()> {
    let head = git::rev_parse(root, "HEAD")?;
    let built = if head == spec.base {
        patch::build(root, &spec)?
    } else {
        println!(
            "gitomic: HEAD moved from {} to {} since the selection; recomputing the patch",
            short(&spec.base),
            short(&head)
        );
        patch::rebuild(root, &spec, &head)?
    };
    let patch = match built {
        Built::Ready(p) => *p,
        Built::Conflicts(files) => {
            let names: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
            return Err(format!(
                "cherry-pick: conflicts need a decision in {}; run 'gitomic cherry-pick' without \
                 commit ids to resolve them interactively. Nothing was modified.",
                names.join(", ")
            )
            .into());
        }
        Built::Unsupported(list) => {
            return Err(format!(
                "cherry-pick: a conflict without an A/B decision was found ({}); nothing was \
                 modified. Use 'git cherry-pick' for this one.",
                list.join("; ")
            )
            .into());
        }
    };

    let verb = if dry_run { "would pick" } else { "picking" };
    for (id, subject) in &patch.spec.picks {
        let subject = if subject.is_empty() {
            "(no message)"
        } else {
            subject
        };
        println!("gitomic: {verb} {} {subject}", short(id));
    }
    for (id, path) in &patch.spec.restore {
        println!("gitomic: {verb} {path} restored whole from {}", short(id));
    }
    if patch.is_empty() {
        println!("  the change is already present in HEAD; nothing to apply");
        return Ok(());
    }
    if dry_run {
        println!("gitomic: dry run; nothing was modified");
        print!("{}", patch.stat);
        return Ok(());
    }

    let file = patch::write(&patch::patch_dir(git_dir), &patch)?;
    if patch_only {
        println!("gitomic: patch written to {}", file.display());
        println!("  apply it with: git apply {}", file.display());
        return Ok(());
    }
    patch::check(root, &file).map_err(|e| {
        format!(
            "cherry-pick: the patch does not apply to the work tree ({e}); commit or stash local \
             edits to the files involved. Nothing was modified. The patch is kept at {}",
            file.display()
        )
    })?;
    patch::apply(root, &file)?;
    print!("{}", patch.stat);

    let in_session = match branch {
        Some(b) => git::rev_exists(root, &git::base_ref(b))?,
        None => false,
    };
    if in_session {
        let paths: Vec<&str> = patch.paths.iter().map(String::as_str).collect();
        match git::commit_only(root, &paths)? {
            Some(sha) => println!("gitomic: recorded as {}", short(&sha)),
            None => println!("gitomic: the patch left no change to record"),
        }
        println!(
            "  undo with: gitomic drop {}",
            short(&git::rev_parse(root, "HEAD")?)
        );
    } else {
        println!("gitomic: applied to the work tree; the change is not committed");
        println!("  undo with: git apply -R {}", file.display());
    }
    println!("  patch file: {}", file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;

    fn opts(hashes: &[&str]) -> Opts {
        Opts {
            only: None,
            from: None,
            hashes: hashes.iter().map(|h| h.to_string()).collect(),
            dry_run: false,
            patch_only: false,
        }
    }

    // `main` holds a.txt and c.txt; `feat` (branched earlier) adds b.txt in one commit and edits
    // f.txt in another. Returns (b.txt commit, f.txt commit). HEAD ends on `main`.
    fn setup() -> (Repo, String, String) {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.write("f.txt", &Repo::lines());
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        let b = r.commit_file("b.txt", "b\n", "add b");
        let f = r.commit_file("f.txt", &Repo::lines_with(&[(3, "FEAT3")]), "edit f");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("c.txt", "c\n", "add c");
        (r, b, f)
    }

    fn patch_files(r: &Repo) -> Vec<String> {
        std::fs::read_dir(patch::patch_dir(&r.0.join(".git")))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn without_a_session_the_change_is_left_uncommitted() {
        let (r, b, _) = setup();
        let head = r.head();

        run(&r.0, opts(&[&b[..10]])).unwrap();

        assert_eq!(r.head(), head, "history is not rewritten or extended");
        assert_eq!(r.read("b.txt"), "b\n");
        assert_eq!(r.git(&["status", "--porcelain"]), "?? b.txt");
        assert_eq!(patch_files(&r).len(), 1);
    }

    #[test]
    fn several_commits_apply_in_the_order_given() {
        let (r, b, f) = setup();
        run(&r.0, opts(&[&b, &f])).unwrap();
        assert_eq!(r.read("b.txt"), "b\n");
        assert_eq!(r.read("f.txt"), Repo::lines_with(&[(3, "FEAT3")]));
        // One patch for the whole selection.
        assert_eq!(patch_files(&r).len(), 1);
    }

    #[test]
    fn a_conflict_is_refused_on_the_command_line_and_nothing_changes() {
        let (r, _, f) = setup();
        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        let head = r.head();

        let err = run(&r.0, opts(&[&f])).unwrap_err().to_string();

        assert!(err.contains("f.txt"), "{err}");
        assert!(err.contains("decision"), "{err}");
        assert_eq!(r.head(), head);
        assert_eq!(r.read("f.txt"), Repo::lines_with(&[(3, "MAIN3")]));
        assert_eq!(r.git(&["status", "--porcelain"]), "");
        assert!(patch_files(&r).is_empty());
    }

    #[test]
    fn dry_run_and_patch_only_do_not_touch_the_work_tree() {
        let (r, b, _) = setup();
        let mut o = opts(&[&b]);
        o.dry_run = true;
        run(&r.0, o).unwrap();
        assert!(!r.exists("b.txt"));
        assert!(patch_files(&r).is_empty());

        let mut o = opts(&[&b]);
        o.patch_only = true;
        run(&r.0, o).unwrap();
        assert!(!r.exists("b.txt"));
        assert_eq!(patch_files(&r).len(), 1);
    }

    #[test]
    fn an_open_session_records_the_change_as_one_atomic_commit() {
        let (r, b, f) = setup();
        r.git(&["update-ref", "refs/gitomic/base/main", "HEAD"]);
        // A change staged by hand must stay out of the recorded commit.
        r.write("staged.txt", "s\n");
        r.git(&["add", "staged.txt"]);
        let head = r.head();

        run(&r.0, opts(&[&b, &f])).unwrap();

        assert_eq!(
            r.git(&["rev-list", "--count", &format!("{head}..HEAD")]),
            "1"
        );
        assert_eq!(r.git(&["log", "-1", "--format=%s"]), "");
        let files = r.git(&["show", "--name-only", "--format=", "HEAD"]);
        let mut names: Vec<&str> = files.lines().collect();
        names.sort();
        assert_eq!(names, ["b.txt", "f.txt"]);
        assert_eq!(r.git(&["diff", "--cached", "--name-only"]), "staged.txt");
        assert_eq!(r.git(&["status", "--porcelain"]), "A  staged.txt");
    }

    #[test]
    fn an_uncommitted_edit_to_a_touched_file_blocks_the_pick() {
        let (r, _, f) = setup();
        r.write("f.txt", "edited locally\n");

        let err = run(&r.0, opts(&[&f])).unwrap_err().to_string();

        assert!(err.contains("does not apply"), "{err}");
        assert_eq!(r.read("f.txt"), "edited locally\n");
    }

    #[test]
    fn contained_merge_root_and_unknown_commits_are_refused() {
        let (r, b, _) = setup();
        let base = r.git(&["rev-parse", "HEAD~1"]);
        let err = run(&r.0, opts(&[&base])).unwrap_err().to_string();
        assert!(err.contains("already contained"), "{err}");

        let err = run(&r.0, opts(&["nonexistent"])).unwrap_err().to_string();
        assert!(err.contains("does not name"), "{err}");

        let root = r.git(&["rev-list", "--max-parents=0", "HEAD"]);
        r.git(&["checkout", "-q", "--orphan", "lone"]);
        r.git(&["rm", "-rfq", "."]);
        let lone = r.commit_file("z.txt", "z\n", "unrelated root");
        r.git(&["checkout", "-q", "main"]);
        let err = run(&r.0, opts(&[&lone])).unwrap_err().to_string();
        assert!(err.contains("root commit"), "{err}");
        assert_ne!(root, lone);

        r.git(&["checkout", "-q", "-b", "mergeme", &b]);
        r.git(&["merge", "-q", "--no-ff", "-m", "merge main", "main"]);
        let merge = r.head();
        r.git(&["checkout", "-q", "main"]);
        let err = run(&r.0, opts(&[&merge])).unwrap_err().to_string();
        assert!(err.contains("merge commit"), "{err}");
    }

    #[test]
    fn a_pick_in_progress_elsewhere_blocks_the_command() {
        let (r, b, _) = setup();
        std::fs::write(r.0.join(".git/CHERRY_PICK_HEAD"), &b).unwrap();
        let err = run(&r.0, opts(&[&b])).unwrap_err().to_string();
        assert!(err.contains("in progress"), "{err}");
    }

    #[test]
    fn a_selection_already_present_under_another_id_changes_nothing() {
        let (r, b, _) = setup();
        r.git(&["cherry-pick", &b]);
        let head = r.head();
        // The same change, offered again by hand: nothing to apply, nothing modified.
        run(&r.0, opts(&[&b])).unwrap();
        assert_eq!(r.head(), head);
        assert_eq!(r.git(&["status", "--porcelain"]), "");
    }

    #[test]
    fn head_moving_after_the_selection_recomputes_the_patch() {
        let (r, b, _) = setup();
        let spec = spec_from_hashes(&r.0, &[b], None).unwrap();
        let old = spec.base.clone();
        // A commit lands between choosing and applying, as a watcher capture would.
        r.commit_file("late.txt", "l\n", "late");
        assert_ne!(r.head(), old);

        apply_spec(&r.0, spec, false, false).unwrap();

        assert_eq!(r.read("b.txt"), "b\n");
        let saved = patch_files(&r);
        assert_eq!(saved.len(), 1);
        let text = std::fs::read(patch::patch_dir(&r.0.join(".git")).join(&saved[0])).unwrap();
        let header = Spec::parse(&text).unwrap();
        assert_eq!(
            header.base,
            r.head(),
            "the patch was rebuilt on the newer HEAD"
        );
    }

    #[test]
    fn decisions_in_a_spec_settle_a_conflict_without_a_terminal() {
        let (r, _, f) = setup();
        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        let spec = spec_from_hashes(&r.0, &[f.clone()], None).unwrap();

        // Decide through the engine, as the interactive screen does, then hand the spec over.
        let mut job = patch::Job::new(&r.0, &spec.base, &spec.ids(), &[]);
        let files = match job.advance().unwrap() {
            patch::Step::Conflicts(mut files) => {
                files[0].set(0, Some(crate::conflict::Side::B));
                files
            }
            _ => panic!("expected a conflict"),
        };
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), patch::Step::Done));
        let decided = job.patch(&spec.picks).unwrap().spec;

        apply_spec(&r.0, decided, false, false).unwrap();
        assert_eq!(r.read("f.txt"), Repo::lines_with(&[(3, "FEAT3")]));
    }

    #[test]
    fn applies_is_false_only_when_the_branch_already_holds_the_change() {
        let (r, b, f) = setup();
        let base = r.head();
        assert!(applies(&r.0, &base, &b).unwrap(), "a new file applies");
        assert!(applies(&r.0, &base, &f).unwrap(), "an edit applies");

        // The same edit made by hand under a different commit: patch-id differs by context, so the
        // commit stays listed, yet replaying it changes nothing.
        r.commit_file(
            "f.txt",
            &Repo::lines_with(&[(3, "FEAT3")]),
            "same edit by hand",
        );
        r.commit_file(
            "f.txt",
            &Repo::lines_with(&[(3, "FEAT3"), (9, "LATER")]),
            "then more",
        );
        let base = r.head();
        assert!(!applies(&r.0, &base, &f).unwrap(), "already present");
        assert!(
            applies(&r.0, &base, &b).unwrap(),
            "unrelated commit still applies"
        );
    }

    #[test]
    fn a_conflicting_commit_still_counts_as_applicable() {
        let (r, _, f) = setup();
        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        assert!(applies(&r.0, &r.head(), &f).unwrap());
    }

    #[test]
    fn candidates_skip_merges_and_changes_already_present() {
        let (r, b, f) = setup();
        r.git(&["cherry-pick", &b]);
        let got: Vec<String> = candidates(&r.0, "feat")
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            got,
            vec![f.clone()],
            "the already-picked change is not offered again"
        );

        let subjects: Vec<String> = candidates(&r.0, "feat")
            .unwrap()
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(subjects, ["edit f"]);

        assert!(candidates(&r.0, "no-such-branch").is_err());
    }

    #[test]
    fn picking_every_candidate_reproduces_the_branch_state_of_the_files() {
        // What `R` in the screen asks for: from the chosen commit down to the oldest the branch
        // holds that HEAD lacks, replayed in order, leaves the touched files as on the branch.
        let r = Repo::new();
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("f.txt", &Repo::lines_with(&[(2, "ONE")]), "one");
        r.commit_file("g.txt", "g\n", "add g");
        r.commit_file("f.txt", &Repo::lines_with(&[(2, "ONE"), (7, "TWO")]), "two");
        let tip = r.head();
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("other.txt", "o\n", "unrelated main work");

        let all: Vec<String> = candidates(&r.0, "feat")
            .unwrap()
            .into_iter()
            .rev()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(all.len(), 3);
        apply_spec(
            &r.0,
            spec_from_hashes(&r.0, &all, None).unwrap(),
            false,
            false,
        )
        .unwrap();

        for file in ["f.txt", "g.txt"] {
            assert_eq!(
                r.read(file),
                r.git(&["show", &format!("{tip}:{file}")]) + "\n"
            );
        }
        assert_eq!(r.read("other.txt"), "o\n");
    }

    #[test]
    fn candidates_are_newest_first() {
        let (r, b, f) = setup();
        let got: Vec<String> = candidates(&r.0, "feat")
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(got, vec![f, b]);
    }

    #[test]
    fn branches_exclude_the_current_branch_and_remote_head_aliases() {
        let (r, _, _) = setup();
        r.git(&["branch", "other"]);
        r.git(&["update-ref", "refs/remotes/origin/main", "main"]);
        r.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        let names = branches(&r.0).unwrap();
        assert!(names.contains(&"feat".to_string()));
        assert!(names.contains(&"other".to_string()));
        assert!(names.contains(&"origin/main".to_string()));
        assert!(!names.contains(&"main".to_string()));
        assert!(!names.iter().any(|n| n.ends_with("/HEAD")));
    }

    #[test]
    fn the_preview_shows_the_change_against_the_base_and_flags_conflicts() {
        let (r, b, f) = setup();
        let base = r.head();
        let clean = preview(&r.0, &base, &b, None).unwrap();
        assert!(clean.contains("add b"), "{clean}");
        assert!(clean.contains("+++ b/b.txt"), "{clean}");
        assert!(!clean.contains("CONFLICT"));

        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        let conflicted = preview(&r.0, &r.head(), &f, None).unwrap();
        assert!(conflicted.contains("CONFLICT"), "{conflicted}");
        assert!(conflicted.contains("f.txt (1 to decide)"), "{conflicted}");
    }

    // `feat` history (oldest first): m1 edits f.txt line 3 and adds other.txt (two files), g adds
    // g.txt, m3 edits f.txt line 7. Returns the three commit ids.
    fn chain() -> (Repo, [String; 3]) {
        let r = Repo::new();
        r.write("f.txt", &Repo::lines());
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.write("f.txt", &Repo::lines_with(&[(3, "FEAT3")]));
        r.write("other.txt", "o\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "f and other"]);
        let m1 = r.head();
        let g = r.commit_file("g.txt", "g\n", "add g");
        let m3 = r.commit_file(
            "f.txt",
            &Repo::lines_with(&[(3, "FEAT3"), (7, "FEAT7")]),
            "f 7",
        );
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("c.txt", "c\n", "add c");
        (r, [m1, g, m3])
    }

    #[test]
    fn changed_paths_and_touching_follow_one_file() {
        let (r, [m1, g, m3]) = chain();
        assert_eq!(
            changed_paths(&r.0, &m1).unwrap(),
            vec!["f.txt", "other.txt"]
        );
        assert_eq!(changed_paths(&r.0, &g).unwrap(), vec!["g.txt"]);
        let mut got = touching(&r.0, "feat", "f.txt").unwrap();
        got.sort();
        let mut want = vec![m1, m3];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(touching(&r.0, "feat", "g.txt").unwrap(), vec![g]);
        assert!(touching(&r.0, "feat", "nothing.txt").unwrap().is_empty());
    }

    #[test]
    fn a_file_chain_brings_the_file_to_the_branch_state_and_leaves_other_files_alone() {
        let (r, [m1, _, m3]) = chain();
        let mut o = opts(&[&m1, &m3]);
        o.only = Some("f.txt".to_string());
        run(&r.0, o).unwrap();
        assert_eq!(
            r.read("f.txt"),
            r.git(&["show", &format!("{m3}:f.txt")]) + "\n"
        );
        assert!(
            !r.exists("other.txt"),
            "the second file of m1 is not applied"
        );
        assert!(!r.exists("g.txt"));
    }

    #[test]
    fn only_names_a_path_the_commits_must_change_or_is_refused() {
        let (r, [m1, g, _]) = chain();
        let mut o = opts(&[&m1, &g]);
        o.only = Some("f.txt".to_string());
        let err = run(&r.0, o).unwrap_err().to_string();
        assert!(err.contains("f.txt"), "{err}");
        assert_eq!(r.git(&["status", "--porcelain"]), "");
    }

    #[test]
    fn a_whole_file_restore_replaces_a_history_that_cannot_be_replayed() {
        let (r, [_, g, m3]) = chain();
        // main now edits the same line, so replaying m3's history for f.txt would conflict.
        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        let base = r.head();
        let spec = Spec {
            base,
            picks: vec![(g.clone(), "add g".to_string())],
            restore: vec![(m3.clone(), "f.txt".to_string())],
            ..Spec::default()
        };
        apply_spec(&r.0, spec, false, false).unwrap();
        assert_eq!(
            r.read("f.txt"),
            r.git(&["show", &format!("{m3}:f.txt")]) + "\n"
        );
        assert_eq!(r.read("g.txt"), "g\n");
    }
    // `main` holds a.txt; `feat` adds a 3 MiB binary blob and a small text file in one commit.
    // Returns (repo, that commit).
    fn setup_big() -> (Repo, String) {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        let blob: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(r.0.join("rom.img"), blob).unwrap();
        r.write("small.txt", "small\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "add image"]);
        let c = r.head();
        r.git(&["checkout", "-q", "main"]);
        (r, c)
    }

    #[test]
    fn human_sizes_use_powers_of_1024() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(32 << 20), "32.0 MB");
        assert_eq!(human(5 << 30), "5.0 GB");
    }

    #[test]
    fn a_file_over_the_limit_is_named_not_read_and_the_rest_of_the_commit_still_shows() {
        let (r, c) = setup_big();
        let base = r.head();
        let p = preview_with(
            &r.0,
            &base,
            &c,
            None,
            &Hints {
                limit: Some(1 << 20),
                ..Hints::default()
            },
        )
        .unwrap();
        assert_eq!(
            p.gated,
            vec![Gated {
                path: "rom.img".into(),
                bytes: 3 << 20
            }]
        );
        assert!(p.text.contains("Not read (over 1.0 MB)"), "{}", p.text);
        assert!(p.text.contains("press D"), "{}", p.text);
        assert!(p.text.contains("rom.img  3.0 MB"), "{}", p.text);
        assert!(
            p.text.contains("+small"),
            "the small file is diffed: {}",
            p.text
        );

        let all = preview_with(&r.0, &base, &c, None, &Hints::default()).unwrap();
        assert!(all.gated.is_empty());
        assert!(!all.text.contains("Not read"));
        assert!(all.text.contains("rom.img"));
    }

    #[test]
    fn a_large_text_file_is_gated_by_size_alike() {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        let c = r.commit_file("big.txt", &"line of text\n".repeat(300_000), "big text");
        r.git(&["checkout", "-q", "main"]);
        let p = preview_with(
            &r.0,
            &r.head(),
            &c,
            None,
            &Hints {
                limit: Some(1 << 20),
                ..Hints::default()
            },
        )
        .unwrap();
        assert_eq!(p.gated.len(), 1, "{}", p.text);
        assert!(!p.text.contains("+line of text"), "not read");
    }

    #[test]
    fn a_preview_larger_than_the_cap_is_cut_and_says_so() {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        let c = r.commit_file("gen.txt", &"abcdefghij\n".repeat(1_200_000), "generated");
        r.git(&["checkout", "-q", "main"]);
        let p = preview_with(&r.0, &r.head(), &c, None, &Hints::default()).unwrap();
        let tail = &p.text[p.text.len().saturating_sub(80)..];
        assert!(p.text.contains("output cut after 8.0 MB"), "tail: {tail}");
        assert!(p.text.len() < MAX_PREVIEW_BYTES + 4096);
    }

    #[test]
    fn the_reader_gives_the_same_answers_and_remembers_the_merge() {
        let (r, b, f) = setup();
        let rd = Reader::new(r.0.clone(), r.head(), 0);
        assert!(rd.applies(&b).unwrap());
        assert!(rd.known().clean.contains_key(&b), "the merge is kept");
        assert!(rd.applies(&b).unwrap(), "answered from what is kept");
        assert!(rd.known().base_tree.is_some());

        let free = preview(&r.0, &r.head(), &b, None).unwrap();
        let via = rd.preview(&b, None, false, None).unwrap();
        assert_eq!(via.text, free);
        assert!(via.gated.is_empty(), "a limit of 0 gates nothing");
        // A commit already present in another form is reported as changing nothing.
        r.write("b.txt", "b\n");
        r.git(&["add", "b.txt"]);
        r.git(&["commit", "-q", "-m", "same edit by hand"]);
        let rd = Reader::new(r.0.clone(), r.head(), 0);
        assert!(!rd.applies(&b).unwrap());
        assert!(rd.applies(&f).unwrap());
    }

    #[test]
    fn a_cancelled_preview_stops_with_the_cancel_error() {
        let (r, c) = setup_big();
        let cancel = git::Cancel::default();
        cancel.cancel();
        let err = preview_with(
            &r.0,
            &r.head(),
            &c,
            None,
            &Hints {
                cancel: Some(&cancel),
                ..Hints::default()
            },
        )
        .err()
        .expect("cancelled");
        assert_eq!(err.to_string(), git::CANCELLED);
    }
    #[test]
    fn oversized_names_the_files_over_the_limit_from_their_headers_alone() {
        let (r, c) = setup_big();
        let big = oversized(&r.0, &c, 1 << 20, None).unwrap();
        assert_eq!(
            big,
            vec![Gated {
                path: "rom.img".into(),
                bytes: 3 << 20
            }]
        );
        assert!(oversized(&r.0, &c, 4 << 20, None).unwrap().is_empty());
        // A root commit is measured against nothing.
        let root = r.git(&["rev-list", "--max-parents=0", "HEAD"]);
        assert!(oversized(&r.0, &root, 1 << 20, None).unwrap().is_empty());
    }

    #[test]
    fn a_commit_with_a_large_file_is_not_merged_and_is_shown_as_it_was_made() {
        let (r, c) = setup_big();
        // The same edit already exists on main, so a merge would find that it changes nothing.
        std::fs::copy(r.0.join("small.txt"), r.0.join("small.txt")).ok();
        let rd = Reader::new(r.0.clone(), r.head(), 1 << 20);
        let p = rd.preview(&c, None, false, None).unwrap();
        assert!(p.text.contains("worked out"), "{}", p.text);
        assert!(
            p.text.contains("shown as it was made")
                || p.text.contains("it is shown as it was made"),
            "{}",
            p.text
        );
        assert!(
            p.text.contains("+small"),
            "the commit's own diff of the small file: {}",
            p.text
        );
        assert!(!p.text.contains("CONFLICT"));
        assert_eq!(p.gated.len(), 1);
        // With D the merge against HEAD is computed and nothing is withheld.
        let full = rd.preview(&c, None, true, None).unwrap();
        assert!(full.gated.is_empty());
        assert!(!full.text.contains("worked out"));
    }

    #[test]
    fn a_commit_with_a_large_file_reports_as_applying_without_a_merge() {
        let (r, c) = setup_big();
        // Put the very change on main by hand: with no limit the commit is found to change nothing.
        r.git(&["checkout", "-q", &c, "--", "rom.img", "small.txt"]);
        r.git(&["commit", "-q", "-m", "same change by hand"]);
        let open = Reader::new(r.0.clone(), r.head(), 0);
        assert!(!open.applies(&c).unwrap(), "already present");
        let limited = Reader::new(r.0.clone(), r.head(), 1 << 20);
        assert!(
            limited.applies(&c).unwrap(),
            "not merged, so it stays reachable"
        );
        assert!(
            !limited.known().clean.contains_key(&c),
            "no merge was made for it"
        );
    }

    #[test]
    fn blob_sizes_answer_in_order_and_mark_unknown_objects() {
        let (r, c) = setup_big();
        let big = r.git(&["rev-parse", &format!("{c}:rom.img")]);
        let small = r.git(&["rev-parse", &format!("{c}:small.txt")]);
        let missing = "1".repeat(40);
        let got = git::blob_sizes(&r.0, &[small, missing, big]).unwrap();
        assert_eq!(got, vec![Some(6), None, Some(3 << 20)]);
        assert!(git::blob_sizes(&r.0, &[]).unwrap().is_empty());
    }
}
