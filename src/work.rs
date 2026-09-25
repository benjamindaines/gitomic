// Background workers for the cherry-pick screen (issue #14).
//
// The screen used to run every git command on the thread that reads keys and draws, so a slow
// command froze the interface for as long as it took. Two worker threads now do that work:
//
//   - The diff worker loads the preview of the commit the selection rests on and nothing else, so
//     it is never queued behind background work. Requests are answered latest first: while one
//     preview runs, any older request still queued is dropped. When the selection moves on, the
//     running git command is killed (`Supersede`), so a preview the operator has scrolled past stops
//     costing CPU and disk at once.
//   - The classification worker works out which commits change nothing on HEAD, nearest to the
//     selection first, for as long as the screen has spare time. It does not start a check while a
//     preview is being loaded or is waiting to be, and it runs at a lower scheduling priority, as
//     do the git commands it starts, so the diff worker has the machine to itself.
//
// The interface thread only sends requests and collects answers; it never waits for either worker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use crate::cherry::{Gated, Preview, Reader};
use crate::git::{Cancel, CANCELLED};
use crate::pick::{style_diff, DiffView};

// A loaded preview: the styled text and the large files it left unread.
pub struct Loaded {
    pub view: DiffView,
    pub gated: Vec<Gated>,
}

// What the workers read from git. Implemented by `cherry::Reader`; tests substitute their own.
pub trait Loader: Send + Sync {
    // The preview of a commit; abandoned with `CANCELLED` when `cancel` fires.
    fn diff(
        &self,
        commit: &str,
        only: Option<&str>,
        full: bool,
        cancel: &Cancel,
    ) -> Result<Preview, String>;

    // Whether replaying a commit onto HEAD would change anything; abandoned with `CANCELLED` when
    // `cancel` fires.
    fn applies(&self, commit: &str, cancel: &Cancel) -> Result<bool, String>;
}

impl Loader for Reader {
    fn diff(
        &self,
        commit: &str,
        only: Option<&str>,
        full: bool,
        cancel: &Cancel,
    ) -> Result<Preview, String> {
        self.preview(commit, only, full, Some(cancel))
            .map_err(|e| e.to_string())
    }

    fn applies(&self, commit: &str, cancel: &Cancel) -> Result<bool, String> {
        self.applies_with(commit, Some(cancel))
            .map_err(|e| e.to_string())
    }
}

// An answer from a worker.
pub enum Done {
    // A preview, for the view key it was requested under.
    Diff {
        key: String,
        loaded: Loaded,
    },
    // A request that will produce no preview: it was cancelled while running or replaced by a newer
    // one while queued.
    Dropped {
        key: String,
    },
    // Whether the commit at `index` of the list changes anything. `commit` lets the receiver discard
    // an answer that belongs to a list it no longer shows.
    Applies {
        index: usize,
        commit: String,
        applies: bool,
    },
}

struct DiffReq {
    key: String,
    commit: String,
    only: Option<String>,
    full: bool,
}

// The classification worker's queue.
#[derive(Default)]
struct Wanted {
    list: VecDeque<(usize, String)>,
    quit: bool,
}

// "No commit is being classified".
const NONE: usize = usize::MAX;

// The key and cancel handle of the preview being loaded, if any.
type Running = Arc<Mutex<Option<(String, Arc<Cancel>)>>>;

pub struct Pool {
    requests: Sender<DiffReq>,
    // The key and cancel handle of the preview being loaded, if any.
    running: Running,
    wanted: Arc<(Mutex<Wanted>, Condvar)>,
    // Index of the commit the classification worker is on, or NONE.
    classifying: Arc<AtomicUsize>,
    // Previews requested and not yet answered; the classification worker waits while there are any.
    previews: Arc<AtomicUsize>,
    // Cancel handle of the classification in progress, if any.
    classify_cancel: Arc<Mutex<Option<Arc<Cancel>>>>,
    done: Receiver<Done>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic on another thread must not turn every later use into a panic as well.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// Lower the processor and disk priority of the calling thread. On Linux a child process inherits
// both values from the thread that starts it, so the git commands of the classification worker
// follow. The disk priority takes effect only under an I/O scheduler that honours it (BFQ, CFQ).
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: both calls only change scheduling parameters of the calling thread; a failure is
    // harmless and ignored.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        // IOPRIO_WHO_PROCESS (1), the calling thread (0), class IDLE (3) in the class bits (<< 13).
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}

