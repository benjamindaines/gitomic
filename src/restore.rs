// `gitomic restore` (issue #24): bring files from other branches into the work tree in the state
// they have there.
//
// A restore is an overwrite, not a merge: the chosen file is set to its exact content in a chosen
// commit, so there is nothing to decide and no conflict screen. The operation is expressed as a
// `patch::Spec` with no picks and one `restore` entry per file, which puts it on the pipeline that
// cherry-pick already uses (cherry::apply_spec): a patch computed against HEAD, `git apply
// --check`, a live watcher stopped and restarted around the change, and, when a session is open,
// one atomic commit restricted to the touched paths. Tracked files with pending changes are
// captured before the patch is applied (see cherry::apply_spec), so the state that is overwritten
// is always a commit.
//
// This module holds everything that needs no terminal:
//   - `Index`: the files in which several branches differ from HEAD, read lazily and cached, merged
//     into one `FileEntry` per path with a `Version` per branch that changes it;
//   - `Filter`: the query applied to that list, kept apart from the list itself so that a further
//     kind of search (file content, for example) can narrow the result through `hits` without
//     changing the index or the screen;
//   - `spec_for` and `run`: the recipe for the patch and the command-line entry point.
// The screen is in restore_ui.rs.
//
// Only the current state (the tip) of each branch is offered, and only for paths in which the tip
// differs from HEAD: a file that is identical to HEAD has nothing to restore, and listing it would
// make the cost of the screen grow with the size of the tree instead of with the size of the change.
// Every `Version` carries the commit it was read from, so history can later contribute further
// versions of the same path without changing the consumers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use crate::cherry;
use crate::conflict;
use crate::patch::{self, Spec};
use crate::{git, Res};

// A branch or stash a file can be restored from.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub name: String,
    pub remote: bool,
    // Shown beside the name in the source overlay: the message of a stash, empty for a branch.
    pub note: String,
}

// Whether a source name designates a stash entry (`stash@{N}`) rather than a branch.
pub fn is_stash(name: &str) -> bool {
    name.starts_with("stash@{") && name.ends_with('}')
}

impl Source {
    pub fn is_stash(&self) -> bool {
        is_stash(&self.name)
    }

    pub fn is_modified(&self) -> bool {
        is_modified(&self.name)
    }
}

// The revision a `--stash` argument names: a bare number N stands for `stash@{N}`, anything else is
// taken as given.
pub fn stash_ref(arg: &str) -> String {
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()) {
        format!("stash@{{{arg}}}")
    } else {
        arg.to_string()
    }
}

// Name of the pseudo-source that offers tracked files that are modified or deleted in the work
// tree and not staged. The name contains a space and parentheses, which no ref name can, so it cannot collide
// with a branch or stash. Its version of a file is the content in the index, i.e. the state
// `git restore <path>` returns the file to. Choosing one discards the edit in place (see
// `Spec::discard`); no commit is made and no session is needed.
pub const MODIFIED: &str = "(modified)";

// Whether a source name designates the modified-files pseudo-source rather than a branch or stash.
pub fn is_modified(name: &str) -> bool {
    name == MODIFIED
}

// One file as it is at the tip of one branch.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    // The branch the version was read from. Shared by all versions of one branch.
    pub source: Arc<str>,
    // The commit that branch pointed at when the index was read; previews and the patch use this
    // commit, not the branch name, so a branch that moves meanwhile cannot change what was shown.
    pub commit: Arc<str>,
    pub blob: String,
}

// How a file compares with HEAD.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    // HEAD has the file and the versions differ from it.
    Differs,
    // HEAD lacks the file; restoring adds it.
    Absent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub path: String,
    // One per selected branch that holds the file, in the order the branches were given.
    pub versions: Vec<Version>,
    // The file's blob at HEAD, if it has one (every listed version differs from it).
    pub head: Option<String>,
}

impl FileEntry {
    // How many different contents the branches hold. Above one the operator has to say which to
    // use.
    pub fn distinct(&self) -> usize {
        let mut seen: Vec<&str> = Vec::new();
        for v in &self.versions {
            if !seen.contains(&v.blob.as_str()) {
                seen.push(&v.blob);
            }
        }
        seen.len()
    }

    pub fn state(&self) -> State {
        match &self.head {
            None => State::Absent,
            Some(_) => State::Differs,
        }
    }
}

// The query applied to the file list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    // Whitespace-separated terms; a path must satisfy every one of them, ignoring case. A term
    // holding `*` or `?` is a glob that must match the whole file name (the whole path when the
    // term holds a `/`); any other term must occur somewhere in the path.
    pub text: String,
    // Keep only files git treats as binary.
    pub binary_only: bool,
}

// Whether `pattern` (with `*` for any run of characters and `?` for one) matches all of `text`.
fn glob_match(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    // Where the last `*` stood in the pattern, and how much of the text it has absorbed so far.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

// Whether the lowercase `term` accepts the lowercase `path`.
fn term_matches(term: &str, path: &str) -> bool {
    if !term.contains(['*', '?']) {
        return path.contains(term);
    }
    let target = if term.contains('/') {
        path
    } else {
        path.rsplit('/').next().unwrap_or(path)
    };
    let pattern: Vec<char> = term.chars().collect();
    let text: Vec<char> = target.chars().collect();
    glob_match(&pattern, &text)
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && !self.binary_only
    }

    // Indices into `entries` of the files that pass. `binary` is the set of binary paths (needed
    // only when `binary_only` is set); `hits`, when given, restricts the result to those paths and
    // is the place for a search that cannot be answered from the path alone.
    pub fn select(
        &self,
        entries: &[FileEntry],
        binary: &HashSet<String>,
        hits: Option<&HashSet<String>>,
    ) -> Vec<usize> {
        let terms: Vec<String> = self
            .text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        // One buffer for the lowercase form of every path, so a keystroke allocates once, not once
        // per file.
        let mut lower = String::new();
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if !terms.is_empty() {
                    lower.clear();
                    lower.extend(e.path.chars().flat_map(char::to_lowercase));
                    if !terms.iter().all(|t| term_matches(t, &lower)) {
                        return false;
                    }
                }
                (!self.binary_only || binary.contains(&e.path))
                    && hits.is_none_or(|h| h.contains(&e.path))
            })
            .map(|(i, _)| i)
            .collect()
    }
}

// One path in which a branch tip differs from HEAD.
struct Change {
    path: String,
    // The blob at the tip.
    blob: String,
    // The blob at HEAD; None when HEAD lacks the path.
    old: Option<String>,
    // The commit that holds the blob when that is not the tip itself: the commit a stash keeps its
    // untracked files in.
    from: Option<Arc<str>>,
}

// What a branch tip changes relative to HEAD. Branches whose trees are identical share the list.
struct Tip {
    commit: Arc<str>,
    changes: Arc<Vec<Change>>,
    // For a stash: the commit holding its untracked files, when it has any.
    untracked: Option<Arc<str>>,
}

// Run git and return stdout untouched; paths need not be UTF-8 or free of trailing white space.
fn git_bytes(dir: &Path, args: &[&str]) -> Res<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.join(" "), msg.trim()).into());
    }
    Ok(out.stdout)
}

// The paths in which `commit` differs from `head`, with added and modified files kept (a deleted
// file has no content to restore), submodules and paths that are not UTF-8 left out; the second
// value counts what was left out. Only the changed part of the two trees is read, so the cost
// follows the size of the difference, not of the tree.
fn read_changes(root: &Path, head: &str, commit: &str) -> Res<(Vec<Change>, usize)> {
    let raw = git_bytes(
        root,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "--no-renames",
            "--no-abbrev",
            head,
            commit,
        ],
    )?;
    Ok(parse_raw(&raw, None))
}

// Paths per git invocation when a pathspec list is passed on the command line.
const PATH_CHUNK: usize = 256;

// The same as `read_changes`, restricted to `paths`, which are matched literally. A stash tree is
// a snapshot of the whole work tree, so a comparison with HEAD would list everything that HEAD has
// gained since; limiting it to the paths the stash itself touched keeps the list to its own change.
fn read_changes_for(
    root: &Path,
    head: &str,
    commit: &str,
    paths: &[String],
    from: Option<&Arc<str>>,
) -> Res<(Vec<Change>, usize)> {
    let mut changes = Vec::new();
    let mut skipped = 0;
    for chunk in paths.chunks(PATH_CHUNK) {
        let mut args = vec![
            "--literal-pathspecs",
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "--no-renames",
            "--no-abbrev",
            head,
            commit,
            "--",
        ];
        args.extend(chunk.iter().map(String::as_str));
        let (list, left_out) = parse_raw(&git_bytes(root, &args)?, from);
        changes.extend(list);
        skipped += left_out;
    }
    Ok((changes, skipped))
}

// The NUL-separated names in `raw`, with the count of those that are not UTF-8.
fn parse_names(raw: &[u8]) -> (Vec<String>, usize) {
    let mut names = Vec::new();
    let mut skipped = 0;
    for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        match std::str::from_utf8(rec) {
            Ok(name) => names.push(name.to_string()),
            Err(_) => skipped += 1,
        }
    }
    (names, skipped)
}

// Decode the output of `git diff-tree --raw -z`. `from` is recorded on every change.
fn parse_raw(raw: &[u8], from: Option<&Arc<str>>) -> (Vec<Change>, usize) {
    let mut changes = Vec::new();
    let mut skipped = 0;
    let mut fields = raw.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(meta) = fields.next() {
        // A record is `:oldmode newmode oldblob newblob status`, then the path in its own field.
        let Some(path) = fields.next() else {
            break;
        };
        let (Ok(meta), Ok(path)) = (std::str::from_utf8(meta), std::str::from_utf8(path)) else {
            skipped += 1;
            continue;
        };
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old, new, status] = f[..] else {
            continue;
        };
        if status.starts_with('D') {
            continue;
        }
        if new_mode == "160000" || old_mode == "160000" {
            skipped += 1;
            continue;
        }
        let zero = |id: &str| id.bytes().all(|b| b == b'0');
        changes.push(Change {
            path: path.to_string(),
            blob: new.to_string(),
            old: if zero(old) {
                None
            } else {
                Some(old.to_string())
            },
            from: from.cloned(),
        });
    }
    (changes, skipped)
}

// The stashes of the repository, newest first, as sources named `stash@{N}`.
fn stashes(root: &Path) -> Res<Vec<Source>> {
    let listing = git::run(root, &["stash", "list", "--format=%gd%x1f%gs"])?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let (name, note) = line.split_once('\u{1f}')?;
            Some(Source {
                name: name.to_string(),
                remote: false,
                note: note.to_string(),
            })
        })
        .collect())
}

