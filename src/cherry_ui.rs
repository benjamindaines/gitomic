// Interactive screen for `gitomic cherry-pick` when no commit is named (issue #13). It follows the
// layout and keys of the `drop` picker (pick.rs): commits on the left, the highlighted commit's
// diff on the right, vim-style movement, space to mark, Enter to submit behind a y/n confirmation
// that Enter cannot answer. Three things are added: Tab opens an overlay that chooses the branch to
// pick from, the diff shows what the commit would change relative to the HEAD that was current when
// the screen opened, and a conflict opens a decision screen where each conflict hunk is settled
// with a single key.
//
// As in pick.rs, everything except the terminal is testable. `App` consumes key events and reports
// an `Outcome`; git is reached only through the `Source` trait, and rendering takes any ratatui
// backend. The screen prepares a `Spec` (which commits, against which base, with which decisions)
// and returns it; building and applying the patch is the command layer's job (cherry.rs), so the
// same code path serves the interactive and the command-line forms.
//
// Keys, list pane:
//   j/k, arrows  move            g/G  first/last          space  toggle the mark
//                Commits that would change nothing on HEAD are gray and skipped by j/k, g/G and
//                (in the diff pane) n/N. The diff loads after the selection rests on a commit
//                for a moment, so holding j or k does not run a merge for every row passed.
//   R            follow one file: mark this commit and every older commit that changes the same
//                file, each restricted to that file (an overlay asks which file when the commit
//                changes several; a second press unmarks the chain)
//   l, Right     open the diff   Tab  choose the source branch
//   Enter        prepare the marked commits (conflicts are decided first), then confirm with y/n
//   q, Esc       quit (confirmed first when commits are marked)
// Keys, diff pane: as for `drop` (j/k h/l g/G Ctrl-d/u, Enter/n next, N previous, space, Tab).
// Keys, branch overlay:
//   j/k, g/G     move     Enter  use the branch (marks are cleared)     Esc, Tab, q  close
// Keys, decision screen:
//   j/k          previous/next conflict     a  keep the tree copy     b  take the picked commit's
//   c            keep both (text conflicts)  u  undo the decision      Ctrl-d/u  scroll
//   X            restore this file whole from the newest commit marked for it with R
//   Enter        continue once every conflict is decided               Esc, q  leave

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::conflict::{Segment, Side};
use crate::patch::{Body, FileConflict, Job, Spec, Step};
use crate::pick::{style_diff, with_terminal, DiffView, H_STEP, RUN_WINDOW};
use crate::{cherry, git, Res};

// Lines of unchanged text shown above and below a conflict hunk.
const CONTEXT: usize = 3;

// How long the selection must rest on a commit before its diff is loaded. Each preview runs a
// merge in the object database, which is noticeable on slow hardware while a key is held down.
const DWELL: Duration = Duration::from_millis(150);

// Rows on either side of the selection whose applicability is worked out while the screen is idle.
const IDLE_REACH: usize = 60;

// Where a replay stands after the picks were submitted.
pub enum Flow {
    // Every conflict is decided; carries the `git diff --stat` summary of the patch.
    Ready(String),
    // A pick conflicts and needs decisions.
    Conflicts(Vec<FileConflict>),
    // The selection cannot be applied.
    Refused(String),
}

// One commit chosen for the patch, optionally restricted to a single file.
#[derive(Clone, PartialEq, Debug)]
pub struct Sel {
    pub id: String,
    pub subject: String,
    pub only: Option<String>,
}

// What the screen needs from git.
pub trait Source {
    fn branches(&mut self) -> Result<Vec<String>, String>;
    // Commits on `branch` that HEAD lacks, newest first.
    fn commits(&mut self, branch: &str) -> Result<Vec<(String, String)>, String>;
    // The preview text for one commit, restricted to one file when `only` is given.
    fn diff(&mut self, commit: &str, only: Option<&str>) -> Result<String, String>;
    // Whether replaying a commit onto HEAD would change anything. Commits that would not are shown
    // gray and skipped by the movement keys.
    fn applies(&mut self, _commit: &str) -> Result<bool, String> {
        Ok(true)
    }
    // Paths a commit changes.
    fn paths(&mut self, commit: &str) -> Result<Vec<String>, String>;
    // Commits of `branch` that HEAD lacks and that change `path`.
    fn touching(&mut self, branch: &str, path: &str) -> Result<Vec<String>, String>;
    // Begin replaying `picks` (oldest first) and report where it stops.
    fn start(&mut self, picks: &[Sel]) -> Result<Flow, String>;
    // Replace the picks restricted to `path` by one whole-file restore of it from the newest of
    // them, and replay again. Errors when no pick is restricted to `path`.
    fn restore(&mut self, path: &str) -> Result<Flow, String>;
    // Supply decisions for the conflict last reported and continue.
    fn resolve(&mut self, files: Vec<FileConflict>) -> Result<Flow, String>;
    // The commit a reported conflict belongs to.
    fn replaying(&self) -> Option<String> {
        None
    }
    // The recipe for the patch once the replay is `Ready`.
    fn spec(&mut self) -> Result<Spec, String>;
}

struct GitSource {
    cwd: PathBuf,
    root: PathBuf,
    // HEAD when the screen opened; every preview and the replay are relative to it.
    base: String,
    job: Option<Job>,
    picks: Vec<Sel>,
    // (commit, path) whole-file restores that replaced restricted picks.
    restores: Vec<(String, String)>,
    // Decisions carried across restarts of the replay, so a cancelled confirmation does not lose
    // them.
    decided: Vec<(String, Side)>,
}

impl GitSource {
    // (id, subject) of every pick, oldest first, as a patch header records them.
    fn subjects(&self) -> Vec<(String, String)> {
        self.picks
            .iter()
            .map(|p| (p.id.clone(), p.subject.clone()))
            .collect()
    }

    // Replay the current picks and restores from the start.
    fn run_job(&mut self) -> Result<Flow, String> {
        let ids: Vec<String> = self.picks.iter().map(|p| p.id.clone()).collect();
        let only: Vec<(String, String)> = self
            .picks
            .iter()
            .filter_map(|p| p.only.clone().map(|path| (p.id.clone(), path)))
            .collect();
        let mut job = Job::new(&self.root, &self.base, &ids, &self.decided)
            .with_scopes(&only, &self.restores);
        let step = job.advance().map_err(|e| e.to_string())?;
        self.decided = job.decisions().to_vec();
        self.job = Some(job);
        self.flow(step)
    }

    fn flow(&mut self, step: Step) -> Result<Flow, String> {
        match step {
            Step::Done => {
                let job = self.job.as_ref().ok_or("no replay in progress")?;
                let patch = job.patch(&self.subjects()).map_err(|e| e.to_string())?;
                if patch.is_empty() {
                    return Ok(Flow::Refused(
                        "the selection changes nothing relative to HEAD".to_string(),
                    ));
                }
                Ok(Flow::Ready(patch.stat))
            }
            Step::Conflicts(files) => Ok(Flow::Conflicts(files)),
            Step::Unsupported(list) => Ok(Flow::Refused(format!(
                "a conflict has no A/B decision ({}); use 'git cherry-pick' for it",
                list.join("; ")
            ))),
        }
    }
}

impl Source for GitSource {
    fn branches(&mut self) -> Result<Vec<String>, String> {
        cherry::branches(&self.cwd).map_err(|e| e.to_string())
    }

    fn commits(&mut self, branch: &str) -> Result<Vec<(String, String)>, String> {
        cherry::candidates(&self.cwd, branch).map_err(|e| e.to_string())
    }

    fn diff(&mut self, commit: &str, only: Option<&str>) -> Result<String, String> {
        cherry::preview(&self.root, &self.base, commit, only).map_err(|e| e.to_string())
    }

    fn applies(&mut self, commit: &str) -> Result<bool, String> {
        cherry::applies(&self.root, &self.base, commit).map_err(|e| e.to_string())
    }

    fn paths(&mut self, commit: &str) -> Result<Vec<String>, String> {
        cherry::changed_paths(&self.root, commit).map_err(|e| e.to_string())
    }

    fn touching(&mut self, branch: &str, path: &str) -> Result<Vec<String>, String> {
        cherry::touching(&self.cwd, branch, path).map_err(|e| e.to_string())
    }

    fn start(&mut self, picks: &[Sel]) -> Result<Flow, String> {
        self.picks = picks.to_vec();
        self.restores.clear();
        self.run_job()
    }

    fn restore(&mut self, path: &str) -> Result<Flow, String> {
        let newest = self
            .picks
            .iter()
            .rev()
            .find(|p| p.only.as_deref() == Some(path))
            .map(|p| p.id.clone())
            .ok_or(
                "whole-file restore is offered for a selection made with R (one file's history)",
            )?;
        self.picks.retain(|p| p.only.as_deref() != Some(path));
        self.restores.push((newest, path.to_string()));
        self.run_job()
    }

    fn resolve(&mut self, files: Vec<FileConflict>) -> Result<Flow, String> {
        let job = self.job.as_mut().ok_or("no replay in progress")?;
        job.resolve(&files).map_err(|e| e.to_string())?;
        let step = job.advance().map_err(|e| e.to_string())?;
        self.decided = job.decisions().to_vec();
        self.flow(step)
    }

    fn replaying(&self) -> Option<String> {
        self.job
            .as_ref()
            .and_then(|j| j.current())
            .map(str::to_string)
    }

