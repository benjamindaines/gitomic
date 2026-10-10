// Interactive screen for `gitomic restore` when no path is named (issue #24). It shares the layout
// and keys of the cherry-pick screen: files on the left, the highlighted file's change on the
// right, vim-style movement, space to mark, Enter to submit behind a y/n confirmation that Enter
// cannot answer.
//
// The list holds the files in which the selected branches differ from HEAD (Tab chooses them; local
// branches are selected at first), so every row can be marked. A file that every selected branch
// holds with the same content is marked with a single key. A file whose content differs between
// branches asks which branch to take it from. The right pane shows what restoring the file would
// change on HEAD. Only the rows on screen are built, so the list may be as long as a tree.
//
// As in cherry_ui.rs, everything except the terminal is testable: `App` consumes key events and
// reports an `Outcome`, git is reached only through the `Files` trait, and rendering takes any
// ratatui backend. The screen returns one `Spec` per file; applying them is cherry::apply_specs's
// job, so the interactive and the command-line forms share one path.
//
// Filtering is a `restore::Filter` applied to the list. Typing after `/` edits its text part;
// further kinds of search extend the filter and the `Files` trait, not the screen's structure.
//
// Keys, list pane:
//   j/k, arrows  move          g/G  first/last       space  mark and move down (asks which
//                                                            branch when they differ)
//   v            choose the branch to take the highlighted file from
//   H            the history of the highlighted file: every commit that changed it, on the selected
//                branches; Enter takes that version
//   D            also list files that were deleted in the history of the selected branches
//   /            filter by name (all words must occur)     Esc  clear the filter, then quit
//   B            only binary files (toggle)                Tab  choose the branches
//   l, Right     open the diff pane                        Enter  restore the marked files (y/n)
//   P            the patch files kept in .git/gitomic-picks (see below)
//   q            quit (confirmed first when files are marked)
// Keys, diff pane: j/k h/l g/G Ctrl-d/u scroll, Enter/n next file, N previous, space, Tab, Esc
//   back to the list.
// Keys, branch overlay: j/k g/G move, space select, a all/none, v invert, Enter use, Esc cancel.
// Keys, version overlay: j/k g/G move, Enter use that branch, Esc cancel.
// Keys, history overlay: j/k g/G move (the right pane follows), Enter use that version, Esc/H cancel.
// Keys, patch list: j/k g/G move, space mark and move down, Ctrl-d/u scroll the patch, d delete the
//   marked patches (the highlighted one when none is marked; y/n first), Esc/q/P back.
// Keys, filter prompt: type to filter, Backspace, Ctrl-u clear, Enter keep, Esc clear.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::git::Cancel;
use crate::patch::Spec;
use crate::pick::{hslice, max_hscroll, style_diff, with_terminal, DiffView, H_STEP, RUN_WINDOW};
use crate::restore::{
    self, is_modified, BranchTip, Deleted, FileEntry, Filter, HistRow, Index, PatchInfo, Source,
    State, Version,
};
use crate::{git, Res};

// How long the selection must rest on a file before its diff is loaded; the same as in the
// cherry-pick screen, so holding j or k does not start a git command for every row passed.
const DWELL: Duration = Duration::from_millis(150);

// How often the event loop looks for the worker's answer while a diff is outstanding.
const POLL: Duration = Duration::from_millis(25);

// What the screen reads from git.
pub trait Files {
    fn entries(&mut self, sources: &[String]) -> Result<Vec<FileEntry>, String>;
    fn binary(&mut self, sources: &[String]) -> Result<HashSet<String>, String>;
    // What restoring `path` from `version` would change on HEAD.
    fn diff(&mut self, version: &Version, path: &str) -> Result<String, String>;
    // One spec per pick: each file is its own patch.
    fn specs(&mut self, picks: &[(Version, String)]) -> Result<Vec<Spec>, String>;
    // The patch files kept by cherry-pick and restore, newest first.
    fn patches(&mut self) -> Result<Vec<PatchInfo>, String>;
    fn patch_text(&mut self, name: &str) -> Result<String, String>;
    fn trash(&mut self, names: &[String]) -> Result<usize, String>;
    // Entries left out of the list because they cannot be restored (submodules, non-UTF-8 names).
    fn skipped(&self) -> usize {
        0
    }
    // The commit each of the named branches points at.
    fn tips(&mut self, names: &[String]) -> Result<Vec<BranchTip>, String>;
    // What a worker thread can use to read history; without one the history keys say so.
    fn history_loader(&self) -> Option<Arc<dyn HistoryLoader>> {
        None
    }
    // What a worker thread can use to load diffs; without one the screen loads them itself.
    fn loader(&self) -> Option<Arc<dyn DiffLoader>> {
        None
    }
}

// Loads a diff on the worker thread; abandoned with an error when `cancel` fires.
pub trait DiffLoader: Send + Sync {
    fn diff(&self, version: &Version, path: &str, cancel: &Cancel) -> Result<String, String>;
}

// Reads history on a worker thread; each call is abandoned with an error when `cancel` fires.
pub trait HistoryLoader: Send + Sync {
    // The commits that changed `path`, and whether older ones were left out.
    fn versions(
        &self,
        tips: &[BranchTip],
        path: &str,
        cancel: &Cancel,
    ) -> Result<(Vec<HistRow>, bool), String>;
    // The files deleted in the history of `tips` that neither HEAD nor those tips hold.
    fn deleted(&self, tips: &[BranchTip], cancel: &Cancel) -> Result<Deleted, String>;
}

struct GitHistory {
    root: PathBuf,
    head: String,
}

impl HistoryLoader for GitHistory {
    fn versions(
        &self,
        tips: &[BranchTip],
        path: &str,
        cancel: &Cancel,
    ) -> Result<(Vec<HistRow>, bool), String> {
        restore::path_history(&self.root, tips, path, Some(cancel)).map_err(|e| e.to_string())
    }
    fn deleted(&self, tips: &[BranchTip], cancel: &Cancel) -> Result<Deleted, String> {
        restore::deleted_files(&self.root, &self.head, tips, Some(cancel))
            .map_err(|e| e.to_string())
    }
}

struct GitLoader {
    root: PathBuf,
    // Blobs above this many bytes are not diffed; 0 for no limit.
    size_limit: u64,
}

impl DiffLoader for GitLoader {
    fn diff(&self, version: &Version, path: &str, cancel: &Cancel) -> Result<String, String> {
        restore::preview_text(&self.root, version, path, self.size_limit, Some(cancel))
            .map_err(|e| e.to_string())
    }
}

struct GitFiles {
    root: PathBuf,
    git_dir: PathBuf,
    index: Index,
    size_limit: u64,
}

impl Files for GitFiles {
    fn entries(&mut self, sources: &[String]) -> Result<Vec<FileEntry>, String> {
        self.index.entries(sources).map_err(|e| e.to_string())
    }
    fn skipped(&self) -> usize {
        self.index.skipped
    }
    fn tips(&mut self, names: &[String]) -> Result<Vec<BranchTip>, String> {
        self.index.tips(names).map_err(|e| e.to_string())
    }
    fn history_loader(&self) -> Option<Arc<dyn HistoryLoader>> {
        Some(Arc::new(GitHistory {
            root: self.root.clone(),
            head: self.index.head_commit().to_string(),
        }))
    }
    fn binary(&mut self, sources: &[String]) -> Result<HashSet<String>, String> {
        self.index.binary_paths(sources).map_err(|e| e.to_string())
    }
    fn diff(&mut self, version: &Version, path: &str) -> Result<String, String> {
        restore::preview_text(&self.root, version, path, self.size_limit, None)
            .map_err(|e| e.to_string())
    }
    fn specs(&mut self, picks: &[(Version, String)]) -> Result<Vec<Spec>, String> {
        picks
            .iter()
            .map(|(v, p)| restore::spec_for(&self.root, v, p).map_err(|e| e.to_string()))
            .collect()
    }
    fn patches(&mut self) -> Result<Vec<PatchInfo>, String> {
        restore::list_patches(&self.git_dir).map_err(|e| e.to_string())
    }
    fn patch_text(&mut self, name: &str) -> Result<String, String> {
        restore::read_patch(&self.git_dir, name).map_err(|e| e.to_string())
    }
    fn trash(&mut self, names: &[String]) -> Result<usize, String> {
        restore::trash_patches(&self.git_dir, names).map_err(|e| e.to_string())
    }
    fn loader(&self) -> Option<Arc<dyn DiffLoader>> {
        Some(Arc::new(GitLoader {
            root: self.root.clone(),
            size_limit: self.size_limit,
        }))
    }
}

type DiffKey = (String, String);

// The request being loaded and the handle that abandons it.
type Running = Arc<Mutex<Option<(DiffKey, Arc<Cancel>)>>>;

struct DiffReq {
    key: DiffKey,
    version: Version,
    path: String,
}

enum Answer {
    Diff {
        key: DiffKey,
        text: Result<String, String>,
    },
    // The request will produce no diff: it was cancelled while running or replaced while queued.
    Dropped {
        key: DiffKey,
    },
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic on another thread must not turn every later use into a panic as well.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// One thread that loads diffs. Requests are answered latest first: while one runs, older queued
// requests are dropped, and when the selection moves on the running git command is killed, so a
// diff scrolled past stops costing CPU and disk at once. The screen only sends and collects.
struct Worker {
    requests: Sender<DiffReq>,
    running: Running,
    done: Receiver<Answer>,
}

impl Worker {
    fn start(loader: Arc<dyn DiffLoader>) -> Worker {
        let (requests, queue) = mpsc::channel::<DiffReq>();
        let (answers, done) = mpsc::channel::<Answer>();
        let running: Running = Arc::new(Mutex::new(None));
        let shared = Arc::clone(&running);
        thread::spawn(move || {
            while let Ok(mut req) = queue.recv() {
                while let Ok(newer) = queue.try_recv() {
                    let _ = answers.send(Answer::Dropped { key: req.key });
                    req = newer;
                }
                let cancel = Arc::new(Cancel::default());
                *lock(&shared) = Some((req.key.clone(), Arc::clone(&cancel)));
                let text = loader.diff(&req.version, &req.path, &cancel);
                *lock(&shared) = None;
                let answer = if cancel.is_cancelled() {
                    Answer::Dropped { key: req.key }
                } else {
                    Answer::Diff { key: req.key, text }
                };
                if answers.send(answer).is_err() {
                    break;
                }
            }
        });
        Worker {
            requests,
            running,
            done,
        }
    }

