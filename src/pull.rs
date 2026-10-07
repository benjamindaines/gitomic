// Divergence report and integration (issue #27). `git pull` on a diverged branch refuses with a hint listing
// three configuration settings, each of which commits to a strategy before the operator has seen what is
// incoming. The information needed to choose is available before any of them is chosen, and git does not
// volunteer it: a divergence made of commits that are already upstream under different hashes — the residue of
// a force-push, a re-created branch, a restored backup — is a different situation from genuine parallel work,
// and "N and M different commits" does not distinguish the two. `git rebase` reports the duplicates, one line
// per dropped commit, but only after the rebase is already under way.
//
// This command reads that answer first and prints it, then asks which of three outcomes is wanted. No
// pull.rebase configuration is read or written, so the choice is made per invocation from the report rather
// than once, globally, in advance.

use std::path::Path;

use crate::{git, Res};

// One commit on the local side, and whether an equivalent patch is already present upstream. `git cherry`
// compares patch ids rather than commit ids, so a commit that was rebased, cherry-picked or re-authored
// upstream is recognised despite having a different hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Local {
    pub id: String,
    pub subject: String,
    // True when the same patch is already upstream; such a commit is dropped by a rebase.
    pub duplicate: bool,
}

// What integrating with the upstream branch would involve.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    pub branch: String,
    pub upstream: String,
    // Commits upstream has that the local branch lacks, newest first, as (id, subject).
    pub incoming: Vec<(String, String)>,
    // Commits the local branch has that upstream lacks, oldest first.
    pub local: Vec<Local>,
}

impl Report {
    pub fn duplicates(&self) -> usize {
        self.local.iter().filter(|c| c.duplicate).count()
    }

    // Local commits that are genuinely new work — the ones a rebase would keep.
    pub fn original(&self) -> usize {
        self.local.len() - self.duplicates()
    }

    pub fn diverged(&self) -> bool {
        !self.incoming.is_empty() && !self.local.is_empty()
    }

    // True when every local commit is already upstream. Integration is then a fast-forward in substance,
    // whatever the commit ids say, and taking the remote outright loses no work.
    pub fn all_duplicate(&self) -> bool {
        !self.local.is_empty() && self.original() == 0
    }
}

// The upstream ref a branch tracks, e.g. "origin/main". Errors when the branch has no upstream configured,
// since every outcome below is defined relative to one.
fn upstream_of(root: &Path, branch: &str) -> Res<String> {
    git::run(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", &format!("{branch}@{{upstream}}")],
    )
    .map_err(|_| {
        format!("pull: '{branch}' tracks no upstream branch; set one with 'git branch --set-upstream-to'")
            .into()
    })
}

// Commit ids `git cherry <upstream> <branch>` reports as NOT present upstream, i.e. the ones a rebase would
// keep. The documented '-' marker for an already-upstream commit is not emitted by this git version (2.43):
// such commits are omitted from the output entirely, as is the equivalent `git log --cherry-mark`. The
// duplicates are therefore derived by difference — every commit in `upstream..HEAD` that this set does not
// contain has an equivalent patch upstream — rather than read from a marker that never arrives.
fn parse_cherry(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let sign = parts.next()?;
            let id = parts.next()?.to_string();
            (sign == "+").then_some(id)
        })
        .collect()
}

// Fetch, then describe the divergence. The fetch is unconditional: a report built against a stale
// remote-tracking ref describes a state that no longer exists, which is the failure mode this command was
// written to remove.
pub fn report(root: &Path, fetch: bool) -> Res<Report> {
    let branch = git::current_branch(root)?;
    let upstream = upstream_of(root, &branch)?;
    if fetch {
        let remote = upstream.split('/').next().unwrap_or("origin").to_string();
        git::run(root, &["fetch", "--quiet", &remote])?;
    }

    let incoming = git::run(
        root,
        &[
            "log",
            "--format=%H%x1f%s",
            upstream.as_str(),
            "--not",
            "HEAD",
        ],
    )?
    .lines()
    .filter_map(|l| {
        let (id, subject) = l.split_once('\u{1f}')?;
        Some((id.to_string(), subject.to_string()))
    })
    .collect();

    // Every local commit, oldest first, then marked against the set git reports as genuinely new.
    let new_ids = parse_cherry(&git::run(root, &["cherry", &upstream, &branch])?);
    let local = git::run(
        root,
        &[
            "log",
            "--reverse",
            "--no-merges",
            "--format=%H%x1f%s",
            &format!("{upstream}..HEAD"),
        ],
    )?
    .lines()
    .filter_map(|l| {
        let (id, subject) = l.split_once('\u{1f}')?;
        Some(Local {
            id: id.to_string(),
            subject: subject.to_string(),
            duplicate: !new_ids.iter().any(|n| n == id),
        })
    })
    .collect();

    Ok(Report {
        branch,
        upstream,
        incoming,
        local,
    })
}

