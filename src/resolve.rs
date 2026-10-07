// Interactive resolution of an interrupted merge, rebase, cherry-pick, or revert (issue #26).
//
// The decision model is the one `cherry-pick` already uses (src/conflict.rs): a conflicted text file is split
// into plain segments and conflict hunks, each hunk is answered with side A (the copy already in the tree),
// side B (the incoming copy), or both, and the file is rendered again once every hunk has an answer. What this
// module adds is a second entry point into that model. `cherry-pick` reaches it from a patch it computed
// itself, in the object database, with nothing in the work tree; here the conflicts are the ones git itself
// left in the index and the work tree, which is where an operator actually meets them.
//
// The correspondence with git's own vocabulary is exact and worth stating once, since the two naming schemes
// meet here: git's "ours" is the checked-out side, which conflict.rs calls A, and git's "theirs" is the
// incoming side, which conflict.rs calls B. During a rebase git swaps the roles relative to the operator's
// intuition — "ours" is the commit being replayed onto, "theirs" the commit being replayed — and no attempt is
// made to un-swap them, because renaming the sides would disagree with every other git command run in the same
// repository. The sides are labelled with the branch or commit git records for them instead, so the screen
// says what each side actually is rather than relying on the word.
//
// Not every conflict has hunks. A binary file, an add/add of two unrelated files, and a modify/delete have no
// marker structure to decide within, so they carry one whole-file decision each and are applied through git's
// own index operations rather than by writing bytes.

use std::io::IsTerminal;
use std::path::Path;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::conflict::{self, Segment, Side};
use crate::pick::with_terminal;
use crate::{git, Res};

// Lines of unchanged text shown above and below a conflict hunk.
const CONTEXT: usize = 3;

// How a conflicted path is decided.
#[derive(Clone, PartialEq, Debug)]
pub enum Body {
    // A text file whose markers parsed: one decision per hunk.
    Hunks(Vec<Segment>),
    // A conflict with no internal structure to decide within — a binary file, an add/add, or a
    // modify/delete. `a` and `b` describe what each side means for this particular path, since for a
    // modify/delete one of the two sides is the file's absence.
    Whole {
        a: WholeSide,
        b: WholeSide,
        choice: Option<Side>,
    },
}

// What taking one side of a whole-file conflict does to the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WholeSide {
    // Check the stage out of the index and stage it.
    Keep,
    // Remove the path.
    Delete,
}