    fn request(&self, req: DiffReq) {
        if let Some((key, cancel)) = lock(&self.running).as_ref() {
            if *key != req.key {
                cancel.cancel();
            }
        }
        let _ = self.requests.send(req);
    }
}

type Versions = Result<(Vec<HistRow>, bool), String>;

enum HistAnswer {
    Versions {
        gen: u64,
        path: String,
        result: Versions,
    },
    Deleted {
        epoch: u64,
        result: Result<Deleted, String>,
    },
}

// The cancel handle of the request a history thread is running, if any.
type Slot = Arc<Mutex<Option<Arc<Cancel>>>>;

// Two threads that read history: one for the versions of a file, which are asked for one after
// another as the operator moves about, and one for the scan for deleted files, which can take long
// on a deep history and must not hold the first up. Both are cancellable, so leaving a mode or
// changing the branches stops the git command instead of waiting for it.
struct HistWorker {
    versions: Sender<(u64, String, Vec<BranchTip>)>,
    deleted: Sender<(u64, Vec<BranchTip>)>,
    running: [Slot; 2],
    done: Receiver<HistAnswer>,
}

impl HistWorker {
    fn start(loader: Arc<dyn HistoryLoader>) -> HistWorker {
        let (versions, vreq) = mpsc::channel::<(u64, String, Vec<BranchTip>)>();
        let (deleted, dreq) = mpsc::channel::<(u64, Vec<BranchTip>)>();
        let (answers, done) = mpsc::channel::<HistAnswer>();
        let running: [Slot; 2] = [Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None))];
        {
            let (loader, answers, slot) = (loader.clone(), answers.clone(), running[0].clone());
            thread::spawn(move || {
                while let Ok((gen, path, tips)) = vreq.recv() {
                    let cancel = Arc::new(Cancel::default());
                    *lock(&slot) = Some(cancel.clone());
                    let result = loader.versions(&tips, &path, &cancel);
                    *lock(&slot) = None;
                    if answers
                        .send(HistAnswer::Versions { gen, path, result })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        {
            let slot = running[1].clone();
            thread::spawn(move || {
                while let Ok((epoch, tips)) = dreq.recv() {
                    let cancel = Arc::new(Cancel::default());
                    *lock(&slot) = Some(cancel.clone());
                    let result = loader.deleted(&tips, &cancel);
                    *lock(&slot) = None;
                    if answers.send(HistAnswer::Deleted { epoch, result }).is_err() {
                        break;
                    }
                }
            });
        }
        HistWorker {
            versions,
            deleted,
            running,
            done,
        }
    }

    fn cancel(&self, which: usize) {
        if let Some(c) = lock(&self.running[which]).as_ref() {
            c.cancel();
        }
    }
}

impl Drop for HistWorker {
    fn drop(&mut self) {
        self.cancel(0);
        self.cancel(1);
    }
}

// What is known of one file's history.
enum HistState {
    Loading,
    Ready(Vec<HistRow>, bool),
    Failed(String),
}

// The calendar date of `secs` since 1970-01-01 (proleptic Gregorian, after Howard Hinnant).
pub(crate) fn civil(secs: i64) -> String {
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    List,
    Diff,
}

#[derive(Clone, PartialEq, Debug)]
enum Mode {
    Browse,
    Filter,
    Branches,
    Versions,
    // The commits that changed the highlighted file.
    History,
    ConfirmApply,
    // Asked when the source overlay is left with the modified files newly chosen; the overlay
    // stays open behind it.
    ConfirmModified,
    ConfirmQuit,
    // The list of patch files.
    Patches,
    // Asked before patch files are deleted; the names are those to delete.
    ConfirmTrash(Vec<String>),
}

#[derive(PartialEq, Debug)]
pub enum Outcome {
    Continue,
    Quit,
    // One spec per file to restore.
    Submit(Vec<Spec>),
}

// The patch list: what is kept, which are marked, and the text of the highlighted one.
struct PatchView {
    items: Vec<PatchInfo>,
    marked: HashSet<String>,
    cursor: usize,
    text: Option<(String, DiffView)>,
    scroll: usize,
}

pub struct App {
    sources: Vec<(Source, bool)>,
    // Edited copy of the selection while the branch overlay is open.
    pending: Vec<bool>,
    all: Vec<FileEntry>,
    filter: Filter,
    input: String,
    binary: Option<HashSet<String>>,
    // Indices into `all` of the files that pass the filter.
    view: Vec<usize>,
    cursor: usize,
    focus: Focus,
    mode: Mode,
    overlay_cursor: usize,
    // Columns the open overlay is scrolled to the left, for rows wider than the box.
    ov_hscroll: usize,
    // path -> the version chosen for it.
    marks: BTreeMap<String, Version>,
    // (commit, path) -> the styled diff, or the reason there is none.
    shown: HashMap<DiffKey, DiffView>,
    // Diffs requested from the worker and not yet answered.
    inflight: HashSet<DiffKey>,
    worker: Option<Worker>,
    // Set when space opened the version overlay: choosing then moves down like a plain mark does.
    advance: bool,
    patches: Option<PatchView>,
    hist: Option<HistWorker>,
    // path -> its history, asked for when H is first pressed on it.
    history: HashMap<String, HistState>,
    // The branches' commits as of the last load; history is read from these, not from the names.
    tips: Vec<BranchTip>,
    // Counts loads; an answer from an earlier load is discarded.
    gen: u64,
    // Counts requests for the deleted-file scan; only the latest is honoured.
    del_epoch: u64,
    show_deleted: bool,
    // Paths in `all` that come from the deleted-file scan.
    gone: HashSet<String>,
    // The last scan's result, kept so that turning the mode off and on again is free.
    scanned: Option<Vec<FileEntry>>,
    // History requests not yet answered.
    hist_pending: usize,
    vscroll: usize,
    hscroll: usize,
    view_h: usize,
    view_w: usize,
    last_h: Option<Instant>,
    moved: Option<Instant>,
    skipped: usize,
    // First row of the list that is drawn; kept so the window moves only when the cursor leaves it.
    top: usize,
    status: String,
    status_is_error: bool,
    list_state: ListState,
}

fn plain(text: &str) -> DiffView {
    style_diff(text)
}

pub(crate) fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn human(bytes: u64) -> String {
    crate::cherry::human(bytes)
}

// How long ago `then` was, in the largest whole unit.
fn age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map_or(0, |d| d.as_secs());
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

impl App {
    // `sources` with the flag saying whether each starts selected.
    pub fn new(sources: Vec<(Source, bool)>) -> App {
        App {
            pending: Vec::new(),
            sources,
            all: Vec::new(),
            filter: Filter::default(),
            input: String::new(),
            binary: None,
            view: Vec::new(),
            cursor: 0,
            focus: Focus::List,
            mode: Mode::Browse,
            overlay_cursor: 0,
            ov_hscroll: 0,
            marks: BTreeMap::new(),
            shown: HashMap::new(),
            inflight: HashSet::new(),
            worker: None,
            advance: false,
            patches: None,
            hist: None,
            history: HashMap::new(),
            tips: Vec::new(),
            gen: 0,
            del_epoch: 0,
            show_deleted: false,
            gone: HashSet::new(),
            scanned: None,
            hist_pending: 0,
            vscroll: 0,
            hscroll: 0,
            view_h: 20,
            view_w: 80,
            last_h: None,
            moved: None,
            skipped: 0,
            top: 0,
            status: String::new(),
            status_is_error: false,
            list_state: ListState::default(),
        }
    }

    fn selected(&self) -> Vec<String> {
        self.sources
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.name.clone())
            .collect()
    }

    // Read the files of the selected branches and rebuild the list. Marks whose branch or file is
    // gone are dropped; the others keep their version, which is pinned to a commit.
    pub fn load(&mut self, src: &mut dyn Files) {
        let names = self.selected();
        match src.entries(&names) {
            Ok(all) => {
                // The view indexes the old list, so the highlighted path is taken before it is
                // replaced.
                let keep = self.current().map(|e| e.path.clone());
                self.view.clear();
                self.all = all;
                let before = self.marks.len();
                // A mark is pinned to a commit that may lie in history, so it stays for as long as
                // its branch is selected.
                self.marks
                    .retain(|_, v| names.iter().any(|n| n.as_str() == &*v.source));
                self.skipped = src.skipped();
                if self.marks.len() < before {
                    let n = before - self.marks.len();
                    self.say(format!("{n} mark(s) dropped with their branch"), false);
                }
                self.shown.clear();
                self.binary = None;
                // History belongs to the branches it was read from.
                self.gen += 1;
                if let Some(h) = &self.hist {
                    h.cancel(0);
                    h.cancel(1);
                }
                self.history.clear();
                self.gone.clear();
                self.scanned = None;
                self.tips = src.tips(&names).unwrap_or_default();
                self.rebuild(src, keep);
                if self.show_deleted {
                    self.request_deleted();
                }
            }
            Err(e) => self.say(e, true),
        }
    }

    fn rebuild(&mut self, src: &mut dyn Files, keep: Option<String>) {
        let keep = keep.or_else(|| self.current().map(|e| e.path.clone()));
        if self.filter.binary_only && self.binary.is_none() {
            match src.binary(&self.selected()) {
                Ok(b) => self.binary = Some(b),
                Err(e) => {
                    self.filter.binary_only = false;
                    self.say(e, true);
                }
            }
        }
        self.refilter(keep);
    }

    // Apply the filter to the list as it stands, using the binary set already read.
    fn refilter(&mut self, keep: Option<String>) {
        let empty = HashSet::new();
        self.view = self
            .filter
            .select(&self.all, self.binary.as_ref().unwrap_or(&empty), None);
        self.cursor = keep
            .and_then(|p| self.view.iter().position(|&i| self.all[i].path == p))
            .unwrap_or(0)
            .min(self.view.len().saturating_sub(1));
        self.moved = Some(Instant::now());
        self.vscroll = 0;
        self.hscroll = 0;
    }

    fn current(&self) -> Option<&FileEntry> {
        self.view.get(self.cursor).and_then(|&i| self.all.get(i))
    }

    // The version whose change the right pane shows: the overlay's choice while it is open, else
    // the one marked for the file, else the first branch's.
    fn candidate(&self) -> Option<(Version, String)> {
        let e = self.current()?;
        let v = if self.mode == Mode::History {
            match self.history.get(&e.path)? {
                HistState::Ready(rows, _) => &rows.get(self.overlay_cursor)?.version,
                _ => return None,
            }
        } else if self.mode == Mode::Versions {
            e.versions.get(self.overlay_cursor)?
        } else if let Some(m) = self.marks.get(&e.path) {
            m
        } else {
            e.versions.first()?
        };
        Some((v.clone(), e.path.clone()))
    }

    fn shown_key(v: &Version, path: &str) -> DiffKey {
        (v.commit.to_string(), path.to_string())
    }

    fn current_view(&self) -> Option<&DiffView> {
        let (v, path) = self.candidate()?;
        self.shown.get(&Self::shown_key(&v, &path))
    }

    // Let a worker thread load the diffs. Called once, before the first frame.
    pub fn use_worker(&mut self, loader: Arc<dyn DiffLoader>) {
        self.worker = Some(Worker::start(loader));
    }

    // Let a worker thread read history. Called once, before the first frame.
    pub fn use_history(&mut self, loader: Arc<dyn HistoryLoader>) {
        self.hist = Some(HistWorker::start(loader));
    }

    fn request_versions(&mut self, path: &str) {
        let Some(h) = &self.hist else {
            self.history.insert(
                path.to_string(),
                HistState::Failed("history is not available".to_string()),
            );
            return;
        };
        self.history.insert(path.to_string(), HistState::Loading);
        let _ = h
            .versions
            .send((self.gen, path.to_string(), self.tips.clone()));
        self.hist_pending += 1;
    }

    fn request_deleted(&mut self) {
        let Some(h) = &self.hist else {
            self.show_deleted = false;
            self.say("history is not available", true);
            return;
        };
        self.del_epoch += 1;
        let _ = h.deleted.send((self.del_epoch, self.tips.clone()));
        self.hist_pending += 1;
        self.say("scanning the history for deleted files...", false);
    }

    fn open_history(&mut self) {
        let Some(path) = self.current().map(|e| e.path.clone()) else {
            return;
        };
        if !self.history.contains_key(&path) {
            self.request_versions(&path);
        }
        self.overlay_cursor = 0;
        self.ov_hscroll = 0;
        self.mode = Mode::History;
        self.moved = Some(Instant::now());
    }

    // h/l and the arrow keys scroll an open overlay sideways; the render clamps the offset to the
    // widest row. Returns whether `key` was one of them.
    fn overlay_hscroll(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                self.ov_hscroll = self.ov_hscroll.saturating_sub(H_STEP)
            }
            KeyCode::Char('l') | KeyCode::Right => self.ov_hscroll += H_STEP,
            _ => return false,
        }
        true
    }

    // Rows moved by PageUp/PageDown in an overlay, about the height of the box.
    fn overlay_page(&self) -> usize {
        (self.view_h * 3 / 5).max(1)
    }

    fn key_history(&mut self, key: KeyEvent, now: Instant) {
        if self.overlay_hscroll(&key) {
            return;
        }
        let page = self.overlay_page();
        let Some(path) = self.current().map(|e| e.path.clone()) else {
            self.mode = Mode::Browse;
            return;
        };
        let rows = match self.history.get(&path) {
            Some(HistState::Ready(rows, _)) => rows.len(),
            _ => 0,
        };
        let last = rows.saturating_sub(1);
        match key.code {
            KeyCode::PageDown => {
                self.overlay_cursor = (self.overlay_cursor + page).min(last);
                self.moved = Some(now);
            }
            KeyCode::PageUp => {
                self.overlay_cursor = self.overlay_cursor.saturating_sub(page);
                self.moved = Some(now);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay_cursor = (self.overlay_cursor + 1).min(last);
                self.moved = Some(now);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay_cursor = self.overlay_cursor.saturating_sub(1);
                self.moved = Some(now);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.overlay_cursor = 0;
                self.moved = Some(now);
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.overlay_cursor = last;
                self.moved = Some(now);
            }
            KeyCode::Enter => {
                let chosen = match self.history.get(&path) {
                    Some(HistState::Ready(rows, _)) => rows.get(self.overlay_cursor).cloned(),
                    _ => None,
                };
                if let Some(row) = chosen {
                    self.mode = Mode::Browse;
                    self.say(
                        format!(
                            "{path} will come from {} ({})",
                            short(&row.version.commit),
                            row.version.source
                        ),
                        false,
                    );
                    self.marks.insert(path, row.version);
                }
            }
            KeyCode::Esc | KeyCode::Char('H') | KeyCode::Char('q') => self.mode = Mode::Browse,
            _ => {}
        }
    }

    fn toggle_deleted(&mut self) {
        self.show_deleted = !self.show_deleted;
        if self.show_deleted {
            match self.scanned.clone() {
                Some(found) => self.merge_deleted(found),
                None => self.request_deleted(),
            }
        } else {
            if let Some(h) = &self.hist {
                h.cancel(1);
            }
            // An answer still on its way belongs to a request that is no longer wanted.
            self.del_epoch += 1;
            self.merge_deleted(Vec::new());
            self.say("deleted files hidden", false);
        }
    }

    // Replace the deleted files in the list with `found`, leaving out any path the list already has.
    fn merge_deleted(&mut self, found: Vec<FileEntry>) {
        let keep = self.current().map(|e| e.path.clone());
        let gone = std::mem::take(&mut self.gone);
        self.all.retain(|e| !gone.contains(&e.path));
        let have: HashSet<&str> = self.all.iter().map(|e| e.path.as_str()).collect();
        let fresh: Vec<FileEntry> = found
            .into_iter()
            .filter(|e| !have.contains(e.path.as_str()))
            .collect();
        self.gone = fresh.iter().map(|e| e.path.clone()).collect();
        self.all.extend(fresh);
        self.all.sort_by(|a, b| a.path.cmp(&b.path));
        self.refilter(keep);
    }