// Print the report. The duplicate count leads, because it is the fact that decides the choice and the one git
// withholds until a strategy has already been picked.
pub fn print_report(r: &Report) {
    println!("gitomic: {} vs {}", r.branch, r.upstream);
    println!("  incoming: {} commit(s) you do not have", r.incoming.len());
    println!(
        "  yours:    {} commit(s) upstream does not have — {} already upstream under another hash, {} genuinely new",
        r.local.len(),
        r.duplicates(),
        r.original()
    );
    if !r.local.is_empty() {
        println!();
        for c in &r.local {
            let mark = if c.duplicate { "dup " } else { "new " };
            println!("  {mark}{} {}", &c.id[..c.id.len().min(8)], c.subject);
        }
    }
    println!();
    if r.all_duplicate() {
        println!("  every commit of yours is already upstream: --take-remote loses no work.");
    } else if r.diverged() {
        println!(
            "  --rebase replays your {} new commit(s) onto {}",
            r.original(),
            r.upstream
        );
        println!("  --merge  joins the two lines with a merge commit");
        println!("  --take-remote discards yours (a backup branch is made first)");
    } else if !r.incoming.is_empty() {
        println!("  no divergence: --rebase or --merge fast-forwards.");
    } else {
        println!("  up to date.");
    }
}

// Which outcome was asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    // Print the report and stop.
    Report,
    Rebase,
    Merge,
    TakeRemote,
}

// Backup ref written before a destructive outcome, so "discard" is recoverable by name rather than through the
// reflog.
fn backup_name(branch: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("backup/{branch}-{secs}")
}

pub fn run(cwd: &Path, action: Action, fetch: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;

    if let Some(op) = git::operation_kind(&git_dir) {
        return Err(crate::commands::blocked_by("pull", op).into());
    }
    // A rebase or a reset moves the branch tip, and an open session's base marker must stay an ancestor of it.
    // Refused rather than worked around: finishing or stopping the session is the operator's decision, and
    // silently re-planting the marker would misattribute whatever the integration brought in.
    let branch = git::current_branch(&root)?;
    if git::rev_exists(&root, &git::base_ref(&branch))? && action != Action::Report {
        return Err(format!(
            "pull: a gitomic session is open on '{branch}'; run 'gitomic finish' or 'gitomic stop' first"
        )
        .into());
    }

    let r = report(&root, fetch)?;
    print_report(&r);

    if action == Action::Report {
        return Ok(());
    }
    if r.incoming.is_empty() && action != Action::TakeRemote {
        println!("gitomic: nothing to integrate");
        return Ok(());
    }

    match action {
        Action::Report => unreachable!(),
        Action::Rebase => {
            println!();
            match git::run(&root, &["rebase", &r.upstream]) {
                Ok(out) => {
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    println!("gitomic: rebased onto {}", r.upstream);
                }
                Err(e) => return Err(conflict_hint("rebase", e)),
            }
        }
        Action::Merge => {
            println!();
            match git::run(&root, &["merge", "--no-edit", &r.upstream]) {
                Ok(out) => {
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    println!("gitomic: merged {}", r.upstream);
                }
                Err(e) => return Err(conflict_hint("merge", e)),
            }
        }
        Action::TakeRemote => {
            let backup = backup_name(&r.branch);
            git::run(&root, &["branch", &backup])?;
            git::run(&root, &["reset", "--hard", &r.upstream])?;
            println!();
            println!("gitomic: '{}' is now {}", r.branch, r.upstream);
            println!("  your previous tip is on '{backup}'");
        }
    }
    Ok(())
}