    fn spec(&mut self) -> Result<Spec, String> {
        let job = self.job.as_ref().ok_or("no replay in progress")?;
        job.patch(&self.subjects())
            .map(|p| p.spec)
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    List,
    Diff,
}

// What the screen is waiting for. Only `Browse` interprets navigation keys; the confirmations
// accept y or n and ignore everything else, Enter included.
#[derive(Clone, PartialEq, Debug)]
enum Mode {
    Browse,
    Overlay,
    Resolve,
    ConfirmApply { summary: String },
    ConfirmQuit,
}

#[derive(PartialEq, Debug)]
pub enum Outcome {
    Continue,
    Quit,
    Submit(Spec),
}

struct Entry {
    full: String,
    subject: String,
    marked: bool,
    // Set when the mark restricts the commit to one file (made with R).
    only: Option<String>,
    // Whether replaying the commit onto HEAD changes anything; unknown until worked out.
    applies: Option<bool>,
}

// What the overlay is choosing.
#[derive(Clone, PartialEq, Debug)]
enum OverlayKind {
    Branch,
    // A file among those the highlighted commit changes; R then follows that file's history.
    File,
}

struct Overlay {
    kind: OverlayKind,
    names: Vec<String>,
    cursor: usize,
}

// The decision screen's state: the conflicted files of the pick being replayed and a cursor over
// their individual decisions.
struct Resolver {
    files: Vec<FileConflict>,
    // (file index, unit index) for every decision, in display order.
    units: Vec<(usize, usize)>,
    cursor: usize,
    vscroll: usize,
}

impl Resolver {
    fn new(files: Vec<FileConflict>) -> Resolver {
        let units = files
            .iter()
            .enumerate()
            .flat_map(|(fi, f)| (0..f.units()).map(move |u| (fi, u)))
            .collect();
        let mut r = Resolver {
            files,
            units,
            cursor: 0,
            vscroll: 0,
        };
        r.cursor = (0..r.units.len())
            .find(|&i| r.choice_at(i).is_none())
            .unwrap_or(0);
        r
    }

    fn choice_at(&self, i: usize) -> Option<Side> {
        let (f, u) = self.units[i];
        self.files[f].choice(u)
    }

    fn undecided(&self) -> usize {
        self.files.iter().map(FileConflict::unresolved).sum()
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.units.len().saturating_sub(1);
        let next = self.cursor.saturating_add_signed(delta).min(last);
        if next != self.cursor {
            self.cursor = next;
            self.vscroll = 0;
        }
    }

    // Record `side` for the current decision and, when deciding (not undoing), move on to the next
    // undecided one if any remains. Returns false when the side is not available here.
    fn decide(&mut self, side: Option<Side>) -> bool {
        let Some(&(f, u)) = self.units.get(self.cursor) else {
            return false;
        };
        if !self.files[f].set(u, side) {
            return false;
        }
        if side.is_some() {
            let after = (self.cursor + 1..self.units.len()).find(|&i| self.choice_at(i).is_none());
            let before = (0..self.cursor).find(|&i| self.choice_at(i).is_none());
            if let Some(i) = after.or(before) {
                self.cursor = i;
                self.vscroll = 0;
            }
        }
        true
    }

    // Rows of the left pane: one per decision.
    fn rows(&self) -> Vec<Line<'static>> {
        self.units
            .iter()
            .enumerate()
            .map(|(i, &(f, u))| {
                let file = &self.files[f];
                let mark = self.choice_at(i).map_or(' ', Side::letter);
                let of = if file.units() > 1 {
                    format!("  {}/{}", u + 1, file.units())
                } else {
                    String::new()
                };
                let color = if mark == ' ' {
                    Color::Yellow
                } else {
                    Color::Green
                };
                Line::from(Span::styled(
                    format!("[{mark}] {}{of}", file.path),
                    Style::new().fg(color),
                ))
            })
            .collect()
    }

    // The right pane for the current decision.
    fn detail(&self, pick: &str) -> Vec<Line<'static>> {
        let Some(&(f, u)) = self.units.get(self.cursor) else {
            return Vec::new();
        };
        let file = &self.files[f];
        let choice = file.choice(u);
        let mut out = vec![
            Line::from(Span::styled(
                format!(
                    " {}  decision {} of {}",
                    file.path,
                    self.cursor + 1,
                    self.units.len()
                ),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        match &file.body {
            Body::Hunks(segs) => out.extend(hunk_lines(segs, u, choice, pick)),
            Body::Whole { ours, theirs, .. } => out.extend(whole_lines(ours, theirs, choice, pick)),
        }
        out
    }
}

// Replace anything a terminal would interpret. Same policy as the diff pane.
fn clean(raw: &str) -> String {
    raw.chars()
        .flat_map(|c| match c {
            '\t' => vec![' '; 4],
            c if c.is_control() => vec!['\u{b7}'],
            c => vec![c],
        })
        .collect()
}

fn side_header(label: &str, chosen: bool, color: Color) -> Line<'static> {
    let mut style = Style::new().fg(color).add_modifier(Modifier::BOLD);
    let mut text = format!("---- {label} ----");
    if chosen {
        style = style.add_modifier(Modifier::REVERSED);
        text.push_str("  <- chosen");
    }
    Line::from(Span::styled(text, style))
}

fn body_lines(bytes: &[u8], color: Color, chosen: bool) -> Vec<Line<'static>> {
    let mut style = Style::new().fg(color);
    if !chosen {
        style = style.add_modifier(Modifier::DIM);
    }
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| Line::from(Span::styled(clean(l), style)))
        .collect()
}

// The `unit`-th conflict hunk of a file with a few lines of surrounding text, both sides laid out
// one above the other.
fn hunk_lines(
    segs: &[Segment],
    unit: usize,
    choice: Option<Side>,
    pick: &str,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut seen = 0;
    for (i, seg) in segs.iter().enumerate() {
        let Segment::Hunk(h) = seg else { continue };
        if seen != unit {
            seen += 1;
            continue;
        }
        let dim = Style::new().fg(Color::DarkGray);
        if let Some(Segment::Text(t)) = i.checked_sub(1).and_then(|p| segs.get(p)) {
            let lines: Vec<String> = String::from_utf8_lossy(t)
                .lines()
                .map(str::to_string)
                .collect();
            let start = lines.len().saturating_sub(CONTEXT);
            for l in &lines[start..] {
                out.push(Line::from(Span::styled(format!("  {}", clean(l)), dim)));
            }
        }
        let a = matches!(choice, Some(Side::A | Side::Both));
        let b = matches!(choice, Some(Side::B | Side::Both));
        out.push(side_header(
            "A  tree copy (checked-out branch)",
            a,
            Color::Cyan,
        ));
        out.extend(body_lines(&h.ours, Color::Cyan, choice.is_none() || a));
        out.push(side_header(
            &format!("B  from picked commit {pick}"),
            b,
            Color::Magenta,
        ));
        out.extend(body_lines(&h.theirs, Color::Magenta, choice.is_none() || b));
        out.push(Line::from(Span::styled("---- end ----", dim)));
        if let Some(Segment::Text(t)) = segs.get(i + 1) {
            for l in String::from_utf8_lossy(t).lines().take(CONTEXT) {
                out.push(Line::from(Span::styled(format!("  {}", clean(l)), dim)));
            }
        }
        return out;
    }
    out
}

// The decision text for a file that is settled as a whole.
fn whole_lines(
    ours: &Option<crate::patch::Blob>,
    theirs: &Option<crate::patch::Blob>,
    choice: Option<Side>,
    pick: &str,
) -> Vec<Line<'static>> {
    let (what, a_text, b_text) = match (ours, theirs) {
        (Some(_), None) => (
            "The tree has this file; the picked commit deletes it.",
            "keep the tree's file".to_string(),
            "delete the file, as the picked commit does".to_string(),
        ),
        (None, Some(_)) => (
            "The tree lacks this file; the picked commit changes it.",
            "leave the file deleted".to_string(),
            "take the picked commit's file".to_string(),
        ),
        _ => (
            "Both sides changed this file and its content cannot be merged line by line.",
            "keep the tree's copy".to_string(),
            "take the picked commit's copy".to_string(),
        ),
    };
    let a = choice == Some(Side::A);
    let b = choice == Some(Side::B);
    vec![
        Line::from(what),
        Line::from(""),
        side_header(&format!("A  {a_text}"), a, Color::Cyan),
        side_header(&format!("B  {b_text} (commit {pick})"), b, Color::Magenta),
    ]
}

pub struct App {
    source_label: Option<String>,
    entries: Vec<Entry>,
    truncated: bool,
    cursor: usize,
    focus: Focus,
    mode: Mode,
    views: HashMap<String, DiffView>,
    vscroll: usize,
    hscroll: usize,
    view_h: usize,
    view_w: usize,
    last_h: Option<Instant>,
    // When the selection last moved to its current commit; the diff waits for `DWELL` after it.
    moved: Option<Instant>,
    // Set by `set_source` until the selection has been moved off a leading commit that changes
    // nothing.
    unsettled: bool,
    status: String,
    status_is_error: bool,
    list_state: ListState,
    overlay: Option<Overlay>,
    resolver: Option<Resolver>,
    // Files restored whole after a decision-screen `X`, named in the confirmation prompt.
    restored: Vec<String>,
    // The commit the decision screen is about.
    replaying: String,
}