impl Pool {
    // Start both workers on `loader`.
    pub fn start(loader: Arc<dyn Loader>) -> Pool {
        let (done_tx, done) = mpsc::channel();
        let (requests, request_rx) = mpsc::channel::<DiffReq>();
        let running: Running = Arc::new(Mutex::new(None));
        let wanted = Arc::new((Mutex::new(Wanted::default()), Condvar::new()));
        let classifying = Arc::new(AtomicUsize::new(NONE));
        let previews = Arc::new(AtomicUsize::new(0));
        let classify_cancel: Arc<Mutex<Option<Arc<Cancel>>>> = Arc::new(Mutex::new(None));

        {
            let (loader, running, done_tx) = (loader.clone(), running.clone(), done_tx.clone());
            let previews = previews.clone();
            std::thread::Builder::new()
                .name("gitomic-diff".into())
                .spawn(move || diff_worker(loader, request_rx, running, previews, done_tx))
                .expect("cannot start the diff worker");
        }
        {
            let (wanted, classifying, previews, cancel) = (
                wanted.clone(),
                classifying.clone(),
                previews.clone(),
                classify_cancel.clone(),
            );
            std::thread::Builder::new()
                .name("gitomic-classify".into())
                .spawn(move || {
                    classify_worker(loader, wanted, classifying, previews, cancel, done_tx)
                })
                .expect("cannot start the classification worker");
        }
        Pool {
            requests,
            running,
            wanted,
            classifying,
            previews,
            classify_cancel,
            done,
        }
    }

    // Ask for the preview of `commit`, to be answered under `key`.
    pub fn request_diff(&self, key: String, commit: String, only: Option<String>, full: bool) {
        self.previews.fetch_add(1, Ordering::SeqCst);
        // A classification in progress is abandoned, not waited for: the operator's request comes
        // first, and the commit is asked for again once the preview is done.
        if let Some(cancel) = lock(&self.classify_cancel).as_ref() {
            cancel.cancel();
        }
        let _ = self.requests.send(DiffReq {
            key,
            commit,
            only,
            full,
        });
    }

    // Kill the preview being loaded unless it is the one under `keep`.
    pub fn supersede(&self, keep: Option<&str>) {
        if let Some((key, cancel)) = lock(&self.running).as_ref() {
            if Some(key.as_str()) != keep {
                cancel.cancel();
            }
        }
    }

    // Replace the commits to classify, nearest the selection first. The one being classified at
    // this moment is not affected.
    pub fn want(&self, list: Vec<(usize, String)>) {
        let (queue, wake) = &*self.wanted;
        lock(queue).list = list.into();
        wake.notify_one();
    }

    // The index the classification worker is on, so that it is not asked for again meanwhile.
    pub fn classifying(&self) -> Option<usize> {
        match self.classifying.load(Ordering::SeqCst) {
            NONE => None,
            i => Some(i),
        }
    }

    // Whether the classification worker has anything queued or in hand.
    pub fn classifying_busy(&self) -> bool {
        self.classifying().is_some() || !lock(&self.wanted.0).list.is_empty()
    }

    // The next answer, if one has arrived.
    pub fn try_recv(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }
}

impl Drop for Pool {
    // The workers are told to stop; they are not waited for, since one may be inside a git command
    // and the process is about to end.
    fn drop(&mut self) {
        self.supersede(None);
        let (queue, wake) = &*self.wanted;
        lock(queue).quit = true;
        wake.notify_all();
    }
}