impl WholeSide {
    fn label(self) -> &'static str {
        match self {
            WholeSide::Keep => "keep this version",
            WholeSide::Delete => "delete the file",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Conflicted {
    pub path: String,
    pub body: Body,
}

impl Conflicted {
    // Number of separate decisions this path needs.
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

    fn decided(&self) -> bool {
        self.unresolved() == 0
    }
}

// Status codes git's porcelain format uses for an unmerged path, and what each side means for it. A code is
// two letters, the index state and the work-tree state; for an unmerged entry both letters describe the two
// sides of the conflict rather than staged-versus-unstaged. U is "updated but unmerged", A is "added by one
// side", D is "deleted by one side".
fn whole_sides(code: &str) -> Option<(WholeSide, WholeSide)> {
    match code {
        // Both sides deleted the path: nothing to choose, handled before a decision is asked for.
        "DD" => Some((WholeSide::Delete, WholeSide::Delete)),
        // Ours has it, theirs deleted it.
        "UD" | "AU" => Some((WholeSide::Keep, WholeSide::Delete)),
        // Ours deleted it, theirs has it.
        "DU" | "UA" => Some((WholeSide::Delete, WholeSide::Keep)),
        // Both sides have content; a decision within the file is attempted first.
        "AA" | "UU" => Some((WholeSide::Keep, WholeSide::Keep)),
        _ => None,
    }
}

// Every unmerged path in the index, with its porcelain status code, in the order git reports them.
fn unmerged(root: &Path) -> Res<Vec<(String, String)>> {
    let out = git::run(root, &["status", "--porcelain", "-z"])?;
    let mut rows = Vec::new();
    let mut fields = out.split('\u{0}').filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        let path = entry[3..].to_string();
        // A rename record carries its source path as a second NUL-delimited field, which must be consumed
        // so it is not read as the next entry. Renames are not unmerged states, but they appear in the same
        // listing.
        if code.starts_with('R') || code.starts_with('C') {
            let _ = fields.next();
            continue;
        }
        if whole_sides(code).is_some() {
            rows.push((code.to_string(), path));
        }
    }
    Ok(rows)
}

// Build the decision list for the current conflict. A path whose work-tree content parses as conflict markers
// becomes a per-hunk decision; anything else becomes one whole-file decision. A both-deleted path needs no
// decision and is staged as a removal immediately, so it never reaches the screen.
pub fn collect(root: &Path) -> Res<Vec<Conflicted>> {
    let mut out = Vec::new();
    for (code, path) in unmerged(root)? {
        let (a, b) = whole_sides(&code).expect("filtered above");
        if a == WholeSide::Delete && b == WholeSide::Delete {
            git::run(root, &["rm", "-q", "--", &path])?;
            continue;
        }
        let body = if a == WholeSide::Keep && b == WholeSide::Keep {
            match std::fs::read(root.join(&path))
                .ok()
                .and_then(|c| conflict::parse(&c))
            {
                Some(segs) => Body::Hunks(segs),
                // A binary file, or one whose markers do not parse. A whole-file decision is always correct,
                // if less fine grained, and is the documented fallback of conflict::parse.
                None => Body::Whole { a, b, choice: None },
            }
        } else {
            Body::Whole { a, b, choice: None }
        };
        out.push(Conflicted { path, body });
    }
    Ok(out)
}

// Apply one decided path to the work tree and the index. A hunk decision is rendered back over the file; a
// whole-file decision is carried out through git's own index operations, since the losing side's content may
// not exist on disk at all.
fn apply(root: &Path, c: &Conflicted) -> Res<()> {
    match &c.body {
        Body::Hunks(segs) => {
            let bytes = conflict::render(segs)
                .ok_or_else(|| format!("resolve: {} still has an undecided conflict", c.path))?;
            std::fs::write(root.join(&c.path), bytes)?;
            git::run(root, &["add", "--", &c.path])?;
        }
        Body::Whole { a, b, choice } => {
            let side = choice.ok_or_else(|| format!("resolve: {} is undecided", c.path))?;
            let taken = match side {
                Side::A => *a,
                // Both is not offered for a whole-file conflict; treated as the incoming side if it somehow
                // arrives, rather than failing after other paths have already been staged.
                Side::B | Side::Both => *b,
            };
            match taken {
                WholeSide::Delete => {
                    git::run(root, &["rm", "-q", "-f", "--", &c.path])?;
                }
                WholeSide::Keep => {
                    let flag = if side == Side::A {
                        "--ours"
                    } else {
                        "--theirs"
                    };
                    git::run(root, &["checkout", flag, "--", &c.path])?;
                    git::run(root, &["add", "--", &c.path])?;
                }
            }
        }
    }
    Ok(())
}

// Decide every conflict in favour of one side without opening the screen. Backs `--ours` / `--theirs`, the
// blunt instrument for a merge whose result is already known to be one side wholesale.
fn decide_all(files: &mut [Conflicted], side: Side) {
    for f in files.iter_mut() {
        match &mut f.body {
            Body::Hunks(segs) => conflict::decide_all(segs, side),
            Body::Whole { choice, .. } => *choice = Some(side),
        }
    }
}

// Finish the operation git is part-way through, once every path is staged. A merge is concluded with a
// commit; the sequencer-driven operations have their own continue verb. `--no-edit` keeps the generated
// message rather than opening an editor, matching gitomic's practice elsewhere of not launching one unless the
// operator asked for it.
fn continue_operation(root: &Path, op: git::InProgress) -> Res<String> {
    let args: &[&str] = match op {
        git::InProgress::Merge => &["commit", "--no-edit"],
        git::InProgress::Rebase => &["rebase", "--continue"],
        git::InProgress::CherryPick => &["cherry-pick", "--continue", "--no-edit"],
        git::InProgress::Revert => &["revert", "--continue", "--no-edit"],
        // A bisect has no conflicts to resolve, so this arm is unreachable through `resolve`.
        git::InProgress::Bisect => return Ok(String::new()),
    };
    git::run(root, args)
}

// How the caller asked for the conflicts to be decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decide {
    Interactive,
    Ours,
    Theirs,
}

