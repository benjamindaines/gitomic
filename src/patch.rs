// Patch construction for `gitomic cherry-pick` (issue #13). A patch is computed against one
// specific commit (its base) by replaying the chosen commits, oldest first, onto that base inside
// the object database with `git merge-tree`; the result is the diff between the base and the final
// tree. Nothing here reads or writes the index or work tree of the repository, so building a patch,
// however often it is repeated, cannot disturb uncommitted work.
//
// The module is deliberately independent of the terminal and of session handling, and a patch
// carries a small header naming its base, its source commits, and every conflict decision taken.
// That makes a patch reproducible: `rebuild` recomputes it against a newer base, reusing each
// recorded decision whose conflict reappears unchanged. The command layer uses this when HEAD
// advances between choosing commits and applying them, and the same function is the natural core of
// a stand-alone patch re-write command should one be added.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::conflict::{self, Segment, Side};
use crate::{git, Res};

// A file version as git's index records it: mode and blob id.
#[derive(Clone, Debug)]
pub struct Blob {
    pub mode: String,
    pub oid: String,
}

// How one conflicted path is decided.
#[derive(Clone, Debug)]
pub enum Body {
    // A text file with conflict hunks, each decided on its own.
    Hunks(Vec<Segment>),
    // A file decided as a whole: binary content, a text file whose markers could not be parsed, or
    // a modify/delete conflict. `None` on either side stands for "absent there".
    Whole {
        ours: Option<Blob>,
        theirs: Option<Blob>,
        choice: Option<Side>,
    },
}

#[derive(Clone, Debug)]
pub struct FileConflict {
    pub path: String,
    // Mode used for a file rebuilt from its hunks.
    pub mode: String,
    pub body: Body,
}

impl FileConflict {
    // Number of separate decisions this file needs.
    pub fn units(&self) -> usize {
        match &self.body {
            Body::Hunks(segs) => conflict::hunk_count(segs),
            Body::Whole { .. } => 1,
        }
    }

    pub fn unresolved(&self) -> usize {
        match &self.body {
            Body::Hunks(segs) => conflict::unresolved_count(segs),
            Body::Whole { choice, .. } => usize::from(choice.is_none()),
        }
    }

    // The current decision of unit `unit`.
    pub fn choice(&self, unit: usize) -> Option<Side> {
        match &self.body {
            Body::Hunks(segs) => hunks(segs).nth(unit).and_then(|h| h.choice),
            Body::Whole { choice, .. } => choice.filter(|_| unit == 0),
        }
    }

    // Record a decision for unit `unit`. Returns false, changing nothing, when the unit does not
    // exist or the side is not available for it (a whole-file conflict has no "both").
    pub fn set(&mut self, unit: usize, side: Option<Side>) -> bool {
        match &mut self.body {
            Body::Hunks(segs) => match hunks_mut(segs).nth(unit) {
                Some(h) => {
                    h.choice = side;
                    true
                }
                None => false,
            },
            Body::Whole { choice, .. } => {
                if unit != 0 || side == Some(Side::Both) {
                    return false;
                }
                *choice = side;
                true
            }
        }
    }

    // Stable identities of the units, in order; see `conflict::fingerprint`.
    pub fn fingerprints(&self) -> Vec<String> {
        match &self.body {
            Body::Hunks(segs) => hunks(segs)
                .map(|h| conflict::fingerprint(&self.path, &h.ours, &h.theirs))
                .collect(),
            Body::Whole { ours, theirs, .. } => {
                let id = |b: &Option<Blob>| b.as_ref().map_or("-".to_string(), |b| b.oid.clone());
                vec![conflict::fingerprint(
                    &self.path,
                    id(ours).as_bytes(),
                    id(theirs).as_bytes(),
                )]
            }
        }
    }

    // Adopt remembered decisions for every unit whose fingerprint is known.
    fn apply_saved(&mut self, saved: &[(String, Side)]) {
        for (unit, fp) in self.fingerprints().iter().enumerate() {
            if let Some((_, side)) = saved.iter().find(|(f, _)| f == fp) {
                self.set(unit, Some(*side));
            }
        }
    }

    // The decisions taken so far, as (fingerprint, side) pairs.
    fn decisions(&self) -> Vec<(String, Side)> {
        self.fingerprints()
            .into_iter()
            .enumerate()
            .filter_map(|(unit, fp)| self.choice(unit).map(|side| (fp, side)))
            .collect()
    }
}

fn hunks(segs: &[Segment]) -> impl Iterator<Item = &conflict::Hunk> {
    segs.iter().filter_map(|s| match s {
        Segment::Hunk(h) => Some(h),
        Segment::Text(_) => None,
    })
}

fn hunks_mut(segs: &mut [Segment]) -> impl Iterator<Item = &mut conflict::Hunk> {
    segs.iter_mut().filter_map(|s| match s {
        Segment::Hunk(h) => Some(h),
        Segment::Text(_) => None,
    })
}

// The result of merging one commit onto a base.
pub enum Merged {
    Clean(String),
    Conflicted {
        tree: String,
        files: Vec<FileConflict>,
    },
    // Conflict kinds that have no A/B decision (renames, directory/file clashes, and the like),
    // with git's own description of each.
    Unsupported(Vec<String>),
}