impl App {
    pub fn new() -> App {
        App {
            source_label: None,
            entries: Vec::new(),
            truncated: false,
            cursor: 0,
            focus: Focus::List,
            mode: Mode::Browse,
            views: HashMap::new(),
            vscroll: 0,
            hscroll: 0,
            view_h: 20,
            view_w: 80,
            last_h: None,
            moved: None,
            unsettled: false,
            status: String::new(),
            status_is_error: false,
            list_state: ListState::default(),
            overlay: None,
            resolver: None,
            restored: Vec::new(),
            replaying: String::new(),
        }
    }

    // Use the commits of `branch` (newest first). Marks, cached previews and scroll positions
    // belong to the previous source and are dropped.
    pub fn set_source(&mut self, branch: &str, commits: Vec<(String, String)>) {
        self.truncated = commits.len() >= cherry::MAX_CANDIDATES;
        self.entries = commits
            .into_iter()
            .map(|(full, subject)| Entry {
                full,
                subject,
                marked: false,
                only: None,
                applies: None,
            })
            .collect();
        self.source_label = Some(branch.to_string());
        self.views.clear();
        self.cursor = 0;
        self.vscroll = 0;
        self.hscroll = 0;
        self.focus = Focus::List;
        self.moved = None;
        self.unsettled = true;
    }

    // Open the branch overlay on `names`.
    pub fn open_overlay(&mut self, names: Vec<String>) {
        self.overlay = Some(Overlay {
            kind: OverlayKind::Branch,
            names,
            cursor: 0,
        });
        self.mode = Mode::Overlay;
    }

    fn marked_count(&self) -> usize {
        self.entries.iter().filter(|e| e.marked).count()
    }

    // The marked commits, oldest first, which is the order they are replayed in.
    fn marked_picks(&self) -> Vec<Sel> {
        self.entries
            .iter()
            .rev()
            .filter(|e| e.marked)
            .map(|e| Sel {
                id: e.full.clone(),
                subject: e.subject.clone(),
                only: e.only.clone(),
            })
            .collect()
    }

    // Preview cache key: a commit previews differently when it is restricted to one file.
    fn view_key(e: &Entry) -> String {
        format!("{}\0{}", e.full, e.only.as_deref().unwrap_or(""))
    }

    // Work out whether entry `i` changes anything; an error is treated as "does", so the commit
    // stays reachable and its preview reports the problem.
    fn classify(&mut self, i: usize, src: &mut dyn Source) {
        let Some(entry) = self.entries.get(i) else {
            return;
        };
        if entry.applies.is_none() {
            let known = src.applies(&entry.full).unwrap_or(true);
            self.entries[i].applies = Some(known);
        }
    }

    fn inert(&self, i: usize) -> bool {
        self.entries
            .get(i)
            .is_some_and(|e| e.applies == Some(false))
    }

    // Move the selection off a leading commit that changes nothing, once, after a source is chosen.
    fn settle(&mut self, src: &mut dyn Source) {
        if !self.unsettled {
            return;
        }
        self.unsettled = false;
        self.land(self.cursor, 1, Instant::now(), src);
    }

    // Select the first commit at or beyond `from` in direction `dir` that changes something. When
    // there is none the selection stays where it is.
    fn land(&mut self, from: usize, dir: isize, now: Instant, src: &mut dyn Source) -> bool {
        let mut i = from as isize;
        while i >= 0 && (i as usize) < self.entries.len() {
            self.classify(i as usize, src);
            if !self.inert(i as usize) {
                self.jump(i as usize, now);
                return true;
            }
            i += dir;
        }
        false
    }

    // How long the event loop may sleep before it has something to do: the rest of the dwell before
    // a diff is loaded, or nothing at all while commits near the selection await classification.
    pub fn wakeup(&self, now: Instant) -> Option<Duration> {
        if let (Some(entry), Some(t)) = (self.entries.get(self.cursor), self.moved) {
            let waited = now.saturating_duration_since(t);
            if !self.views.contains_key(&Self::view_key(entry)) && waited < DWELL {
                return Some(DWELL - waited);
            }
        }
        self.next_unclassified().map(|_| Duration::ZERO)
    }

    // The nearest unclassified commit within `IDLE_REACH` of the selection, below it first.
    fn next_unclassified(&self) -> Option<usize> {
        let end = (self.cursor + IDLE_REACH).min(self.entries.len());
        let below = (self.cursor..end).find(|&i| self.entries[i].applies.is_none());
        below.or_else(|| {
            let start = self.cursor.saturating_sub(IDLE_REACH);
            (start..self.cursor)
                .rev()
                .find(|&i| self.entries[i].applies.is_none())
        })
    }

    // One unit of idle work: classify a commit near the selection.
    pub fn tick(&mut self, src: &mut dyn Source, now: Instant) {
        if self.wakeup(now).is_some_and(|d| d > Duration::ZERO) {
            return;
        }
        if let Some(i) = self.next_unclassified() {
            self.classify(i, src);
        }
    }

    // Load the highlighted commit's diff once the selection has rested on it for `DWELL`; a diff
    // already loaded is shown at once.
    pub fn ensure_diff(&mut self, src: &mut dyn Source, now: Instant) {
        self.settle(src);
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        let key = Self::view_key(entry);
        if self.views.contains_key(&key) {
            return;
        }
        if self
            .moved
            .is_some_and(|t| now.saturating_duration_since(t) < DWELL)
        {
            return;
        }
        let (full, only) = (entry.full.clone(), entry.only.clone());
        let view = match src.diff(&full, only.as_deref()) {
            Ok(text) => style_diff(&text),
            Err(e) => style_diff(&format!("could not show {full}: {e}")),
        };
        self.views.insert(key, view);
    }

    fn view(&self) -> Option<&DiffView> {
        self.entries
            .get(self.cursor)
            .and_then(|e| self.views.get(&Self::view_key(e)))
    }

    fn max_v(&self) -> usize {
        self.view()
            .map_or(0, |v| v.lines.len().saturating_sub(self.view_h))
    }

    fn max_h(&self) -> usize {
        self.view()
            .map_or(0, |v| v.max_width.saturating_sub(self.view_w))
    }

    fn clamp_scroll(&mut self) {
        self.vscroll = self.vscroll.min(self.max_v());
        self.hscroll = self.hscroll.min(self.max_h());
    }

    // Move to the next commit in the direction of `delta`, passing over those that change nothing
    // on HEAD.
    fn step(&mut self, delta: isize, now: Instant, src: &mut dyn Source) {
        let dir = delta.signum();
        if self.entries.is_empty() || dir == 0 {
            return;
        }
        let mut skipped = false;
        let mut i = self.cursor as isize;
        loop {
            i += dir;
            if i < 0 || i as usize >= self.entries.len() {
                if skipped {
                    self.say("no further commit changes anything on HEAD", false);
                }
                return;
            }
            self.classify(i as usize, src);
            if !self.inert(i as usize) {
                self.jump(i as usize, now);
                return;
            }
            skipped = true;
        }
    }

    fn jump(&mut self, index: usize, now: Instant) {
        if index != self.cursor {
            self.cursor = index;
            self.vscroll = 0;
            self.hscroll = 0;
            self.moved = Some(now);
        }
    }

    // Space: mark the highlighted commit whole, or clear its mark (whatever kind it was).
    fn toggle(&mut self) {
        if let Some(e) = self.entries.get_mut(self.cursor) {
            e.marked = !e.marked;
            e.only = None;
        }
    }

    // R: follow one file's history. The file is the one the highlighted commit changes; when it
    // changes several, an overlay asks which. See `mark_chain`.
    fn mark_file_chain(&mut self, src: &mut dyn Source) {
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        let commit = entry.full.clone();
        match src.paths(&commit) {
            Err(e) => self.say(e, true),
            Ok(paths) if paths.is_empty() => self.say("this commit changes no file", true),
            Ok(paths) if paths.len() == 1 => self.mark_chain(&paths[0], src),
            Ok(paths) => {
                self.overlay = Some(Overlay {
                    kind: OverlayKind::File,
                    names: paths,
                    cursor: 0,
                });
                self.mode = Mode::Overlay;
            }
        }
    }

    // Mark the highlighted commit and every older commit of the branch that changes `path`, each
    // restricted to that file. Replayed oldest first they bring the file to its state on the
    // source branch at the highlighted commit; a commit that also changes other files contributes
    // only its part for `path`. Commits above the cursor keep their marks. When that whole chain
    // is already marked for `path`, the same call clears it.
    fn mark_chain(&mut self, path: &str, src: &mut dyn Source) {
        let Some(branch) = self.source_label.clone() else {
            return;
        };
        let touching = match src.touching(&branch, path) {
            Ok(t) => t,
            Err(e) => {
                self.say(e, true);
                return;
            }
        };
        let cursor = self.cursor;
        let chain: Vec<usize> = (cursor..self.entries.len())
            .filter(|&i| i == cursor || touching.contains(&self.entries[i].full))
            .collect();
        let all = chain
            .iter()
            .all(|&i| self.entries[i].marked && self.entries[i].only.as_deref() == Some(path));
        for &i in &chain {
            self.entries[i].marked = !all;
            self.entries[i].only = if all { None } else { Some(path.to_string()) };
        }
        let text = if all {
            format!("unmarked {} commit(s) for {path}", chain.len())
        } else {
            let partial = chain
                .iter()
                .filter(|&&i| src.paths(&self.entries[i].full).is_ok_and(|p| p.len() > 1))
                .count();
            let extra = if partial > 0 {
                format!("; {partial} also change other files and are applied for it only")
            } else {
                String::new()
            };
            format!("marked {} commit(s) for {path}{extra}", chain.len())
        };
        self.say(text, false);
    }