// Resolve the conflicts of the operation in progress. `finish` completes that operation once every path is
// staged; without it the staged result is left for the operator to commit or continue by hand, which is the
// safer default in the middle of a rebase whose next step may conflict again.
pub fn run(cwd: &Path, decide: Decide, finish: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;

    let Some(op) = git::operation_kind(&git_dir) else {
        return Err("resolve: no merge, rebase, cherry-pick, or revert is in progress".into());
    };
    if op == git::InProgress::Bisect {
        return Err(
            "resolve: a bisect has no conflicts to resolve; 'gitomic unstick --yes' ends it".into(),
        );
    }

    let mut files = collect(&root)?;
    if files.is_empty() {
        println!("gitomic: no unmerged paths remain in the {}", op.name());
        if finish {
            let out = continue_operation(&root, op)?;
            if !out.is_empty() {
                println!("{out}");
            }
        }
        return Ok(());
    }

    match decide {
        Decide::Ours => decide_all(&mut files, Side::A),
        Decide::Theirs => decide_all(&mut files, Side::B),
        Decide::Interactive => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                return Err(
                    "resolve: the decision screen needs a terminal; use --ours or --theirs instead"
                        .into(),
                );
            }
            let labels = side_labels(&root, &git_dir, op);
            let mut app = App::new(files, labels);
            let done = with_terminal(|t| event_loop(t, &mut app))?;
            if !done {
                println!("gitomic: nothing resolved; the {} is untouched", op.name());
                return Ok(());
            }
            files = app.files;
        }
    }

    // Nothing is written until every path is decided, so a partial answer cannot leave half the conflict
    // staged and half not.
    if let Some(c) = files.iter().find(|c| !c.decided()) {
        return Err(format!("resolve: {} is undecided; nothing was staged", c.path).into());
    }
    for c in &files {
        apply(&root, c)?;
    }
    println!("gitomic: resolved and staged {} path(s)", files.len());

    if finish {
        let out = continue_operation(&root, op)?;
        if !out.is_empty() {
            println!("{out}");
        }
        println!("gitomic: {} completed", op.name());
    } else {
        match op {
            git::InProgress::Merge => println!("  run 'git commit' to conclude the merge"),
            _ => println!("  run 'git {} --continue' to carry on", op.name()),
        }
    }
    Ok(())
}

// What each side of the conflict is, for the screen's headings. A merge records the incoming side in
// MERGE_HEAD and a sequencer operation in CHERRY_PICK_HEAD or REVERT_HEAD; a rebase has neither, and its
// swapped roles are named explicitly rather than left to be inferred.
fn side_labels(root: &Path, git_dir: &Path, op: git::InProgress) -> (String, String) {
    let describe = |r: &str| {
        git::run(root, &["rev-parse", "--short", r])
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| r.to_string())
    };
    match op {
        git::InProgress::Rebase => (
            "A  ours — the commit being replayed onto".to_string(),
            "B  theirs — the commit being replayed".to_string(),
        ),
        git::InProgress::Merge => {
            let head = describe("HEAD");
            let other = if git_dir.join("MERGE_HEAD").exists() {
                describe("MERGE_HEAD")
            } else {
                "incoming".to_string()
            };
            (format!("A  ours — {head}"), format!("B  theirs — {other}"))
        }
        _ => (
            format!("A  ours — {}", describe("HEAD")),
            "B  theirs — the commit being applied".to_string(),
        ),
    }
}