// Apply `pick`'s own change (its diff against its first parent) onto the commit `onto`, in the
// object database. `merge.conflictStyle` is pinned so the marker layout is the one
// `conflict::parse` expects whatever the user's configuration says.
pub fn merge(root: &Path, onto: &str, pick: &str) -> Res<Merged> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "merge.conflictStyle=merge"])
        .args(["merge-tree", "--write-tree", "-z"])
        .arg(format!("--merge-base={pick}^"))
        .arg(onto)
        .arg(pick)
        .output()?;
    match out.status.code() {
        Some(0) => {
            let tree = out
                .stdout
                .split(|&b| b == 0)
                .next()
                .map(|t| String::from_utf8_lossy(t).trim().to_string())
                .unwrap_or_default();
            if tree.is_empty() {
                return Err("git merge-tree: no tree id in output".into());
            }
            Ok(Merged::Clean(tree))
        }
        Some(1) => parse_conflicts(root, &out.stdout),
        _ => {
            let msg = String::from_utf8_lossy(&out.stderr);
            Err(format!("git merge-tree: {}", msg.trim()).into())
        }
    }
}

// Conflict kinds `merge` can present for a decision.
const SUPPORTED: [&str; 4] = [
    "CONFLICT (contents)",
    // Reported alongside a contents conflict when the file is binary.
    "CONFLICT (binary)",
    "CONFLICT (modify/delete)",
    "CONFLICT (add/add)",
];

#[derive(Default)]
struct Stages {
    ours: Option<Blob>,
    theirs: Option<Blob>,
    base: bool,
}

// Parse the NUL-delimited output of `git merge-tree --write-tree -z` for a conflicted merge: the
// tree id, one `<mode> <oid> <stage>\t<path>` record per conflicted index entry, an empty field,
// and then groups of informational messages (`<n>`, n paths, a type, a message).
fn parse_conflicts(root: &Path, stdout: &[u8]) -> Res<Merged> {
    let mut fields = stdout.split(|&b| b == 0);
    let tree = String::from_utf8_lossy(fields.next().unwrap_or_default())
        .trim()
        .to_string();
    if tree.is_empty() {
        return Err("git merge-tree: no tree id in output".into());
    }

    let mut stages: BTreeMap<String, Stages> = BTreeMap::new();
    for field in fields.by_ref() {
        if field.is_empty() {
            break;
        }
        let text = String::from_utf8_lossy(field);
        let (meta, path) = text
            .split_once('\t')
            .ok_or("git merge-tree: malformed conflict entry")?;
        let mut parts = meta.split(' ');
        let (mode, oid, stage) = (parts.next(), parts.next(), parts.next());
        let (Some(mode), Some(oid), Some(stage)) = (mode, oid, stage) else {
            return Err("git merge-tree: malformed conflict entry".into());
        };
        let entry = stages.entry(path.to_string()).or_default();
        let blob = Blob {
            mode: mode.to_string(),
            oid: oid.to_string(),
        };
        match stage {
            "1" => entry.base = true,
            "2" => entry.ours = Some(blob),
            "3" => entry.theirs = Some(blob),
            _ => return Err("git merge-tree: unexpected stage".into()),
        }
    }

    let mut unsupported = Vec::new();
    let rest: Vec<&[u8]> = fields.collect();
    let mut i = 0;
    while i < rest.len() {
        let Ok(n) = String::from_utf8_lossy(rest[i]).trim().parse::<usize>() else {
            break;
        };
        let ty = rest.get(i + 1 + n).map(|f| String::from_utf8_lossy(f));
        let msg = rest.get(i + 2 + n).map(|f| String::from_utf8_lossy(f));
        if let (Some(ty), Some(msg)) = (ty, msg) {
            if ty.starts_with("CONFLICT") && !SUPPORTED.contains(&ty.as_ref()) {
                unsupported.push(msg.trim().to_string());
            }
        }
        i += 3 + n;
    }
    if !unsupported.is_empty() {
        return Ok(Merged::Unsupported(unsupported));
    }

    let mut files = Vec::new();
    for (path, st) in stages {
        let mode = st
            .ours
            .as_ref()
            .or(st.theirs.as_ref())
            .map_or("100644".to_string(), |b| b.mode.clone());
        let body = match (&st.ours, &st.theirs) {
            (Some(_), Some(_)) => {
                let content = git::cat_blob(root, &format!("{tree}:{path}")).ok();
                let parsed = content
                    .filter(|c| !c.contains(&0))
                    .and_then(|c| conflict::parse(&c));
                match parsed {
                    Some(segs) => Body::Hunks(segs),
                    None => Body::Whole {
                        ours: st.ours,
                        theirs: st.theirs,
                        choice: None,
                    },
                }
            }
            (Some(_), None) | (None, Some(_)) => Body::Whole {
                ours: st.ours,
                theirs: st.theirs,
                choice: None,
            },
            (None, None) => {
                return Ok(Merged::Unsupported(vec![format!(
                    "{path}: conflict with no version on either side (base only)"
                )]));
            }
        };
        files.push(FileConflict { path, mode, body });
    }
    Ok(Merged::Conflicted { tree, files })
}

static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);

// A throwaway index file, removed when dropped. Trees are assembled in it with plumbing commands
// (`GIT_INDEX_FILE` points git at it), so the repository's own index and work tree are never
// involved.
struct TempIndex<'a> {
    root: &'a Path,
    path: PathBuf,
}