// The branches a file can come from: local ones first, then remote-tracking ones, without the
// checked-out branch; the stashes follow them.
pub fn sources(cwd: &Path) -> Res<Vec<Source>> {
    let root = git::work_tree(cwd)?;
    let local: HashSet<String> = git::run(
        &root,
        &["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"],
    )?
    .lines()
    .map(str::to_string)
    .collect();
    let mut out: Vec<Source> = cherry::branches(cwd)?
        .into_iter()
        .map(|name| Source {
            remote: !local.contains(&name),
            name,
            note: String::new(),
        })
        .collect();
    let (modified, _) = read_modified(&root)?;
    if !modified.is_empty() {
        out.push(Source {
            name: MODIFIED.to_string(),
            remote: false,
            note: format!(
                "{} with unstaged changes; restoring discards them for good",
                count_files(modified.len())
            ),
        });
    }
    out.extend(stashes(&root)?);
    Ok(out)
}

// "1 file" or "N files".
fn count_files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

// The tracked files whose work-tree content differs from the index, modified or deleted, as
// (path, blob in the index), with the count of entries left out (submodules and paths that are not
// UTF-8). Type changes are not listed: git reports them apart. The index blob, not the HEAD blob,
// is what a restore returns to, so that staged work survives.
fn read_modified(root: &Path) -> Res<(Vec<(String, String)>, usize)> {
    let raw = git_bytes(
        root,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            "--no-ext-diff",
            "--diff-filter=MD",
        ],
    )?;
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut fields = raw.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(meta) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let (Ok(meta), Ok(path)) = (std::str::from_utf8(meta), std::str::from_utf8(path)) else {
            skipped += 1;
            continue;
        };
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old, _new, _status] = f[..] else {
            continue;
        };
        if old_mode == "160000" || new_mode == "160000" {
            skipped += 1;
            continue;
        }
        out.push((path.to_string(), old.to_string()));
    }
    Ok((out, skipped))
}

// The blob each of `paths` has at `head`; paths that HEAD lacks are absent from the map.
fn head_blobs(root: &Path, head: &str, paths: &[String]) -> Res<HashMap<String, String>> {
    let mut map = HashMap::new();
    for chunk in paths.chunks(PATH_CHUNK) {
        let mut args = vec![
            "--literal-pathspecs",
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            head,
            "--",
        ];
        args.extend(chunk.iter().map(String::as_str));
        let raw = git_bytes(root, &args)?;
        for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            // `<mode> SP <type> SP <blob> TAB <path>`
            let Ok(rec) = std::str::from_utf8(rec) else {
                continue;
            };
            if let Some((meta, path)) = rec.split_once('\t') {
                if let Some(blob) = meta.split(' ').nth(2) {
                    map.insert(path.to_string(), blob.to_string());
                }
            }
        }
    }
    Ok(map)
}

// The differences between HEAD and several branches, read on demand and kept.
pub struct Index {
    root: PathBuf,
    // The commit HEAD named when the index was opened; every comparison uses it.
    head: String,
    tips: HashMap<String, Tip>,
    // Changes by tree id, so branches that hold the same tree are read once.
    by_tree: HashMap<String, Arc<Vec<Change>>>,
    // Paths git treats as binary, per branch; worked out when first asked for.
    binary: HashMap<String, HashSet<String>>,
    // Entries left out of the list: submodules and paths that are not UTF-8.
    pub skipped: usize,
}

impl Index {
    pub fn open(root: &Path) -> Res<Index> {
        Ok(Index {
            root: root.to_path_buf(),
            head: git::rev_parse(root, "HEAD")?,
            tips: HashMap::new(),
            by_tree: HashMap::new(),
            binary: HashMap::new(),
            skipped: 0,
        })
    }

    // A stash is a snapshot of the whole work tree, so what it offers is what it changed itself:
    // the paths that differ between its base commit and its work-tree commit, plus the files in
    // its untracked-files commit (`stash push -u`), each compared with HEAD. Nothing here reads or
    // changes the stash.
    fn stash_tip(&mut self, source: &str) -> Res<Tip> {
        let commit = git::rev_parse(&self.root, &format!("{source}^{{commit}}"))
            .map_err(|_| format!("'{source}' is not a stash"))?;
        let base = format!("{commit}^1");
        let (names, left_out) = parse_names(&git_bytes(
            &self.root,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                &base,
                &commit,
            ],
        )?);
        let (mut changes, skipped) =
            read_changes_for(&self.root, &self.head, &commit, &names, None)?;
        let mut skipped = skipped + left_out;
        let untracked = git::rev_parse(&self.root, &format!("{commit}^3"))
            .ok()
            .filter(|c| !c.is_empty())
            .map(|c| Arc::<str>::from(c.as_str()));
        if let Some(ut) = &untracked {
            let (names, left_out) = parse_names(&git_bytes(
                &self.root,
                &["ls-tree", "-r", "-z", "--name-only", "--full-tree", ut],
            )?);
            let (list, more) = read_changes_for(&self.root, &self.head, ut, &names, Some(ut))?;
            changes.extend(list);
            skipped += more + left_out;
        }
        self.skipped = self.skipped.max(skipped);
        Ok(Tip {
            commit: Arc::from(commit.as_str()),
            changes: Arc::new(changes),
            untracked,
        })
    }

    // The work tree's unstaged edits, offered as a return to the index. Every listed path is one
    // whose work-tree content differs from the index, so, unlike a branch, a listed version may equal
    // HEAD (nothing is staged); the entry's `head` blob is kept for the comparison all the same.
    // There is no commit to read the versions from, and none is needed: nothing is applied from
    // them, the edit is discarded in place. `Version::commit` is HEAD, which previews and the size
    // check do not read for this source. Nothing here changes the index or the work tree beyond
    // what `git diff` does in refreshing stat data.
    fn modified_tip(&mut self) -> Res<Tip> {
        let (modified, skipped) = read_modified(&self.root)?;
        self.skipped = self.skipped.max(skipped);
        let paths: Vec<String> = modified.iter().map(|(p, _)| p.clone()).collect();
        let heads = head_blobs(&self.root, &self.head, &paths)?;
        let changes: Vec<Change> = modified
            .into_iter()
            .map(|(path, blob)| Change {
                old: heads.get(&path).cloned(),
                path,
                blob,
                from: None,
            })
            .collect();
        Ok(Tip {
            commit: Arc::from(self.head.as_str()),
            changes: Arc::new(changes),
            untracked: None,
        })
    }

    fn tip(&mut self, source: &str) -> Res<&Tip> {
        if is_stash(source) && !self.tips.contains_key(source) {
            let tip = self.stash_tip(source)?;
            self.tips.insert(source.to_string(), tip);
        }
        if is_modified(source) && !self.tips.contains_key(source) {
            let tip = self.modified_tip()?;
            self.tips.insert(source.to_string(), tip);
        }
        if !self.tips.contains_key(source) {
            let ids = git::run(
                &self.root,
                &[
                    "rev-parse",
                    &format!("{source}^{{commit}}"),
                    &format!("{source}^{{tree}}"),
                ],
            )
            .map_err(|_| format!("'{source}' does not name a commit"))?;
            let mut lines = ids.lines();
            let (Some(commit), Some(tree)) = (lines.next(), lines.next()) else {
                return Err(format!("'{source}' does not name a commit").into());
            };
            let changes = match self.by_tree.get(tree) {
                Some(shared) => Arc::clone(shared),
                None => {
                    let (list, skipped) = read_changes(&self.root, &self.head, commit)?;
                    self.skipped = self.skipped.max(skipped);
                    let shared = Arc::new(list);
                    self.by_tree.insert(tree.to_string(), Arc::clone(&shared));
                    shared
                }
            };
            self.tips.insert(
                source.to_string(),
                Tip {
                    commit: Arc::from(commit),
                    changes,
                    untracked: None,
                },
            );
        }
        Ok(&self.tips[source])
    }

    // The commit each of `sources` names, read once and kept.
    pub fn tips(&mut self, sources: &[String]) -> Res<Vec<BranchTip>> {
        let mut out = Vec::new();
        for s in sources {
            // A stash has no history of its own to read; its parents belong to the branch it was
            // made on. The modified files are the work tree's, with no history either.
            if is_stash(s) || is_modified(s) {
                continue;
            }
            out.push((s.clone(), Arc::clone(&self.tip(s)?.commit)));
        }
        Ok(out)
    }

    // The commit HEAD named when the index was opened.
    pub fn head_commit(&self) -> &str {
        &self.head
    }

    // One entry per path in which any of `sources` differs from HEAD, sorted by path. A source's
    // versions appear in the order the sources are given.
    pub fn entries(&mut self, sources: &[String]) -> Res<Vec<FileEntry>> {
        for s in sources {
            self.tip(s)?;
        }
        let mut by_path: HashMap<&str, (Option<&str>, Vec<Version>)> = HashMap::new();
        for s in sources {
            let tip = &self.tips[s.as_str()];
            let name: Arc<str> = Arc::from(s.as_str());
            for c in tip.changes.iter() {
                let slot = by_path
                    .entry(c.path.as_str())
                    .or_insert_with(|| (c.old.as_deref(), Vec::new()));
                slot.1.push(Version {
                    source: Arc::clone(&name),
                    commit: c.from.clone().unwrap_or_else(|| Arc::clone(&tip.commit)),
                    blob: c.blob.clone(),
                });
            }
        }
        let mut rows: Vec<(&str, (Option<&str>, Vec<Version>))> = by_path.into_iter().collect();
        rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
        Ok(rows
            .into_iter()
            .map(|(path, (head, versions))| FileEntry {
                path: path.to_string(),
                head: head.map(str::to_string),
                versions,
            })
            .collect())
    }

    // The version of `path` at the tip of `source`, whether or not it differs from HEAD; None when
    // the branch lacks the file. Serves the command line, which may name a file that already
    // matches.
    pub fn version_at(&mut self, source: &str, path: &str) -> Res<Option<Version>> {
        let (commit, untracked) = {
            let tip = self.tip(source)?;
            (tip.commit.clone(), tip.untracked.clone())
        };
        // A stash may keep the file in its untracked-files commit instead of its own tree.
        for holder in std::iter::once(commit).chain(untracked) {
            let spec = format!("{holder}:{path}");
            let blob = git::run(&self.root, &["rev-parse", "--verify", "--quiet", &spec]).ok();
            if let Some(blob) = blob.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()) {
                return Ok(Some(Version {
                    source: Arc::from(source),
                    commit: holder,
                    blob,
                }));
            }
        }
        Ok(None)
    }

    // Paths that are binary in at least one of `sources`. Uses git's own detection (attributes
    // included) through a numstat against HEAD, which prints `-` for binary files and covers only
    // the paths that differ.
    pub fn binary_paths(&mut self, sources: &[String]) -> Res<HashSet<String>> {
        let mut all = HashSet::new();
        for s in sources {
            if !self.binary.contains_key(s) {
                let (commit, untracked) = {
                    let tip = self.tip(s)?;
                    (tip.commit.clone(), tip.untracked.clone())
                };
                let mut set = HashSet::new();
                if is_modified(s) {
                    // The change a restore makes is index to work tree, not HEAD to a commit.
                    set = numstat_binary(&git_bytes(
                        &self.root,
                        &["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff"],
                    )?);
                } else if is_stash(s) {
                    // Against its own base, since only the paths the stash changed are listed.
                    let base = format!("{commit}^1");
                    set.extend(numstat_binary(&git_bytes(
                        &self.root,
                        &[
                            "diff-tree",
                            "--no-commit-id",
                            "--numstat",
                            "-z",
                            "-r",
                            "--no-renames",
                            &base,
                            &commit,
                        ],
                    )?));
                    if let Some(ut) = &untracked {
                        set.extend(numstat_binary(&git_bytes(
                            &self.root,
                            &[
                                "diff-tree",
                                "--root",
                                "--no-commit-id",
                                "--numstat",
                                "-z",
                                "-r",
                                ut,
                            ],
                        )?));
                    }
                } else {
                    set = numstat_binary(&git_bytes(
                        &self.root,
                        &[
                            "diff",
                            "--numstat",
                            "-z",
                            "--no-renames",
                            &self.head,
                            &commit,
                        ],
                    )?);
                }
                self.binary.insert(s.clone(), set);
            }
            all.extend(self.binary[s].iter().cloned());
        }
        Ok(all)
    }
}