    fn describe(text: Result<String, String>, path: &str) -> String {
        match text {
            Ok(t) if t.trim().is_empty() => format!("{path} is identical to HEAD."),
            Ok(t) => t,
            Err(e) => format!("could not read the change: {e}"),
        }
    }

    // Collect the diffs the worker has finished.
    pub fn pump(&mut self) {
        self.pump_history();
        let Some(w) = &self.worker else {
            return;
        };
        while let Ok(answer) = w.done.try_recv() {
            match answer {
                Answer::Diff { key, text } => {
                    self.inflight.remove(&key);
                    let text = Self::describe(text, &key.1);
                    self.shown.insert(key, plain(&text));
                }
                Answer::Dropped { key } => {
                    self.inflight.remove(&key);
                }
            }
        }
    }

    fn pump_history(&mut self) {
        let mut answers = Vec::new();
        if let Some(h) = &self.hist {
            while let Ok(a) = h.done.try_recv() {
                answers.push(a);
            }
        }
        for answer in answers {
            self.hist_pending = self.hist_pending.saturating_sub(1);
            match answer {
                HistAnswer::Versions { gen, path, result } if gen == self.gen => {
                    let state = match result {
                        Ok((rows, cut)) => HistState::Ready(rows, cut),
                        Err(e) => HistState::Failed(e),
                    };
                    self.history.insert(path, state);
                }
                HistAnswer::Deleted { epoch, result } if epoch == self.del_epoch => match result {
                    Ok(found) => {
                        let n = found.entries.len();
                        let note = if found.truncated {
                            " (the scan stopped at its size limit; older deletions may be missing)"
                        } else {
                            ""
                        };
                        self.scanned = Some(found.entries.clone());
                        if self.show_deleted {
                            self.merge_deleted(found.entries);
                            self.say(format!("{n} deleted file(s) listed{note}"), false);
                        }
                    }
                    Err(e) => {
                        self.show_deleted = false;
                        self.say(format!("could not scan for deleted files: {e}"), true);
                    }
                },
                _ => {}
            }
        }
    }

    // Load the diff of the candidate once the selection has rested for `DWELL`: through the worker
    // when there is one, else on the spot.
    pub fn ensure_diff(&mut self, src: &mut dyn Files, now: Instant) {
        let Some((v, path)) = self.candidate() else {
            return;
        };
        let key = Self::shown_key(&v, &path);
        if self.shown.contains_key(&key) || self.inflight.contains(&key) {
            return;
        }
        if self
            .moved
            .is_some_and(|t| now.saturating_duration_since(t) < DWELL)
        {
            return;
        }
        if let Some(w) = &self.worker {
            w.request(DiffReq {
                key: key.clone(),
                version: v,
                path,
            });
            self.inflight.insert(key);
        } else {
            let text = Self::describe(src.diff(&v, &path), &path);
            self.shown.insert(key, plain(&text));
        }
    }

    // How long the event loop may sleep: a short poll while the worker has a diff outstanding,
    // else until the dwell ends when a diff is waiting for it.
    pub fn wakeup(&self, now: Instant) -> Option<Duration> {
        if !self.inflight.is_empty() || self.hist_pending > 0 {
            return Some(POLL);
        }
        let (v, path) = self.candidate()?;
        if self.shown.contains_key(&Self::shown_key(&v, &path)) {
            return None;
        }
        let rest = self.moved.map_or(Duration::ZERO, |t| {
            DWELL.saturating_sub(now.saturating_duration_since(t))
        });
        Some(rest)
    }

    fn max_v(&self) -> usize {
        self.current_view()
            .map_or(0, |v| v.lines.len().saturating_sub(self.view_h))
    }

    fn max_h(&self) -> usize {
        self.current_view()
            .map_or(0, |v| v.max_width.saturating_sub(self.view_w))
    }

    fn clamp_scroll(&mut self) {
        self.vscroll = self.vscroll.min(self.max_v());
        self.hscroll = self.hscroll.min(self.max_h());
    }

    fn say(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = text.into();
        self.status_is_error = is_error;
    }