// ----------------------------------------------------------------------------------------------
// The screen. Kept as plain state with a key handler and a render function, neither of which touches a
// terminal, so the whole interaction is exercised by ordinary unit tests in the way pick.rs and cherry_ui.rs
// are.
// ----------------------------------------------------------------------------------------------

pub struct App {
    pub files: Vec<Conflicted>,
    // Index into `files`.
    pub file: usize,
    // Index of the selected hunk within the current file, counted over hunks only.
    pub unit: usize,
    pub scroll: usize,
    pub message: Option<String>,
    labels: (String, String),
    quit: bool,
    submitted: bool,
}

impl App {
    pub fn new(files: Vec<Conflicted>, labels: (String, String)) -> App {
        let mut app = App {
            files,
            file: 0,
            unit: 0,
            scroll: 0,
            message: None,
            labels,
            quit: false,
            submitted: false,
        };
        app.seek_undecided();
        app
    }

    fn current(&self) -> &Conflicted {
        &self.files[self.file]
    }

    fn units(&self) -> usize {
        self.current().units()
    }

    // Move to the first file that still has an undecided unit, and to the first such unit within it. Opening
    // on work still to do saves a pass over decisions already made when the screen is re-entered.
    fn seek_undecided(&mut self) {
        if let Some(i) = self.files.iter().position(|c| !c.decided()) {
            self.file = i;
            self.unit = self.first_undecided_unit();
            self.scroll = 0;
        }
    }

    fn first_undecided_unit(&self) -> usize {
        match &self.files[self.file].body {
            Body::Hunks(segs) => conflict::hunks(segs)
                .iter()
                .position(|h| h.choice.is_none())
                .unwrap_or(0),
            Body::Whole { .. } => 0,
        }
    }

    // Record a decision for the selected unit and step to the next undecided one.
    fn choose(&mut self, side: Side) {
        let unit = self.unit;
        match &mut self.files[self.file].body {
            Body::Hunks(segs) => conflict::decide(segs, unit, Some(side)),
            Body::Whole { choice, .. } => {
                // Both sides of a whole-file conflict cannot be kept: there is one path and one blob.
                if side == Side::Both {
                    self.message = Some("this conflict has no hunks; choose a or b".to_string());
                    return;
                }
                *choice = Some(side);
            }
        }
        self.message = None;
        self.advance();
    }

    fn undo(&mut self) {
        let unit = self.unit;
        match &mut self.files[self.file].body {
            Body::Hunks(segs) => conflict::decide(segs, unit, None),
            Body::Whole { choice, .. } => *choice = None,
        }
        self.message = None;
    }

    // Step to the next undecided unit, wrapping across files. Leaves the selection where it is when
    // everything is decided, so the final decision stays on screen.
    fn advance(&mut self) {
        let n = self.files.len();
        for step in 0..=n {
            let f = (self.file + step) % n;
            let start = if step == 0 { self.unit + 1 } else { 0 };
            let undecided = match &self.files[f].body {
                Body::Hunks(segs) => conflict::hunks(segs)
                    .iter()
                    .enumerate()
                    .skip(start)
                    .find(|(_, h)| h.choice.is_none())
                    .map(|(i, _)| i),
                Body::Whole { choice, .. } => (start == 0 && choice.is_none()).then_some(0),
            };
            if let Some(u) = undecided {
                self.file = f;
                self.unit = u;
                self.scroll = 0;
                return;
            }
        }
    }

    fn move_unit(&mut self, delta: isize) {
        let n = self.units();
        if n == 0 {
            return;
        }
        let next = self.unit as isize + delta;
        self.unit = next.clamp(0, n as isize - 1) as usize;
        self.scroll = 0;
        self.message = None;
    }