impl<'a> TempIndex<'a> {
    fn new(root: &'a Path) -> Res<TempIndex<'a>> {
        let git_dir = git::git_dir(root)?;
        let n = NEXT_INDEX.fetch_add(1, Ordering::SeqCst);
        let path = git_dir.join(format!("gitomic-pick-{}-{n}.idx", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Ok(TempIndex { root, path })
    }

    fn run(&self, args: &[&str]) -> Res<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.root)
            .args(args)
            .env("GIT_INDEX_FILE", &self.path)
            .output()?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr);
            return Err(format!("git {}: {}", args.join(" "), msg.trim()).into());
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }

    // Make `path` in the index match `path` in the commit `source`: the same mode and blob, or
    // absent when `source` has no such file.
    fn set_from(&self, source: &str, path: &str) -> Res<()> {
        let listing = git::run(
            self.root,
            &["ls-tree", "-z", "--full-tree", source, "--", path],
        )?;
        match listing.split('\t').next().filter(|m| !m.is_empty()) {
            Some(meta) => {
                let mut parts = meta.split(' ');
                let (Some(mode), Some(_), Some(oid)) = (parts.next(), parts.next(), parts.next())
                else {
                    return Err(format!("git ls-tree: unexpected output for {path}").into());
                };
                self.run(&["update-index", "--add", "--cacheinfo", mode, oid, path])?;
            }
            None => {
                self.run(&["update-index", "--force-remove", "--", path])?;
            }
        }
        Ok(())
    }
}

impl Drop for TempIndex<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// Turn a conflicted tree and a full set of decisions into a clean tree.
fn resolved_tree(root: &Path, tree: &str, files: &[FileConflict]) -> Res<String> {
    let idx = TempIndex::new(root)?;
    idx.run(&["read-tree", tree])?;
    for file in files {
        match &file.body {
            Body::Hunks(segs) => {
                let bytes = conflict::render(segs)
                    .ok_or_else(|| format!("{}: a hunk is still undecided", file.path))?;
                let oid = git::hash_object_write(root, &bytes)?;
                let args = [
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &file.mode,
                    &oid,
                    &file.path,
                ];
                idx.run(&args)?;
            }
            Body::Whole {
                ours,
                theirs,
                choice,
            } => {
                let picked = match choice {
                    Some(Side::A) => ours,
                    Some(Side::B) => theirs,
                    _ => return Err(format!("{}: not decided", file.path).into()),
                };
                match picked {
                    Some(b) => {
                        let args = [
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &b.mode,
                            &b.oid,
                            &file.path,
                        ];
                        idx.run(&args)?;
                    }
                    None => {
                        idx.run(&["update-index", "--force-remove", "--", &file.path])?;
                    }
                }
            }
        }
    }
    idx.run(&["write-tree"])
}

// A commit carrying only `path`'s part of `pick`'s change: its parent is `pick`'s parent, and its
// tree is that parent's tree with `path` set to what it is in `pick` (or removed, if `pick` deleted
// it). Replaying it applies the one file's change and leaves every other file of a multi-file
// commit out. Merge and root commits have no single parent to diff against and are refused.
pub fn scoped_commit(root: &Path, pick: &str, path: &str) -> Res<String> {
    if path.contains('\n') {
        return Err(
            format!("cannot restrict a pick to a path containing a newline: {path:?}").into(),
        );
    }
    let parents = git::run(root, &["show", "-s", "--format=%P", pick])?;
    let parent = match parents.split_whitespace().collect::<Vec<_>>()[..] {
        [p] => p.to_string(),
        [] => return Err(format!("{pick} is a root commit").into()),
        _ => return Err(format!("{pick} is a merge commit").into()),
    };
    let idx = TempIndex::new(root)?;
    idx.run(&["read-tree", &parent])?;
    idx.set_from(pick, path)?;
    let tree = idx.run(&["write-tree"])?;
    let message = git::commit_message(root, pick)?;
    let (name, email, date) = git::author_of(root, pick)?;
    git::commit_tree(root, &tree, &parent, &message, &name, &email, &date)
}

// The tree of the commit `onto` with `path` replaced by its content in the commit `source`.
fn restored_tree(root: &Path, onto: &str, source: &str, path: &str) -> Res<String> {
    let idx = TempIndex::new(root)?;
    idx.run(&["read-tree", onto])?;
    idx.set_from(source, path)?;
    idx.run(&["write-tree"])
}

// Progress of a replay: what remains to be merged and the running result.
pub struct Job {
    root: PathBuf,
    base: String,
    picks: Vec<String>,
    // A scratch commit whose tree is the running result; `base` until the first pick is merged.
    // Merge inputs must be commits, and the scratch commits are unreferenced, so they vanish at gc.
    tip: String,
    done: usize,
    pending_tree: Option<String>,
    decided: Vec<(String, Side)>,
    // (pick id, path): the pick is replayed restricted to that path.
    only: Vec<(String, String)>,
    // (commit, path): after the picks, `path` is set to its content in `commit`.
    restores: Vec<(String, String)>,
    restored: usize,
}

// Where a replay stopped.
pub enum Step {
    Done,
    // A pick conflicts; the files carry whatever decisions were already known.
    Conflicts(Vec<FileConflict>),
    Unsupported(Vec<String>),
}