// Turn a failed integration into a diagnosis that names the next command. A rebase or merge that stops does so
// almost always on a conflict, which `gitomic resolve` is exactly for; git's own text is kept, since it names
// the paths.
fn conflict_hint(verb: &str, e: Box<dyn std::error::Error>) -> Box<dyn std::error::Error> {
    format!(
        "pull: the {verb} stopped:\n{e}\n\
         If this is a conflict, run 'gitomic resolve' to decide it, or 'gitomic unstick --yes' to abandon \
         the {verb}."
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;

    // A local branch and a bare "remote" it tracks, diverged: the remote has one commit the local lacks, and
    // the local has two the remote lacks, one of which is the same patch as one already upstream.
    fn diverged() -> Repo {
        let r = Repo::new();
        r.commit_file("f.txt", "base\n", "base");

        let remote = r.0.join("remote.git");
        r.git(&["init", "-q", "--bare", remote.to_str().unwrap()]);
        r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        r.git(&["push", "-q", "-u", "origin", "main"]);

        // A commit that will exist on both sides with different hashes: made locally, pushed, then rewound
        // and re-created so the local copy is a distinct object with the same patch.
        r.commit_file("shared.txt", "shared\n", "shared work");
        r.git(&["push", "-q", "origin", "main"]);
        r.commit_file("theirs.txt", "theirs\n", "upstream only");
        r.git(&["push", "-q", "origin", "main"]);
        // Rewind past both, then re-create only the shared one under a different message. The patch is
        // identical and the commit id is not, which is exactly the shape a force-push or a restored branch
        // leaves behind — and the one `N and M different commits` cannot distinguish from real divergence.
        r.git(&["reset", "-q", "--hard", "HEAD~2"]);
        r.commit_file("shared.txt", "shared\n", "shared work, recreated");
        r.commit_file("mine.txt", "mine\n", "my new work");
        r.git(&["fetch", "-q", "origin"]);
        r
    }

    #[test]
    fn only_the_commits_marked_new_are_taken_from_cherry() {
        let rows = parse_cherry("- abc123 already there\n+ def456 new thing\n");
        assert_eq!(rows, vec!["def456".to_string()]);
    }

    #[test]
    fn a_cherry_line_without_a_subject_still_parses() {
        assert_eq!(parse_cherry("+ abc123\n"), vec!["abc123".to_string()]);
    }

    #[test]
    fn the_report_separates_duplicates_from_new_work() {
        let r = diverged();
        let rep = report(&r.0, false).unwrap();
        assert_eq!(rep.upstream, "origin/main");
        assert!(rep.diverged());
        assert_eq!(rep.local.len(), 2);
        assert_eq!(rep.duplicates(), 1);
        assert_eq!(rep.original(), 1);
        assert_eq!(rep.incoming.len(), 2);
    }

    #[test]
    fn rebase_keeps_the_new_commit_and_drops_the_duplicate() {
        let r = diverged();
        run(&r.0, Action::Rebase, false).unwrap();
        let log = r.git(&["log", "--format=%s", "origin/main..HEAD"]);
        assert_eq!(log.trim(), "my new work");
        assert!(r.exists("theirs.txt"));
    }

    #[test]
    fn take_remote_moves_the_branch_and_leaves_a_backup() {
        let r = diverged();
        let before = r.head();
        run(&r.0, Action::TakeRemote, false).unwrap();
        assert_eq!(r.head(), r.git(&["rev-parse", "origin/main"]));
        let backups = r.git(&["branch", "--list", "backup/*", "--format=%(refname:short)"]);
        let backup = backups.lines().next().unwrap();
        assert_eq!(r.git(&["rev-parse", backup]), before);
    }

    #[test]
    fn merge_joins_both_lines() {
        let r = diverged();
        run(&r.0, Action::Merge, false).unwrap();
        assert_eq!(
            r.git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .split(' ')
                .count(),
            3
        );
        assert!(r.exists("theirs.txt") && r.exists("mine.txt"));
    }

    #[test]
    fn report_only_changes_nothing() {
        let r = diverged();
        let before = r.head();
        run(&r.0, Action::Report, false).unwrap();
        assert_eq!(r.head(), before);
    }

    #[test]
    fn an_all_duplicate_divergence_is_named_as_such() {
        let r = Repo::new();
        r.commit_file("f.txt", "base\n", "base");
        let remote = r.0.join("remote.git");
        r.git(&["init", "-q", "--bare", remote.to_str().unwrap()]);
        r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        r.git(&["push", "-q", "-u", "origin", "main"]);
        r.commit_file("shared.txt", "shared\n", "shared work");
        r.git(&["push", "-q", "origin", "main"]);
        r.git(&["reset", "-q", "--hard", "HEAD~1"]);
        r.commit_file("shared.txt", "shared\n", "shared work, recreated");
        r.git(&["fetch", "-q", "origin"]);

        let rep = report(&r.0, false).unwrap();
        assert!(rep.all_duplicate());
        assert_eq!(rep.original(), 0);
    }

    #[test]
    fn a_branch_without_an_upstream_is_refused() {
        let r = Repo::new();
        r.commit_file("f.txt", "x\n", "base");
        let err = report(&r.0, false).unwrap_err().to_string();
        assert!(err.contains("tracks no upstream"), "{err}");
    }

    #[test]
    fn an_open_session_blocks_an_integration_but_not_a_report() {
        let r = diverged();
        r.git(&["update-ref", &git::base_ref("main"), "HEAD"]);
        let err = run(&r.0, Action::Rebase, false).unwrap_err().to_string();
        assert!(err.contains("session is open"), "{err}");
        run(&r.0, Action::Report, false).unwrap();
    }
}