fn diff_worker(
    loader: Arc<dyn Loader>,
    requests: Receiver<DiffReq>,
    running: Running,
    previews: Arc<AtomicUsize>,
    done: Sender<Done>,
) {
    while let Ok(mut req) = requests.recv() {
        // Latest wins: whatever is queued behind this request is newer, and this one is dropped.
        while let Ok(newer) = requests.try_recv() {
            previews.fetch_sub(1, Ordering::SeqCst);
            let _ = done.send(Done::Dropped { key: req.key });
            req = newer;
        }
        let cancel = Arc::new(Cancel::default());
        *lock(&running) = Some((req.key.clone(), cancel.clone()));
        let result = loader.diff(&req.commit, req.only.as_deref(), req.full, &cancel);
        *lock(&running) = None;
        previews.fetch_sub(1, Ordering::SeqCst);
        let answer = match result {
            Ok(p) => Done::Diff {
                key: req.key,
                loaded: Loaded {
                    view: style_diff(&p.text),
                    gated: p.gated,
                },
            },
            Err(e) if e == CANCELLED => Done::Dropped { key: req.key },
            Err(e) => Done::Diff {
                key: req.key,
                loaded: Loaded {
                    view: style_diff(&format!("could not show {}: {e}", req.commit)),
                    gated: Vec::new(),
                },
            },
        };
        if done.send(answer).is_err() {
            return;
        }
    }
}