    fn move_file(&mut self, delta: isize) {
        let n = self.files.len() as isize;
        let next = (self.file as isize + delta).clamp(0, n - 1) as usize;
        if next != self.file {
            self.file = next;
            self.unit = 0;
            self.scroll = 0;
            self.message = None;
        }
    }

    pub fn remaining(&self) -> usize {
        self.files.iter().map(Conflicted::unresolved).sum()
    }

    // Handle one key. Returns false when the screen should close.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit = true;
                return false;
            }
            KeyCode::Char('a') => self.choose(Side::A),
            KeyCode::Char('b') => self.choose(Side::B),
            KeyCode::Char('c') => self.choose(Side::Both),
            KeyCode::Char('u') if ctrl => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('j') | KeyCode::Down => self.move_unit(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_unit(-1),
            KeyCode::Char(']') | KeyCode::Tab => self.move_file(1),
            KeyCode::Char('[') | KeyCode::BackTab => self.move_file(-1),
            KeyCode::Char('d') if ctrl => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Enter => {
                if self.remaining() == 0 {
                    self.submitted = true;
                    return false;
                }
                self.message = Some(format!("{} conflict(s) still undecided", self.remaining()));
            }
            _ => {}
        }
        true
    }

    // The lines of the selected unit: both sides with their labels, and the surrounding context of the file.
    pub fn view(&self) -> Vec<String> {
        let c = self.current();
        let mut out = vec![c.path.clone(), String::new()];
        match &c.body {
            Body::Whole { a, b, choice } => {
                out.push("no hunks to decide within this file".to_string());
                out.push(String::new());
                out.push(format!("{}   ({})", self.labels.0, a.label()));
                out.push(format!("{}   ({})", self.labels.1, b.label()));
                out.push(String::new());
                out.push(match choice {
                    Some(Side::A) => "chosen: A".to_string(),
                    Some(_) => "chosen: B".to_string(),
                    None => "undecided".to_string(),
                });
            }
            Body::Hunks(segs) => {
                let hunks = conflict::hunks(segs);
                let Some(h) = hunks.get(self.unit) else {
                    out.push("(no conflict in this file)".to_string());
                    return out;
                };
                out.push(format!("conflict {}/{}", self.unit + 1, hunks.len()));
                out.push(String::new());
                for line in context_before(segs, self.unit) {
                    out.push(format!("  {line}"));
                }
                out.push(self.labels.0.clone());
                for line in text_lines(&h.ours) {
                    out.push(format!("A {line}"));
                }
                out.push(self.labels.1.clone());
                for line in text_lines(&h.theirs) {
                    out.push(format!("B {line}"));
                }
                for line in context_after(segs, self.unit) {
                    out.push(format!("  {line}"));
                }
                out.push(String::new());
                out.push(match h.choice {
                    Some(s) => format!("chosen: {}", s.letter()),
                    None => "undecided".to_string(),
                });
            }
        }
        out
    }
}

fn text_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

// The last CONTEXT lines of plain text preceding hunk `n`, and the first CONTEXT lines following it. Context
// is taken from the segment list rather than the file on disk, so it reflects the decisions already made
// around the hunk being looked at.
fn context_before(segs: &[Segment], n: usize) -> Vec<String> {
    let Some(i) = hunk_index(segs, n) else {
        return Vec::new();
    };
    match segs[..i].iter().rev().find_map(|s| match s {
        Segment::Text(t) => Some(t),
        Segment::Hunk(_) => None,
    }) {
        Some(t) => {
            let lines = text_lines(t);
            lines[lines.len().saturating_sub(CONTEXT)..].to_vec()
        }
        None => Vec::new(),
    }
}

fn context_after(segs: &[Segment], n: usize) -> Vec<String> {
    let Some(i) = hunk_index(segs, n) else {
        return Vec::new();
    };
    match segs[i + 1..].iter().find_map(|s| match s {
        Segment::Text(t) => Some(t),
        Segment::Hunk(_) => None,
    }) {
        Some(t) => text_lines(t).into_iter().take(CONTEXT).collect(),
        None => Vec::new(),
    }
}