    fn step(&mut self, delta: isize, now: Instant) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() - 1;
        let to = (self.cursor as isize + delta).clamp(0, last as isize) as usize;
        self.land(to, now);
    }

    fn land(&mut self, to: usize, now: Instant) {
        let to = to.min(self.view.len().saturating_sub(1));
        if to != self.cursor {
            self.cursor = to;
            self.vscroll = 0;
            self.hscroll = 0;
            self.moved = Some(now);
        }
    }

    fn toggle(&mut self) {
        let Some(e) = self.current().cloned() else {
            return;
        };
        if self.marks.remove(&e.path).is_some() {
            return;
        }
        if e.distinct() > 1 {
            self.open_versions();
        } else {
            self.marks.insert(e.path, e.versions[0].clone());
        }
    }

    // Space: mark or unmark the highlighted file and move to the next one, so a run of presses
    // marks a run of files. When the file needs a branch chosen first, moving on waits for it.
    fn mark_and_advance(&mut self, now: Instant) {
        self.toggle();
        if self.mode == Mode::Versions {
            self.advance = true;
        } else {
            self.step(1, now);
        }
    }

    fn open_versions(&mut self) {
        let Some(e) = self.current() else {
            return;
        };
        let start = self
            .marks
            .get(&e.path)
            .and_then(|m| e.versions.iter().position(|v| v.commit == m.commit))
            .unwrap_or(0);
        self.overlay_cursor = start;
        self.ov_hscroll = 0;
        self.mode = Mode::Versions;
        self.moved = Some(Instant::now());
    }

    fn open_branches(&mut self) {
        if self.sources.is_empty() {
            self.say("there is no other branch to restore from", true);
            return;
        }
        self.pending = self.sources.iter().map(|(_, on)| *on).collect();
        self.overlay_cursor = 0;
        self.ov_hscroll = 0;
        self.mode = Mode::Branches;
    }

    fn start_filter(&mut self) {
        self.input = self.filter.text.clone();
        self.mode = Mode::Filter;
    }

    fn toggle_binary(&mut self, src: &mut dyn Files) {
        self.filter.binary_only = !self.filter.binary_only;
        self.rebuild(src, None);
        if self.filter.binary_only {
            self.say("binary files only", false);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant, src: &mut dyn Files) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Quit;
        }
        match self.mode.clone() {
            Mode::Filter => {
                self.key_filter(key, src);
                Outcome::Continue
            }
            Mode::Branches => {
                self.key_branches(key, src);
                Outcome::Continue
            }
            Mode::Versions => {
                self.key_versions(key, now);
                Outcome::Continue
            }
            Mode::History => {
                self.key_history(key, now);
                Outcome::Continue
            }
            Mode::Patches => {
                self.key_patches(key, now, src);
                Outcome::Continue
            }
            Mode::ConfirmTrash(names) => {
                self.key_confirm_trash(key, names, src);
                Outcome::Continue
            }
            Mode::ConfirmModified => {
                self.key_confirm_modified(key, src);
                Outcome::Continue
            }
            Mode::ConfirmApply => self.key_confirm_apply(key, src),
            Mode::ConfirmQuit => self.key_confirm_quit(key),
            Mode::Browse => {
                self.status.clear();
                match self.focus {
                    Focus::List => self.key_list(key, now, src),
                    Focus::Diff => self.key_diff(key, now, src),
                }
            }
        }
    }

    fn key_list(&mut self, key: KeyEvent, now: Instant, src: &mut dyn Files) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1, now),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1, now),
            KeyCode::Char('g') | KeyCode::Home => self.land(0, now),
            KeyCode::Char('G') | KeyCode::End => self.land(usize::MAX, now),
            KeyCode::PageDown => self.step(self.view_h.max(1) as isize, now),
            KeyCode::PageUp => self.step(-(self.view_h.max(1) as isize), now),
            KeyCode::Char(' ') => self.mark_and_advance(now),
            KeyCode::Char('v') if self.current().is_some() => self.open_versions(),
            KeyCode::Char('H') if self.current().is_some() => self.open_history(),
            KeyCode::Char('D') => self.toggle_deleted(),
            KeyCode::Char('/') => self.start_filter(),
            KeyCode::Char('B') => self.toggle_binary(src),
            KeyCode::Char('P') => self.open_patches(src),
            KeyCode::Tab => self.open_branches(),
            KeyCode::Char('l') | KeyCode::Right if !self.view.is_empty() => {
                self.focus = Focus::Diff
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter = Filter::default();
                self.rebuild(src, None);
            }
            KeyCode::Char('q') | KeyCode::Esc => return self.request_quit(),
            _ => {}
        }
        Outcome::Continue
    }

    fn key_diff(&mut self, key: KeyEvent, now: Instant, _src: &mut dyn Files) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.vscroll = (self.vscroll + 1).min(self.max_v())
            }
            KeyCode::Char('k') | KeyCode::Up => self.vscroll = self.vscroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => {
                self.vscroll = (self.vscroll + self.view_h / 2).min(self.max_v())
            }
            KeyCode::Char('u') if ctrl => {
                self.vscroll = self.vscroll.saturating_sub(self.view_h / 2)
            }
            KeyCode::Char('g') | KeyCode::Home => self.vscroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.vscroll = self.max_v(),
            KeyCode::PageDown => {
                self.vscroll = (self.vscroll + self.view_h.max(1)).min(self.max_v())
            }
            KeyCode::PageUp => self.vscroll = self.vscroll.saturating_sub(self.view_h.max(1)),
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = (self.hscroll + H_STEP).min(self.max_h())
            }
            KeyCode::Char('h') | KeyCode::Left => self.left(now),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => self.step(-1, now),
            KeyCode::Enter | KeyCode::Char('n') => self.step(1, now),
            KeyCode::Char('N') => self.step(-1, now),
            KeyCode::Char(' ') => self.mark_and_advance(now),
            KeyCode::Tab => self.open_branches(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Char('q') => return self.request_quit(),
            _ => {}
        }
        Outcome::Continue
    }

    // Same rule as the other pickers: an h that follows another within RUN_WINDOW is part of one
    // run and never leaves the diff, so holding h to reach the left edge cannot overshoot.
    fn left(&mut self, now: Instant) {
        let in_run = self
            .last_h
            .is_some_and(|t| now.saturating_duration_since(t) < RUN_WINDOW);
        self.last_h = Some(now);
        if self.hscroll > 0 {
            self.hscroll = self.hscroll.saturating_sub(H_STEP);
        } else if !in_run {
            self.focus = Focus::List;
        }
    }

    fn key_filter(&mut self, key: KeyEvent, src: &mut dyn Files) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.mode = Mode::Browse,
            KeyCode::Esc => {
                self.filter.text.clear();
                self.input.clear();
                self.mode = Mode::Browse;
                self.rebuild(src, None);
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.filter.text.clear();
                self.rebuild(src, None);
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.filter.text = self.input.clone();
                self.rebuild(src, None);
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                self.filter.text = self.input.clone();
                self.rebuild(src, None);
            }
            _ => {}
        }
    }

    fn key_branches(&mut self, key: KeyEvent, src: &mut dyn Files) {
        if self.overlay_hscroll(&key) {
            return;
        }
        let page = self.overlay_page();
        let last = self.pending.len().saturating_sub(1);
        match key.code {
            KeyCode::PageDown => self.overlay_cursor = (self.overlay_cursor + page).min(last),
            KeyCode::PageUp => self.overlay_cursor = self.overlay_cursor.saturating_sub(page),
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay_cursor = (self.overlay_cursor + 1).min(last)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay_cursor = self.overlay_cursor.saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => self.overlay_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.overlay_cursor = last,
            KeyCode::Char(' ') => self.toggle_pending(self.overlay_cursor),
            KeyCode::Char('a') => {
                // The modified files stand alone, so "all" is every other source.
                let others: Vec<usize> = (0..self.sources.len())
                    .filter(|&i| !self.sources[i].0.is_modified())
                    .collect();
                if !others.is_empty() {
                    let all_on = others.iter().all(|&i| self.pending[i]);
                    for i in 0..self.pending.len() {
                        self.pending[i] = !self.sources[i].0.is_modified() && !all_on;
                    }
                }
            }
            // Inverts every row. With the default selection (branches on, stashes off) one press
            // leaves exactly the stashes selected. Scoped to this overlay: the commit and file
            // lists have no select-all key, since a stray press there would mark many entries.
            KeyCode::Char('v') => self.pending.iter_mut().for_each(|on| *on = !*on),
            KeyCode::Enter => {
                if !self.pending.iter().any(|on| *on) {
                    self.say("select at least one branch", true);
                    return;
                }
                // Choosing the modified files is a way of throwing work away, so it is asked
                // about before the list of files is shown.
                let chosen = self.pending_has_modified();
                let already = self.sources.iter().any(|(s, on)| s.is_modified() && *on);
                if chosen && !already {
                    self.mode = Mode::ConfirmModified;
                    return;
                }
                self.apply_pending(src);
            }
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => self.mode = Mode::Browse,
            _ => {}
        }
    }

    // Flip the source at `i` in the overlay. The modified files cannot be combined with a branch
    // or stash (their restore throws the change away, the others bring a file in), so selecting
    // either kind clears the other.
    fn toggle_pending(&mut self, i: usize) {
        let Some(&on) = self.pending.get(i) else {
            return;
        };
        if !on {
            let modified = self.sources[i].0.is_modified();
            for j in 0..self.pending.len() {
                if j != i && (modified || self.sources[j].0.is_modified()) {
                    self.pending[j] = false;
                }
            }
        }
        self.pending[i] = !on;
    }

    fn pending_has_modified(&self) -> bool {
        self.sources
            .iter()
            .zip(&self.pending)
            .any(|((s, _), on)| s.is_modified() && *on)
    }

    // Make the overlay's choice the selection and read its files.
    fn apply_pending(&mut self, src: &mut dyn Files) {
        for ((_, on), new) in self.sources.iter_mut().zip(&self.pending) {
            *on = *new;
        }
        self.mode = Mode::Browse;
        self.load(src);
    }

    fn key_confirm_modified(&mut self, key: KeyEvent, src: &mut dyn Files) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.apply_pending(src),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.mode = Mode::Branches,
            _ => {}
        }
    }

    fn key_versions(&mut self, key: KeyEvent, now: Instant) {
        let Some(e) = self.current().cloned() else {
            self.mode = Mode::Browse;
            return;
        };
        if self.overlay_hscroll(&key) {
            return;
        }
        let page = self.overlay_page();
        let last = e.versions.len().saturating_sub(1);
        match key.code {
            KeyCode::PageDown => {
                self.overlay_cursor = (self.overlay_cursor + page).min(last);
                self.moved = Some(now);
            }
            KeyCode::PageUp => {
                self.overlay_cursor = self.overlay_cursor.saturating_sub(page);
                self.moved = Some(now);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay_cursor = (self.overlay_cursor + 1).min(last);
                self.moved = Some(now);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay_cursor = self.overlay_cursor.saturating_sub(1);
                self.moved = Some(now);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.overlay_cursor = 0;
                self.moved = Some(now);
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.overlay_cursor = last;
                self.moved = Some(now);
            }
            KeyCode::Enter => {
                let v = e.versions[self.overlay_cursor.min(last)].clone();
                self.mode = Mode::Browse;
                self.say(format!("{} will come from {}", e.path, v.source), false);
                self.marks.insert(e.path, v);
                if std::mem::take(&mut self.advance) {
                    self.step(1, now);
                }
            }
            KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('q') => {
                self.advance = false;
                self.mode = Mode::Browse;
            }
            _ => {}
        }
    }

    fn open_patches(&mut self, src: &mut dyn Files) {
        match src.patches() {
            Ok(items) if items.is_empty() => {
                self.say("no patch files are kept in .git/gitomic-picks", false)
            }
            Ok(items) => {
                self.patches = Some(PatchView {
                    items,
                    marked: HashSet::new(),
                    cursor: 0,
                    text: None,
                    scroll: 0,
                });
                self.mode = Mode::Patches;
                self.load_patch(src);
            }
            Err(e) => self.say(e, true),
        }
    }

    // Read the highlighted patch for the right pane.
    fn load_patch(&mut self, src: &mut dyn Files) {
        let Some(pv) = self.patches.as_mut() else {
            return;
        };
        pv.scroll = 0;
        let Some(item) = pv.items.get(pv.cursor) else {
            pv.text = None;
            return;
        };
        if pv.text.as_ref().is_some_and(|(n, _)| *n == item.name) {
            return;
        }
        let name = item.name.clone();
        let body = match src.patch_text(&name) {
            Ok(t) => t,
            Err(e) => format!("could not read the patch: {e}"),
        };
        pv.text = Some((name, plain(&body)));
    }

    fn patch_step(&mut self, delta: isize, src: &mut dyn Files) {
        if let Some(pv) = self.patches.as_mut() {
            let last = pv.items.len().saturating_sub(1) as isize;
            pv.cursor = (pv.cursor as isize + delta).clamp(0, last) as usize;
        }
        self.load_patch(src);
    }

    fn key_patches(&mut self, key: KeyEvent, _now: Instant, src: &mut dyn Files) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = self.view_h / 2;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.patch_step(1, src),
            KeyCode::Char('k') | KeyCode::Up => self.patch_step(-1, src),
            KeyCode::Char('g') | KeyCode::Home => self.patch_step(isize::MIN / 2, src),
            KeyCode::Char('G') | KeyCode::End => self.patch_step(isize::MAX / 2, src),
            KeyCode::PageDown => self.patch_step(self.view_h.max(1) as isize, src),
            KeyCode::PageUp => self.patch_step(-(self.view_h.max(1) as isize), src),
            KeyCode::Char(' ') => {
                if let Some(pv) = self.patches.as_mut() {
                    if let Some(item) = pv.items.get(pv.cursor) {
                        if !pv.marked.remove(&item.name) {
                            pv.marked.insert(item.name.clone());
                        }
                    }
                }
                self.patch_step(1, src);
            }
            KeyCode::Char('d') if ctrl => {
                if let Some(pv) = self.patches.as_mut() {
                    pv.scroll = pv.scroll.saturating_add(half.max(1));
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(pv) = self.patches.as_mut() {
                    pv.scroll = pv.scroll.saturating_sub(half.max(1));
                }
            }
            KeyCode::Char('d') => {
                let Some(pv) = self.patches.as_ref() else {
                    return;
                };
                let mut names: Vec<String> = pv
                    .items
                    .iter()
                    .filter(|i| pv.marked.contains(&i.name))
                    .map(|i| i.name.clone())
                    .collect();
                if names.is_empty() {
                    names.extend(pv.items.get(pv.cursor).map(|i| i.name.clone()));
                }
                if !names.is_empty() {
                    self.mode = Mode::ConfirmTrash(names);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('P') => {
                self.patches = None;
                self.mode = Mode::Browse;
            }
            _ => {}
        }
    }

    fn key_confirm_trash(&mut self, key: KeyEvent, names: Vec<String>, src: &mut dyn Files) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => match src.trash(&names) {
                Ok(n) => {
                    let mut left = 0;
                    if let Some(pv) = self.patches.as_mut() {
                        pv.items.retain(|i| !names.contains(&i.name));
                        pv.marked.clear();
                        pv.text = None;
                        pv.cursor = pv.cursor.min(pv.items.len().saturating_sub(1));
                        left = pv.items.len();
                    }
                    if left == 0 {
                        self.patches = None;
                        self.mode = Mode::Browse;
                    } else {
                        self.mode = Mode::Patches;
                        self.load_patch(src);
                    }
                    self.say(format!("deleted {n} patch file(s)"), false);
                }
                Err(e) => {
                    self.mode = Mode::Patches;
                    self.say(e, true);
                }
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Patches;
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        if self.marks.is_empty() {
            self.say("nothing is marked; press space on a file first", true);
            return;
        }
        self.mode = Mode::ConfirmApply;
    }

    fn key_confirm_apply(&mut self, key: KeyEvent, src: &mut dyn Files) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let picks: Vec<(Version, String)> = self
                    .marks
                    .iter()
                    .map(|(p, v)| (v.clone(), p.clone()))
                    .collect();
                match src.specs(&picks) {
                    Ok(specs) => Outcome::Submit(specs),
                    Err(e) => {
                        self.mode = Mode::Browse;
                        self.say(e, true);
                        Outcome::Continue
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.say("cancelled; marks kept", false);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn request_quit(&mut self) -> Outcome {
        if self.marks.is_empty() {
            Outcome::Quit
        } else {
            self.mode = Mode::ConfirmQuit;
            Outcome::Continue
        }
    }

    fn key_confirm_quit(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Quit,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let left_w = (area.width * 2 / 5).clamp(28, 60).min(area.width / 2);
        let cols =
            Layout::horizontal([Constraint::Length(left_w), Constraint::Min(1)]).split(rows[0]);
        if matches!(self.mode, Mode::Patches | Mode::ConfirmTrash(_)) {
            self.render_patches(frame, cols[0], cols[1]);
        } else {
            self.render_browse(frame, cols[0], cols[1]);
        }
        self.render_bar(frame, rows[1]);
        match self.mode {
            Mode::Branches | Mode::ConfirmModified | Mode::Versions | Mode::History => {
                self.render_overlay(frame, area)
            }
            _ => {}
        }
    }

    fn render_browse(&mut self, frame: &mut Frame, left: Rect, right: Rect) {
        let focused = self.focus;
        let focus_style = |f: Focus| {
            if focused == f {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new().fg(Color::DarkGray)
            }
        };

        // Only the rows that fit are built; the list can hold a hundred thousand files, and
        // building a row for each on every frame is what the cost would otherwise follow.
        let rows_h = left.height.saturating_sub(2) as usize;
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if rows_h > 0 && self.cursor >= self.top + rows_h {
            self.top = self.cursor + 1 - rows_h;
        }
        self.top = self.top.min(self.view.len().saturating_sub(rows_h.max(1)));
        let items: Vec<ListItem> = self
            .view
            .iter()
            .skip(self.top)
            .take(rows_h)
            .map(|&i| {
                let e = &self.all[i];
                let mark = self.marks.get(&e.path);
                let flag = if e.distinct() > 1 { " *" } else { "" };
                let deleted = if self.gone.contains(&e.path) {
                    "  (deleted)"
                } else {
                    ""
                };
                let text = match mark {
                    // A version taken from history says which commit it is.
                    Some(v) if e.versions.iter().any(|x| x.commit == v.commit) => {
                        format!("[x] {}{flag}{deleted}  <- {}", e.path, v.source)
                    }
                    Some(v) => format!(
                        "[x] {}{flag}{deleted}  <- {}@{}",
                        e.path,
                        v.source,
                        short(&v.commit)
                    ),
                    None => format!("[ ] {}{flag}{deleted}", e.path),
                };
                let style = if mark.is_some() {
                    Style::new().fg(Color::Green)
                } else if e.state() == State::Absent {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new()
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect();
        let on = self.sources.iter().filter(|(_, on)| *on).count();
        let mut title = format!(
            " files {} of {}  {} marked  branches {}/{}",
            self.view.len(),
            self.all.len(),
            self.marks.len(),
            on,
            self.sources.len()
        );
        if !self.filter.text.trim().is_empty() {
            title.push_str(&format!("  /{}", self.filter.text.trim()));
        }
        if self.filter.binary_only {
            title.push_str("  binary");
        }
        if self.show_deleted {
            title.push_str("  +deleted");
        }
        title.push(' ');
        let cursor_style = if self.focus == Focus::List {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(focus_style(Focus::List)),
            )
            .highlight_style(cursor_style);
        self.list_state.select(if self.view.is_empty() {
            None
        } else {
            Some(self.cursor - self.top)
        });
        *self.list_state.offset_mut() = 0;
        frame.render_stateful_widget(list, left, &mut self.list_state);

        let inner_h = right.height.saturating_sub(2) as usize;
        let inner_w = right.width.saturating_sub(2) as usize;
        self.view_h = inner_h.max(1);
        self.view_w = inner_w.max(1);
        self.clamp_scroll();
        let (visible, total): (Vec<Line>, usize) = match self.current_view() {
            Some(v) => (
                v.lines
                    .iter()
                    .skip(self.vscroll)
                    .take(inner_h)
                    .cloned()
                    .collect(),
                v.lines.len(),
            ),
            None if self.view.is_empty() && self.all.is_empty() => (
                vec![Line::from(
                    "No file on the selected branches. Tab chooses the branches.",
                )],
                0,
            ),
            None if self.view.is_empty() => (vec![Line::from("No file matches the filter.")], 0),
            None => (vec![Line::from("(loading the change...)")], 0),
        };
        let heading = match (self.current(), self.candidate()) {
            (Some(e), Some((v, _))) => format!(
                " {}  from {} {}{}  line {}/{} ",
                e.path,
                v.source,
                short(&v.blob),
                if e.distinct() > 1 && !self.marks.contains_key(&e.path) {
                    format!("  ({} differing versions; v chooses)", e.distinct())
                } else {
                    String::new()
                },
                (self.vscroll + 1).min(total.max(1)),
                total
            ),
            _ => " no file ".to_string(),
        };
        let diff = Paragraph::new(visible)
            .block(
                Block::bordered()
                    .title(heading)
                    .border_style(focus_style(Focus::Diff)),
            )
            .scroll((0, self.hscroll.min(u16::MAX as usize) as u16));
        frame.render_widget(diff, right);
    }

    fn render_patches(&mut self, frame: &mut Frame, left: Rect, right: Rect) {
        let Some(pv) = self.patches.as_ref() else {
            return;
        };
        let now = SystemTime::now();
        let items: Vec<ListItem> = pv
            .items
            .iter()
            .map(|i| {
                let mark = if pv.marked.contains(&i.name) {
                    'x'
                } else {
                    ' '
                };
                let age = i.modified.map_or("?".to_string(), |m| age(now, m));
                let style = if pv.marked.contains(&i.name) {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("[{mark}] {age:>4}  {}", i.summary),
                    style,
                )))
            })
            .collect();
        let title = format!(" patches {}  {} marked ", pv.items.len(), pv.marked.len());
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::Cyan)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(pv.cursor));
        frame.render_stateful_widget(list, left, &mut state);

        let inner_h = right.height.saturating_sub(2) as usize;
        self.view_h = inner_h.max(1);
        let (heading, lines): (String, Vec<Line>) = match (&pv.text, pv.items.get(pv.cursor)) {
            (Some((name, view)), Some(item)) => {
                let max = view.lines.len().saturating_sub(inner_h);
                let start = pv.scroll.min(max);
                (
                    format!(" {name}  {} ", human(item.size)),
                    view.lines
                        .iter()
                        .skip(start)
                        .take(inner_h)
                        .cloned()
                        .collect(),
                )
            }
            _ => (" no patch ".to_string(), Vec::new()),
        };
        let text = Paragraph::new(lines).block(
            Block::bordered()
                .title(heading)
                .border_style(Style::new().fg(Color::DarkGray)),
        );
        frame.render_widget(text, right);
    }

    fn render_overlay(&mut self, frame: &mut Frame, area: Rect) {
        let (title, rows): (&str, Vec<String>) = match self.mode {
            Mode::Branches | Mode::ConfirmModified => (
                " Restore from which branches? ",
                self.sources
                    .iter()
                    .zip(&self.pending)
                    .map(|((s, _), on)| {
                        format!(
                            "[{}] {}{}{}",
                            if *on { 'x' } else { ' ' },
                            s.name,
                            if s.remote { "  (remote)" } else { "" },
                            if s.note.is_empty() {
                                String::new()
                            } else {
                                format!("  {}", s.note)
                            }
                        )
                    })
                    .collect(),
            ),
            Mode::History => (" History: take this version ", self.history_rows()),
            _ => (
                " Take the file from which branch? ",
                self.current()
                    .map(|e| {
                        e.versions
                            .iter()
                            .map(|v| format!("{}  {}", v.source, short(&v.blob)))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
        };
        let w = (area.width * 3 / 5).clamp(30.min(area.width), area.width);
        let h = (area.height * 3 / 5).clamp(6.min(area.height), area.height);
        let popup = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };
        // Rows wider than the box scroll sideways. The `[x]` marker of the source overlay stays put.
        let fixed = if matches!(self.mode, Mode::Branches | Mode::ConfirmModified) {
            4
        } else {
            0
        };
        let reach = max_hscroll(&rows, w.saturating_sub(2) as usize);
        self.ov_hscroll = self.ov_hscroll.min(reach);
        let title = if reach > 0 {
            format!("{title}‹ h/l › ")
        } else {
            title.to_string()
        };
        let items: Vec<ListItem> = rows
            .iter()
            .map(|r| ListItem::new(hslice(r, fixed, self.ov_hscroll)))
            .collect();
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::Yellow)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(self.overlay_cursor));
        frame.render_widget(Clear, popup);
        frame.render_stateful_widget(list, popup, &mut state);
    }

    // The lines of the history overlay for the highlighted file.
    fn history_rows(&self) -> Vec<String> {
        let Some(e) = self.current() else {
            return Vec::new();
        };
        match self.history.get(&e.path) {
            None | Some(HistState::Loading) => vec!["reading the history...".to_string()],
            Some(HistState::Failed(why)) => vec![format!("could not read it: {why}")],
            Some(HistState::Ready(rows, cut)) => {
                let mut lines: Vec<String> = rows
                    .iter()
                    .map(|r| {
                        format!(
                            "{} {}  {}  {}",
                            short(&r.version.commit),
                            civil(r.when),
                            r.version.source,
                            r.subject
                        )
                    })
                    .collect();
                if lines.is_empty() {
                    lines.push("no commit on the selected branches left this file in place".into());
                }
                if *cut {
                    lines.push("(older commits are not listed)".to_string());
                }
                lines
            }
        }
    }

    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        let prompt = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let bar = match &self.mode {
            Mode::ConfirmModified => Line::from(Span::styled(
                " Switch to the modified files? Restoring one discards its unstaged changes for good \
                 (nothing is recorded); other sources are deselected. [y/n] ",
                prompt,
            )),
            Mode::ConfirmApply if self.marks.values().all(|v| is_modified(&v.source)) => {
                Line::from(Span::styled(
                    format!(
                        " Discard the unstaged changes to {} file(s)? Nothing is recorded; they \
                         cannot be brought back. [y/n] ",
                        self.marks.len()
                    ),
                    prompt,
                ))
            }
            Mode::ConfirmApply => {
                let mut from: Vec<&str> = self.marks.values().map(|v| &*v.source).collect();
                from.sort();
                from.dedup();
                Line::from(Span::styled(
                    format!(
                        " Restore {} file(s) from {}? Local copies are overwritten. [y/n] ",
                        self.marks.len(),
                        from.join(", ")
                    ),
                    prompt,
                ))
            }
            Mode::ConfirmTrash(names) => Line::from(Span::styled(
                format!(" Delete {} patch file(s)? [y/n] ", names.len()),
                prompt,
            )),
            Mode::ConfirmQuit => Line::from(Span::styled(
                format!(" Quit and discard {} mark(s)? [y/n] ", self.marks.len()),
                prompt,
            )),
            Mode::Filter => Line::from(Span::styled(format!(" /{}_", self.input), prompt)),
            _ if !self.status.is_empty() => {
                let color = if self.status_is_error {
                    Color::Red
                } else {
                    Color::Green
                };
                let skipped = if self.skipped > 0 {
                    format!("  ({} submodule/odd paths hidden)", self.skipped)
                } else {
                    String::new()
                };
                Line::from(Span::styled(
                    format!(" {}{skipped} ", self.status),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ))
            }
            Mode::Patches => hint(" j/k move  space mark  d delete  Ctrl-d/u scroll  Esc back"),
            Mode::Branches => hint(
                " j/k PgUp/PgDn move  h/l scroll  space select  a all/none  v invert  Enter use  Esc ",
            ),
            Mode::Versions => hint(" j/k move  h/l scroll  Enter use this branch  Esc cancel "),
            Mode::History => hint(" j/k PgUp/PgDn move  h/l scroll  Enter use this version  Esc cancel "),
            Mode::Browse => match self.focus {
                Focus::List => hint(
                    " j/k PgUp/PgDn move  space mark  v branch  H history  D deleted  / filter  B binary  \
                     P patches  Tab sources  Enter go",
                ),
                Focus::Diff => hint(
                    " j/k h/l scroll  Enter/n next  N prev  space mark  Tab branches  Esc back",
                ),
            },
        };
        frame.render_widget(Paragraph::new(bar), area);
    }
}