fn classify_worker(
    loader: Arc<dyn Loader>,
    wanted: Arc<(Mutex<Wanted>, Condvar)>,
    classifying: Arc<AtomicUsize>,
    previews: Arc<AtomicUsize>,
    current: Arc<Mutex<Option<Arc<Cancel>>>>,
    done: Sender<Done>,
) {
    lower_priority();
    let (queue, wake) = &*wanted;
    loop {
        // Stay out of the way of a preview: it is what the operator is waiting for.
        while previews.load(Ordering::SeqCst) > 0 {
            if lock(queue).quit {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (index, commit) = {
            let mut q = lock(queue);
            loop {
                if q.quit {
                    return;
                }
                if let Some(next) = q.list.pop_front() {
                    // Recorded while the queue is still locked, so that a request made in between
                    // does not ask for this commit a second time.
                    classifying.store(next.0, Ordering::SeqCst);
                    break next;
                }
                q = wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let cancel = Arc::new(Cancel::default());
        *lock(&current) = Some(cancel.clone());
        // A preview may have been requested between the wait above and now; it did not see this
        // handle, so it is looked for here.
        if previews.load(Ordering::SeqCst) > 0 {
            cancel.cancel();
        }
        let result = loader.applies(&commit, &cancel);
        *lock(&current) = None;
        classifying.store(NONE, Ordering::SeqCst);
        // An abandoned check has no answer; the commit stays unknown and is asked for again.
        let applies = match result {
            Ok(applies) => applies,
            Err(e) if e == CANCELLED => continue,
            // Any other error counts as "changes something", so the commit stays reachable.
            Err(_) => true,
        };
        if done
            .send(Done::Applies {
                index,
                commit,
                applies,
            })
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    // A loader whose previews wait on a gate and whose classifications are recorded, so that a test
    // controls what the workers are doing at each moment.
    pub struct Slow {
        // Previews block until this is set, or until they are cancelled.
        pub gate: AtomicBool,
        // Classifications block while this is set, until they are cancelled.
        pub hold: AtomicBool,
        pub started: Mutex<Vec<String>>,
        pub classified: Mutex<Vec<String>>,
        pub inert: Vec<String>,
        pub gated: Vec<(String, u64)>,
    }

    impl Slow {
        pub fn new() -> Arc<Slow> {
            Arc::new(Slow {
                gate: AtomicBool::new(false),
                hold: AtomicBool::new(false),
                started: Mutex::new(Vec::new()),
                classified: Mutex::new(Vec::new()),
                inert: Vec::new(),
                gated: Vec::new(),
            })
        }

        pub fn open(&self) {
            self.gate.store(true, Ordering::SeqCst);
        }
    }

    impl Loader for Slow {
        fn diff(
            &self,
            commit: &str,
            _only: Option<&str>,
            _full: bool,
            cancel: &Cancel,
        ) -> Result<Preview, String> {
            lock(&self.started).push(commit.to_string());
            while !self.gate.load(Ordering::SeqCst) {
                if cancel.is_cancelled() {
                    return Err(CANCELLED.to_string());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(Preview {
                text: format!("commit {commit}\n\n diff --git a/x b/x\n+new\n"),
                gated: self
                    .gated
                    .iter()
                    .filter(|(c, _)| c == commit)
                    .map(|(_, bytes)| Gated {
                        path: "big.img".into(),
                        bytes: *bytes,
                    })
                    .collect(),
            })
        }

        fn applies(&self, commit: &str, cancel: &Cancel) -> Result<bool, String> {
            lock(&self.classified).push(commit.to_string());
            while self.hold.load(Ordering::SeqCst) {
                if cancel.is_cancelled() {
                    return Err(CANCELLED.to_string());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(!self.inert.iter().any(|c| c == commit))
        }
    }

    // Collect answers until `enough` says so, or fail after a generous time.
    pub fn collect(pool: &Pool, enough: impl Fn(&[Done]) -> bool) -> Vec<Done> {
        let mut got = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        while !enough(&got) {
            assert!(Instant::now() < end, "timed out waiting for the workers");
            match pool.try_recv() {
                Some(d) => got.push(d),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        got
    }

    fn diffs(got: &[Done]) -> Vec<&str> {
        got.iter()
            .filter_map(|d| match d {
                Done::Diff { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }

    fn dropped(got: &[Done]) -> Vec<&str> {
        got.iter()
            .filter_map(|d| match d {
                Done::Dropped { key } => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_requested_preview_arrives_styled_with_its_key() {
        let slow = Slow::new();
        slow.open();
        let pool = Pool::start(slow);
        pool.request_diff("k1".into(), "c1".into(), None, false);
        let got = collect(&pool, |g| !g.is_empty());
        match &got[0] {
            Done::Diff { key, loaded } => {
                assert_eq!(key, "k1");
                assert!(loaded.view.lines.len() >= 3);
                assert!(loaded.gated.is_empty());
            }
            _ => panic!("expected a preview"),
        }
    }

    #[test]
    fn the_newest_request_wins_and_older_queued_ones_are_dropped() {
        let slow = Slow::new();
        let pool = Pool::start(slow.clone());
        pool.request_diff("a".into(), "ca".into(), None, false);
        // Wait until the worker is inside the first preview, then queue two more behind it.
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&slow.started).is_empty() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        pool.request_diff("b".into(), "cb".into(), None, false);
        pool.request_diff("c".into(), "cc".into(), None, false);
        // Leaving "a" kills it; "b" is replaced by "c". Both are answered before the gate opens, so
        // "a" cannot slip through by finishing normally.
        pool.supersede(Some("c"));
        let mut got = collect(&pool, |g| dropped(g).len() >= 2);
        slow.open();
        got.extend(collect(&pool, |g| diffs(g).contains(&"c")));
        assert_eq!(diffs(&got), vec!["c"]);
        let mut d = dropped(&got);
        d.sort_unstable();
        assert_eq!(
            d,
            vec!["a", "b"],
            "both older requests are answered as dropped"
        );
        assert_eq!(
            *lock(&slow.started),
            vec!["ca", "cc"],
            "b was never started"
        );
    }

    #[test]
    fn superseding_spares_the_preview_that_is_kept() {
        let slow = Slow::new();
        let pool = Pool::start(slow.clone());
        pool.request_diff("a".into(), "ca".into(), None, false);
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&slow.started).is_empty() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        pool.supersede(Some("a"));
        std::thread::sleep(Duration::from_millis(50));
        slow.open();
        let got = collect(&pool, |g| !g.is_empty());
        assert_eq!(diffs(&got), vec!["a"]);
    }

    #[test]
    fn classification_follows_the_order_given_and_a_new_list_replaces_the_old() {
        let mut slow = Slow::new();
        Arc::get_mut(&mut slow).unwrap().inert = vec!["c2".into()];
        let pool = Pool::start(slow.clone());
        pool.want(vec![(2, "c2".into()), (1, "c1".into()), (0, "c0".into())]);
        let got = collect(&pool, |g| g.len() >= 1);
        // Whatever was taken first, the rest can be replaced before it is reached.
        pool.want(vec![(9, "c9".into())]);
        let mut all = got;
        let more = collect(&pool, |g| {
            g.iter()
                .any(|d| matches!(d, Done::Applies { index: 9, .. }))
        });
        all.extend(more);
        let first = match &all[0] {
            Done::Applies {
                index,
                commit,
                applies,
            } => (*index, commit.clone(), *applies),
            _ => panic!("expected a classification"),
        };
        assert_eq!(
            first,
            (2, "c2".to_string(), false),
            "the first commit asked for is first"
        );
        assert!(all.iter().any(|d| matches!(
            d,
            Done::Applies {
                index: 9,
                applies: true,
                ..
            }
        )));
    }

    #[test]
    fn classification_holds_back_while_a_preview_is_outstanding_and_resumes_after() {
        let slow = Slow::new();
        let pool = Pool::start(slow.clone());
        pool.request_diff("a".into(), "ca".into(), None, false);
        pool.want(vec![(0, "c0".into()), (1, "c1".into())]);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            lock(&slow.classified).is_empty(),
            "nothing is classified while the preview waits"
        );
        assert!(pool.try_recv().is_none());
        slow.open();
        let got = collect(&pool, |g| {
            diffs(g).contains(&"a")
                && g.iter()
                    .filter(|d| matches!(d, Done::Applies { .. }))
                    .count()
                    >= 2
        });
        assert_eq!(*lock(&slow.classified), vec!["c0", "c1"]);
        assert!(got.len() >= 3);
    }

    #[test]
    fn a_classification_in_progress_is_abandoned_for_a_preview_and_asked_for_again() {
        let slow = Slow::new();
        slow.hold.store(true, Ordering::SeqCst);
        let pool = Pool::start(slow.clone());
        pool.want(vec![(0, "c0".into())]);
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&slow.classified).is_empty() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
        // The worker is inside the classification. A preview request abandons it.
        slow.open();
        pool.request_diff("a".into(), "ca".into(), None, false);
        let got = collect(&pool, |g| diffs(g).contains(&"a"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !got.iter().any(|d| matches!(d, Done::Applies { .. })) && pool.try_recv().is_none(),
            "the abandoned check gave no answer"
        );
        assert_eq!(pool.classifying(), None);
        // Asked for again, it completes.
        slow.hold.store(false, Ordering::SeqCst);
        pool.want(vec![(0, "c0".into())]);
        collect(&pool, |g| {
            g.iter().any(|d| {
                matches!(
                    d,
                    Done::Applies {
                        index: 0,
                        applies: true,
                        ..
                    }
                )
            })
        });
    }

    #[test]
    fn a_failing_preview_is_shown_as_the_error_not_lost() {
        struct Fails;
        impl Loader for Fails {
            fn diff(
                &self,
                _: &str,
                _: Option<&str>,
                _: bool,
                _: &Cancel,
            ) -> Result<Preview, String> {
                Err("boom".into())
            }
            fn applies(&self, _: &str, _: &Cancel) -> Result<bool, String> {
                Err("boom".into())
            }
        }
        let pool = Pool::start(Arc::new(Fails));
        pool.request_diff("k".into(), "c1".into(), None, false);
        pool.want(vec![(0, "c1".into())]);
        let got = collect(&pool, |g| g.len() >= 2);
        let shown = got.iter().find_map(|d| match d {
            Done::Diff { loaded, .. } => Some(format!("{:?}", loaded.view.lines)),
            _ => None,
        });
        assert!(shown.unwrap().contains("could not show c1: boom"));
        assert!(
            got.iter()
                .any(|d| matches!(d, Done::Applies { applies: true, .. })),
            "an error keeps the commit reachable"
        );
    }
}