    fn say(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = text.into();
        self.status_is_error = is_error;
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant, src: &mut dyn Source) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Quit;
        }
        match self.mode.clone() {
            Mode::Overlay => {
                self.key_overlay(key, src);
                return Outcome::Continue;
            }
            Mode::Resolve => {
                self.key_resolve(key, src);
                return Outcome::Continue;
            }
            Mode::ConfirmApply { .. } => return self.key_confirm_apply(key, src),
            Mode::ConfirmQuit => return self.key_confirm_quit(key),
            Mode::Browse => {}
        }
        self.status.clear();
        match self.focus {
            Focus::List => self.key_list(key, now, src),
            Focus::Diff => self.key_diff(key, now, src),
        }
    }

    fn open_branches(&mut self, src: &mut dyn Source) {
        match src.branches() {
            Ok(names) if names.is_empty() => {
                self.say("there is no other branch to pick from", true)
            }
            Ok(names) => self.open_overlay(names),
            Err(e) => self.say(e, true),
        }
    }

    fn key_list(&mut self, key: KeyEvent, now: Instant, src: &mut dyn Source) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1, now, src),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1, now, src),
            KeyCode::Char('g') | KeyCode::Home => {
                self.land(0, 1, now, src);
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.land(self.entries.len().saturating_sub(1), -1, now, src);
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('R') => self.mark_file_chain(src),
            KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Diff,
            KeyCode::Tab => self.open_branches(src),
            KeyCode::Enter => self.submit(src),
            KeyCode::Char('q') | KeyCode::Esc => return self.request_quit(),
            _ => {}
        }
        Outcome::Continue
    }

    fn key_diff(&mut self, key: KeyEvent, now: Instant, src: &mut dyn Source) -> Outcome {
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
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = (self.hscroll + H_STEP).min(self.max_h())
            }
            KeyCode::Char('h') | KeyCode::Left => self.left(now),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.step(-1, now, src)
            }
            KeyCode::Enter | KeyCode::Char('n') => self.step(1, now, src),
            KeyCode::Char('N') => self.step(-1, now, src),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('R') => self.mark_file_chain(src),
            KeyCode::Tab => self.open_branches(src),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Char('q') => return self.request_quit(),
            _ => {}
        }
        Outcome::Continue
    }

    // Same rule as the `drop` picker: an h that follows another within RUN_WINDOW is part of one
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

    fn key_overlay(&mut self, key: KeyEvent, src: &mut dyn Source) {
        let Some(ov) = self.overlay.as_mut() else {
            self.mode = Mode::Browse;
            return;
        };
        let last = ov.names.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => ov.cursor = (ov.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => ov.cursor = ov.cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => ov.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => ov.cursor = last,
            KeyCode::Enter if ov.kind == OverlayKind::File => {
                let path = ov.names[ov.cursor].clone();
                self.overlay = None;
                self.mode = Mode::Browse;
                self.mark_chain(&path, src);
            }
            KeyCode::Enter => {
                let name = ov.names[ov.cursor].clone();
                match src.commits(&name) {
                    Ok(commits) if commits.is_empty() => {
                        self.say(format!("{name} has no commit that HEAD lacks"), true);
                    }
                    Ok(commits) => {
                        self.set_source(&name, commits);
                        self.overlay = None;
                        self.mode = Mode::Browse;
                        self.status.clear();
                    }
                    Err(e) => self.say(e, true),
                }
            }
            KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => {
                self.overlay = None;
                self.mode = Mode::Browse;
            }
            _ => {}
        }
    }

    // Enter in the browse screen: replay the marked commits and report where that stops.
    fn submit(&mut self, src: &mut dyn Source) {
        let picks = self.marked_picks();
        if picks.is_empty() {
            self.say("nothing is marked; press space on a commit first", true);
            return;
        }
        self.restored.clear();
        let flow = src.start(&picks);
        self.after_flow(flow, &*src);
    }

    fn after_flow(&mut self, flow: Result<Flow, String>, src: &dyn Source) {
        self.replaying = src.replaying().unwrap_or_default();
        match flow {
            Ok(Flow::Ready(summary)) => {
                self.resolver = None;
                self.mode = Mode::ConfirmApply { summary };
            }
            Ok(Flow::Conflicts(files)) => {
                self.resolver = Some(Resolver::new(files));
                self.mode = Mode::Resolve;
            }
            Ok(Flow::Refused(reason)) => {
                self.resolver = None;
                self.mode = Mode::Browse;
                self.say(reason, true);
            }
            Err(e) => {
                self.mode = Mode::Browse;
                self.say(e, true);
            }
        }
    }

    fn key_resolve(&mut self, key: KeyEvent, src: &mut dyn Source) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(res) = self.resolver.as_mut() else {
            self.mode = Mode::Browse;
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => res.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => res.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => res.move_by(-(res.units.len() as isize)),
            KeyCode::Char('G') | KeyCode::End => res.move_by(res.units.len() as isize),
            KeyCode::Char('d') if ctrl => res.vscroll += self.view_h / 2,
            KeyCode::Char('u') if ctrl => res.vscroll = res.vscroll.saturating_sub(self.view_h / 2),
            KeyCode::Char('a') => {
                res.decide(Some(Side::A));
            }
            KeyCode::Char('b') => {
                res.decide(Some(Side::B));
            }
            KeyCode::Char('c') => {
                if !res.decide(Some(Side::Both)) {
                    self.status = "keeping both is offered for text conflicts only".to_string();
                    self.status_is_error = true;
                }
            }
            KeyCode::Char('u') => {
                res.decide(None);
            }
            KeyCode::Char('X') => {
                let Some(&(f, _)) = res.units.get(res.cursor) else {
                    return;
                };
                let path = res.files[f].path.clone();
                match src.restore(&path) {
                    Ok(flow) => {
                        self.restored.push(path);
                        self.resolver = None;
                        self.after_flow(Ok(flow), &*src);
                    }
                    Err(e) => self.say(e, true),
                }
            }
            KeyCode::Enter => {
                let left = res.undecided();
                if left > 0 {
                    self.say(format!("{left} conflict(s) still need a decision"), true);
                    return;
                }
                let files = std::mem::take(&mut res.files);
                self.resolver = None;
                let flow = src.resolve(files);
                self.after_flow(flow, &*src);
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.resolver = None;
                self.mode = Mode::Browse;
                self.say("decisions abandoned; selection kept", false);
            }
            _ => {}
        }
    }

    // Only y and n answer. Every other key, Enter included, is ignored.
    fn key_confirm_apply(&mut self, key: KeyEvent, src: &mut dyn Source) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => match src.spec() {
                Ok(spec) => Outcome::Submit(spec),
                Err(e) => {
                    self.mode = Mode::Browse;
                    self.say(e, true);
                    Outcome::Continue
                }
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.say("cancelled; selection kept", false);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn request_quit(&mut self) -> Outcome {
        if self.marked_count() == 0 {
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

        if self.mode == Mode::Resolve {
            self.render_resolve(frame, cols[0], cols[1]);
        } else {
            self.render_browse(frame, cols[0], cols[1]);
        }
        self.render_bar(frame, rows[1]);
        if self.mode == Mode::Overlay {
            self.render_overlay(frame, area);
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

        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                let subject = if e.subject.is_empty() {
                    "(no message)"
                } else {
                    &e.subject
                };
                let mark = match (e.marked, &e.only) {
                    (false, _) => ' ',
                    (true, None) => 'x',
                    (true, Some(_)) => 'f',
                };
                let text = format!("[{mark}] {} {subject}", &e.full[..e.full.len().min(8)]);
                let style = if e.marked && e.only.is_some() {
                    Style::new().fg(Color::Cyan)
                } else if e.marked {
                    Style::new().fg(Color::Green)
                } else if e.applies == Some(false) {
                    Style::new().fg(Color::DarkGray)
                } else {
                    Style::new()
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect();
        let cursor_style = if self.focus == Focus::List {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        };
        let title = match &self.source_label {
            Some(name) => format!(
                " {name}  {} of {} marked{} ",
                self.marked_count(),
                self.entries.len(),
                if self.truncated {
                    "  (list capped)"
                } else {
                    ""
                }
            ),
            None => " Commits  (Tab: choose a branch) ".to_string(),
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(focus_style(Focus::List)),
            )
            .highlight_style(cursor_style);
        self.list_state.select(Some(self.cursor));
        frame.render_stateful_widget(list, left, &mut self.list_state);

        let inner_h = right.height.saturating_sub(2) as usize;
        let inner_w = right.width.saturating_sub(2) as usize;
        self.view_h = inner_h.max(1);
        self.view_w = inner_w.max(1);
        self.clamp_scroll();
        let (visible, total): (Vec<Line>, usize) = match self.view() {
            Some(v) => (
                v.lines
                    .iter()
                    .skip(self.vscroll)
                    .take(inner_h)
                    .cloned()
                    .collect(),
                v.lines.len(),
            ),
            None if !self.entries.is_empty() => (vec![Line::from("(loading the diff...)")], 0),
            None => (
                vec![Line::from(
                    "Press Tab to choose the branch to pick commits from.",
                )],
                0,
            ),
        };
        let heading = match self.entries.get(self.cursor) {
            Some(e) => format!(
                " {}{}  line {}/{}  col {} ",
                &e.full[..e.full.len().min(8)],
                e.only
                    .as_ref()
                    .map_or(String::new(), |p| format!("  only: {p}")),
                (self.vscroll + 1).min(total.max(1)),
                total,
                self.hscroll + 1
            ),
            None => " no commit ".to_string(),
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

    fn render_resolve(&mut self, frame: &mut Frame, left: Rect, right: Rect) {
        let Some(res) = self.resolver.as_mut() else {
            return;
        };
        let items: Vec<ListItem> = res.rows().into_iter().map(ListItem::new).collect();
        let title = format!(
            " Conflicts  {} of {} decided ",
            res.units.len() - res.undecided(),
            res.units.len()
        );
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::Cyan)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(res.cursor));
        frame.render_stateful_widget(list, left, &mut state);

        let pick = self.replaying[..self.replaying.len().min(8)].to_string();
        let lines = res.detail(&pick);
        let inner_h = right.height.saturating_sub(2) as usize;
        self.view_h = inner_h.max(1);
        res.vscroll = res.vscroll.min(lines.len().saturating_sub(self.view_h));
        let visible: Vec<Line> = lines.into_iter().skip(res.vscroll).take(inner_h).collect();
        let detail = Paragraph::new(visible).block(
            Block::bordered()
                .title(" Decide: a = tree copy, b = picked commit ")
                .border_style(Style::new().fg(Color::Cyan)),
        );
        frame.render_widget(detail, right);
    }

    fn render_overlay(&mut self, frame: &mut Frame, area: Rect) {
        let Some(ov) = self.overlay.as_ref() else {
            return;
        };
        let w = (area.width * 3 / 5).clamp(30.min(area.width), area.width);
        let h = (area.height * 3 / 5).clamp(6.min(area.height), area.height);
        let popup = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + (area.height - h) / 2,
            width: w,
            height: h,
        };
        let items: Vec<ListItem> = ov.names.iter().map(|n| ListItem::new(n.as_str())).collect();
        let title = match ov.kind {
            OverlayKind::Branch => " Pick from which branch?  (Enter use, Esc close) ",
            OverlayKind::File => " Follow which file?  (Enter use, Esc close) ",
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::Yellow)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(ov.cursor));
        frame.render_widget(Clear, popup);
        frame.render_stateful_widget(list, popup, &mut state);
    }

    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        let prompt = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let bar = match &self.mode {
            Mode::ConfirmApply { summary } => {
                let last = summary.lines().last().unwrap_or("").trim();
                let whole = if self.restored.is_empty() {
                    String::new()
                } else {
                    format!(" (restored whole: {})", self.restored.join(", "))
                };
                Line::from(Span::styled(
                    format!(
                        " Apply {} commit(s) as a patch? {last}{whole} [y/n] ",
                        self.marked_count()
                    ),
                    prompt,
                ))
            }
            Mode::ConfirmQuit => Line::from(Span::styled(
                format!(" Quit and discard {} mark(s)? [y/n] ", self.marked_count()),
                prompt,
            )),
            _ if !self.status.is_empty() => {
                let color = if self.status_is_error {
                    Color::Red
                } else {
                    Color::Green
                };
                Line::from(Span::styled(
                    format!(" {} ", self.status),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ))
            }
            Mode::Overlay => hint(" j/k move  Enter use  Esc close "),
            Mode::Resolve => hint(
                " j/k conflict  a tree  b picked  c both  u undo  X restore file  Enter go  Esc leave ",
            ),
            Mode::Browse => match self.focus {
                Focus::List => hint(
                    " j/k move  space mark  R file history  l diff  Tab branch  Enter pick  q quit",
                ),
                Focus::Diff => {
                    hint(" j/k h/l scroll  Enter/n next  N prev  space mark  Tab branch  q quit")
                }
            },
        };
        frame.render_widget(Paragraph::new(bar), area);
    }
}