fn hint(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

// The sources offered and which start selected: the local branches (the stashes and the modified
// files are listed but start off), just `from` when it names a source, or every stash when
// `stashes` is set and `from` is not.
fn initial_sources(
    all: Vec<Source>,
    from: Option<&str>,
    stashes: bool,
) -> Res<Vec<(Source, bool)>> {
    if all.is_empty() {
        return Err("restore: there is no other branch or stash to restore from".into());
    }
    match from {
        Some(name) => {
            if !all.iter().any(|s| s.name == name) {
                if restore::is_modified(name) {
                    return Err("restore: no tracked file has unstaged changes".into());
                }
                return Err(
                    format!("restore: '{name}' is not a branch or stash to restore from").into(),
                );
            }
            Ok(all
                .into_iter()
                .map(|s| {
                    let on = s.name == name;
                    (s, on)
                })
                .collect())
        }
        None if stashes => {
            if !all.iter().any(Source::is_stash) {
                return Err("restore: there is no stash".into());
            }
            Ok(all
                .into_iter()
                .map(|s| {
                    let on = s.is_stash();
                    (s, on)
                })
                .collect())
        }
        None => Ok(all
            .into_iter()
            .map(|s| {
                let on = !s.remote && !s.is_stash() && !s.is_modified();
                (s, on)
            })
            .collect()),
    }
}

// Run the screen on the real terminal. Returns the recipe of the patch the operator confirmed, or
// None when the screen was left without confirming. `stashes` starts it with every stash selected.
pub fn run(cwd: &Path, from: Option<&str>, stashes: bool) -> Res<Option<Vec<Spec>>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("restore: no path given, and the interactive screen needs a terminal".into());
    }
    with_terminal(|terminal| browse(terminal, cwd, from, stashes))
}

// The same on a terminal that is already set up; used by the cherry-pick screen's F and S keys.
pub fn browse(
    term: &mut DefaultTerminal,
    cwd: &Path,
    from: Option<&str>,
    stashes: bool,
) -> Res<Option<Vec<Spec>>> {
    let root = git::work_tree(cwd)?;
    let sources = initial_sources(restore::sources(cwd)?, from, stashes)?;
    let size_limit = crate::config::Config::load()?
        .cherry_size_limit_mb
        .saturating_mul(1024 * 1024);
    let index = Index::open(&root)?;
    let git_dir = git::git_dir(cwd)?;
    let mut src = GitFiles {
        root,
        git_dir,
        index,
        size_limit,
    };
    let mut app = App::new(sources);
    if let Some(loader) = src.loader() {
        app.use_worker(loader);
    }
    if let Some(loader) = src.history_loader() {
        app.use_history(loader);
    }
    app.load(&mut src);
    match event_loop(term, &mut app, &mut src)? {
        Outcome::Submit(specs) => Ok(Some(specs)),
        _ => Ok(None),
    }
}