impl Job {
    // `picks` are full commit ids, oldest first. `decided` seeds the decisions reused when a
    // matching conflict appears.
    pub fn new(root: &Path, base: &str, picks: &[String], decided: &[(String, Side)]) -> Job {
        Job {
            root: root.to_path_buf(),
            base: base.to_string(),
            picks: picks.to_vec(),
            tip: base.to_string(),
            done: 0,
            pending_tree: None,
            decided: decided.to_vec(),
            only: Vec::new(),
            restores: Vec::new(),
            restored: 0,
        }
    }

    // Restrict the listed picks to one path each, and add whole-file restores that run after the
    // picks. See `Spec::only` and `Spec::restore`.
    pub fn with_scopes(mut self, only: &[(String, String)], restores: &[(String, String)]) -> Job {
        self.only = only.to_vec();
        self.restores = restores.to_vec();
        self
    }

    // Merge picks until one needs a decision or all are merged. A conflict whose every unit already
    // has a remembered decision is resolved without stopping.
    pub fn advance(&mut self) -> Res<Step> {
        loop {
            if self.done < self.picks.len() {
                let pick = self.effective(&self.picks[self.done])?;
                match merge(&self.root, &self.tip, &pick)? {
                    Merged::Clean(tree) => {
                        self.take(&tree)?;
                        self.done += 1;
                    }
                    Merged::Unsupported(list) => return Ok(Step::Unsupported(list)),
                    Merged::Conflicted { tree, mut files } => {
                        for f in &mut files {
                            f.apply_saved(&self.decided);
                        }
                        if files.iter().all(|f| f.unresolved() == 0) {
                            let clean = resolved_tree(&self.root, &tree, &files)?;
                            self.take(&clean)?;
                            self.done += 1;
                        } else {
                            self.pending_tree = Some(tree);
                            return Ok(Step::Conflicts(files));
                        }
                    }
                }
            } else if self.restored < self.restores.len() {
                let (source, path) = self.restores[self.restored].clone();
                let tree = restored_tree(&self.root, &self.tip, &source, &path)?;
                self.take(&tree)?;
                self.restored += 1;
            } else {
                return Ok(Step::Done);
            }
        }
    }

    // The commit to merge for a pick: the pick itself, or its restricted form when scoped.
    fn effective(&self, id: &str) -> Res<String> {
        match self.only.iter().find(|(k, _)| k == id) {
            Some((_, path)) => scoped_commit(&self.root, id, path),
            None => Ok(id.to_string()),
        }
    }

    // Supply the decisions for the conflict `advance` reported, then continue with `advance`.
    pub fn resolve(&mut self, files: &[FileConflict]) -> Res<()> {
        let tree = self
            .pending_tree
            .take()
            .ok_or("no conflict is waiting for a decision")?;
        let clean = match resolved_tree(&self.root, &tree, files) {
            Ok(t) => t,
            Err(e) => {
                self.pending_tree = Some(tree);
                return Err(e);
            }
        };
        for f in files {
            for (fp, side) in f.decisions() {
                if !self.decided.iter().any(|(k, _)| *k == fp) {
                    self.decided.push((fp, side));
                }
            }
        }
        self.take(&clean)?;
        self.done += 1;
        Ok(())
    }

    // Adopt `tree` as the running result.
    fn take(&mut self, tree: &str) -> Res<()> {
        self.tip = git::commit_tree(
            &self.root,
            tree,
            &self.tip,
            "gitomic cherry-pick scratch",
            "gitomic",
            "gitomic@localhost",
            "1970-01-01T00:00:00+00:00",
        )?;
        Ok(())
    }

    // Every decision known to this job, taken now or seeded, for recording in a patch header.
    // The pick whose merge the replay stopped at, while there is one.
    pub fn current(&self) -> Option<&str> {
        self.picks.get(self.done).map(String::as_str)
    }

    pub fn decisions(&self) -> &[(String, Side)] {
        &self.decided
    }

    // The patch between the base and the running result.
    pub fn patch(&self, subjects: &[(String, String)]) -> Res<Patch> {
        let base = &self.base;
        let tip = &self.tip;
        let body = diff_bytes(
            &self.root,
            &[
                "diff",
                "--binary",
                "--full-index",
                "--no-renames",
                "--no-color",
                base,
                tip,
            ],
        )?;
        let names = diff_bytes(
            &self.root,
            &["diff", "--name-only", "-z", "--no-renames", base, tip],
        )?;
        let stat = String::from_utf8_lossy(&diff_bytes(
            &self.root,
            &["diff", "--stat", "--no-renames", "--no-color", base, tip],
        )?)
        .into_owned();
        let paths = names
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        Ok(Patch {
            spec: Spec {
                base: self.base.clone(),
                picks: subjects.to_vec(),
                decisions: self.decided.clone(),
                only: self.only.clone(),
                restore: self.restores.clone(),
            },
            body,
            paths,
            stat,
        })
    }
}

fn diff_bytes(root: &Path, args: &[&str]) -> Res<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.join(" "), msg.trim()).into());
    }
    Ok(out.stdout)
}

// What a patch was built from; enough to build it again.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    // The commit the patch applies to.
    pub base: String,
    // (full id, subject) of each source commit, oldest first.
    pub picks: Vec<(String, String)>,
    // (fingerprint, side) of every conflict decision.
    pub decisions: Vec<(String, Side)>,
    // (pick id, path): that pick contributes only its change to that path. This is how a multi-file
    // commit is applied for one file.
    pub only: Vec<(String, String)>,
    // (commit, path): after the picks, the file is set to its exact content in that commit, or
    // removed if the commit lacks it. A whole-file alternative to replaying a conflicting history.
    pub restore: Vec<(String, String)>,
}