fn hint(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

// Run the screen on the real terminal. Returns the recipe of the patch the operator confirmed, or
// None when the screen was left without confirming. `from` names the branch to list at once;
// without it the branch overlay opens first.
pub fn run(cwd: &Path, from: Option<&str>) -> Res<Option<Spec>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "cherry-pick: no commit given, and the interactive screen needs a terminal".into(),
        );
    }
    let root = git::work_tree(cwd)?;
    let base = git::rev_parse(&root, "HEAD")?;
    let mut src = GitSource {
        cwd: cwd.to_path_buf(),
        root,
        base,
        job: None,
        picks: Vec::new(),
        restores: Vec::new(),
        decided: Vec::new(),
    };
    let mut app = App::new();
    match from {
        Some(branch) => {
            let commits = src.commits(branch)?;
            if commits.is_empty() {
                return Err(
                    format!("cherry-pick: '{branch}' has no commit that HEAD lacks").into(),
                );
            }
            app.set_source(branch, commits);
        }
        None => {
            let names = src.branches()?;
            if names.is_empty() {
                return Err("cherry-pick: there is no other branch to pick from".into());
            }
            app.open_overlay(names);
        }
    }

    let result = with_terminal(|terminal| event_loop(terminal, &mut app, &mut src));
    match result? {
        Outcome::Submit(spec) => Ok(Some(spec)),
        _ => Ok(None),
    }
}