// The paths a `--numstat -z` listing marks as binary (git prints `-` for both counts).
fn numstat_binary(raw: &[u8]) -> HashSet<String> {
    raw.split(|b| *b == 0)
        .filter_map(|rec| std::str::from_utf8(rec).ok())
        .filter_map(|rec| rec.strip_prefix("-\t-\t"))
        .map(str::to_string)
        .collect()
}

// Output above this many bytes is cut, so the diff of a huge generated file is never read in full.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

// What restoring `path` from `version` would change on HEAD, as a git diff. Needs nothing but the
// repository, so a worker thread can run it; `cancel` abandons the git command from another thread.
pub fn diff_text(
    root: &Path,
    version: &Version,
    path: &str,
    cancel: Option<&git::Cancel>,
) -> Res<String> {
    let literal = format!(":(literal){path}");
    // A modified file goes back to its staged state, so the change shown is work tree to index
    // (`-R` turns the diff of the index against the work tree around); anything else is restored
    // onto HEAD.
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-renames"];
    if is_modified(&version.source) {
        args.push("-R");
    } else {
        args.extend(["HEAD", &*version.commit]);
    }
    args.extend(["--", literal.as_str()]);
    let out = git::run_capped(root, &args, MAX_DIFF_BYTES, cancel)?;
    let mut text = out.text;
    if out.truncated {
        text.push_str("\n... (truncated)\n");
    }
    Ok(text)
}

// The text the preview pane shows for `path` from `version`: the change, or a note when the blob is
// larger than `limit` bytes (0 for no limit). The size is read from the object header, so a huge
// file is never loaded to find out that it is huge.
pub fn preview_text(
    root: &Path,
    version: &Version,
    path: &str,
    limit: u64,
    cancel: Option<&git::Cancel>,
) -> Res<String> {
    if limit > 0 {
        let size = git::blob_sizes(root, std::slice::from_ref(&version.blob))?[0];
        if let Some(size) = size.filter(|s| *s > limit) {
            return Ok(format!(
                "{path} is {} on {}; larger than the preview limit ({}).\n\
                 Restoring it still works.",
                cherry::human(size),
                version.source,
                cherry::human(limit)
            ));
        }
    }
    diff_text(root, version, path, cancel)
}

// A branch and the commit it pointed at when the index was read.
pub type BranchTip = (String, Arc<str>);

// One commit that changed a path, offered as a version to restore.
#[derive(Clone, Debug, PartialEq)]
pub struct HistRow {
    pub version: Version,
    // Commit time, seconds since the epoch.
    pub when: i64,
    pub subject: String,
}

// The most commits read per branch for one path.
const MAX_HISTORY: usize = 300;

// The commits reachable from each of `tips` that changed `path` and left it in existence, newest
// first, one row per commit (a commit reachable from several branches belongs to the first named).
// The second value is true when a branch had more commits than `MAX_HISTORY`. Nothing is read
// until this is called: the tip list never needs history.
pub fn path_history(
    root: &Path,
    tips: &[BranchTip],
    path: &str,
    cancel: Option<&git::Cancel>,
) -> Res<(Vec<HistRow>, bool)> {
    let literal = format!(":(literal){path}");
    let limit = format!("-n{}", MAX_HISTORY + 1);
    let mut rows: Vec<HistRow> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;
    for (name, tip) in tips {
        let out = git::run_capped(
            root,
            &[
                "log",
                "--full-history",
                "--no-renames",
                &limit,
                "--format=%H%x1f%ct%x1f%s",
                tip,
                "--",
                &literal,
            ],
            8 * 1024 * 1024,
            cancel,
        )?;
        let mut lines: Vec<(&str, i64, &str)> = out
            .text
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\x1f');
                Some((f.next()?, f.next()?.parse().ok()?, f.next().unwrap_or("")))
            })
            .collect();
        if lines.len() > MAX_HISTORY {
            lines.truncate(MAX_HISTORY);
            truncated = true;
        }
        lines.retain(|(id, _, _)| seen.insert(id.to_string()));
        // A commit that deleted the file has no blob at that path; it drops out here.
        let specs: Vec<String> = lines
            .iter()
            .map(|(id, _, _)| format!("{id}:{path}"))
            .collect();
        let blobs = git::resolve_objects(root, &specs)?;
        for ((id, when, subject), blob) in lines.iter().zip(blobs) {
            if let Some((blob, _)) = blob.filter(|(_, k)| k == "blob") {
                rows.push(HistRow {
                    version: Version {
                        source: Arc::from(name.as_str()),
                        commit: Arc::from(*id),
                        blob,
                    },
                    when: *when,
                    subject: subject.to_string(),
                });
            }
        }
    }
    rows.sort_by(|a, b| b.when.cmp(&a.when));
    Ok((rows, truncated))
}

// Files that existed in the history of `tips` and are gone from both HEAD and the tip they were
// deleted from, each with its last version. `truncated` is true when the scan stopped at its size
// limit, so that older deletions may be missing.
pub struct Deleted {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

// The most output read from one branch's history, in bytes.
const MAX_SCAN_BYTES: usize = 16 * 1024 * 1024;

pub fn deleted_files(
    root: &Path,
    head: &str,
    tips: &[BranchTip],
    cancel: Option<&git::Cancel>,
) -> Res<Deleted> {
    let mut by_path: HashMap<String, Vec<Version>> = HashMap::new();
    let mut truncated = false;
    for (name, tip) in tips {
        let out = git::run_capped(
            root,
            &[
                "-c",
                "core.quotepath=false",
                "log",
                "--full-history",
                "--no-renames",
                "--diff-filter=D",
                "--name-only",
                "--format=%x01%H",
                tip,
            ],
            MAX_SCAN_BYTES,
            cancel,
        )?;
        truncated |= out.truncated;
        // Newest deletion of each path first; a path quoted by git (odd characters) is left out.
        let mut newest: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut commit = "";
        for line in out.text.lines() {
            if let Some(id) = line.strip_prefix('\x01') {
                commit = id;
            } else if !line.is_empty() && !commit.is_empty() && !line.starts_with('"') {
                if seen.insert(line) {
                    newest.push((line.to_string(), commit.to_string()));
                }
            }
        }
        // Still present at HEAD or at this tip: not deleted as far as the operator is concerned.
        let probes: Vec<String> = newest
            .iter()
            .flat_map(|(p, _)| [format!("{head}:{p}"), format!("{tip}:{p}")])
            .collect();
        let present = git::resolve_objects(root, &probes)?;
        let gone: Vec<&(String, String)> = newest
            .iter()
            .enumerate()
            .filter(|(i, _)| present[2 * i].is_none() && present[2 * i + 1].is_none())
            .map(|(_, n)| n)
            .collect();
        // The last version is the one the deleting commit's first parent held.
        let parents = git::resolve_objects(
            root,
            &gone
                .iter()
                .map(|(_, c)| format!("{c}^"))
                .collect::<Vec<_>>(),
        )?;
        let specs: Vec<String> = gone
            .iter()
            .zip(&parents)
            .map(|((p, _), parent)| match parent {
                Some((id, _)) => format!("{id}:{p}"),
                None => String::new(),
            })
            .collect();
        let blobs = git::resolve_objects(root, &specs)?;
        for (((path, _), parent), blob) in gone.iter().zip(parents).zip(blobs) {
            if let (Some((parent, _)), Some((blob, kind))) = (parent, blob) {
                if kind == "blob" {
                    by_path.entry(path.clone()).or_default().push(Version {
                        source: Arc::from(name.as_str()),
                        commit: Arc::from(parent.as_str()),
                        blob,
                    });
                }
            }
        }
    }
    let mut entries: Vec<FileEntry> = by_path
        .into_iter()
        .map(|(path, versions)| FileEntry {
            path,
            head: None,
            versions,
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Deleted { entries, truncated })
}

// The recipe for restoring `path` from `version` onto the current HEAD. Each file is its own
// spec, hence its own patch, so a file that cannot be applied does not hold up the others.
pub fn spec_for(root: &Path, version: &Version, path: &str) -> Res<Spec> {
    // Unstaged edits have no commit to restore from; the spec says to discard them.
    if is_modified(&version.source) {
        return Ok(Spec {
            base: git::rev_parse(root, "HEAD")?,
            discard: vec![path.to_string()],
            ..Spec::default()
        });
    }
    Ok(Spec {
        base: git::rev_parse(root, "HEAD")?,
        restore: vec![(version.commit.to_string(), path.to_string())],
        ..Spec::default()
    })
}

pub struct Opts {
    // A branch, tag or commit to take the files from; the interactive screen starts with only this
    // branch selected.
    pub from: Option<String>,
    pub paths: Vec<String>,
    pub dry_run: bool,
    pub patch_only: bool,
    // Take the files from a stash: `from` then names one (`stash@{N}`, or just N), and without it
    // the screen opens with every stash selected and named paths come from `stash@{0}`.
    pub stash: bool,
    // Merge each named file with the work-tree copy change by change instead of overwriting it. Needs
    // paths and a terminal; the source is read as for any restore and is never altered.
    pub merge: bool,
}

// Names relative to the directory the command was run in, as `git` does, become paths relative to
// the top of the work tree.
pub(crate) fn from_top(cwd: &Path, given: &str) -> Res<String> {
    let prefix = git::run(cwd, &["rev-parse", "--show-prefix"])?;
    let mut parts: Vec<&str> = prefix.split('/').filter(|p| !p.is_empty()).collect();
    for seg in given.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!("restore: '{given}' lies outside the repository").into());
                }
            }
            other => parts.push(other),
        }
    }
    Ok(parts.join("/"))
}