const HEADER_TAG: &str = "# gitomic-patch 1";

impl Spec {
    pub fn ids(&self) -> Vec<String> {
        self.picks.iter().map(|(id, _)| id.clone()).collect()
    }

    // The header lines, ending with a blank line. `git apply` skips text ahead of the first
    // `diff --git` line, so a patch carrying this header applies unchanged.
    pub fn header(&self) -> String {
        let mut out = format!("{HEADER_TAG}\n# base {}\n", self.base);
        for (id, subject) in &self.picks {
            let subject = subject.replace(['\n', '\r'], " ");
            out.push_str(&format!("# pick {id} {subject}\n"));
        }
        for (id, path) in &self.only {
            out.push_str(&format!("# only {id} {path}\n"));
        }
        for (id, path) in &self.restore {
            out.push_str(&format!("# restore {id} {path}\n"));
        }
        for (fp, side) in &self.decisions {
            out.push_str(&format!("# decision {fp} {}\n", side.letter()));
        }
        out.push('\n');
        out
    }

    // Read a header back from the start of a patch file. None when the file does not begin with
    // one. Not used by a command yet: it is the entry point for re-basing an existing patch file,
    // and is covered by tests so that the format cannot drift unnoticed.
    #[allow(dead_code)]
    pub fn parse(patch: &[u8]) -> Option<Spec> {
        let text = String::from_utf8_lossy(&patch[..patch.len().min(64 * 1024)]).into_owned();
        let mut lines = text.lines();
        if lines.next()? != HEADER_TAG {
            return None;
        }
        let mut base = None;
        let mut picks = Vec::new();
        let mut decisions = Vec::new();
        let mut only = Vec::new();
        let mut restore = Vec::new();
        for line in lines {
            let Some(rest) = line.strip_prefix("# ") else {
                break;
            };
            if let Some(b) = rest.strip_prefix("base ") {
                base = Some(b.trim().to_string());
            } else if let Some(p) = rest.strip_prefix("pick ") {
                let (id, subject) = p.split_once(' ').unwrap_or((p, ""));
                picks.push((id.to_string(), subject.to_string()));
            } else if let Some(o) = rest.strip_prefix("only ") {
                let (id, path) = o.split_once(' ')?;
                only.push((id.to_string(), path.to_string()));
            } else if let Some(r) = rest.strip_prefix("restore ") {
                let (id, path) = r.split_once(' ')?;
                restore.push((id.to_string(), path.to_string()));
            } else if let Some(d) = rest.strip_prefix("decision ") {
                let (fp, side) = d.split_once(' ')?;
                let side = Side::from_letter(side.trim().chars().next()?)?;
                decisions.push((fp.to_string(), side));
            }
        }
        Some(Spec {
            base: base?,
            picks,
            decisions,
            only,
            restore,
        })
    }
}

// A finished patch: its recipe, the diff, and the paths the diff touches.
pub struct Patch {
    pub spec: Spec,
    body: Vec<u8>,
    pub paths: Vec<String>,
    // `git diff --stat` of the change, for reports.
    pub stat: String,
}

impl Patch {
    // True when applying the patch would change nothing (every pick is already present in the
    // base).
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    // The full file content: header, then the diff.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = self.spec.header().into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

// What a build produced.
pub enum Built {
    Ready(Box<Patch>),
    // A conflict needs a decision; the files are returned undecided.
    Conflicts(Vec<FileConflict>),
    Unsupported(Vec<String>),
}

// Replay `spec` against its own base. Decisions in the spec are reused where their conflicts recur.
pub fn build(root: &Path, spec: &Spec) -> Res<Built> {
    let mut job = Job::new(root, &spec.base, &spec.ids(), &spec.decisions)
        .with_scopes(&spec.only, &spec.restore);
    match job.advance()? {
        Step::Done => Ok(Built::Ready(Box::new(job.patch(&spec.picks)?))),
        Step::Conflicts(files) => Ok(Built::Conflicts(files)),
        Step::Unsupported(list) => Ok(Built::Unsupported(list)),
    }
}

// Recompute `spec` against `new_base`, typically the HEAD that appeared after the original patch
// was prepared. Decisions carry over wherever the same conflict recurs.
pub fn rebuild(root: &Path, spec: &Spec, new_base: &str) -> Res<Built> {
    build(
        root,
        &Spec {
            base: new_base.to_string(),
            ..spec.clone()
        },
    )
}

// Directory holding written patch files. Kept beside, not inside, the per-branch session state so a
// branch named like a subdirectory cannot collide with it.
pub fn patch_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("gitomic-picks")
}

static NEXT_PATCH: AtomicUsize = AtomicUsize::new(0);

// Write `patch` to a new file under `dir` and return its path. The name carries the base and the
// time, so successive patches never overwrite one another.
pub fn write(dir: &Path, patch: &Patch) -> Res<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let n = NEXT_PATCH.fetch_add(1, Ordering::SeqCst);
    let base: String = patch.spec.base.chars().take(8).collect();
    let path = dir.join(format!("pick-{secs}-{n}-{base}.patch"));
    std::fs::write(&path, patch.bytes())?;
    Ok(path)
}