// Position in `segs` of the `n`th hunk.
fn hunk_index(segs: &[Segment], n: usize) -> Option<usize> {
    segs.iter()
        .enumerate()
        .filter(|(_, s)| matches!(s, Segment::Hunk(_)))
        .nth(n)
        .map(|(i, _)| i)
}

const HINTS: &str =
    " j/k conflict  [ ] file  a ours  b theirs  c both  u undo  Enter stage  q leave ";

pub fn draw(f: &mut Frame, app: &App) {
    let [body, hint] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Min(10)]).areas(body);
    draw_files(f, app, left);
    draw_unit(f, app, right);
    let text = match &app.message {
        Some(m) => format!(" {m} "),
        None => HINTS.to_string(),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Black).bg(Color::Gray),
        ))),
        hint,
    );
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|c| {
            let left = c.unresolved();
            let mark = if left == 0 { "ok" } else { "--" };
            let style = if left == 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            ListItem::new(Line::from(Span::styled(
                format!("{mark} {} ({left}/{})", c.path, c.units()),
                style,
            )))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.file));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title("unmerged"))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_unit(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .view()
        .into_iter()
        .skip(app.scroll)
        .map(|l| {
            let style = match l.as_bytes().first() {
                Some(b'A') => Style::default().fg(Color::Cyan),
                Some(b'B') => Style::default().fg(Color::Magenta),
                _ => Style::default(),
            };
            Line::from(Span::styled(l, style))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("conflict")),
        area,
    );
}