// Resolve named paths into one spec per file. With `from` every file comes from that revision.
// Without it a file must have the same content on every local branch that holds it; if the
// branches disagree the choice is refused and the branches are listed.
fn spec_from_paths(cwd: &Path, opts: &Opts) -> Res<Vec<Spec>> {
    let root = git::work_tree(cwd)?;
    let picks = picks_from_paths(cwd, opts)?;
    let mut specs = Vec::new();
    for (i, (version, path)) in picks.iter().enumerate() {
        if picks[..i].iter().any(|(_, p)| p == path) {
            continue;
        }
        specs.push(spec_for(&root, version, path)?);
    }
    Ok(specs)
}

// The version of each named path that a restore would take, in the order named.
fn picks_from_paths(cwd: &Path, opts: &Opts) -> Res<Vec<(Version, String)>> {
    let root = git::work_tree(cwd)?;
    let mut index = Index::open(&root)?;
    let mut picks: Vec<(Version, String)> = Vec::new();
    match &opts.from {
        Some(rev) => {
            let list = std::slice::from_ref(rev);
            let entries = index.entries(list)?;
            for given in &opts.paths {
                let path = from_top(cwd, given)?;
                let version = match entries.iter().find(|e| e.path == path) {
                    Some(entry) => entry.versions[0].clone(),
                    // A file without unstaged edits has nothing to discard.
                    None if is_modified(rev) => {
                        return Err(format!("restore: {path} has no unstaged changes").into())
                    }
                    // Not listed: absent on the branch, or identical to HEAD.
                    None => index
                        .version_at(rev, &path)?
                        .ok_or_else(|| format!("restore: '{rev}' does not hold {path}"))?,
                };
                picks.push((version, path));
            }
        }
        None => {
            let locals: Vec<String> = sources(cwd)?
                .into_iter()
                .filter(|s| !s.remote)
                .map(|s| s.name)
                .collect();
            if locals.is_empty() {
                return Err("restore: there is no other local branch to restore from".into());
            }
            let entries = index.entries(&locals)?;
            for given in &opts.paths {
                let path = from_top(cwd, given)?;
                let Some(entry) = entries.iter().find(|e| e.path == path) else {
                    // No local branch differs from HEAD in this file; one that holds it at all
                    // holds it unchanged.
                    let mut found = None;
                    for l in &locals {
                        if let Some(v) = index.version_at(l, &path)? {
                            found = Some(v);
                            break;
                        }
                    }
                    let v =
                        found.ok_or_else(|| format!("restore: no local branch holds {path}"))?;
                    picks.push((v, path));
                    continue;
                };
                if entry.distinct() > 1 {
                    let list: Vec<String> = entry
                        .versions
                        .iter()
                        .map(|v| format!("{} ({})", v.source, &v.blob[..v.blob.len().min(8)]))
                        .collect();
                    return Err(format!(
                        "restore: the branches disagree about {path}: {}; name one with --from",
                        list.join(", ")
                    )
                    .into());
                }
                picks.push((entry.versions[0].clone(), path));
            }
        }
    }
    Ok(picks)
}

// True when the bytes look like binary content, by the rule git applies: a NUL in the first 8000 bytes.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

// The files a change-by-change merge would decide, each as a screen entry, together with the paths
// that need no decision. A file is refused, with the reason, when it cannot be merged by lines: binary
// content on either side, or no work-tree copy to merge into.
fn merge_entries(
    cwd: &Path,
    picks: &[(Version, String)],
) -> Res<(Vec<crate::resolve::Conflicted>, Vec<String>)> {
    let root = git::work_tree(cwd)?;
    let mut entries = Vec::new();
    let mut same = Vec::new();
    for (i, (version, path)) in picks.iter().enumerate() {
        if picks[..i].iter().any(|(_, p)| p == path) {
            continue;
        }
        let ours = std::fs::read(root.join(path)).map_err(|_| {
            format!("restore --merge: {path} is not in the work tree; restore it whole instead")
        })?;
        let theirs = git::cat_blob(&root, &version.blob)?;
        if looks_binary(&ours) || looks_binary(&theirs) {
            return Err(format!(
                "restore --merge: {path} is binary; it can only be restored whole"
            )
            .into());
        }
        let segs = conflict::diff_segments(&ours, &theirs);
        if conflict::hunk_count(&segs) == 0 {
            same.push(path.clone());
        } else {
            entries.push(crate::resolve::Conflicted {
                path: path.clone(),
                body: crate::resolve::Body::Hunks(segs),
            });
        }
    }
    Ok((entries, same))
}

// `restore --merge`: decide, change by change, what of the source's copy to take and what of the work-tree
// copy to keep. Side A of each hunk is the work tree, side B the source. Nothing is written until every
// change of every file is decided, and the source (a stash included) is only read.
fn merge_run(cwd: &Path, opts: &Opts) -> Res<()> {
    if opts.paths.is_empty() {
        return Err("restore --merge: name the files to merge".into());
    }
    if opts.patch_only {
        return Err("restore --merge: no patch file is written for a merge".into());
    }
    let picks = picks_from_paths(cwd, opts)?;
    let (entries, same) = merge_entries(cwd, &picks)?;
    for path in &same {
        println!("gitomic: {path} is already identical to the source; nothing to merge");
    }
    if entries.is_empty() {
        return Ok(());
    }
    let from = picks
        .first()
        .map(|(v, _)| v.source.to_string())
        .unwrap_or_default();
    if opts.dry_run {
        for e in &entries {
            println!(
                "gitomic: {} would ask about {} change(s)",
                e.path,
                e.units()
            );
        }
        println!("gitomic: dry run; nothing was modified");
        return Ok(());
    }
    let labels = (
        "A  the work tree copy (kept unless B is chosen)".to_string(),
        format!("B  {from} (taken when chosen)"),
    );
    let Some(decided) = crate::resolve::decide_files(entries, labels)? else {
        println!("gitomic: nothing merged");
        return Ok(());
    };
    let mut out = Vec::new();
    for c in &decided {
        let crate::resolve::Body::Hunks(segs) = &c.body else {
            continue;
        };
        let bytes = conflict::render(segs).ok_or_else(|| {
            format!(
                "restore --merge: {} is undecided; nothing was written",
                c.path
            )
        })?;
        out.push((c.path.clone(), bytes));
    }
    cherry::apply_contents(cwd, out, &from)
}

// Entry point for the command.
pub fn run(cwd: &Path, mut opts: Opts) -> Res<()> {
    let git_dir = git::git_dir(cwd)?;
    if git::operation_in_progress(&git_dir) {
        return Err(
            "restore: a merge, rebase, cherry-pick, revert, or bisect is in progress".into(),
        );
    }
    if opts.stash {
        opts.from = opts.from.as_deref().map(stash_ref);
        if !opts.from.as_deref().is_none_or(is_stash) {
            return Err("restore: --from must name a stash (stash@{N}, or N) with --stash".into());
        }
    }
    if opts.merge {
        if opts.stash && opts.from.is_none() {
            opts.from = Some("stash@{0}".to_string());
        }
        return merge_run(cwd, &opts);
    }
    let specs = if opts.paths.is_empty() {
        match crate::restore_ui::run(cwd, opts.from.as_deref(), opts.stash)? {
            Some(specs) => specs,
            None => {
                println!("gitomic: nothing restored");
                return Ok(());
            }
        }
    } else {
        if opts.stash && opts.from.is_none() {
            opts.from = Some("stash@{0}".to_string());
        }
        spec_from_paths(cwd, &opts)?
    };
    cherry::apply_specs(cwd, specs, opts.dry_run, opts.patch_only)
}

// A patch file written by cherry-pick or restore, as the patch list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PatchInfo {
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    // What the header says the patch does; empty text for a file without a gitomic header.
    pub summary: String,
}

fn summarise(bytes: &[u8]) -> String {
    let Some(spec) = Spec::parse(bytes) else {
        return "(no gitomic header)".to_string();
    };
    let mut parts: Vec<String> = spec
        .picks
        .iter()
        .map(|(id, subject)| format!("pick {} {}", &id[..id.len().min(8)], subject))
        .collect();
    parts.extend(
        spec.restore
            .iter()
            .map(|(id, path)| format!("restore {path} from {}", &id[..id.len().min(8)])),
    );
    parts.join("; ")
}

// The patch files kept under `.git/gitomic-picks`, newest first.
pub fn list_patches(git_dir: &Path) -> Res<Vec<PatchInfo>> {
    let dir = patch::patch_dir(git_dir);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in read.filter_map(|e| e.ok()) {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let head = std::fs::read(entry.path())
            .map(|b| b[..b.len().min(64 * 1024)].to_vec())
            .unwrap_or_default();
        out.push(PatchInfo {
            name: entry.file_name().to_string_lossy().into_owned(),
            size: meta.len(),
            modified: meta.modified().ok(),
            summary: summarise(&head),
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.name.cmp(&a.name)));
    Ok(out)
}

fn patch_path(git_dir: &Path, name: &str) -> Res<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err(format!("restore: '{name}' is not a patch file name").into());
    }
    Ok(patch::patch_dir(git_dir).join(name))
}

// Most of a patch file the viewer reads.
const MAX_PATCH_VIEW: usize = 256 * 1024;

// The text of one patch file, cut at a fixed size.
pub fn read_patch(git_dir: &Path, name: &str) -> Res<String> {
    let bytes = std::fs::read(patch_path(git_dir, name)?)?;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PATCH_VIEW)]).into_owned();
    if bytes.len() > MAX_PATCH_VIEW {
        text.push_str("\n... (truncated)\n");
    }
    Ok(text)
}