// Test whether the patch file applies to the work tree, changing nothing. The error carries git's
// own explanation, which names the offending file and hunk.
pub fn check(root: &Path, file: &Path) -> Res<()> {
    apply_with(root, file, true)
}

// Apply the patch file to the work tree only; the index is not touched.
pub fn apply(root: &Path, file: &Path) -> Res<()> {
    apply_with(root, file, false)
}

fn apply_with(root: &Path, file: &Path, check_only: bool) -> Res<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).arg("apply");
    if check_only {
        cmd.arg("--check");
    }
    let out = cmd.arg(file).output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(msg.trim().to_string().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;

    // `main` and `feat` both edit the given lines of one file, starting from the same base. HEAD is
    // left on `main`. Returns the id of the commit on `feat`.
    fn diverge(r: &Repo, main_edits: &[(usize, &str)], feat_edits: &[(usize, &str)]) -> String {
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        let feat = r.commit_file("f.txt", &Repo::lines_with(feat_edits), "feat edit");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("f.txt", &Repo::lines_with(main_edits), "main edit");
        feat
    }

    fn conflicts(job: &mut Job) -> Vec<FileConflict> {
        match job.advance().unwrap() {
            Step::Conflicts(files) => files,
            Step::Done => panic!("expected a conflict, got a clean replay"),
            Step::Unsupported(l) => panic!("unsupported: {l:?}"),
        }
    }

    // Apply the job's patch to the work tree of `r` and return the resulting file.
    fn applied(r: &Repo, job: &Job, name: &str) -> String {
        let patch = job.patch(&[]).unwrap();
        let file = r.0.join("out.patch");
        std::fs::write(&file, patch.bytes()).unwrap();
        r.git(&["apply", file.to_str().unwrap()]);
        r.read(name)
    }

    #[test]
    fn a_clean_replay_needs_no_decision_and_yields_the_change() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        let feat = r.commit_file("b.txt", "b\n", "add b");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("c.txt", "c\n", "add c");

        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let patch = job.patch(&[]).unwrap();
        assert_eq!(patch.paths, vec!["b.txt".to_string()]);
        assert!(!patch.is_empty());
        assert!(patch.stat.contains("b.txt"));
    }

    #[test]
    fn each_hunk_can_take_a_different_side() {
        let r = Repo::new();
        let feat = diverge(
            &r,
            &[(2, "MAIN2"), (9, "MAIN9")],
            &[(2, "FEAT2"), (9, "FEAT9")],
        );
        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);

        let mut files = conflicts(&mut job);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].units(), 2);
        assert_eq!(files[0].unresolved(), 2);
        assert!(files[0].set(0, Some(Side::B)));
        assert!(files[0].set(1, Some(Side::A)));
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), Step::Done));

        let want = Repo::lines_with(&[(2, "FEAT2"), (9, "MAIN9")]);
        assert_eq!(applied(&r, &job, "f.txt"), want);
        assert_eq!(job.decisions().len(), 2);
    }

    #[test]
    fn keeping_both_sides_orders_the_tree_copy_first() {
        let r = Repo::new();
        let feat = diverge(&r, &[(5, "MAIN5")], &[(5, "FEAT5")]);
        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        let mut files = conflicts(&mut job);
        assert!(files[0].set(0, Some(Side::Both)));
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let want = Repo::lines_with(&[(5, "MAIN5\nFEAT5")]);
        assert_eq!(applied(&r, &job, "f.txt"), want);
    }

    #[test]
    fn an_undecided_hunk_blocks_resolution_and_keeps_the_conflict_pending() {
        let r = Repo::new();
        let feat = diverge(&r, &[(2, "MAIN2")], &[(2, "FEAT2")]);
        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        let files = conflicts(&mut job);
        assert!(job.resolve(&files).is_err());
        // The pending conflict survives the failed attempt.
        let mut files = files;
        files[0].set(0, Some(Side::A));
        job.resolve(&files).unwrap();
    }

    #[test]
    fn modify_delete_is_decided_as_a_whole_file() {
        for (side, survives) in [(Side::A, true), (Side::B, false)] {
            let r = Repo::new();
            r.commit_file("keep.txt", "k\n", "base");
            r.commit_file("gone.txt", "x\n", "gone");
            r.git(&["checkout", "-q", "-b", "feat"]);
            r.git(&["rm", "-q", "gone.txt"]);
            r.git(&["commit", "-q", "-m", "delete gone"]);
            let feat = r.head();
            r.git(&["checkout", "-q", "main"]);
            r.commit_file("gone.txt", "y\n", "modify gone");

            let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
            let mut files = conflicts(&mut job);
            assert!(matches!(files[0].body, Body::Whole { .. }));
            assert!(
                !files[0].set(0, Some(Side::Both)),
                "no 'both' for a whole file"
            );
            assert!(files[0].set(0, Some(side)));
            job.resolve(&files).unwrap();
            assert!(matches!(job.advance().unwrap(), Step::Done));

            let patch = job.patch(&[]).unwrap();
            assert_eq!(patch.is_empty(), survives, "side {side:?}");
            if !survives {
                assert_eq!(patch.paths, vec!["gone.txt".to_string()]);
            }
        }
    }

    #[test]
    fn binary_conflicts_fall_back_to_a_whole_file_decision() {
        let r = Repo::new();
        std::fs::write(r.0.join("b.bin"), [0u8, 1, 2]).unwrap();
        r.git(&["add", "b.bin"]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        std::fs::write(r.0.join("b.bin"), [0u8, 9, 9]).unwrap();
        r.git(&["commit", "-qam", "feat bin"]);
        let feat = r.head();
        r.git(&["checkout", "-q", "main"]);
        std::fs::write(r.0.join("b.bin"), [0u8, 7, 7]).unwrap();
        r.git(&["commit", "-qam", "main bin"]);

        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        let mut files = conflicts(&mut job);
        assert!(matches!(files[0].body, Body::Whole { .. }));
        files[0].set(0, Some(Side::B));
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let patch = job.patch(&[]).unwrap();
        // The patch is a binary patch and applies.
        let file = r.0.join("bin.patch");
        std::fs::write(&file, patch.bytes()).unwrap();
        r.git(&["apply", file.to_str().unwrap()]);
        assert_eq!(std::fs::read(r.0.join("b.bin")).unwrap(), vec![0u8, 9, 9]);
    }

    #[test]
    fn a_rename_conflict_is_reported_as_unsupported() {
        let r = Repo::new();
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.git(&["mv", "f.txt", "h.txt"]);
        r.git(&["commit", "-q", "-m", "rename to h"]);
        let feat = r.head();
        r.git(&["checkout", "-q", "main"]);
        r.git(&["mv", "f.txt", "g.txt"]);
        r.git(&["commit", "-q", "-m", "rename to g"]);

        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        match job.advance().unwrap() {
            Step::Unsupported(list) => assert!(list.iter().any(|m| m.contains("rename"))),
            _ => panic!("a rename/rename conflict must not be offered as an A/B decision"),
        }
    }

    #[test]
    fn later_picks_build_on_the_result_of_earlier_ones() {
        let r = Repo::new();
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        let one = r.commit_file("f.txt", &Repo::lines_with(&[(2, "ONE")]), "one");
        let two = r.commit_file("f.txt", &Repo::lines_with(&[(2, "TWO")]), "two");
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("other.txt", "o\n", "other");

        let mut job = Job::new(&r.0, &r.head(), &[one.clone(), two.clone()], &[]);
        assert!(matches!(job.advance().unwrap(), Step::Done));
        assert_eq!(applied(&r, &job, "f.txt"), Repo::lines_with(&[(2, "TWO")]));

        // The second alone depends on the first and conflicts.
        let mut alone = Job::new(&r.0, &r.head(), &[two], &[]);
        assert!(matches!(alone.advance().unwrap(), Step::Conflicts(_)));
    }

    #[test]
    fn decisions_are_reused_when_the_patch_is_rebuilt_on_a_newer_head() {
        let r = Repo::new();
        let feat = diverge(&r, &[(2, "MAIN2")], &[(2, "FEAT2")]);
        let old_head = r.head();
        let mut job = Job::new(&r.0, &old_head, std::slice::from_ref(&feat), &[]);
        let mut files = conflicts(&mut job);
        files[0].set(0, Some(Side::B));
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let spec = job.patch(&[(feat, "feat edit".to_string())]).unwrap().spec;
        assert_eq!(spec.decisions.len(), 1);

        // HEAD advances with an unrelated commit.
        let new_head = r.commit_file("later.txt", "l\n", "later");
        match rebuild(&r.0, &spec, &new_head).unwrap() {
            Built::Ready(p) => {
                assert_eq!(p.spec.base, new_head);
                assert_eq!(p.paths, vec!["f.txt".to_string()]);
            }
            _ => panic!("the recorded decision should have carried over"),
        }
    }

    #[test]
    fn a_changed_conflict_is_not_decided_by_an_old_decision() {
        let r = Repo::new();
        let feat = diverge(&r, &[(2, "MAIN2")], &[(2, "FEAT2")]);
        let mut job = Job::new(&r.0, &r.head(), std::slice::from_ref(&feat), &[]);
        let mut files = conflicts(&mut job);
        files[0].set(0, Some(Side::B));
        job.resolve(&files).unwrap();
        let spec = job.patch(&[(feat, String::new())]).unwrap().spec;

        // The tree side of the same line changes again: same place, different conflict.
        let new_head = r.commit_file("f.txt", &Repo::lines_with(&[(2, "MAIN2 again")]), "again");
        assert!(matches!(
            rebuild(&r.0, &spec, &new_head).unwrap(),
            Built::Conflicts(_)
        ));
    }

    #[test]
    fn the_header_round_trips_and_does_not_disturb_git_apply() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "base");
        r.git(&["checkout", "-q", "-b", "feat"]);
        let feat = r.commit_file("b.txt", "b\n", "add b: with \"quotes\"");
        r.git(&["checkout", "-q", "main"]);

        let mut job = Job::new(
            &r.0,
            &r.head(),
            std::slice::from_ref(&feat),
            &[("00ff".to_string(), Side::Both)],
        );
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let picks = vec![(feat, "add b: with \"quotes\"".to_string())];
        let patch = job.patch(&picks).unwrap();
        let bytes = patch.bytes();

        assert_eq!(Spec::parse(&bytes), Some(patch.spec.clone()));
        assert!(Spec::parse(b"diff --git a/x b/x\n").is_none());

        let file = write(&patch_dir(&r.0.join(".git")), &patch).unwrap();
        check(&r.0, &file).unwrap();
        apply(&r.0, &file).unwrap();
        assert_eq!(r.read("b.txt"), "b\n");
        // Applied a second time it no longer fits.
        assert!(check(&r.0, &file).is_err());
    }

    #[test]
    fn building_never_touches_the_index_or_work_tree() {
        let r = Repo::new();
        let feat = diverge(&r, &[(2, "MAIN2")], &[(2, "FEAT2")]);
        r.write("scratch.txt", "untracked\n");
        r.write("f.txt", "locally edited\n");
        let before = r.git(&["status", "--porcelain"]);
        let mut job = Job::new(&r.0, &r.head(), &[feat], &[]);
        let mut files = conflicts(&mut job);
        files[0].set(0, Some(Side::A));
        job.resolve(&files).unwrap();
        assert!(matches!(job.advance().unwrap(), Step::Done));
        assert_eq!(r.git(&["status", "--porcelain"]), before);
        assert_eq!(r.read("f.txt"), "locally edited\n");
        // No throwaway index is left behind.
        let leftovers = std::fs::read_dir(r.0.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("gitomic-pick-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    // main: f.txt untouched. feat: one commit changes f.txt, o.txt (edited) and d.txt (deleted).
    fn multi(r: &Repo) -> String {
        r.write("f.txt", &Repo::lines());
        r.write("o.txt", "o\n");
        r.write("d.txt", "d\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.write("f.txt", &Repo::lines_with(&[(2, "F2")]));
        r.write("o.txt", "o2\n");
        r.git(&["rm", "-q", "d.txt"]);
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "three files"]);
        let id = r.head();
        r.git(&["checkout", "-q", "main"]);
        id
    }

    #[test]
    fn a_scoped_commit_carries_one_files_part_of_a_multi_file_commit() {
        let r = Repo::new();
        let pick = multi(&r);
        let parent = r.git(&["rev-parse", &format!("{pick}^")]);

        let only_f = scoped_commit(&r.0, &pick, "f.txt").unwrap();
        assert_eq!(r.git(&["rev-parse", &format!("{only_f}^")]), parent);
        let names = r.git(&["diff", "--name-only", &parent, &only_f]);
        assert_eq!(names, "f.txt");
        assert_eq!(r.git(&["log", "-1", "--format=%s", &only_f]), "three files");

        // A deletion is carried as a deletion.
        let only_d = scoped_commit(&r.0, &pick, "d.txt").unwrap();
        assert_eq!(
            r.git(&["diff", "--name-status", &parent, &only_d]),
            "D\td.txt"
        );
    }

    #[test]
    fn scoped_commits_refuse_root_merge_and_newline_paths() {
        let r = Repo::new();
        let root = r.commit_file("a.txt", "a\n", "root");
        assert!(scoped_commit(&r.0, &root, "a.txt").is_err());
        assert!(scoped_commit(&r.0, &root, "a\nb").is_err());
    }

    #[test]
    fn a_replay_restricted_to_one_file_leaves_the_others_out() {
        let r = Repo::new();
        let pick = multi(&r);
        let mut job = Job::new(&r.0, &r.head(), std::slice::from_ref(&pick), &[])
            .with_scopes(&[(pick.clone(), "o.txt".to_string())], &[]);
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let patch = job.patch(&[]).unwrap();
        assert_eq!(patch.paths, vec!["o.txt".to_string()]);
    }

    #[test]
    fn a_restore_sets_the_file_to_its_content_at_the_source_or_removes_it() {
        let r = Repo::new();
        let pick = multi(&r);
        // Restore f.txt to the branch's copy; d.txt does not exist in the source, so it is removed.
        let mut job = Job::new(&r.0, &r.head(), &[], &[]).with_scopes(
            &[],
            &[
                (pick.clone(), "f.txt".to_string()),
                (pick.clone(), "d.txt".to_string()),
            ],
        );
        assert!(matches!(job.advance().unwrap(), Step::Done));
        let patch = job.patch(&[]).unwrap();
        let mut paths = patch.paths.clone();
        paths.sort();
        assert_eq!(paths, vec!["d.txt".to_string(), "f.txt".to_string()]);
        assert_eq!(applied(&r, &job, "f.txt"), Repo::lines_with(&[(2, "F2")]));
        assert!(!r.exists("d.txt"));
    }

    #[test]
    fn only_and_restore_survive_the_header_round_trip_and_a_rebuild() {
        let r = Repo::new();
        let pick = multi(&r);
        let spec = Spec {
            base: r.head(),
            picks: vec![(pick.clone(), "three files".to_string())],
            only: vec![(pick.clone(), "o.txt".to_string())],
            restore: vec![(pick.clone(), "f.txt".to_string())],
            ..Spec::default()
        };
        let Built::Ready(patch) = build(&r.0, &spec).unwrap() else {
            panic!("expected a clean build");
        };
        let parsed = Spec::parse(&patch.bytes()).unwrap();
        assert_eq!(parsed.only, spec.only);
        assert_eq!(parsed.restore, spec.restore);

        // HEAD moves; the rebuilt patch keeps the scoping.
        r.commit_file("c.txt", "c\n", "add c");
        let Built::Ready(again) = rebuild(&r.0, &parsed, &r.head()).unwrap() else {
            panic!("expected a clean rebuild");
        };
        assert_eq!(again.spec.only, spec.only);
        assert_eq!(again.spec.restore, spec.restore);
        let mut paths = again.paths.clone();
        paths.sort();
        assert_eq!(paths, vec!["f.txt".to_string(), "o.txt".to_string()]);
    }
}
