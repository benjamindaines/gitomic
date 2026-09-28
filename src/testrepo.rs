// Throwaway repositories for tests, one per value, created in the system temporary directory and
// removed on drop, so no test depends on another's state or on the developer's own git
// configuration.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::git;

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Repo(pub PathBuf);

impl Repo {
    // A repository on branch `main` with a local identity.
    pub fn new() -> Repo {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gitomic-t-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repo(dir);
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo
    }

    pub fn git(&self, args: &[&str]) -> String {
        git::run(&self.0, args).unwrap()
    }

    pub fn write(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap()
    }

    pub fn exists(&self, name: &str) -> bool {
        self.0.join(name).exists()
    }

    // Write `content` to `name` and record it as one commit with `msg`; returns the commit id.
    pub fn commit_file(&self, name: &str, content: &str, msg: &str) -> String {
        self.write(name, content);
        self.git(&["add", name]);
        self.git(&["commit", "-q", "-m", msg]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    // Ten numbered lines, the shape most conflict tests edit.
    pub fn lines() -> String {
        (1..=10).map(|i| format!("l{i}\n")).collect()
    }

    // Replace the numbered line `n` (1-based) of `lines()` with `text`.
    pub fn lines_with(edits: &[(usize, &str)]) -> String {
        (1..=10)
            .map(|i| match edits.iter().find(|(n, _)| *n == i) {
                Some((_, t)) => format!("{t}\n"),
                None => format!("l{i}\n"),
            })
            .collect()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