// Delete patch files; returns how many were removed. A file that is already gone counts as removed.
pub fn trash_patches(git_dir: &Path, names: &[String]) -> Res<usize> {
    let mut n = 0;
    for name in names {
        match std::fs::remove_file(patch_path(git_dir, name)?) {
            Ok(()) => n += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => n += 1,
            Err(e) => return Err(format!("restore: cannot delete {name}: {e}").into()),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;

    // main: a.txt, shared.txt, sub/n.txt, bin.dat (binary).
    // one:  shared.txt edited, only_one.txt added, bin.dat changed.
    // two:  shared.txt edited differently, only_two.txt added.
    // HEAD ends on main.
    fn setup() -> Repo {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.write("shared.txt", "base\n");
        r.write("sub/n.txt", "n\n");
        std::fs::write(r.0.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "one"]);
        r.write("shared.txt", "one\n");
        r.write("only_one.txt", "1\n");
        std::fs::write(r.0.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "one"]);
        r.git(&["checkout", "-q", "main"]);
        r.git(&["checkout", "-q", "-b", "two"]);
        r.write("shared.txt", "two\n");
        r.write("only_two.txt", "2\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "two"]);
        r.git(&["checkout", "-q", "main"]);
        r
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn opts(from: Option<&str>, paths: &[&str]) -> Opts {
        Opts {
            from: from.map(str::to_string),
            paths: names(paths),
            dry_run: false,
            patch_only: false,
            stash: false,
            merge: false,
        }
    }

    #[test]
    fn sources_list_the_other_local_branches() {
        let r = setup();
        let s = sources(&r.0).unwrap();
        let mut n: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        n.sort();
        assert_eq!(n, ["one", "two"]);
        assert!(s.iter().all(|s| !s.remote));
    }

    #[test]
    fn entries_merge_the_branches_by_path() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one", "two"])).unwrap();
        let paths: Vec<&str> = e.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["bin.dat", "only_one.txt", "only_two.txt", "shared.txt"]
        );
        let shared = e.iter().find(|e| e.path == "shared.txt").unwrap();
        assert_eq!(shared.versions.len(), 2);
        assert_eq!(shared.distinct(), 2);
        assert_eq!(shared.state(), State::Differs);
        assert_eq!(&*shared.versions[0].source, "one");
        assert!(
            e.iter().all(|e| e.path != "a.txt"),
            "a.txt equals HEAD everywhere"
        );
        assert!(idx.version_at("one", "a.txt").unwrap().is_some());
        assert!(idx.version_at("one", "nope.txt").unwrap().is_none());
        let only = e.iter().find(|e| e.path == "only_two.txt").unwrap();
        assert_eq!(only.state(), State::Absent);
        assert_eq!(only.versions.len(), 1);
    }

    #[test]
    fn only_the_paths_that_differ_from_head_are_read_and_equal_trees_are_read_once() {
        let r = setup();
        r.git(&["branch", "twin", "one"]);
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one", "twin"])).unwrap();
        let paths: Vec<&str> = e.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["bin.dat", "only_one.txt", "shared.txt"]);
        assert_eq!(idx.by_tree.len(), 1, "one tree, one read");
        assert!(e.iter().all(|e| e.versions.len() == 2 && e.distinct() == 1));
        // The versions of one branch share the name and the commit instead of copying them.
        assert!(Arc::ptr_eq(
            &e[0].versions[0].commit,
            &e[1].versions[0].commit
        ));
    }

    #[test]
    fn a_deleted_file_is_not_offered() {
        let r = setup();
        r.git(&["checkout", "-q", "one"]);
        r.git(&["rm", "-q", "a.txt"]);
        r.git(&["commit", "-q", "-m", "drop a"]);
        r.git(&["checkout", "-q", "main"]);
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one"])).unwrap();
        assert!(e.iter().all(|e| e.path != "a.txt"));
    }

    // main: base. one: edits shared.txt twice, adds gone.txt, deletes it, adds keep.txt.
    fn with_history() -> Repo {
        let r = setup();
        r.git(&["checkout", "-q", "one"]);
        r.commit_file("shared.txt", "v2\n", "second");
        r.commit_file("gone.txt", "g1\n", "add gone");
        r.commit_file("gone.txt", "g2\n", "edit gone");
        r.git(&["rm", "-q", "gone.txt"]);
        r.git(&["commit", "-q", "-m", "remove gone"]);
        r.commit_file("shared.txt", "v3\n", "third");
        r.git(&["checkout", "-q", "main"]);
        r
    }

    #[test]
    fn the_history_of_a_path_lists_each_commit_that_left_it_in_existence() {
        let r = with_history();
        let mut idx = Index::open(&r.0).unwrap();
        let tips = idx.tips(&names(&["one"])).unwrap();
        let (rows, cut) = path_history(&r.0, &tips, "shared.txt", None).unwrap();
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["third", "second", "one", "base"]);
        assert!(!cut);
        assert!(rows.windows(2).all(|w| w[0].when >= w[1].when));
        // Each row restores exactly what that commit held.
        let blob = |row: &HistRow| r.git(&["show", &format!("{}:shared.txt", row.version.commit)]);
        assert_eq!(blob(&rows[1]), "v2");
        assert_eq!(&*rows[0].version.source, "one");
        // A deleted file: the deleting commit has no version, the ones before it do.
        let (gone, _) = path_history(&r.0, &tips, "gone.txt", None).unwrap();
        let subjects: Vec<&str> = gone.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["edit gone", "add gone"]);
    }

    #[test]
    fn a_commit_reachable_from_two_branches_is_listed_once() {
        let r = with_history();
        r.git(&["branch", "twin", "one"]);
        let mut idx = Index::open(&r.0).unwrap();
        let tips = idx.tips(&names(&["one", "twin"])).unwrap();
        let (rows, _) = path_history(&r.0, &tips, "shared.txt", None).unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| &*r.version.source == "one"));
    }

    #[test]
    fn deleted_files_come_back_with_their_last_version() {
        let r = with_history();
        let mut idx = Index::open(&r.0).unwrap();
        let tips = idx.tips(&names(&["one", "two"])).unwrap();
        let d = deleted_files(&r.0, idx.head_commit(), &tips, None).unwrap();
        assert!(!d.truncated);
        let paths: Vec<&str> = d.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["gone.txt"]);
        let v = &d.entries[0].versions[0];
        assert_eq!(&*v.source, "one");
        assert_eq!(r.git(&["show", &format!("{}:gone.txt", v.commit)]), "g2");
        assert_eq!(d.entries[0].state(), State::Absent);
        // Restoring it writes the last version back.
        let spec = spec_for(&r.0, v, "gone.txt").unwrap();
        crate::cherry::apply_specs(&r.0, vec![spec], false, false).unwrap();
        assert_eq!(r.read("gone.txt"), "g2\n");
    }

    #[test]
    fn a_file_deleted_and_back_at_the_tip_or_on_head_is_not_reported_deleted() {
        let r = with_history();
        r.git(&["checkout", "-q", "one"]);
        r.commit_file("gone.txt", "back\n", "again");
        r.git(&["checkout", "-q", "main"]);
        let mut idx = Index::open(&r.0).unwrap();
        let tips = idx.tips(&names(&["one"])).unwrap();
        let d = deleted_files(&r.0, idx.head_commit(), &tips, None).unwrap();
        assert!(d.entries.is_empty(), "present again at the tip of `one`");
    }

    #[test]
    fn a_cancelled_scan_stops_with_the_cancel_error() {
        let r = with_history();
        let mut idx = Index::open(&r.0).unwrap();
        let tips = idx.tips(&names(&["one"])).unwrap();
        let cancel = git::Cancel::default();
        cancel.cancel();
        assert!(deleted_files(&r.0, idx.head_commit(), &tips, Some(&cancel)).is_err());
        assert!(path_history(&r.0, &tips, "shared.txt", Some(&cancel)).is_err());
    }

    #[test]
    fn a_blob_over_the_limit_is_described_and_not_diffed() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one"])).unwrap();
        let shared = e.iter().find(|e| e.path == "shared.txt").unwrap();
        let note = preview_text(&r.0, &shared.versions[0], "shared.txt", 2, None).unwrap();
        assert!(note.contains("larger than the preview limit"), "{note}");
        assert!(!note.contains("diff --git"));
        let full = preview_text(&r.0, &shared.versions[0], "shared.txt", 0, None).unwrap();
        assert!(full.contains("+one"), "{full}");
    }

    #[test]
    fn the_commit_of_each_version_is_pinned_when_read() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let before = r.git(&["rev-parse", "one"]);
        let e = idx.entries(&names(&["one"])).unwrap();
        r.git(&["checkout", "-q", "one"]);
        r.commit_file("shared.txt", "moved\n", "moves one");
        r.git(&["checkout", "-q", "main"]);
        let shared = e.iter().find(|e| e.path == "shared.txt").unwrap();
        assert_eq!(&*shared.versions[0].commit, before);
        let d = diff_text(&r.0, &shared.versions[0], "shared.txt", None).unwrap();
        assert!(d.contains("+one") && !d.contains("moved"), "{d}");
    }

    #[test]
    fn the_filter_matches_every_term_ignoring_case() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one", "two"])).unwrap();
        let none = HashSet::new();
        let f = Filter {
            text: "ONLY txt".into(),
            binary_only: false,
        };
        let got: Vec<&str> = f
            .select(&e, &none, None)
            .into_iter()
            .map(|i| e[i].path.as_str())
            .collect();
        assert_eq!(got, ["only_one.txt", "only_two.txt"]);
        assert_eq!(Filter::default().select(&e, &none, None).len(), e.len());
        assert!(Filter::default().is_empty());
    }

    #[test]
    fn hits_narrow_the_result_further() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one", "two"])).unwrap();
        let hits: HashSet<String> = ["bin.dat".to_string(), "shared.txt".to_string()].into();
        let f = Filter {
            text: "txt".into(),
            binary_only: false,
        };
        let got: Vec<&str> = f
            .select(&e, &HashSet::new(), Some(&hits))
            .into_iter()
            .map(|i| e[i].path.as_str())
            .collect();
        assert_eq!(got, ["shared.txt"]);
    }

    #[test]
    fn binary_files_are_found_and_the_filter_keeps_only_them() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let src = names(&["one", "two"]);
        let bin = idx.binary_paths(&src).unwrap();
        assert_eq!(bin, ["bin.dat".to_string()].into());
        let e = idx.entries(&src).unwrap();
        let f = Filter {
            text: String::new(),
            binary_only: true,
        };
        let got = f.select(&e, &bin, None);
        assert_eq!(got.len(), 1);
        assert_eq!(e[got[0]].path, "bin.dat");
    }

    #[test]
    fn submodules_are_left_out_and_counted() {
        let r = setup();
        r.git(&["checkout", "-q", "one"]);
        r.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,mod",
        ]);
        r.git(&["commit", "-q", "-m", "gitlink"]);
        r.git(&["checkout", "-q", "main"]);
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one"])).unwrap();
        assert!(e.iter().all(|e| e.path != "mod"));
        assert_eq!(idx.skipped, 1);
    }

    #[test]
    fn the_diff_is_what_the_restore_would_change_on_head() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one"])).unwrap();
        let shared = e.iter().find(|e| e.path == "shared.txt").unwrap();
        let d = diff_text(&r.0, &shared.versions[0], "shared.txt", None).unwrap();
        assert!(d.contains("-base") && d.contains("+one"), "{d}");
        let added = e.iter().find(|e| e.path == "only_one.txt").unwrap();
        let d = diff_text(&r.0, &added.versions[0], "only_one.txt", None).unwrap();
        assert!(d.contains("new file") && d.contains("+1"), "{d}");
    }

    #[test]
    fn a_file_is_restored_from_a_branch_into_the_work_tree() {
        let r = setup();
        let head = r.head();
        run(&r.0, opts(Some("one"), &["shared.txt"])).unwrap();
        assert_eq!(r.head(), head, "history is not extended outside a session");
        assert_eq!(r.read("shared.txt"), "one\n");
        assert_eq!(r.git(&["status", "--porcelain"]), " M shared.txt");
    }

    #[test]
    fn a_file_missing_on_head_is_added() {
        let r = setup();
        run(&r.0, opts(Some("two"), &["only_two.txt"])).unwrap();
        assert_eq!(r.read("only_two.txt"), "2\n");
    }

    #[test]
    fn a_binary_file_is_restored_byte_for_byte() {
        let r = setup();
        run(&r.0, opts(Some("one"), &["bin.dat"])).unwrap();
        assert_eq!(std::fs::read(r.0.join("bin.dat")).unwrap(), [0u8, 9, 9, 9]);
    }

    fn patch_count(r: &Repo) -> usize {
        std::fs::read_dir(patch::patch_dir(&r.0.join(".git")))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn each_file_is_its_own_patch() {
        let r = setup();
        run(&r.0, opts(Some("one"), &["shared.txt", "only_one.txt"])).unwrap();
        assert_eq!(r.read("shared.txt"), "one\n");
        assert_eq!(r.read("only_one.txt"), "1\n");
        assert_eq!(patch_count(&r), 2);
    }

    #[test]
    fn a_file_that_cannot_be_applied_does_not_halt_the_others() {
        let r = setup();
        // An untracked file in the way of only_one.txt; the two other files are unaffected.
        r.write("only_one.txt", "mine\n");
        let err = run(
            &r.0,
            opts(Some("one"), &["shared.txt", "only_one.txt", "bin.dat"]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("1 of 3"), "{err}");
        assert!(err.contains("only_one.txt"), "{err}");
        assert_eq!(r.read("shared.txt"), "one\n");
        assert_eq!(std::fs::read(r.0.join("bin.dat")).unwrap(), [0u8, 9, 9, 9]);
        assert_eq!(r.read("only_one.txt"), "mine\n");
    }

    #[test]
    fn in_a_session_each_file_is_its_own_commit() {
        let r = setup();
        r.git(&["update-ref", "refs/gitomic/base/main", "HEAD"]);
        let head = r.head();
        run(&r.0, opts(Some("one"), &["shared.txt", "only_one.txt"])).unwrap();
        assert_eq!(
            r.git(&["rev-list", "--count", &format!("{head}..HEAD")]),
            "2"
        );
        let mut touched: Vec<String> = ["HEAD", "HEAD~1"]
            .iter()
            .map(|c| r.git(&["show", "--name-only", "--format=", c]))
            .collect();
        touched.sort();
        assert_eq!(touched, ["only_one.txt", "shared.txt"]);
    }

    #[test]
    fn without_from_the_branches_must_agree() {
        let r = setup();
        let err = run(&r.0, opts(None, &["shared.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("disagree") && err.contains("--from"), "{err}");
        assert!(err.contains("one") && err.contains("two"), "{err}");
        assert_eq!(r.read("shared.txt"), "base\n");

        // A file only one branch has needs no choice.
        run(&r.0, opts(None, &["only_one.txt"])).unwrap();
        assert_eq!(r.read("only_one.txt"), "1\n");
    }

    #[test]
    fn unknown_paths_and_revisions_are_refused() {
        let r = setup();
        let err = run(&r.0, opts(Some("one"), &["nope.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not hold"), "{err}");
        let err = run(&r.0, opts(Some("nonexistent"), &["a.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not name a commit"), "{err}");
        let err = run(&r.0, opts(None, &["nope.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no local branch"), "{err}");
    }

    #[test]
    fn a_commit_or_tag_may_stand_in_for_a_branch() {
        let r = setup();
        let c = r.git(&["rev-parse", "two"]);
        run(&r.0, opts(Some(&c), &["shared.txt"])).unwrap();
        assert_eq!(r.read("shared.txt"), "two\n");
    }

    #[test]
    fn paths_are_relative_to_the_directory_the_command_runs_in() {
        let r = setup();
        r.git(&["checkout", "-q", "one"]);
        r.commit_file("sub/n.txt", "n-one\n", "edit n");
        r.git(&["checkout", "-q", "main"]);
        let sub = r.0.join("sub");
        run(&sub, opts(Some("one"), &["n.txt"])).unwrap();
        assert_eq!(r.read("sub/n.txt"), "n-one\n");
        run(&sub, opts(Some("one"), &["../shared.txt"])).unwrap();
        assert_eq!(r.read("shared.txt"), "one\n");
        let err = run(&sub, opts(Some("one"), &["../../x"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn dry_run_and_patch_only_leave_the_work_tree_alone() {
        let r = setup();
        let mut o = opts(Some("one"), &["shared.txt"]);
        o.dry_run = true;
        run(&r.0, o).unwrap();
        assert_eq!(r.read("shared.txt"), "base\n");
        let mut o = opts(Some("one"), &["shared.txt"]);
        o.patch_only = true;
        run(&r.0, o).unwrap();
        assert_eq!(r.read("shared.txt"), "base\n");
    }

    #[test]
    fn a_file_that_already_matches_is_reported_and_left_alone() {
        let r = setup();
        run(&r.0, opts(Some("one"), &["a.txt"])).unwrap();
        assert_eq!(r.git(&["status", "--porcelain"]), "");
    }

    #[test]
    fn an_untracked_file_in_the_way_blocks_the_restore() {
        let r = setup();
        r.write("only_one.txt", "mine\n");
        let err = run(&r.0, opts(Some("one"), &["only_one.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not apply"), "{err}");
        assert_eq!(r.read("only_one.txt"), "mine\n");
    }

    #[test]
    fn in_a_session_pending_edits_are_captured_then_the_restore_is_one_more_commit() {
        let r = setup();
        r.git(&["update-ref", "refs/gitomic/base/main", "HEAD"]);
        let head = r.head();
        // An edit made while no watcher ran, to a file the restore does not touch, and one it does.
        r.write("a.txt", "edited\n");
        r.write("shared.txt", "edited too\n");

        run(&r.0, opts(Some("one"), &["shared.txt"])).unwrap();

        assert_eq!(
            r.git(&["rev-list", "--count", &format!("{head}..HEAD")]),
            "2"
        );
        assert_eq!(r.read("shared.txt"), "one\n");
        assert_eq!(r.git(&["status", "--porcelain"]), "");
        // The overwritten content is recoverable from the capture commit.
        assert_eq!(r.git(&["show", "HEAD~1:shared.txt"]), "edited too");
        assert_eq!(r.git(&["show", "HEAD~1:a.txt"]), "edited");
        let files = r.git(&["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files, "shared.txt");
    }

    #[test]
    fn outside_a_session_a_local_edit_to_the_file_blocks_the_restore() {
        let r = setup();
        r.write("shared.txt", "edited\n");
        let err = run(&r.0, opts(Some("one"), &["shared.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not apply"), "{err}");
        assert_eq!(r.read("shared.txt"), "edited\n");
    }

    #[test]
    fn spec_for_names_the_commit_and_the_path() {
        let r = setup();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&["one"])).unwrap();
        let v = e[0].versions[0].clone();
        let spec = spec_for(&r.0, &v, "a.txt").unwrap();
        assert!(spec.picks.is_empty());
        assert_eq!(spec.restore, [(v.commit.to_string(), "a.txt".to_string())]);
        assert_eq!(spec.base, r.head());
    }

    #[test]
    fn a_path_named_twice_is_restored_once() {
        let r = setup();
        run(&r.0, opts(Some("one"), &["shared.txt", "shared.txt"])).unwrap();
        assert_eq!(patch_count(&r), 1);
    }

    #[test]
    fn globs_match_the_file_name_or_the_whole_path() {
        let e = |p: &str| FileEntry {
            path: p.to_string(),
            versions: Vec::new(),
            head: None,
        };
        let entries = vec![
            e("a.img"),
            e("boot/b.IMG"),
            e("boot/c.img.bak"),
            e("d.imgx"),
            e("sub/deep/e.txt"),
            e("x1.txt"),
        ];
        let pick = |text: &str| -> Vec<&str> {
            Filter {
                text: text.into(),
                binary_only: false,
            }
            .select(&entries, &HashSet::new(), None)
            .into_iter()
            .map(|i| entries[i].path.as_str())
            .collect()
        };
        assert_eq!(pick("*.img"), ["a.img", "boot/b.IMG"]);
        assert_eq!(
            pick("*.img*"),
            ["a.img", "boot/b.IMG", "boot/c.img.bak", "d.imgx"]
        );
        assert_eq!(pick("x?.txt"), ["x1.txt"]);
        assert_eq!(pick("sub/*"), ["sub/deep/e.txt"]);
        assert_eq!(pick("boot/*.img"), ["boot/b.IMG"]);
        assert_eq!(pick("*.img boot"), ["boot/b.IMG"]);
        assert_eq!(pick("img").len(), 4, "a plain word is still a substring");
        assert_eq!(pick("*").len(), 6);
        assert!(pick("*.zip").is_empty());
    }

    #[test]
    fn patches_are_listed_read_and_trashed() {
        let r = setup();
        let git_dir = r.0.join(".git");
        assert!(list_patches(&git_dir).unwrap().is_empty());

        run(&r.0, opts(Some("one"), &["shared.txt", "only_one.txt"])).unwrap();
        let list = list_patches(&git_dir).unwrap();
        assert_eq!(list.len(), 2);
        let mut sums: Vec<&str> = list.iter().map(|p| p.summary.as_str()).collect();
        sums.sort();
        assert!(
            sums[0].starts_with("restore only_one.txt from "),
            "{sums:?}"
        );
        assert!(sums[1].starts_with("restore shared.txt from "), "{sums:?}");
        assert!(list.iter().all(|p| p.size > 0 && p.modified.is_some()));

        let text = read_patch(&git_dir, &list[0].name).unwrap();
        assert!(text.contains("diff --git"), "{text}");

        assert_eq!(trash_patches(&git_dir, &[list[0].name.clone()]).unwrap(), 1);
        assert_eq!(list_patches(&git_dir).unwrap().len(), 1);
        // Already gone is fine; a name with a path in it is refused and nothing outside is touched.
        assert_eq!(trash_patches(&git_dir, &[list[0].name.clone()]).unwrap(), 1);
        assert!(trash_patches(&git_dir, &["../HEAD".to_string()]).is_err());
        assert!(read_patch(&git_dir, "../HEAD").is_err());
        assert!(git_dir.join("HEAD").exists());
    }

    #[test]
    fn a_file_without_a_header_is_listed_as_such() {
        let r = setup();
        let dir = patch::patch_dir(&r.0.join(".git"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foreign.patch"), "not a gitomic patch\n").unwrap();
        let list = list_patches(&r.0.join(".git")).unwrap();
        assert_eq!(list[0].summary, "(no gitomic header)");
    }

    // `main` holds a.txt, b.txt and bin.dat. A stash (message "my work") changes a.txt and bin.dat
    // and carries the untracked new.txt and new.bin; afterwards HEAD gains a commit that changes
    // b.txt, so the stash's tree differs from HEAD in b.txt too without the stash having touched it.
    fn stashed() -> Repo {
        let r = Repo::new();
        r.write("a.txt", "a\n");
        r.write("b.txt", "b\n");
        std::fs::write(r.0.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.write("a.txt", "a-stashed\n");
        std::fs::write(r.0.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        r.write("new.txt", "new\n");
        std::fs::write(r.0.join("new.bin"), [0u8, 7]).unwrap();
        r.git(&["stash", "push", "-q", "-u", "-m", "my work"]);
        r.commit_file("b.txt", "b2\n", "drift");
        r
    }

    fn stash_opts(from: Option<&str>, paths: &[&str]) -> Opts {
        Opts {
            stash: true,
            ..opts(from, paths)
        }
    }

    #[test]
    fn stash_names_are_recognised_and_a_bare_number_stands_for_one() {
        assert!(is_stash("stash@{0}") && is_stash("stash@{12}"));
        assert!(!is_stash("main") && !is_stash("stash") && !is_stash("origin/stash@{0}"));
        assert_eq!(stash_ref("2"), "stash@{2}");
        assert_eq!(stash_ref("stash@{1}"), "stash@{1}");
        assert_eq!(stash_ref("main"), "main");
    }

    #[test]
    fn stashes_are_listed_after_the_branches_with_their_message() {
        let r = stashed();
        let list = sources(&r.0).unwrap();
        let last = list.last().unwrap();
        assert_eq!(last.name, "stash@{0}");
        assert!(last.is_stash() && !last.remote);
        assert!(last.note.ends_with("my work"), "{}", last.note);
    }

    #[test]
    fn a_stash_offers_what_it_changed_and_not_what_head_gained_since() {
        let r = stashed();
        let mut index = Index::open(&r.0).unwrap();
        let entries = index.entries(&names(&["stash@{0}"])).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "bin.dat", "new.bin", "new.txt"]);
        let new = entries.iter().find(|e| e.path == "new.txt").unwrap();
        assert_eq!(new.state(), State::Absent);
        // The untracked file is read from the commit that holds it, not from the stash's own tree.
        let from = &new.versions[0].commit;
        assert_eq!(
            r.git(&["cat-file", "-p", &format!("{from}:new.txt")]),
            "new"
        );
        let a = entries.iter().find(|e| e.path == "a.txt").unwrap();
        assert_eq!(a.state(), State::Differs);
    }

    #[test]
    fn binary_files_of_a_stash_are_found_tracked_or_not() {
        let r = stashed();
        let mut index = Index::open(&r.0).unwrap();
        let bin = index.binary_paths(&names(&["stash@{0}"])).unwrap();
        let mut got: Vec<&str> = bin.iter().map(String::as_str).collect();
        got.sort();
        assert_eq!(got, ["bin.dat", "new.bin"]);
    }

    #[test]
    fn a_stash_has_no_history_of_its_own() {
        let r = stashed();
        let mut index = Index::open(&r.0).unwrap();
        assert!(index.tips(&names(&["stash@{0}"])).unwrap().is_empty());
    }

    #[test]
    fn one_file_comes_out_of_a_stash_and_the_stash_stays_as_it_was() {
        let r = stashed();
        let before = r.git(&["rev-parse", "stash@{0}"]);
        run(&r.0, stash_opts(None, &["a.txt"])).unwrap();
        assert_eq!(r.read("a.txt"), "a-stashed\n");
        assert_eq!(r.read("b.txt"), "b2\n", "other files are left alone");
        assert!(!r.exists("new.txt"));
        assert_eq!(r.git(&["rev-parse", "stash@{0}"]), before);
        assert_eq!(r.git(&["stash", "list"]).lines().count(), 1);
    }

    #[test]
    fn an_untracked_file_of_a_stash_can_be_taken() {
        let r = stashed();
        run(&r.0, stash_opts(Some("0"), &["new.txt"])).unwrap();
        assert_eq!(r.read("new.txt"), "new\n");
        assert!(!r.exists("a.txt") || r.read("a.txt") == "a\n");
        assert_eq!(r.git(&["stash", "list"]).lines().count(), 1);
    }

    #[test]
    fn a_number_after_from_picks_the_older_stash() {
        let r = stashed();
        r.write("a.txt", "a-second\n");
        r.git(&["stash", "push", "-q", "-m", "second"]);
        run(&r.0, stash_opts(Some("1"), &["a.txt"])).unwrap();
        assert_eq!(r.read("a.txt"), "a-stashed\n");
        r.git(&["checkout", "-q", "--", "a.txt"]);
        run(&r.0, stash_opts(None, &["a.txt"])).unwrap();
        assert_eq!(
            r.read("a.txt"),
            "a-second\n",
            "the default is the newest stash"
        );
    }

    #[test]
    fn stash_refuses_a_branch_and_a_missing_stash() {
        let r = setup();
        assert!(run(&r.0, stash_opts(Some("one"), &["shared.txt"])).is_err());
        let err = run(&r.0, stash_opts(None, &["a.txt"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a stash"), "{err}");
    }

    // `main` holds a.txt, b.txt, c.txt, gone.txt and bin.dat. Unstaged changes: a.txt (only in the
    // work tree), c.txt (staged as c-staged, then edited again), bin.dat (binary), gone.txt
    // (deleted); b.txt is edited and staged only; untracked.txt is new. So the listed files are
    // a.txt, bin.dat, c.txt and gone.txt.
    fn edited() -> Repo {
        let r = Repo::new();
        for f in ["a.txt", "b.txt", "c.txt", "gone.txt"] {
            r.write(f, &format!("{f}\n"));
        }
        std::fs::write(r.0.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.write("a.txt", "a-edited\n");
        r.write("b.txt", "b-staged\n");
        r.git(&["add", "b.txt"]);
        r.write("c.txt", "c-staged\n");
        r.git(&["add", "c.txt"]);
        r.write("c.txt", "c-edited\n");
        std::fs::write(r.0.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        std::fs::remove_file(r.0.join("gone.txt")).unwrap();
        r.write("untracked.txt", "u\n");
        r
    }

    fn modified_opts(paths: &[&str]) -> Opts {
        opts(Some(MODIFIED), paths)
    }

    fn open_session(r: &Repo) {
        r.git(&["update-ref", "refs/gitomic/base/main", "HEAD"]);
    }

    #[test]
    fn the_modified_files_are_offered_only_when_there_are_some() {
        let r = edited();
        let s = sources(&r.0).unwrap();
        let m = s.iter().find(|s| s.is_modified()).expect("offered");
        assert_eq!(
            m.note,
            "4 files with unstaged changes; restoring discards them for good"
        );
        assert!(!m.remote && !m.is_stash());
        r.git(&["checkout", "-q", "--", "."]);
        assert!(!sources(&r.0).unwrap().iter().any(|s| s.is_modified()));
    }

    #[test]
    fn only_tracked_files_with_unstaged_edits_are_listed_at_their_staged_state() {
        let r = edited();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&[MODIFIED])).unwrap();
        let paths: Vec<&str> = e.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a.txt", "bin.dat", "c.txt", "gone.txt"],
            "not b (staged only) or the untracked file"
        );
        let a = &e[0];
        assert_eq!(a.versions.len(), 1);
        assert_eq!(a.versions[0].blob, r.git(&["rev-parse", "HEAD:a.txt"]));
        assert_eq!(a.head.as_deref(), Some(a.versions[0].blob.as_str()));
        assert_eq!(a.state(), State::Differs);
        // c.txt's version is the staged content, which is not HEAD's.
        let c = e.iter().find(|e| e.path == "c.txt").unwrap();
        assert_eq!(c.versions[0].blob, r.git(&["rev-parse", ":c.txt"]));
        assert_ne!(Some(c.versions[0].blob.as_str()), c.head.as_deref());
    }

    #[test]
    fn the_version_commit_is_always_head() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "base");
        r.write("a.txt", "a2\n");
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&[MODIFIED])).unwrap();
        assert_eq!(&*e[0].versions[0].commit, r.head());
    }

    #[test]
    fn modified_files_have_no_history_and_binary_ones_are_found() {
        let r = edited();
        let mut idx = Index::open(&r.0).unwrap();
        assert!(idx.tips(&names(&[MODIFIED])).unwrap().is_empty());
        let bin = idx.binary_paths(&names(&[MODIFIED])).unwrap();
        assert_eq!(bin.into_iter().collect::<Vec<_>>(), ["bin.dat"]);
    }

    #[test]
    fn the_preview_shows_the_work_tree_turning_back_into_the_staged_state() {
        let r = edited();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&[MODIFIED])).unwrap();
        let c = e.iter().find(|e| e.path == "c.txt").unwrap();
        let text = diff_text(&r.0, &c.versions[0], "c.txt", None).unwrap();
        assert!(
            text.contains("-c-edited") && text.contains("+c-staged"),
            "{text}"
        );
    }

    #[test]
    fn named_files_go_back_and_nothing_else_is_touched_or_recorded() {
        let r = edited();
        let head = r.head();
        run(&r.0, modified_opts(&["a.txt"])).unwrap();
        assert_eq!(r.read("a.txt"), "a.txt\n");
        // No commit, and the other edits stay where they were.
        assert_eq!(r.head(), head);
        assert_eq!(r.read("c.txt"), "c-edited\n");
        assert_eq!(r.read("b.txt"), "b-staged\n");
        assert_eq!(r.read("untracked.txt"), "u\n");
        assert_eq!(r.git(&["diff", "--cached", "--name-only"]), "b.txt\nc.txt");
        assert!(patch_files(&r).is_empty(), "no patch file is written");
    }

    // The patch files in the repository's patch directory.
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
    fn a_file_with_staged_work_returns_to_the_staged_state_not_to_head() {
        let r = edited();
        run(&r.0, modified_opts(&["c.txt"])).unwrap();
        assert_eq!(r.read("c.txt"), "c-staged\n");
        assert_eq!(r.git(&["show", ":c.txt"]), "c-staged");
        assert_eq!(r.git(&["show", "HEAD:c.txt"]), "c.txt");
    }

    #[test]
    fn it_works_with_or_without_a_session_and_records_nothing_either_way() {
        for session in [false, true] {
            let r = edited();
            if session {
                open_session(&r);
            }
            let head = r.head();
            run(&r.0, modified_opts(&["a.txt", "bin.dat"])).unwrap();
            assert_eq!(r.read("a.txt"), "a.txt\n");
            assert_eq!(std::fs::read(r.0.join("bin.dat")).unwrap(), [0u8, 1, 2, 3]);
            assert_eq!(r.head(), head, "session: {session}");
        }
    }

    #[test]
    fn the_spec_of_a_modified_file_discards_it_and_names_no_commit() {
        let r = edited();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&[MODIFIED])).unwrap();
        let spec = spec_for(&r.0, &e[0].versions[0], "a.txt").unwrap();
        assert_eq!(spec.discard, ["a.txt"]);
        assert!(spec.restore.is_empty() && spec.picks.is_empty());
    }

    #[test]
    fn a_file_without_unstaged_edits_is_refused() {
        let r = edited();
        open_session(&r);
        for f in ["b.txt", "untracked.txt", "nope.txt"] {
            let err = run(&r.0, modified_opts(&[f])).unwrap_err().to_string();
            assert!(err.contains("no unstaged changes"), "{f}: {err}");
        }
        assert_eq!(r.read("a.txt"), "a-edited\n");
    }

    #[test]
    fn a_dry_run_changes_nothing_and_a_patch_only_run_is_refused() {
        let r = edited();
        open_session(&r);
        let head = r.head();
        let mut o = modified_opts(&["a.txt"]);
        o.dry_run = true;
        run(&r.0, o).unwrap();
        assert_eq!(r.read("a.txt"), "a-edited\n");
        assert_eq!(r.head(), head);
        let mut o = modified_opts(&["a.txt"]);
        o.patch_only = true;
        assert!(run(&r.0, o).is_err());
        assert_eq!(r.read("a.txt"), "a-edited\n");
    }

    // A stash holding edits to lines 2 and 8 of a ten-line file, taken before HEAD gained edits to lines 5
    // and 8 of the same file.
    fn merge_case() -> Repo {
        let r = Repo::new();
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.write("f.txt", &Repo::lines_with(&[(2, "S2"), (8, "S8")]));
        r.git(&["stash", "push", "-q", "-m", "mine"]);
        r.commit_file("f.txt", &Repo::lines_with(&[(5, "W5"), (8, "W8")]), "drift");
        r
    }

    fn merge_opts(paths: &[&str]) -> Opts {
        Opts {
            merge: true,
            ..stash_opts(None, paths)
        }
    }

    // Answer the screen's units in order with the given keys, as an operator would, and return the files.
    fn answered(
        entries: Vec<crate::resolve::Conflicted>,
        keys: &str,
    ) -> Vec<crate::resolve::Conflicted> {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::resolve::App::new(entries, (String::new(), String::new()));
        for c in keys.chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.remaining(), 0);
        app.files
    }

    fn merged_bytes(files: &[crate::resolve::Conflicted]) -> Vec<(String, Vec<u8>)> {
        files
            .iter()
            .map(|c| match &c.body {
                crate::resolve::Body::Hunks(segs) => {
                    (c.path.clone(), conflict::render(segs).unwrap())
                }
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn a_stashed_file_is_compared_with_the_work_tree_change_by_change() {
        let r = merge_case();
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, same) = merge_entries(&r.0, &picks).unwrap();
        assert!(same.is_empty());
        assert_eq!(entries.len(), 1);
        // Lines 2, 5 and 8 differ between the work tree and the stash.
        assert_eq!(entries[0].units(), 3);
    }

    fn merge_opts_from_stash(paths: &[&str]) -> Opts {
        Opts {
            from: Some("stash@{0}".to_string()),
            ..merge_opts(paths)
        }
    }

    #[test]
    fn each_change_is_taken_or_kept_on_its_own_and_the_stash_is_untouched() {
        let r = merge_case();
        let stash = r.git(&["rev-parse", "stash@{0}"]);
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, _) = merge_entries(&r.0, &picks).unwrap();
        // Take the stash's line 2, keep the work tree's line 5, keep the work tree's line 8.
        let files = answered(entries, "baa");
        cherry::apply_contents(&r.0, merged_bytes(&files), "stash@{0}").unwrap();
        assert_eq!(
            r.read("f.txt"),
            Repo::lines_with(&[(2, "S2"), (5, "W5"), (8, "W8")])
        );
        assert_eq!(r.git(&["rev-parse", "stash@{0}"]), stash);
        assert_eq!(r.git(&["stash", "list"]).lines().count(), 1);
    }

    #[test]
    fn both_sides_of_a_change_can_be_kept() {
        let r = merge_case();
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, _) = merge_entries(&r.0, &picks).unwrap();
        let files = answered(entries, "aac");
        cherry::apply_contents(&r.0, merged_bytes(&files), "stash@{0}").unwrap();
        assert!(r.read("f.txt").contains("W8\nS8\n"), "{}", r.read("f.txt"));
    }

    #[test]
    fn in_a_session_the_merge_is_one_commit_after_the_pending_edits_are_captured() {
        let r = merge_case();
        r.git(&["update-ref", "refs/gitomic/base/main", "HEAD"]);
        let head = r.head();
        // A pending edit to another tracked line must be recorded first, not swallowed by the merge.
        r.write(
            "f.txt",
            &Repo::lines_with(&[(5, "W5"), (8, "W8"), (10, "PENDING")]),
        );
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, _) = merge_entries(&r.0, &picks).unwrap();
        let files = answered(entries, "bbbb");
        cherry::apply_contents(&r.0, merged_bytes(&files), "stash@{0}").unwrap();
        assert_eq!(
            r.git(&["rev-list", "--count", &format!("{head}..HEAD")]),
            "2"
        );
        // The first of the two commits holds the pending edit, the second the merge.
        assert!(r.git(&["show", "HEAD~1:f.txt"]).contains("PENDING"));
        assert!(!r.git(&["show", "HEAD:f.txt"]).contains("PENDING"));
        assert!(r.git(&["show", "HEAD:f.txt"]).contains("S2"));
    }

    #[test]
    fn outside_a_session_the_merge_is_left_uncommitted() {
        let r = merge_case();
        let head = r.head();
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, _) = merge_entries(&r.0, &picks).unwrap();
        let files = answered(entries, "bbb");
        cherry::apply_contents(&r.0, merged_bytes(&files), "stash@{0}").unwrap();
        assert_eq!(r.head(), head);
        assert!(r.git(&["status", "--porcelain"]).contains("f.txt"));
    }

    #[test]
    fn a_file_the_stash_holds_unchanged_needs_no_decision() {
        let r = merge_case();
        r.write("f.txt", &Repo::lines_with(&[(2, "S2"), (8, "S8")]));
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["f.txt"])).unwrap();
        let (entries, same) = merge_entries(&r.0, &picks).unwrap();
        assert!(entries.is_empty());
        assert_eq!(same, ["f.txt"]);
    }

    #[test]
    fn binary_and_missing_files_are_refused_with_a_reason() {
        let r = stashed();
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["bin.dat"])).unwrap();
        let err = merge_entries(&r.0, &picks).unwrap_err().to_string();
        assert!(err.contains("binary"), "{err}");

        // new.txt exists only in the stash; the work tree has no copy to merge into.
        let picks = picks_from_paths(&r.0, &merge_opts_from_stash(&["new.txt"])).unwrap();
        let err = merge_entries(&r.0, &picks).unwrap_err().to_string();
        assert!(err.contains("not in the work tree"), "{err}");
    }

    #[test]
    fn merge_needs_paths_and_has_no_patch_only_form() {
        let r = merge_case();
        let err = run(&r.0, merge_opts(&[])).unwrap_err().to_string();
        assert!(err.contains("name the files"), "{err}");
        let mut o = merge_opts(&["f.txt"]);
        o.patch_only = true;
        let err = run(&r.0, o).unwrap_err().to_string();
        assert!(err.contains("patch"), "{err}");
    }

    #[test]
    fn a_dry_run_reports_the_changes_and_writes_nothing() {
        let r = merge_case();
        let before = r.read("f.txt");
        let mut o = merge_opts(&["f.txt"]);
        o.dry_run = true;
        run(&r.0, o).unwrap();
        assert_eq!(r.read("f.txt"), before);
    }

    #[test]
    fn a_deleted_file_comes_back_and_is_previewed_as_returning() {
        let r = edited();
        let mut idx = Index::open(&r.0).unwrap();
        let e = idx.entries(&names(&[MODIFIED])).unwrap();
        let gone = e.iter().find(|e| e.path == "gone.txt").unwrap();
        let text = diff_text(&r.0, &gone.versions[0], "gone.txt", None).unwrap();
        assert!(text.contains("+gone.txt"), "{text}");
        run(&r.0, modified_opts(&["gone.txt"])).unwrap();
        assert_eq!(r.read("gone.txt"), "gone.txt\n");
        assert_eq!(r.read("a.txt"), "a-edited\n", "the other changes stay");
    }
}