fn event_loop(term: &mut DefaultTerminal, app: &mut App, src: &mut dyn Source) -> Res<Outcome> {
    loop {
        app.ensure_diff(src, Instant::now());
        term.draw(|f| app.render(f))?;
        // Sleep until a key arrives, the dwell before a diff ends, or idle work is due.
        if let Some(wait) = app.wakeup(Instant::now()) {
            if !event::poll(wait)? {
                app.tick(src, Instant::now());
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
    use crate::conflict;
    use crate::patch::Blob;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::VecDeque;

    struct Fake {
        branches: Vec<String>,
        commits: Vec<(String, String)>,
        flows: VecDeque<Result<Flow, String>>,
        started: Vec<Vec<Sel>>,
        resolved: Vec<Vec<FileConflict>>,
        // Paths changed by a commit; a commit not listed changes only a.txt.
        paths: HashMap<String, Vec<String>>,
        // Commits that `touching` reports.
        touching: Vec<String>,
        // (commit, only) of every preview requested.
        diffs: Vec<(String, Option<String>)>,
        restored: Vec<String>,
        // Commits that would change nothing on HEAD.
        inert: Vec<String>,
        // Every commit asked about, in order.
        asked: Vec<String>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                branches: vec!["feat".to_string(), "origin/dev".to_string()],
                commits: vec![
                    (id('a'), "newest".to_string()),
                    (id('b'), String::new()),
                    (id('c'), "oldest".to_string()),
                ],
                flows: VecDeque::new(),
                started: Vec::new(),
                resolved: Vec::new(),
                paths: HashMap::new(),
                touching: vec![id('a'), id('c')],
                diffs: Vec::new(),
                restored: Vec::new(),
                inert: Vec::new(),
                asked: Vec::new(),
            }
        }
    }

    impl Source for Fake {
        fn branches(&mut self) -> Result<Vec<String>, String> {
            Ok(self.branches.clone())
        }

        fn commits(&mut self, branch: &str) -> Result<Vec<(String, String)>, String> {
            if branch == "empty" {
                Ok(Vec::new())
            } else {
                Ok(self.commits.clone())
            }
        }

        fn diff(&mut self, commit: &str, only: Option<&str>) -> Result<String, String> {
            self.diffs
                .push((commit.to_string(), only.map(str::to_string)));
            Ok(format!(
                "commit {commit}\nAuthor: T <t@e.invalid>\nDate:   now\n\n    msg\n\n \
                 a.txt | 1 +\n\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
                 @@ -1 +1 @@\n-old\n+new\n"
            ))
        }

        fn applies(&mut self, commit: &str) -> Result<bool, String> {
            self.asked.push(commit.to_string());
            Ok(!self.inert.iter().any(|c| c == commit))
        }

        fn paths(&mut self, commit: &str) -> Result<Vec<String>, String> {
            Ok(self
                .paths
                .get(commit)
                .cloned()
                .unwrap_or_else(|| vec!["a.txt".to_string()]))
        }

        fn touching(&mut self, _branch: &str, _path: &str) -> Result<Vec<String>, String> {
            Ok(self.touching.clone())
        }

        fn restore(&mut self, path: &str) -> Result<Flow, String> {
            let scoped = self
                .started
                .last()
                .is_some_and(|p| p.iter().any(|s| s.only.as_deref() == Some(path)));
            if !scoped {
                return Err("whole-file restore is offered for a selection made with R".into());
            }
            self.restored.push(path.to_string());
            self.flows
                .pop_front()
                .unwrap_or(Ok(Flow::Ready("1 file changed".into())))
        }

        fn start(&mut self, picks: &[Sel]) -> Result<Flow, String> {
            self.started.push(picks.to_vec());
            self.flows
                .pop_front()
                .unwrap_or(Ok(Flow::Ready("1 file changed".into())))
        }

        fn resolve(&mut self, files: Vec<FileConflict>) -> Result<Flow, String> {
            self.resolved.push(files);
            self.flows
                .pop_front()
                .unwrap_or(Ok(Flow::Ready("1 file changed".into())))
        }

        fn replaying(&self) -> Option<String> {
            self.started
                .last()
                .and_then(|p| p.first())
                .map(|s| s.id.clone())
        }

        fn spec(&mut self) -> Result<Spec, String> {
            Ok(Spec {
                base: id('0'),
                picks: self
                    .started
                    .last()
                    .map(|p| {
                        p.iter()
                            .map(|s| (s.id.clone(), s.subject.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                decisions: Vec::new(),
                ..Spec::default()
            })
        }
    }

    fn id(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyCode {
        KeyCode::Char(c)
    }

    fn press(app: &mut App, fake: &mut Fake, code: KeyCode) -> Outcome {
        let now = Instant::now();
        let out = app.handle_key(key(code), now, fake);
        // Long enough after the key for the dwell to have passed.
        app.ensure_diff(fake, now + DWELL);
        out
    }

    // An app already showing the fake's commits.
    fn app() -> (App, Fake) {
        let mut fake = Fake::new();
        let mut app = App::new();
        app.set_source("feat", fake.commits.clone());
        app.view_h = 10;
        app.view_w = 40;
        app.ensure_diff(&mut fake, Instant::now());
        (app, fake)
    }

    fn text_conflict(path: &str, src: &str) -> FileConflict {
        FileConflict {
            path: path.to_string(),
            mode: "100644".to_string(),
            body: Body::Hunks(conflict::parse(src.as_bytes()).unwrap()),
        }
    }

    fn whole_conflict(path: &str) -> FileConflict {
        let blob = |o: &str| {
            Some(Blob {
                mode: "100644".to_string(),
                oid: o.repeat(40),
            })
        };
        FileConflict {
            path: path.to_string(),
            mode: "100644".to_string(),
            body: Body::Whole {
                ours: blob("1"),
                theirs: None,
                choice: None,
            },
        }
    }

    const TWO_HUNKS: &str = "a\n<<<<<<< HEAD\nA1\n=======\nB1\n>>>>>>> x\nmid\n\
         <<<<<<< HEAD\nA2\n=======\nB2\n>>>>>>> x\n";

    // Mark the first commit and press Enter with the given flows queued.
    fn submit_with(flows: Vec<Result<Flow, String>>) -> (App, Fake) {
        let (mut app, mut fake) = app();
        fake.flows = flows.into();
        press(&mut app, &mut fake, ch(' '));
        press(&mut app, &mut fake, KeyCode::Enter);
        (app, fake)
    }

    #[test]
    fn tab_opens_the_overlay_and_enter_switches_source_clearing_marks() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        assert_eq!(a.marked_count(), 1);

        press(&mut a, &mut f, KeyCode::Tab);
        assert_eq!(a.mode, Mode::Overlay);
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, ch('j'));
        assert_eq!(
            a.overlay.as_ref().unwrap().cursor,
            1,
            "stops at the last branch"
        );
        press(&mut a, &mut f, KeyCode::Enter);

        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.source_label.as_deref(), Some("origin/dev"));
        assert_eq!(a.marked_count(), 0);
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn escape_closes_the_overlay_without_changing_anything() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, KeyCode::Tab);
        press(&mut a, &mut f, KeyCode::Esc);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.source_label.as_deref(), Some("feat"));
        assert_eq!(a.marked_count(), 1);
    }

    #[test]
    fn tab_works_from_the_diff_pane_and_a_branch_without_commits_is_refused() {
        let (mut a, mut f) = app();
        f.branches = vec!["empty".to_string()];
        press(&mut a, &mut f, ch('l'));
        press(&mut a, &mut f, KeyCode::Tab);
        assert_eq!(a.mode, Mode::Overlay);
        press(&mut a, &mut f, KeyCode::Enter);
        assert_eq!(a.mode, Mode::Overlay, "stays open");
        assert!(a.status_is_error);
        assert_eq!(a.source_label.as_deref(), Some("feat"));
    }

    #[test]
    fn with_no_other_branch_tab_says_so() {
        let (mut a, mut f) = app();
        f.branches.clear();
        press(&mut a, &mut f, KeyCode::Tab);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("no other branch"));
    }

    #[test]
    fn enter_with_nothing_marked_explains_and_starts_nothing() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, KeyCode::Enter);
        assert!(a.status.contains("nothing is marked"));
        assert!(f.started.is_empty());
    }

    #[test]
    fn marked_commits_are_submitted_oldest_first() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, ch('G'));
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, KeyCode::Enter);
        let picks: Vec<String> = f.started[0].iter().map(|s| s.id.clone()).collect();
        assert_eq!(picks, vec![id('c'), id('a')]);
    }

    #[test]
    fn r_marks_the_file_chain_older_only_restricted_and_leaves_newer_marks_alone() {
        // Commit b does not touch the file; a (newer) and c (older) do. The cursor is on c.
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch('G'));
        press(&mut a, &mut f, ch('R'));
        let marks: Vec<(bool, Option<&str>)> = a
            .entries
            .iter()
            .map(|e| (e.marked, e.only.as_deref()))
            .collect();
        assert_eq!(marks, [(false, None), (false, None), (true, Some("a.txt"))]);
        assert!(
            a.status.contains("marked 1 commit(s) for a.txt"),
            "{}",
            a.status
        );

        // From the newest commit the chain is a and c; b does not touch the file and is skipped.
        press(&mut a, &mut f, ch('g'));
        press(&mut a, &mut f, ch('R'));
        let marked: Vec<bool> = a.entries.iter().map(|e| e.marked).collect();
        assert_eq!(marked, [true, false, true]);
        press(&mut a, &mut f, KeyCode::Enter);
        let picks: Vec<(String, Option<String>)> = f.started[0]
            .iter()
            .map(|s| (s.id.clone(), s.only.clone()))
            .collect();
        assert_eq!(
            picks,
            vec![
                (id('c'), Some("a.txt".to_string())),
                (id('a'), Some("a.txt".to_string()))
            ]
        );
    }

    #[test]
    fn r_keeps_a_whole_mark_above_the_cursor_and_a_second_r_clears_the_chain() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, ch('l'));
        press(&mut a, &mut f, ch('R'));
        assert_eq!(a.focus, Focus::Diff, "R works from the diff pane");
        assert_eq!(a.marked_count(), 2);
        press(&mut a, &mut f, ch('R'));
        assert_eq!(
            a.marked_count(),
            1,
            "the chain is cleared, the newer mark stays"
        );
        assert!(a.status.contains("unmarked 1"), "{}", a.status);
        assert_eq!(a.entries[0].only, None);
    }

    #[test]
    fn a_commit_with_several_files_asks_which_one_to_follow() {
        let (mut a, mut f) = app();
        f.paths
            .insert(id('a'), vec!["x.txt".into(), "y.txt".into()]);
        press(&mut a, &mut f, ch('R'));
        assert_eq!(a.mode, Mode::Overlay);
        assert_eq!(a.overlay.as_ref().unwrap().kind, OverlayKind::File);
        assert_eq!(a.marked_count(), 0);
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, KeyCode::Enter);
        assert_eq!(a.mode, Mode::Browse);
        assert_eq!(a.entries[0].only.as_deref(), Some("y.txt"));
        assert!(a.status.contains("y.txt"), "{}", a.status);
        // The overlay can be dismissed without marking anything.
        let (mut b, mut g) = app();
        g.paths
            .insert(id('a'), vec!["x.txt".into(), "y.txt".into()]);
        press(&mut b, &mut g, ch('R'));
        press(&mut b, &mut g, KeyCode::Esc);
        assert_eq!(b.mode, Mode::Browse);
        assert_eq!(b.marked_count(), 0);
    }

    #[test]
    fn a_chain_member_that_also_changes_other_files_is_reported() {
        let (mut a, mut f) = app();
        f.paths
            .insert(id('c'), vec!["a.txt".into(), "z.txt".into()]);
        press(&mut a, &mut f, ch('R'));
        assert!(
            a.status.contains("1 also change other files"),
            "{}",
            a.status
        );
    }

    #[test]
    fn a_scoped_mark_renders_as_f_and_titles_its_preview_with_the_file() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch('R'));
        assert!(f
            .diffs
            .iter()
            .any(|(c, o)| *c == id('a') && o.as_deref() == Some("a.txt")));
        let screen = render_text(&mut a, 100, 12);
        assert!(screen.contains("[f] aaaaaaaa newest"), "{screen}");
        assert!(screen.contains("[f] cccccccc oldest"), "{screen}");
        assert!(screen.contains("only: a.txt"), "{screen}");
        // A plain space mark is whole again, and its preview is not restricted.
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, ch(' '));
        assert_eq!(a.entries[0].only, None);
    }

    #[test]
    fn r_with_no_list_is_harmless() {
        let mut f = Fake::new();
        let mut empty = App::new();
        press(&mut empty, &mut f, ch('R'));
        assert_eq!(empty.marked_count(), 0);
    }

    #[test]
    fn x_restores_the_file_whole_from_the_decision_screen() {
        let conflict_flow = Ok(Flow::Conflicts(vec![text_conflict("a.txt", TWO_HUNKS)]));
        let (mut a, mut f) = app();
        f.flows = vec![conflict_flow, Ok(Flow::Ready("1 file changed".into()))].into();
        press(&mut a, &mut f, ch('R'));
        press(&mut a, &mut f, KeyCode::Enter);
        assert_eq!(a.mode, Mode::Resolve);
        press(&mut a, &mut f, ch('X'));
        assert_eq!(f.restored, vec!["a.txt".to_string()]);
        assert!(matches!(a.mode, Mode::ConfirmApply { .. }));
        assert!(a.resolver.is_none());
        let screen = render_text(&mut a, 120, 10);
        assert!(screen.contains("restored whole: a.txt"), "{screen}");
        // Starting over clears the note.
        press(&mut a, &mut f, ch('n'));
        press(&mut a, &mut f, KeyCode::Enter);
        assert!(a.restored.is_empty());
    }

    #[test]
    fn x_is_refused_for_a_selection_not_made_with_r() {
        let conflict_flow = Ok(Flow::Conflicts(vec![text_conflict("a.txt", TWO_HUNKS)]));
        let (mut a, mut f) = app();
        f.flows = vec![conflict_flow].into();
        press(&mut a, &mut f, ch(' '));
        press(&mut a, &mut f, KeyCode::Enter);
        press(&mut a, &mut f, ch('X'));
        assert_eq!(a.mode, Mode::Resolve, "stays on the decision screen");
        assert!(a.status_is_error);
        assert!(a.status.contains("selection made with R"), "{}", a.status);
        assert!(f.restored.is_empty());
    }

    #[test]
    fn a_clean_selection_asks_for_confirmation_that_only_y_can_give() {
        let (mut a, mut f) = submit_with(vec![Ok(Flow::Ready("2 files changed".into()))]);
        assert!(matches!(a.mode, Mode::ConfirmApply { .. }));
        for code in [KeyCode::Enter, ch(' '), ch('j'), ch('x')] {
            assert_eq!(press(&mut a, &mut f, code), Outcome::Continue);
            assert!(matches!(a.mode, Mode::ConfirmApply { .. }));
        }
        match press(&mut a, &mut f, ch('y')) {
            Outcome::Submit(spec) => assert_eq!(spec.picks.len(), 1),
            other => panic!("expected a submit, got {other:?}"),
        }
    }

    #[test]
    fn n_and_escape_cancel_the_confirmation_and_keep_the_marks() {
        for code in [ch('n'), ch('N'), KeyCode::Esc] {
            let (mut a, mut f) = submit_with(vec![Ok(Flow::Ready(String::new()))]);
            assert_eq!(press(&mut a, &mut f, code), Outcome::Continue);
            assert_eq!(a.mode, Mode::Browse);
            assert_eq!(a.marked_count(), 1);
        }
    }

    #[test]
    fn a_refused_selection_is_reported_with_the_marks_intact() {
        let (a, _) = submit_with(vec![Ok(Flow::Refused("cannot".into()))]);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status_is_error);
        assert_eq!(a.status, "cannot");
        assert_eq!(a.marked_count(), 1);
    }

    #[test]
    fn a_conflict_opens_the_decision_screen_on_the_first_undecided_unit() {
        let (a, _) = submit_with(vec![Ok(Flow::Conflicts(vec![text_conflict(
            "f.txt", TWO_HUNKS,
        )]))]);
        assert_eq!(a.mode, Mode::Resolve);
        let res = a.resolver.as_ref().unwrap();
        assert_eq!(res.units.len(), 2);
        assert_eq!(res.cursor, 0);
        assert_eq!(res.undecided(), 2);
    }

    #[test]
    fn deciding_moves_on_and_enter_waits_until_everything_is_decided() {
        let (mut a, mut f) = submit_with(vec![Ok(Flow::Conflicts(vec![text_conflict(
            "f.txt", TWO_HUNKS,
        )]))]);
        press(&mut a, &mut f, ch('b'));
        assert_eq!(
            a.resolver.as_ref().unwrap().cursor,
            1,
            "advances to the next undecided"
        );
        press(&mut a, &mut f, KeyCode::Enter);
        assert!(a.status.contains("1 conflict(s) still need a decision"));
        assert_eq!(a.mode, Mode::Resolve);
        assert!(f.resolved.is_empty());

        f.flows.push_back(Ok(Flow::Ready("done".into())));
        press(&mut a, &mut f, ch('a'));
        press(&mut a, &mut f, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::ConfirmApply { .. }));
        let sent = &f.resolved[0][0];
        assert_eq!(sent.choice(0), Some(Side::B));
        assert_eq!(sent.choice(1), Some(Side::A));
    }

    #[test]
    fn navigation_undo_and_both_work_in_the_decision_screen() {
        let (mut a, mut f) = submit_with(vec![Ok(Flow::Conflicts(vec![text_conflict(
            "f.txt", TWO_HUNKS,
        )]))]);
        press(&mut a, &mut f, ch('j'));
        assert_eq!(a.resolver.as_ref().unwrap().cursor, 1);
        press(&mut a, &mut f, ch('j'));
        assert_eq!(a.resolver.as_ref().unwrap().cursor, 1, "stops at the last");
        press(&mut a, &mut f, ch('k'));
        press(&mut a, &mut f, ch('c'));
        assert_eq!(a.resolver.as_ref().unwrap().choice_at(0), Some(Side::Both));
        // Deciding moved the cursor on to the other hunk; go back and undo.
        press(&mut a, &mut f, ch('k'));
        press(&mut a, &mut f, ch('u'));
        assert_eq!(a.resolver.as_ref().unwrap().choice_at(0), None);
        assert_eq!(a.resolver.as_ref().unwrap().undecided(), 2);
    }

    #[test]
    fn a_whole_file_conflict_refuses_keeping_both() {
        let (mut a, mut f) =
            submit_with(vec![Ok(Flow::Conflicts(vec![whole_conflict("gone.txt")]))]);
        press(&mut a, &mut f, ch('c'));
        assert!(a.status_is_error);
        assert_eq!(a.resolver.as_ref().unwrap().undecided(), 1);
        press(&mut a, &mut f, ch('a'));
        assert_eq!(a.resolver.as_ref().unwrap().undecided(), 0);
    }

    #[test]
    fn a_later_pick_can_raise_another_conflict_after_the_first_is_resolved() {
        let (mut a, mut f) = submit_with(vec![
            Ok(Flow::Conflicts(vec![whole_conflict("one.txt")])),
            Ok(Flow::Conflicts(vec![text_conflict("two.txt", TWO_HUNKS)])),
        ]);
        press(&mut a, &mut f, ch('b'));
        press(&mut a, &mut f, KeyCode::Enter);
        assert_eq!(a.mode, Mode::Resolve);
        assert_eq!(a.resolver.as_ref().unwrap().files[0].path, "two.txt");
    }

    #[test]
    fn leaving_the_decision_screen_returns_to_browsing_with_marks_kept() {
        for code in [KeyCode::Esc, ch('q')] {
            let (mut a, mut f) =
                submit_with(vec![Ok(Flow::Conflicts(vec![whole_conflict("g.txt")]))]);
            assert_eq!(press(&mut a, &mut f, code), Outcome::Continue);
            assert_eq!(a.mode, Mode::Browse);
            assert_eq!(a.marked_count(), 1);
            assert!(a.resolver.is_none());
        }
    }

    #[test]
    fn quitting_asks_only_when_something_is_marked() {
        let (mut a, mut f) = app();
        assert_eq!(press(&mut a, &mut f, ch('q')), Outcome::Quit);

        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        assert_eq!(press(&mut a, &mut f, ch('q')), Outcome::Continue);
        assert_eq!(a.mode, Mode::ConfirmQuit);
        assert_eq!(press(&mut a, &mut f, KeyCode::Enter), Outcome::Continue);
        assert_eq!(press(&mut a, &mut f, ch('y')), Outcome::Quit);
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let (mut a, mut f) = submit_with(vec![Ok(Flow::Conflicts(vec![whole_conflict("g.txt")]))]);
        let ctrl_c = KeyEvent::new(ch('c'), KeyModifiers::CONTROL);
        assert_eq!(a.handle_key(ctrl_c, Instant::now(), &mut f), Outcome::Quit);
    }

    #[test]
    fn diff_pane_keys_match_the_drop_picker() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch('l'));
        assert_eq!(a.focus, Focus::Diff);
        press(&mut a, &mut f, ch('n'));
        assert_eq!(a.cursor, 1);
        press(&mut a, &mut f, ch('N'));
        assert_eq!(a.cursor, 0);
        press(&mut a, &mut f, ch('h'));
        assert_eq!(a.focus, Focus::List);
    }

    // A screen with three commits of which the middle one changes nothing on HEAD.
    fn app_with_inert_middle() -> (App, Fake) {
        let mut fake = Fake::new();
        fake.inert = vec![id('b')];
        let mut app = App::new();
        app.set_source("feat", fake.commits.clone());
        app.view_h = 10;
        app.view_w = 40;
        app.ensure_diff(&mut fake, Instant::now());
        (app, fake)
    }

    #[test]
    fn j_and_k_pass_over_a_commit_that_changes_nothing() {
        let (mut a, mut f) = app_with_inert_middle();
        assert_eq!(a.cursor, 0);
        press(&mut a, &mut f, ch('j'));
        assert_eq!(a.cursor, 2, "the inert middle commit is skipped going down");
        press(&mut a, &mut f, ch('k'));
        assert_eq!(a.cursor, 0, "and going up");
    }

    #[test]
    fn movement_at_the_end_of_the_list_stays_put_and_says_why_after_a_skip() {
        let (mut a, mut f) = app_with_inert_middle();
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, ch('j'));
        assert_eq!(a.cursor, 2);
        assert!(
            a.status.is_empty(),
            "no skip happened, so nothing to explain"
        );

        // Nothing applicable below the selection once the last commit is inert too.
        let (mut a, mut f) = app_with_inert_middle();
        f.inert.push(id('c'));
        press(&mut a, &mut f, ch('j'));
        assert_eq!(a.cursor, 0);
        assert!(a.status.contains("no further commit"), "{}", a.status);
    }

    #[test]
    fn g_and_capital_g_land_on_the_nearest_commit_that_applies() {
        let mut fake = Fake::new();
        fake.inert = vec![id('a'), id('c')];
        let mut a = App::new();
        a.set_source("feat", fake.commits.clone());
        // The first commit is inert, so the screen opens on the next one.
        a.ensure_diff(&mut fake, Instant::now());
        assert_eq!(a.cursor, 1);
        press(&mut a, &mut fake, ch('g'));
        assert_eq!(a.cursor, 1);
        press(&mut a, &mut fake, ch('G'));
        assert_eq!(a.cursor, 1);
    }

    #[test]
    fn diff_pane_next_and_previous_skip_the_same_commits() {
        let (mut a, mut f) = app_with_inert_middle();
        press(&mut a, &mut f, ch('l'));
        press(&mut a, &mut f, ch('n'));
        assert_eq!(a.cursor, 2);
        press(&mut a, &mut f, ch('N'));
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn a_commit_that_changes_nothing_is_gray_until_marked() {
        let (mut a, mut f) = app_with_inert_middle();
        a.classify(1, &mut f);
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|fr| a.render(fr)).unwrap();
        let buf = term.backend().buffer().clone();
        // Row 1 is the border; commits start on row 2 (index 1 is the third line of the frame).
        let fg = |y: u16| buf[(6, y)].fg;
        assert_eq!(fg(2), Color::DarkGray, "the inert commit is drawn gray");
        assert_ne!(fg(1), Color::DarkGray, "an applicable commit is not");
        assert_ne!(fg(3), Color::DarkGray, "neither is the last one");
    }

    #[test]
    fn classification_is_lazy_and_done_once_per_commit() {
        let (mut a, mut f) = app_with_inert_middle();
        assert!(
            f.asked.len() <= 1,
            "only the leading commit was needed to open"
        );
        press(&mut a, &mut f, ch('j'));
        press(&mut a, &mut f, ch('k'));
        press(&mut a, &mut f, ch('j'));
        for c in [id('a'), id('b'), id('c')] {
            assert!(
                f.asked.iter().filter(|x| **x == c).count() <= 1,
                "{c} asked twice"
            );
        }
    }

    #[test]
    fn idle_ticks_classify_the_commits_near_the_selection() {
        let (mut a, mut f) = app_with_inert_middle();
        let now = Instant::now() + DWELL;
        while a.wakeup(now).is_some() {
            a.tick(&mut f, now);
        }
        assert!(a.entries.iter().all(|e| e.applies.is_some()));
        assert_eq!(a.entries[1].applies, Some(false));
    }

    #[test]
    fn the_diff_waits_for_the_selection_to_rest_and_is_then_loaded_once() {
        let mut fake = Fake::new();
        let mut a = App::new();
        a.set_source("feat", fake.commits.clone());
        a.view_h = 10;
        a.view_w = 40;
        let t0 = Instant::now();
        a.ensure_diff(&mut fake, t0);
        assert_eq!(fake.diffs.len(), 1, "the first commit shows at once");

        a.handle_key(key(ch('j')), t0, &mut fake);
        a.ensure_diff(&mut fake, t0 + DWELL / 2);
        assert_eq!(
            fake.diffs.len(),
            1,
            "not loaded while the selection is still fresh"
        );
        assert!(a
            .wakeup(t0 + DWELL / 2)
            .is_some_and(|d| d <= DWELL / 2 + Duration::from_millis(1)));
        let screen = render_text(&mut a, 100, 12);
        assert!(screen.contains("loading the diff"), "{screen}");

        // Passing over further commits within the dwell never loads them.
        a.handle_key(key(ch('j')), t0 + DWELL / 2, &mut fake);
        a.ensure_diff(&mut fake, t0 + DWELL);
        assert_eq!(
            fake.diffs.len(),
            1,
            "the dwell restarted with the second move"
        );

        a.ensure_diff(&mut fake, t0 + DWELL / 2 + DWELL);
        assert_eq!(fake.diffs.len(), 2);
        assert_eq!(
            fake.diffs[1].0,
            id('c'),
            "only the commit rested on was loaded"
        );
        a.ensure_diff(&mut fake, t0 + DWELL * 3);
        assert_eq!(fake.diffs.len(), 2, "loaded once");
    }

    #[test]
    fn a_commit_visited_before_shows_its_cached_diff_without_waiting() {
        let mut fake = Fake::new();
        let mut a = App::new();
        a.set_source("feat", fake.commits.clone());
        a.view_h = 10;
        a.view_w = 40;
        let t0 = Instant::now();
        a.ensure_diff(&mut fake, t0);
        a.handle_key(key(ch('j')), t0, &mut fake);
        a.ensure_diff(&mut fake, t0 + DWELL);
        a.handle_key(key(ch('k')), t0 + DWELL, &mut fake);
        let before = fake.diffs.len();
        a.ensure_diff(&mut fake, t0 + DWELL);
        assert_eq!(fake.diffs.len(), before);
        assert!(a.view().is_some());
    }

    fn render_text(a: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.render(f)).unwrap();
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
    fn browse_screen_shows_source_marks_diff_and_hints() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '));
        let screen = render_text(&mut a, 100, 20);
        assert!(screen.contains("feat  1 of 3 marked"), "{screen}");
        assert!(screen.contains("[x] aaaaaaaa newest"), "{screen}");
        assert!(screen.contains("(no message)"), "{screen}");
        assert!(screen.contains("+new"), "{screen}");
        assert!(screen.contains("Tab branch"), "{screen}");
    }

    #[test]
    fn overlay_lists_the_branches_over_the_browse_screen() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, KeyCode::Tab);
        let screen = render_text(&mut a, 100, 20);
        assert!(screen.contains("Pick from which branch?"), "{screen}");
        assert!(screen.contains("origin/dev"), "{screen}");
    }

    #[test]
    fn without_a_source_the_screen_points_at_tab() {
        let mut a = App::new();
        let screen = render_text(&mut a, 100, 12);
        assert!(screen.contains("Tab: choose a branch"), "{screen}");
        assert!(
            screen.contains("Press Tab to choose the branch"),
            "{screen}"
        );
    }

    #[test]
    fn decision_screen_shows_both_sides_context_and_the_chosen_one() {
        let (mut a, mut f) = submit_with(vec![Ok(Flow::Conflicts(vec![text_conflict(
            "f.txt", TWO_HUNKS,
        )]))]);
        let screen = render_text(&mut a, 110, 24);
        assert!(screen.contains("Conflicts  0 of 2 decided"), "{screen}");
        assert!(screen.contains("[ ] f.txt  1/2"), "{screen}");
        assert!(screen.contains("A  tree copy"), "{screen}");
        assert!(
            screen.contains("B  from picked commit aaaaaaaa"),
            "{screen}"
        );
        assert!(screen.contains("A1") && screen.contains("B1"), "{screen}");
        assert!(!screen.contains("<- chosen"));

        press(&mut a, &mut f, ch('b'));
        press(&mut a, &mut f, ch('k'));
        let screen = render_text(&mut a, 110, 24);
        assert!(screen.contains("[B] f.txt  1/2"), "{screen}");
        assert!(screen.contains("<- chosen"), "{screen}");
        assert!(screen.contains("1 of 2 decided"), "{screen}");
    }

    #[test]
    fn decision_screen_describes_a_whole_file_conflict() {
        let (mut a, _) = submit_with(vec![Ok(Flow::Conflicts(vec![whole_conflict("gone.txt")]))]);
        let screen = render_text(&mut a, 110, 20);
        assert!(screen.contains("the picked commit deletes it"), "{screen}");
        assert!(screen.contains("keep the tree's file"), "{screen}");
        assert!(screen.contains("delete the file"), "{screen}");
    }

    #[test]
    fn confirmation_bar_names_the_count_and_the_change_summary() {
        let (mut a, _) = submit_with(vec![Ok(Flow::Ready(
            " b.txt | 1 +\n 1 file changed".into(),
        ))]);
        let screen = render_text(&mut a, 120, 12);
        assert!(
            screen.contains("Apply 1 commit(s) as a patch? 1 file changed [y/n]"),
            "{screen}"
        );
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let (mut a, mut f) = app();
        for (w, h) in [(1, 1), (10, 3), (30, 5)] {
            render_text(&mut a, w, h);
        }
        press(&mut a, &mut f, KeyCode::Tab);
        render_text(&mut a, 12, 4);
        let (mut a, _) = submit_with(vec![Ok(Flow::Conflicts(vec![text_conflict(
            "f.txt", TWO_HUNKS,
        )]))]);
        render_text(&mut a, 12, 4);
    }
}