// Returns true when the operator submitted a complete set of decisions.
fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Res<bool> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if let Event::Key(key) = event::read()? {
            // Windows terminals report both press and release; only presses are acted on, so a decision is
            // not recorded twice.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if !app.key(key) {
                break;
            }
        }
    }
    Ok(app.submitted && !app.quit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    // A repository sitting on a conflicted merge of one text file.
    fn conflicted() -> Repo {
        let r = Repo::new();
        r.commit_file("f.txt", &Repo::lines(), "base");
        r.git(&["checkout", "-q", "-b", "other"]);
        r.commit_file(
            "f.txt",
            &Repo::lines_with(&[(2, "other2"), (8, "other8")]),
            "other",
        );
        r.git(&["checkout", "-q", "main"]);
        r.commit_file(
            "f.txt",
            &Repo::lines_with(&[(2, "main2"), (8, "main8")]),
            "main",
        );
        let _ = git::run(&r.0, &["merge", "other"]);
        r
    }

    fn app_for(r: &Repo) -> App {
        App::new(
            collect(&r.0).unwrap(),
            ("A ours".to_string(), "B theirs".to_string()),
        )
    }

    #[test]
    fn a_text_conflict_is_collected_as_hunks() {
        let r = conflicted();
        let files = collect(&r.0).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "f.txt");
        assert_eq!(files[0].units(), 2);
    }

    #[test]
    fn the_screen_opens_on_the_first_undecided_conflict() {
        let r = conflicted();
        let app = app_for(&r);
        assert_eq!(app.unit, 0);
        assert_eq!(app.remaining(), 2);
    }

    #[test]
    fn a_decision_advances_to_the_next_undecided_conflict() {
        let r = conflicted();
        let mut app = app_for(&r);
        app.key(key('a'));
        assert_eq!(app.unit, 1);
        assert_eq!(app.remaining(), 1);
        app.key(key('b'));
        assert_eq!(app.remaining(), 0);
    }

    #[test]
    fn undo_returns_a_conflict_to_undecided() {
        let r = conflicted();
        let mut app = app_for(&r);
        app.key(key('a'));
        app.key(key('k'));
        app.key(key('u'));
        assert_eq!(app.remaining(), 2);
    }

    #[test]
    fn enter_is_refused_while_a_conflict_is_undecided() {
        let r = conflicted();
        let mut app = app_for(&r);
        assert!(app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(app.message.as_deref().unwrap().contains("undecided"));
    }

    #[test]
    fn the_view_shows_both_sides_and_the_choice() {
        let r = conflicted();
        let mut app = app_for(&r);
        let before = app.view().join("\n");
        assert!(before.contains("main2"), "{before}");
        assert!(before.contains("other2"), "{before}");
        assert!(before.contains("undecided"), "{before}");
        app.key(key('a'));
        app.key(key('k'));
        assert!(app.view().join("\n").contains("chosen: A"));
    }

    #[test]
    fn ours_resolves_and_stages_the_merge() {
        let r = conflicted();
        run(&r.0, Decide::Ours, false).unwrap();
        assert!(collect(&r.0).unwrap().is_empty());
        assert!(r.read("f.txt").contains("main2"));
        assert!(!r.read("f.txt").contains("other2"));
        assert!(!r.read("f.txt").contains("<<<<<<<"));
    }

    #[test]
    fn theirs_takes_the_incoming_side() {
        let r = conflicted();
        run(&r.0, Decide::Theirs, false).unwrap();
        assert!(r.read("f.txt").contains("other2"));
        assert!(!r.read("f.txt").contains("main2"));
    }

    #[test]
    fn finish_completes_the_merge() {
        let r = conflicted();
        let before = r.head();
        run(&r.0, Decide::Ours, true).unwrap();
        assert!(git::operation_kind(&git::git_dir(&r.0).unwrap()).is_none());
        assert_ne!(r.head(), before);
        // The completed merge has two parents.
        assert_eq!(
            r.git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .split(' ')
                .count(),
            3
        );
    }

    #[test]
    fn without_finish_the_merge_is_left_staged() {
        let r = conflicted();
        run(&r.0, Decide::Ours, false).unwrap();
        assert_eq!(
            git::operation_kind(&git::git_dir(&r.0).unwrap()),
            Some(git::InProgress::Merge)
        );
    }

    #[test]
    fn a_modify_delete_conflict_is_a_whole_file_decision() {
        let r = Repo::new();
        r.commit_file("f.txt", "one\n", "base");
        r.git(&["checkout", "-q", "-b", "other"]);
        r.git(&["rm", "-q", "f.txt"]);
        r.git(&["commit", "-q", "-m", "delete"]);
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("f.txt", "two\n", "edit");
        let _ = git::run(&r.0, &["merge", "other"]);
        let files = collect(&r.0).unwrap();
        assert_eq!(files.len(), 1);
        assert!(matches!(files[0].body, Body::Whole { .. }));
        run(&r.0, Decide::Theirs, false).unwrap();
        assert!(!r.exists("f.txt"));
    }

    #[test]
    fn keeping_our_side_of_a_modify_delete_keeps_the_file() {
        let r = Repo::new();
        r.commit_file("f.txt", "one\n", "base");
        r.git(&["checkout", "-q", "-b", "other"]);
        r.git(&["rm", "-q", "f.txt"]);
        r.git(&["commit", "-q", "-m", "delete"]);
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("f.txt", "two\n", "edit");
        let _ = git::run(&r.0, &["merge", "other"]);
        run(&r.0, Decide::Ours, false).unwrap();
        assert_eq!(r.read("f.txt"), "two\n");
    }

    #[test]
    fn running_without_an_operation_in_progress_is_refused() {
        let r = Repo::new();
        r.commit_file("f.txt", "x\n", "base");
        let err = run(&r.0, Decide::Ours, false).unwrap_err().to_string();
        assert!(err.contains("no merge"), "{err}");
    }

    #[test]
    fn both_keeps_each_side_of_a_text_hunk() {
        let r = conflicted();
        let mut app = app_for(&r);
        app.key(key('c'));
        app.key(key('c'));
        assert_eq!(app.remaining(), 0);
        for c in &app.files {
            apply(&r.0, c).unwrap();
        }
        let text = r.read("f.txt");
        assert!(text.contains("main2") && text.contains("other2"), "{text}");
    }
}