fn event_loop(term: &mut DefaultTerminal, app: &mut App, src: &mut dyn Files) -> Res<Outcome> {
    loop {
        app.pump();
        app.ensure_diff(src, Instant::now());
        term.draw(|f| app.render(f))?;
        if let Some(wait) = app.wakeup(Instant::now()) {
            if !event::poll(wait)? {
                continue;
            }
        }
        if let Event::Key(key) = event::read()? {
            match app.handle_key(key, Instant::now(), src) {
                Outcome::Continue => {}
                done => return Ok(done),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::Version;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn ver(source: &str, blob: &str) -> Version {
        Version {
            source: source.into(),
            commit: format!("commit-{source}").into(),
            blob: blob.to_string(),
        }
    }

    fn entry(path: &str, head: Option<&str>, versions: &[(&str, &str)]) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            head: head.map(str::to_string),
            versions: versions.iter().map(|(s, b)| ver(s, b)).collect(),
        }
    }

    // Files as the fake presents them for the branches `one` and `two`:
    //   a.txt   changed the same way on both
    //   b.txt   changed the same way on both
    //   c.txt   changed differently on the two branches
    //   d.txt   absent on HEAD, only on `two`
    //   img.png binary, changed on `one`
    struct Fake {
        diffs: Vec<(String, String)>,
        specs: Vec<Vec<(Version, String)>>,
        loads: Vec<Vec<String>>,
        binary_calls: usize,
        patch_items: Vec<PatchInfo>,
        trashed: Vec<Vec<String>>,
        hist: Arc<FakeHist>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                diffs: Vec::new(),
                specs: Vec::new(),
                loads: Vec::new(),
                binary_calls: 0,
                patch_items: Vec::new(),
                trashed: Vec::new(),
                hist: Arc::new(FakeHist {
                    scans: std::sync::atomic::AtomicUsize::new(0),
                    fail: AtomicBool::new(false),
                }),
            }
        }
    }

    // History as the fake presents it: two commits for every path, and one deleted file.
    struct FakeHist {
        scans: std::sync::atomic::AtomicUsize,
        fail: AtomicBool,
    }

    impl HistoryLoader for FakeHist {
        fn versions(
            &self,
            tips: &[BranchTip],
            path: &str,
            _cancel: &Cancel,
        ) -> Result<(Vec<HistRow>, bool), String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("no such history".to_string());
            }
            let name = tips[0].0.as_str();
            let row = |n: i64| HistRow {
                version: ver_at(name, &format!("commit-h{n}"), &format!("{path}-h{n}")),
                when: 1_700_000_000 - n * 86_400,
                subject: format!("edit number {n}"),
            };
            Ok((vec![row(1), row(2)], false))
        }
        fn deleted(&self, _tips: &[BranchTip], _cancel: &Cancel) -> Result<Deleted, String> {
            self.scans.fetch_add(1, Ordering::SeqCst);
            Ok(Deleted {
                entries: vec![FileEntry {
                    path: "gone.txt".to_string(),
                    head: None,
                    versions: vec![ver_at("one", "commit-old", "G")],
                }],
                truncated: false,
            })
        }
    }

    fn ver_at(source: &str, commit: &str, blob: &str) -> Version {
        Version {
            source: source.into(),
            commit: commit.into(),
            blob: blob.to_string(),
        }
    }

    impl Files for Fake {
        fn tips(&mut self, names: &[String]) -> Result<Vec<BranchTip>, String> {
            Ok(names
                .iter()
                .map(|n| (n.clone(), Arc::from(format!("commit-{n}").as_str())))
                .collect())
        }
        fn history_loader(&self) -> Option<Arc<dyn HistoryLoader>> {
            Some(self.hist.clone())
        }
        fn entries(&mut self, sources: &[String]) -> Result<Vec<FileEntry>, String> {
            self.loads.push(sources.to_vec());
            let has = |s: &str| sources.iter().any(|x| x == s);
            let pick = |list: &[(&str, &str)]| -> Vec<(String, String)> {
                list.iter()
                    .filter(|(s, _)| has(s))
                    .map(|(s, b)| (s.to_string(), b.to_string()))
                    .collect()
            };
            let mut out = Vec::new();
            let mut add = |path: &str, head: Option<&str>, list: &[(&str, &str)]| {
                let v = pick(list);
                if !v.is_empty() {
                    let v: Vec<(&str, &str)> =
                        v.iter().map(|(s, b)| (s.as_str(), b.as_str())).collect();
                    out.push(entry(path, head, &v));
                }
            };
            add("a.txt", Some("A"), &[("one", "A2"), ("two", "A2")]);
            add("b.txt", Some("B"), &[("one", "B2"), ("two", "B2")]);
            add("c.txt", Some("C"), &[("one", "C1"), ("two", "C2")]);
            add("d.txt", None, &[("two", "D")]);
            add("img.png", Some("I"), &[("one", "I2")]);
            add("m.txt", Some("M"), &[("(modified)", "M")]);
            Ok(out)
        }
        fn binary(&mut self, _sources: &[String]) -> Result<HashSet<String>, String> {
            self.binary_calls += 1;
            Ok(["img.png".to_string()].into())
        }
        fn diff(&mut self, version: &Version, path: &str) -> Result<String, String> {
            self.diffs
                .push((version.source.to_string(), path.to_string()));
            Ok(format!(
                "diff --git a/{path} b/{path}\n+from {}\n",
                version.source
            ))
        }
        fn specs(&mut self, picks: &[(Version, String)]) -> Result<Vec<Spec>, String> {
            self.specs.push(picks.to_vec());
            Ok(picks
                .iter()
                .map(|(v, p)| Spec {
                    base: "head".into(),
                    restore: vec![(v.commit.to_string(), p.clone())],
                    ..Spec::default()
                })
                .collect())
        }
        fn patches(&mut self) -> Result<Vec<PatchInfo>, String> {
            Ok(self.patch_items.clone())
        }
        fn patch_text(&mut self, name: &str) -> Result<String, String> {
            Ok(format!("diff --git a/{name} b/{name}\n+text of {name}\n"))
        }
        fn trash(&mut self, names: &[String]) -> Result<usize, String> {
            self.trashed.push(names.to_vec());
            self.patch_items.retain(|p| !names.contains(&p.name));
            Ok(names.len())
        }
    }

    fn sources() -> Vec<(Source, bool)> {
        vec![
            (
                Source {
                    name: "one".into(),
                    remote: false,
                    note: String::new(),
                },
                true,
            ),
            (
                Source {
                    name: "two".into(),
                    remote: false,
                    note: String::new(),
                },
                true,
            ),
            (
                Source {
                    name: "origin/x".into(),
                    remote: true,
                    note: String::new(),
                },
                false,
            ),
        ]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyCode {
        KeyCode::Char(c)
    }

    fn app() -> (App, Fake) {
        let mut fake = Fake::new();
        let mut app = App::new(sources());
        app.use_history(fake.hist.clone());
        app.load(&mut fake);
        (app, fake)
    }

    // Wait for the history threads to answer everything asked so far.
    fn settle(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(10);
        while app.hist_pending > 0 {
            assert!(Instant::now() < end, "history never answered");
            app.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn press(app: &mut App, fake: &mut Fake, code: KeyCode) -> Outcome {
        app.handle_key(key(code), Instant::now(), fake)
    }

    fn type_text(app: &mut App, fake: &mut Fake, text: &str) {
        for c in text.chars() {
            press(app, fake, ch(c));
        }
    }

    fn paths(app: &App) -> Vec<&str> {
        app.view.iter().map(|&i| app.all[i].path.as_str()).collect()
    }

    fn at(app: &mut App, fake: &mut Fake, path: &str) {
        let i = paths(app).iter().position(|p| *p == path).unwrap();
        app.cursor = i;
        let _ = fake;
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_local_branches_are_read_first_and_the_list_is_sorted() {
        let (app, fake) = app();
        assert_eq!(fake.loads, [["one", "two"]]);
        assert_eq!(paths(&app), ["a.txt", "b.txt", "c.txt", "d.txt", "img.png"]);
    }

    #[test]
    fn a_file_the_branches_agree_on_is_marked_with_one_key_and_unmarked_with_another() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.marks["b.txt"].source.as_ref(), "one");
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        assert!(app.marks.is_empty());
    }

    #[test]
    fn a_file_the_branches_disagree_on_asks_which_branch() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "c.txt");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.mode, Mode::Versions);
        assert!(app.marks.is_empty());
        press(&mut app, &mut fake, ch('j'));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.marks["c.txt"].source.as_ref(), "two");
    }

    #[test]
    fn escape_leaves_the_version_overlay_without_marking() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "c.txt");
        press(&mut app, &mut fake, ch('v'));
        assert_eq!(app.mode, Mode::Versions);
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.marks.is_empty());
    }

    #[test]
    fn a_file_absent_on_head_can_be_marked() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "d.txt");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.marks["d.txt"].source.as_ref(), "two");
    }

    #[test]
    fn the_filter_narrows_live_and_keeps_marks_of_hidden_files() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, ch('/'));
        assert_eq!(app.mode, Mode::Filter);
        type_text(&mut app, &mut fake, "IMG");
        assert_eq!(paths(&app), ["img.png"]);
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(paths(&app), ["img.png"]);
        assert!(
            app.marks.contains_key("b.txt"),
            "the mark survives the filter"
        );
    }

    #[test]
    fn several_words_must_all_match_and_backspace_widens_again() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "txt c");
        assert_eq!(paths(&app), ["c.txt"]);
        press(&mut app, &mut fake, KeyCode::Backspace);
        press(&mut app, &mut fake, KeyCode::Backspace);
        assert_eq!(paths(&app).len(), 4);
    }

    #[test]
    fn escape_in_the_prompt_clears_and_escape_in_the_list_clears_before_quitting() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "b");
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(paths(&app).len(), 5);
        assert_eq!(app.mode, Mode::Browse);

        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "b");
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(paths(&app), ["b.txt"]);
        assert_eq!(press(&mut app, &mut fake, KeyCode::Esc), Outcome::Continue);
        assert_eq!(paths(&app).len(), 5);
        assert_eq!(press(&mut app, &mut fake, KeyCode::Esc), Outcome::Quit);
    }

    #[test]
    fn b_shows_only_binary_files_and_reads_them_once() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('B'));
        assert_eq!(paths(&app), ["img.png"]);
        assert_eq!(fake.binary_calls, 1);
        press(&mut app, &mut fake, ch('B'));
        assert_eq!(paths(&app).len(), 5);
        press(&mut app, &mut fake, ch('B'));
        assert_eq!(fake.binary_calls, 1, "the answer is kept");
    }

    #[test]
    fn the_binary_filter_combines_with_the_text_filter() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('B'));
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "txt");
        assert!(paths(&app).is_empty());
        assert!(screen(&mut app, 100, 12).contains("No file matches the filter"));
    }

    #[test]
    fn tab_selects_branches_and_reloads_the_list() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, KeyCode::Tab);
        assert_eq!(app.mode, Mode::Branches);
        // Deselect `one`, keep `two`.
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(fake.loads.last().unwrap(), &["two".to_string()]);
        assert_eq!(paths(&app), ["a.txt", "b.txt", "c.txt", "d.txt"]);
        assert_eq!(app.all[2].distinct(), 1, "c.txt has one version now");
    }

    #[test]
    fn a_remote_branch_can_be_added() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, KeyCode::Tab);
        press(&mut app, &mut fake, ch('G'));
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(
            fake.loads.last().unwrap(),
            &["one".to_string(), "two".to_string(), "origin/x".to_string()]
        );
    }

    #[test]
    fn the_branch_overlay_needs_one_branch_and_escape_changes_nothing() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, KeyCode::Tab);
        press(&mut app, &mut fake, ch('a'));
        assert!(app.pending.iter().all(|on| *on));
        press(&mut app, &mut fake, ch('a'));
        assert!(app.pending.iter().all(|on| !*on));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Branches);
        assert!(app.status.contains("at least one"));
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.selected(), ["one", "two"]);
        assert_eq!(fake.loads.len(), 1);
    }

    #[test]
    fn v_inverts_the_branch_overlay_selection_and_twice_restores_it() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, KeyCode::Tab);
        let before = app.pending.clone();
        press(&mut app, &mut fake, ch('v'));
        let flipped: Vec<bool> = before.iter().map(|on| !*on).collect();
        assert_eq!(app.pending, flipped);
        press(&mut app, &mut fake, ch('v'));
        assert_eq!(app.pending, before);
        // Inverting a full selection leaves none, which Enter refuses like any empty selection.
        press(&mut app, &mut fake, ch('a'));
        assert!(app.pending.iter().all(|on| *on));
        press(&mut app, &mut fake, ch('v'));
        assert!(app.pending.iter().all(|on| !*on));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Branches);
        assert!(app.status.contains("at least one"));
    }

    #[test]
    fn marks_whose_branch_is_deselected_are_dropped() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "d.txt");
        press(&mut app, &mut fake, ch(' '));
        at(&mut app, &mut fake, "img.png");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.marks.len(), 2);
        // Leave only `two`: img.png (only on `one`) disappears together with its mark.
        press(&mut app, &mut fake, KeyCode::Tab);
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.marks.keys().collect::<Vec<_>>(), ["d.txt"]);
        assert!(app.status.contains("dropped"));
    }

    #[test]
    fn the_diff_waits_for_the_selection_to_rest_and_is_loaded_once() {
        let (mut app, mut fake) = app();
        let t0 = Instant::now();
        app.moved = Some(t0);
        app.ensure_diff(&mut fake, t0);
        assert!(fake.diffs.is_empty(), "still within the dwell");
        assert!(app.wakeup(t0).is_some());
        let later = t0 + DWELL + Duration::from_millis(1);
        app.ensure_diff(&mut fake, later);
        app.ensure_diff(&mut fake, later);
        assert_eq!(fake.diffs, [("one".to_string(), "a.txt".to_string())]);
        assert!(app.wakeup(later).is_none());
    }

    #[test]
    fn the_pane_follows_the_version_overlay_and_a_mark() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "c.txt");
        let late = Instant::now() + Duration::from_secs(1);
        press(&mut app, &mut fake, ch('v'));
        press(&mut app, &mut fake, ch('j'));
        app.ensure_diff(&mut fake, late + Duration::from_secs(1));
        assert_eq!(fake.diffs.last().unwrap().0, "two");
        press(&mut app, &mut fake, KeyCode::Enter);
        app.ensure_diff(&mut fake, late + Duration::from_secs(2));
        assert_eq!(app.candidate().unwrap().0.source.as_ref(), "two");
    }

    #[test]
    fn a_long_list_draws_only_the_rows_that_fit_and_scrolls_with_the_cursor() {
        let (mut app, mut fake) = app();
        app.all = (0..50_000)
            .map(|i| entry(&format!("dir/file{i:05}.txt"), Some("H"), &[("one", "X")]))
            .collect();
        app.view = (0..app.all.len()).collect();
        app.cursor = 0;
        let first = screen(&mut app, 120, 12);
        assert!(
            first.contains("file00000.txt") && !first.contains("file00020.txt"),
            "{first}"
        );
        press(&mut app, &mut fake, ch('G'));
        let last = screen(&mut app, 120, 12);
        assert!(
            last.contains("file49999.txt") && !last.contains("file00000.txt"),
            "{last}"
        );
        // Moving up inside the window leaves the window where it is.
        let top = app.top;
        press(&mut app, &mut fake, ch('k'));
        let up = screen(&mut app, 120, 12);
        assert_eq!(app.top, top);
        assert!(
            up.contains("file49998.txt") && up.contains("file49999.txt"),
            "{up}"
        );
    }

    #[test]
    fn h_lists_the_commits_that_changed_the_file_and_enter_takes_one() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch('H'));
        assert_eq!(app.mode, Mode::History);
        settle(&mut app);
        let text = screen(&mut app, 140, 24);
        assert!(
            text.contains("edit number 1") && text.contains("edit number 2"),
            "{text}"
        );
        assert!(text.contains("2023-11-1"), "dates are shown: {text}");
        // The right pane follows the highlighted commit.
        assert_eq!(&*app.candidate().unwrap().0.commit, "commit-h1");
        press(&mut app, &mut fake, ch('j'));
        assert_eq!(&*app.candidate().unwrap().0.commit, "commit-h2");
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(&*app.marks["b.txt"].commit, "commit-h2");
        let text = screen(&mut app, 140, 24);
        assert!(
            text.contains("<- one@commit-h"),
            "a historic mark says so: {text}"
        );
    }

    #[test]
    fn escape_leaves_the_history_without_marking() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('H'));
        settle(&mut app);
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.marks.is_empty());
        // A second look does not read it again.
        press(&mut app, &mut fake, ch('H'));
        assert_eq!(app.hist_pending, 0);
    }

    #[test]
    fn a_history_that_cannot_be_read_says_so() {
        let (mut app, mut fake) = app();
        fake.hist.fail.store(true, Ordering::SeqCst);
        press(&mut app, &mut fake, ch('H'));
        assert!(screen(&mut app, 140, 24).contains("reading the history"));
        settle(&mut app);
        let text = screen(&mut app, 140, 24);
        assert!(
            text.contains("could not read it: no such history"),
            "{text}"
        );
        press(&mut app, &mut fake, KeyCode::Enter);
        assert!(app.marks.is_empty());
    }

    #[test]
    fn d_lists_deleted_files_once_and_hides_them_again() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('D'));
        settle(&mut app);
        assert!(paths(&app).contains(&"gone.txt"));
        let text = screen(&mut app, 140, 24);
        assert!(
            text.contains("gone.txt") && text.contains("(deleted)") && text.contains("+deleted"),
            "{text}"
        );
        at(&mut app, &mut fake, "gone.txt");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(&*app.marks["gone.txt"].commit, "commit-old");
        press(&mut app, &mut fake, ch('D'));
        assert!(!paths(&app).contains(&"gone.txt"));
        // Turned on again, the earlier scan is reused.
        press(&mut app, &mut fake, ch('D'));
        assert!(paths(&app).contains(&"gone.txt"));
        settle(&mut app);
        assert_eq!(fake.hist.scans.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn changing_the_branches_drops_history_and_rescans_only_when_deleted_files_are_shown() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('H'));
        settle(&mut app);
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(&*app.marks["a.txt"].commit, "commit-h1");
        press(&mut app, &mut fake, ch('D'));
        settle(&mut app);
        assert_eq!(fake.hist.scans.load(Ordering::SeqCst), 1);
        // Only `two` stays selected: the mark from `one` goes with its branch, history is dropped
        // and the scan runs again for the new branches.
        app.sources[0].1 = false;
        app.load(&mut fake);
        assert!(app.marks.is_empty());
        assert!(app.history.is_empty());
        settle(&mut app);
        assert_eq!(fake.hist.scans.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_answer_that_belongs_to_an_earlier_load_is_discarded() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('H'));
        press(&mut app, &mut fake, KeyCode::Esc);
        app.load(&mut fake);
        settle(&mut app);
        assert!(
            app.history.is_empty(),
            "the old answer must not repopulate it"
        );
    }

    #[test]
    fn dates_are_calendar_dates() {
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(1_700_000_000), "2023-11-14");
        assert_eq!(civil(951_782_400), "2000-02-29");
    }

    #[test]
    fn enter_with_nothing_marked_explains_and_asks_nothing() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_is_error && app.status.contains("nothing is marked"));
    }

    #[test]
    fn the_confirmation_is_answered_only_by_y_and_yields_one_spec_per_file() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        at(&mut app, &mut fake, "d.txt");
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::ConfirmApply);
        for code in [KeyCode::Enter, ch(' '), ch('j')] {
            assert_eq!(press(&mut app, &mut fake, code), Outcome::Continue);
            assert_eq!(app.mode, Mode::ConfirmApply);
        }
        let out = press(&mut app, &mut fake, ch('y'));
        let Outcome::Submit(specs) = out else {
            panic!("expected specs");
        };
        assert_eq!(specs.len(), 2, "each file is its own patch");
        assert_eq!(
            specs[0].restore,
            [("commit-one".to_string(), "b.txt".to_string())]
        );
        assert_eq!(
            specs[1].restore,
            [("commit-two".to_string(), "d.txt".to_string())]
        );
        assert!(specs.iter().all(|s| s.picks.is_empty()));
        assert_eq!(fake.specs[0].len(), 2);
    }

    #[test]
    fn n_and_escape_cancel_the_confirmation_and_keep_the_marks() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        for code in [ch('n'), KeyCode::Esc] {
            press(&mut app, &mut fake, KeyCode::Enter);
            assert_eq!(press(&mut app, &mut fake, code), Outcome::Continue);
            assert_eq!(app.mode, Mode::Browse);
            assert_eq!(app.marks.len(), 1);
        }
    }

    #[test]
    fn quitting_asks_only_when_something_is_marked() {
        let (mut app, mut fake) = app();
        assert_eq!(press(&mut app, &mut fake, ch('q')), Outcome::Quit);
        let (mut app, mut fake) = self::app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(press(&mut app, &mut fake, ch('q')), Outcome::Continue);
        assert_eq!(app.mode, Mode::ConfirmQuit);
        assert_eq!(press(&mut app, &mut fake, ch('n')), Outcome::Continue);
        press(&mut app, &mut fake, ch('q'));
        assert_eq!(press(&mut app, &mut fake, ch('y')), Outcome::Quit);
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('/'));
        let out = app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Instant::now(),
            &mut fake,
        );
        assert_eq!(out, Outcome::Quit);
    }

    #[test]
    fn movement_stays_inside_the_list_and_g_capital_g_jump() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('k'));
        assert_eq!(app.cursor, 0);
        press(&mut app, &mut fake, ch('G'));
        assert_eq!(app.cursor, 4);
        press(&mut app, &mut fake, ch('j'));
        assert_eq!(app.cursor, 4);
        press(&mut app, &mut fake, ch('g'));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn the_diff_pane_moves_between_files_and_h_returns() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('l'));
        assert_eq!(app.focus, Focus::Diff);
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.cursor, 1);
        press(&mut app, &mut fake, ch('N'));
        assert_eq!(app.cursor, 0);
        press(&mut app, &mut fake, ch('h'));
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn an_empty_selection_of_files_is_harmless() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "zzz");
        press(&mut app, &mut fake, KeyCode::Enter);
        for code in [ch('j'), ch('k'), ch(' '), ch('v'), ch('l'), KeyCode::Enter] {
            press(&mut app, &mut fake, code);
        }
        assert_eq!(app.mode, Mode::Browse);
        assert!(screen(&mut app, 100, 12).contains("No file matches"));
    }

    #[test]
    fn rendering_shows_marks_sources_and_differing_versions() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        at(&mut app, &mut fake, "c.txt");
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("[x] b.txt  <- one"), "{text}");
        assert!(text.contains("c.txt *"), "{text}");
        assert!(text.contains("branches 2/3"), "{text}");
        assert!(text.contains("2 differing versions"), "{text}");
        press(&mut app, &mut fake, ch('v'));
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("Take the file from which branch"), "{text}");
        press(&mut app, &mut fake, KeyCode::Esc);
        press(&mut app, &mut fake, KeyCode::Tab);
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("origin/x  (remote)"), "{text}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let (mut app, mut fake) = app();
        for (w, h) in [(1, 1), (10, 3), (30, 5)] {
            screen(&mut app, w, h);
        }
        press(&mut app, &mut fake, KeyCode::Tab);
        screen(&mut app, 12, 4);
    }

    #[test]
    fn from_selects_only_that_branch_and_unknown_names_are_refused() {
        let all: Vec<Source> = sources().into_iter().map(|(s, _)| s).collect();
        let picked = initial_sources(all.clone(), Some("two"), false).unwrap();
        let on: Vec<&str> = picked
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.name.as_str())
            .collect();
        assert_eq!(on, ["two"]);
        assert!(initial_sources(all.clone(), Some("nope"), false).is_err());
        let default = initial_sources(all, None, false).unwrap();
        assert_eq!(default.iter().filter(|(_, on)| *on).count(), 2);
        assert!(initial_sources(Vec::new(), None, false).is_err());
    }

    // Against a real repository: the marks become a spec that the shared pipeline applies.
    #[test]
    fn the_git_backed_files_produce_a_spec_that_restores_the_file() {
        use crate::testrepo::Repo;
        let r = Repo::new();
        r.write("f.txt", "base\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit_file("f.txt", "feat\n", "feat");
        r.git(&["checkout", "-q", "main"]);

        let root = git::work_tree(&r.0).unwrap();
        let index = Index::open(&root).unwrap();
        let git_dir = git::git_dir(&r.0).unwrap();
        let mut src = GitFiles {
            root,
            git_dir,
            index,
            size_limit: 0,
        };
        let srcs = initial_sources(restore::sources(&r.0).unwrap(), None, false).unwrap();
        let mut app = App::new(srcs);
        app.load(&mut src);
        assert_eq!(paths(&app), ["f.txt"]);
        app.handle_key(key(ch(' ')), Instant::now(), &mut src);
        app.handle_key(key(KeyCode::Enter), Instant::now(), &mut src);
        let out = app.handle_key(key(ch('y')), Instant::now(), &mut src);
        let Outcome::Submit(specs) = out else {
            panic!("expected specs");
        };
        crate::cherry::apply_specs(&r.0, specs, false, false).unwrap();
        assert_eq!(r.read("f.txt"), "feat\n");
    }

    #[test]
    fn space_marks_and_moves_down_so_a_run_of_presses_marks_a_run_of_files() {
        let (mut app, mut fake) = app();
        // a.txt and b.txt are agreed, c.txt needs a choice, d.txt is new on HEAD.
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.cursor, 1);
        assert!(app.marks.contains_key("a.txt"));
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.cursor, 2);
        assert!(app.marks.contains_key("b.txt"));
        // The disagreeing file asks first, and moving on waits for the answer.
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.mode, Mode::Versions);
        assert_eq!(app.cursor, 2);
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.cursor, 3);
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.cursor, 4);
        assert_eq!(
            app.marks.keys().collect::<Vec<_>>(),
            ["a.txt", "b.txt", "c.txt", "d.txt"]
        );
    }

    #[test]
    fn space_on_a_marked_file_unmarks_it_and_still_moves_down() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        at(&mut app, &mut fake, "b.txt");
        press(&mut app, &mut fake, ch(' '));
        assert!(app.marks.is_empty());
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn space_at_the_end_of_the_list_marks_and_stays() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('G'));
        press(&mut app, &mut fake, ch(' '));
        assert!(app.marks.contains_key("img.png") || app.status_is_error);
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn escape_from_the_version_overlay_started_by_space_does_not_move_on() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "c.txt");
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(app.cursor, 2);
        // A later explicit v-choice is not affected by the abandoned space.
        press(&mut app, &mut fake, ch('v'));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.cursor, 2);
        assert!(app.marks.contains_key("c.txt"));
    }

    #[test]
    fn the_filter_accepts_globs() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "*.png");
        assert_eq!(paths(&app), ["img.png"]);
        press(&mut app, &mut fake, KeyCode::Esc);
        press(&mut app, &mut fake, ch('/'));
        type_text(&mut app, &mut fake, "?.txt");
        assert_eq!(paths(&app), ["a.txt", "b.txt", "c.txt", "d.txt"]);
    }

    fn patch(name: &str, summary: &str) -> PatchInfo {
        PatchInfo {
            name: name.to_string(),
            size: 120,
            modified: Some(SystemTime::now()),
            summary: summary.to_string(),
        }
    }

    fn fake_with_patches() -> (App, Fake) {
        let (app, mut fake) = app();
        fake.patch_items = vec![
            patch("p1.patch", "restore a.txt from aaaa1111"),
            patch("p2.patch", "restore b.txt from bbbb2222"),
            patch("p3.patch", "pick cccc3333 a subject"),
        ];
        (app, fake)
    }

    #[test]
    fn p_with_no_patch_files_says_so() {
        let (mut app, mut fake) = app();
        press(&mut app, &mut fake, ch('P'));
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status.contains("no patch files"), "{}", app.status);
    }

    #[test]
    fn p_lists_the_patches_and_shows_the_highlighted_one() {
        let (mut app, mut fake) = fake_with_patches();
        press(&mut app, &mut fake, ch('P'));
        assert_eq!(app.mode, Mode::Patches);
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("restore a.txt from aaaa1111"), "{text}");
        assert!(text.contains("+text of p1.patch"), "{text}");
        press(&mut app, &mut fake, ch('j'));
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("+text of p2.patch"), "{text}");
        press(&mut app, &mut fake, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn marked_patches_are_deleted_after_a_y_and_only_a_y() {
        let (mut app, mut fake) = fake_with_patches();
        press(&mut app, &mut fake, ch('P'));
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, ch('j'));
        press(&mut app, &mut fake, ch(' '));
        // Space moved down each time, so p1 and p3 are marked.
        let marked = app.patches.as_ref().unwrap().marked.clone();
        assert_eq!(
            marked,
            ["p1.patch".to_string(), "p3.patch".to_string()].into()
        );
        press(&mut app, &mut fake, ch('d'));
        assert!(matches!(app.mode, Mode::ConfirmTrash(_)));
        for code in [KeyCode::Enter, ch(' '), ch('j')] {
            press(&mut app, &mut fake, code);
            assert!(matches!(app.mode, Mode::ConfirmTrash(_)));
        }
        assert!(fake.trashed.is_empty());
        press(&mut app, &mut fake, ch('y'));
        assert_eq!(fake.trashed, [["p1.patch", "p3.patch"]]);
        assert_eq!(app.mode, Mode::Patches);
        let pv = app.patches.as_ref().unwrap();
        assert_eq!(pv.items.len(), 1);
        assert_eq!(pv.items[0].name, "p2.patch");
        assert!(pv.marked.is_empty());
        assert!(app.status.contains("deleted 2"), "{}", app.status);
    }

    #[test]
    fn d_with_nothing_marked_deletes_the_highlighted_patch_and_n_cancels() {
        let (mut app, mut fake) = fake_with_patches();
        press(&mut app, &mut fake, ch('P'));
        press(&mut app, &mut fake, ch('j'));
        press(&mut app, &mut fake, ch('d'));
        press(&mut app, &mut fake, ch('n'));
        assert_eq!(app.mode, Mode::Patches);
        assert!(fake.trashed.is_empty());
        press(&mut app, &mut fake, ch('d'));
        let text = screen(&mut app, 120, 14);
        assert!(text.contains("Delete 1 patch file(s)? [y/n]"), "{text}");
        press(&mut app, &mut fake, ch('y'));
        assert_eq!(fake.trashed, [["p2.patch"]]);
    }

    #[test]
    fn deleting_the_last_patch_returns_to_the_file_list() {
        let (mut app, mut fake) = app();
        fake.patch_items = vec![patch("only.patch", "restore a.txt from aaaa1111")];
        press(&mut app, &mut fake, ch('P'));
        press(&mut app, &mut fake, ch('d'));
        press(&mut app, &mut fake, ch('y'));
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.patches.is_none());
    }

    // A loader that answers at once.
    struct Quick {
        calls: Mutex<Vec<String>>,
    }

    impl DiffLoader for Quick {
        fn diff(&self, v: &Version, path: &str, _c: &Cancel) -> Result<String, String> {
            lock(&self.calls).push(path.to_string());
            Ok(format!(
                "diff --git a/{path} b/{path}\n+from {}\n",
                v.source
            ))
        }
    }

    // Wait until the worker has answered for the candidate.
    fn wait_for_view(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(5);
        while app.current_view().is_none() {
            assert!(Instant::now() < end, "the worker did not answer");
            app.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_worker_loads_the_diff_and_the_screen_only_polls() {
        let (mut app, mut fake) = app();
        let quick = Arc::new(Quick {
            calls: Mutex::new(Vec::new()),
        });
        app.use_worker(quick.clone());
        let late = Instant::now() + Duration::from_secs(1);
        app.ensure_diff(&mut fake, late);
        assert!(
            app.current_view().is_none(),
            "nothing is read on this thread"
        );
        assert!(fake.diffs.is_empty());
        assert_eq!(app.wakeup(late), Some(POLL));
        // Asking again while the request is outstanding sends nothing more.
        app.ensure_diff(&mut fake, late);
        wait_for_view(&mut app);
        assert_eq!(*lock(&quick.calls), ["a.txt"]);
        assert_eq!(app.wakeup(late), None);
        assert!(app.inflight.is_empty());
    }

    // A loader that runs until it is cancelled for one path and answers at once for the others.
    struct Blocking {
        started: AtomicBool,
        cancelled: AtomicBool,
    }

    impl DiffLoader for Blocking {
        fn diff(&self, v: &Version, path: &str, cancel: &Cancel) -> Result<String, String> {
            if path == "a.txt" {
                self.started.store(true, Ordering::SeqCst);
                let end = Instant::now() + Duration::from_secs(5);
                while !cancel.is_cancelled() && Instant::now() < end {
                    thread::sleep(Duration::from_millis(2));
                }
                self.cancelled
                    .store(cancel.is_cancelled(), Ordering::SeqCst);
                return Err("cancelled".to_string());
            }
            Ok(format!("+{path} from {}\n", v.source))
        }
    }

    #[test]
    fn moving_on_cancels_the_running_diff_and_loads_the_new_one() {
        let (mut app, mut fake) = app();
        let loader = Arc::new(Blocking {
            started: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        app.use_worker(loader.clone());
        let late = Instant::now() + Duration::from_secs(1);
        app.ensure_diff(&mut fake, late);
        let end = Instant::now() + Duration::from_secs(5);
        while !loader.started.load(Ordering::SeqCst) {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        // Move to b.txt and let the dwell pass: the request for it cancels the one running.
        app.step(1, Instant::now());
        app.ensure_diff(&mut fake, late + Duration::from_secs(1));
        wait_for_view(&mut app);
        assert!(loader.cancelled.load(Ordering::SeqCst));
        assert!(app.current().is_some_and(|e| e.path == "b.txt"));
        // The abandoned diff left nothing behind, so a return to a.txt asks for it again.
        let key = ("commit-one".to_string(), "a.txt".to_string());
        assert!(!app.shown.contains_key(&key));
        let end = Instant::now() + Duration::from_secs(5);
        while app.inflight.contains(&key) {
            assert!(Instant::now() < end);
            app.pump();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!app.shown.contains_key(&key));
    }

    #[test]
    fn page_keys_move_a_screenful_through_the_file_list() {
        let (mut app, mut fake) = app();
        app.all = (0..100)
            .map(|i| entry(&format!("f{i:03}.txt"), Some("H"), &[("one", "X")]))
            .collect();
        app.view = (0..app.all.len()).collect();
        app.cursor = 0;
        app.view_h = 10;
        press(&mut app, &mut fake, KeyCode::PageDown);
        assert_eq!(app.cursor, 10);
        press(&mut app, &mut fake, KeyCode::PageDown);
        assert_eq!(app.cursor, 20);
        press(&mut app, &mut fake, KeyCode::PageUp);
        assert_eq!(app.cursor, 10);
        app.cursor = 95;
        press(&mut app, &mut fake, KeyCode::PageDown);
        assert_eq!(app.cursor, 99, "the end of the list stops the page");
        app.cursor = 3;
        press(&mut app, &mut fake, KeyCode::PageUp);
        assert_eq!(app.cursor, 0, "so does the start");
    }

    #[test]
    fn page_keys_move_the_overlay_cursor_by_most_of_a_screen() {
        let (mut app, mut fake) = app();
        app.sources = (0..30)
            .map(|i| {
                let source = Source {
                    name: format!("b{i}"),
                    remote: false,
                    note: String::new(),
                };
                (source, i == 0)
            })
            .collect();
        app.view_h = 10;
        press(&mut app, &mut fake, KeyCode::Tab);
        press(&mut app, &mut fake, KeyCode::PageDown);
        assert_eq!(app.overlay_cursor, 6);
        for _ in 0..10 {
            press(&mut app, &mut fake, KeyCode::PageDown);
        }
        assert_eq!(app.overlay_cursor, 29);
        press(&mut app, &mut fake, KeyCode::PageUp);
        assert_eq!(app.overlay_cursor, 23);
    }

    #[test]
    fn rows_of_the_source_overlay_scroll_sideways_and_keep_their_marker() {
        let (mut app, mut fake) = app();
        app.sources[0].0.name = format!("{}TAILMARK", "y".repeat(100));
        press(&mut app, &mut fake, KeyCode::Tab);
        let start = screen(&mut app, 80, 20);
        assert!(
            start.contains("[x] yyyy") && !start.contains("TAILMARK"),
            "{start}"
        );
        for _ in 0..20 {
            press(&mut app, &mut fake, ch('l'));
        }
        let end = screen(&mut app, 80, 20);
        assert!(end.contains("[x] ") && end.contains("TAILMARK"), "{end}");
        // The offset stops at the end of the widest row, so one press back already moves.
        let reach = app.ov_hscroll;
        press(&mut app, &mut fake, KeyCode::Left);
        assert_eq!(app.ov_hscroll, reach.saturating_sub(H_STEP));
        for _ in 0..20 {
            press(&mut app, &mut fake, ch('h'));
        }
        let back = screen(&mut app, 80, 20);
        assert!(
            back.contains("[x] yyyy") && !back.contains("TAILMARK"),
            "{back}"
        );
        assert_eq!(
            app.mode,
            Mode::Branches,
            "scrolling does not leave the overlay"
        );
    }

    #[test]
    fn the_version_overlay_scrolls_sideways_too() {
        let (mut app, mut fake) = app();
        at(&mut app, &mut fake, "c.txt");
        app.sources[0].0.name = format!("{}TAILMARK", "y".repeat(100));
        press(&mut app, &mut fake, ch('v'));
        assert_eq!(app.mode, Mode::Versions);
        press(&mut app, &mut fake, KeyCode::Right);
        assert_eq!(app.ov_hscroll, H_STEP);
        press(&mut app, &mut fake, KeyCode::Esc);
        press(&mut app, &mut fake, ch('v'));
        assert_eq!(
            app.ov_hscroll, 0,
            "a newly opened overlay starts at the left"
        );
    }

    #[test]
    fn stashes_start_unselected_unless_asked_for_and_can_be_named() {
        let mut all: Vec<Source> = sources().into_iter().map(|(s, _)| s).collect();
        let branches_only = all.clone();
        all.push(Source {
            name: "stash@{0}".into(),
            remote: false,
            note: "On main: x".into(),
        });
        let default = initial_sources(all.clone(), None, false).unwrap();
        assert!(
            !default.last().unwrap().1,
            "a stash is not offered by default"
        );
        assert_eq!(default.iter().filter(|(_, on)| *on).count(), 2);
        let stash = initial_sources(all.clone(), None, true).unwrap();
        let on: Vec<&str> = stash
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.name.as_str())
            .collect();
        assert_eq!(on, ["stash@{0}"]);
        let named = initial_sources(all, Some("stash@{0}"), true).unwrap();
        assert_eq!(named.iter().filter(|(_, on)| *on).count(), 1);
        let err = initial_sources(branches_only, None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no stash"), "{err}");
    }

    #[test]
    fn the_modified_files_start_unselected_and_can_be_named() {
        let branches_only: Vec<Source> = sources().into_iter().map(|(s, _)| s).collect();
        let mut all = branches_only.clone();
        all.push(Source {
            name: restore::MODIFIED.into(),
            remote: false,
            note: "2 files with unstaged changes".into(),
        });
        let default = initial_sources(all.clone(), None, false).unwrap();
        assert!(!default.last().unwrap().1, "not offered by default");
        assert_eq!(default.iter().filter(|(_, on)| *on).count(), 2);
        let named = initial_sources(all, Some(restore::MODIFIED), false).unwrap();
        let on: Vec<&str> = named
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.name.as_str())
            .collect();
        assert_eq!(on, [restore::MODIFIED]);
        let err = initial_sources(branches_only, Some(restore::MODIFIED), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no tracked file has unstaged changes"),
            "{err}"
        );
    }

    // The usual two branches plus the modified files, with the branches selected.
    fn app_with_modified() -> (App, Fake) {
        let mut srcs = sources();
        srcs.push((
            Source {
                name: restore::MODIFIED.into(),
                remote: false,
                note: "1 file with unstaged changes".into(),
            },
            false,
        ));
        let mut fake = Fake::new();
        let mut app = App::new(srcs);
        app.use_history(fake.hist.clone());
        app.load(&mut fake);
        (app, fake)
    }

    // Move the overlay cursor to the source called `name`.
    fn on_source(app: &mut App, fake: &mut Fake, name: &str) {
        press(app, fake, ch('g'));
        while app.sources[app.overlay_cursor].0.name != name {
            press(app, fake, ch('j'));
        }
    }

    #[test]
    fn choosing_the_modified_files_deselects_everything_else_and_back() {
        let (mut app, mut fake) = app_with_modified();
        press(&mut app, &mut fake, KeyCode::Tab);
        assert_eq!(app.pending, [true, true, false, false]);
        on_source(&mut app, &mut fake, restore::MODIFIED);
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.pending, [false, false, false, true]);
        // Choosing a branch again drops the modified files.
        on_source(&mut app, &mut fake, "one");
        press(&mut app, &mut fake, ch(' '));
        assert_eq!(app.pending, [true, false, false, false]);
        // "all" never includes them, and clears them when it is pressed.
        on_source(&mut app, &mut fake, restore::MODIFIED);
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, ch('a'));
        assert_eq!(app.pending, [true, true, true, false]);
        press(&mut app, &mut fake, ch('a'));
        assert_eq!(app.pending, [false, false, false, false]);
    }

    #[test]
    fn leaving_the_overlay_with_the_modified_files_chosen_asks_first() {
        let (mut app, mut fake) = app_with_modified();
        let loads = fake.loads.len();
        press(&mut app, &mut fake, KeyCode::Tab);
        on_source(&mut app, &mut fake, restore::MODIFIED);
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::ConfirmModified);
        assert_eq!(fake.loads.len(), loads, "nothing is read before the answer");
        assert!(screen(&mut app, 120, 14).contains("discards its unstaged changes for good"));
        // Other keys do nothing; n goes back to the overlay with the choice kept.
        press(&mut app, &mut fake, ch('x'));
        assert_eq!(app.mode, Mode::ConfirmModified);
        press(&mut app, &mut fake, ch('n'));
        assert_eq!(app.mode, Mode::Branches);
        assert_eq!(app.pending, [false, false, false, true]);
        assert_eq!(app.selected(), ["one", "two"], "still the branches");
        // y makes it the selection and shows its files.
        press(&mut app, &mut fake, KeyCode::Enter);
        press(&mut app, &mut fake, ch('y'));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.selected(), [restore::MODIFIED]);
        assert_eq!(paths(&app), ["m.txt"]);
    }

    #[test]
    fn no_question_when_the_modified_files_were_already_the_selection() {
        let (mut app, mut fake) = app_with_modified();
        press(&mut app, &mut fake, KeyCode::Tab);
        on_source(&mut app, &mut fake, restore::MODIFIED);
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        press(&mut app, &mut fake, ch('y'));
        press(&mut app, &mut fake, KeyCode::Tab);
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        // Going to a branch needs no question either.
        press(&mut app, &mut fake, KeyCode::Tab);
        on_source(&mut app, &mut fake, "one");
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.selected(), ["one"]);
    }

    #[test]
    fn the_final_question_for_modified_files_says_they_are_discarded() {
        let (mut app, mut fake) = app_with_modified();
        press(&mut app, &mut fake, KeyCode::Tab);
        on_source(&mut app, &mut fake, restore::MODIFIED);
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        press(&mut app, &mut fake, ch('y'));
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        assert_eq!(app.mode, Mode::ConfirmApply);
        let text = screen(&mut app, 120, 14);
        assert!(
            text.contains("Discard the unstaged changes to 1 file(s)"),
            "{text}"
        );
        assert!(text.contains("cannot be brought back"), "{text}");
    }
}
