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

use std::path::Path;

use crate::commands::{live_watcher, report_flush, short, terminate_watcher};
use crate::config::Config;
use crate::patch::{self, Built, Merged, Spec};
use crate::{commands, git, Res};

// Cap on the commits offered from one branch, so a branch with a long history does not make the
// listing slow. The picker states when the cap was reached.
pub const MAX_CANDIDATES: usize = 500;

pub struct Opts {
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

// The text the picker shows for one commit: its header and message, then what applying it to `base`
// would change. A conflict is announced ahead of the diff, whose conflicted files show the markers
// that the decision screen later resolves.
pub fn preview(root: &Path, base: &str, pick: &str) -> Res<String> {
    let header = git::run(
        root,
        &[
            "show",
            "-s",
            "--no-color",
            "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
            pick,
        ],
    )?;
    let mut text = header;
    text.push_str("\n\n");
    let tree = match patch::merge(root, base, pick)? {
        Merged::Clean(tree) => tree,
        Merged::Conflicted { tree, files } => {
            text.push_str("CONFLICT: applying this commit needs a decision in\n");
            for f in &files {
                text.push_str(&format!("  {} ({} to decide)\n", f.path, f.units()));
            }
            text.push_str("Enter on a marked selection opens the decision screen for each.\n\n");
            tree
        }
        Merged::Unsupported(list) => {
            text.push_str("CANNOT BE PICKED here: the conflict has no A/B decision.\n");
            for l in list {
                text.push_str(&format!("  {l}\n"));
            }
            return Ok(text);
        }
    };
    let diff = git::run(
        root,
        &[
            "diff",
            "--stat",
            "--patch",
            "--no-color",
            "--no-renames",
            base,
            &tree,
        ],
    )?;
    if diff.is_empty() {
        text.push_str("(this commit changes nothing relative to the current HEAD)\n");
    } else {
        text.push_str(&diff);
        text.push('\n');
    }
    Ok(text)
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
        spec_from_hashes(&root, &opts.hashes)?
    };
    apply_spec(cwd, spec, opts.dry_run, opts.patch_only)
}

// Resolve command-line selectors into a spec against the current HEAD. The order given is the order
// of replay. A commit already contained in HEAD, a merge commit, or a root commit is refused.
fn spec_from_hashes(root: &Path, hashes: &[String]) -> Res<Spec> {
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
    Ok(Spec {
        base,
        picks,
        decisions: Vec::new(),
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
        let spec = spec_from_hashes(&r.0, &[b]).unwrap();
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
        let spec = spec_from_hashes(&r.0, &[f.clone()]).unwrap();

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
        let clean = preview(&r.0, &base, &b).unwrap();
        assert!(clean.contains("add b"), "{clean}");
        assert!(clean.contains("+++ b/b.txt"), "{clean}");
        assert!(!clean.contains("CONFLICT"));

        r.commit_file("f.txt", &Repo::lines_with(&[(3, "MAIN3")]), "main edits f");
        let conflicted = preview(&r.0, &r.head(), &f).unwrap();
        assert!(conflicted.contains("CONFLICT"), "{conflicted}");
        assert!(conflicted.contains("f.txt (1 to decide)"), "{conflicted}");
    }
}
