// `gitomic history <path>` (issue #33): the commits that changed one file on the checked-out branch,
// each with the change it made to that file, and a way back to any of those versions.
//
// The list is `restore::path_history` over the tip of the checked-out branch, so it is the same
// reading as the `H` overlay of the restore screen, except that it covers the branch the work tree is
// on instead of the other branches, and that it works for a file that matches HEAD (the restore
// screen lists only files that differ). A commit that deleted the file is not listed, since it left
// nothing to look at; the version before it is, so a file that is gone can still be recovered. The
// atomic commits of an open session are listed like any other (their placeholder message shows as
// "(no message)" until `finish` stamps them). Renames are not followed: the file is looked up under
// the one path, as in `restore`.
//
// The right pane shows what the highlighted commit did to the file (`git show` restricted to the
// path, message included). `v` switches it to what restoring that version would change on HEAD, the
// text the restore screen shows. Enter (or `r`) restores the highlighted version behind a y/n
// confirmation that Enter cannot answer; the screen returns that version and the restore itself is
// `cherry::apply_specs`, the pipeline `restore` uses, so a session records it as one atomic commit,
// `gitomic drop` undoes it, and a local edit to the file refuses it when there is no session.
//
// Without a terminal on stdin and stdout the command prints the list instead (short id, date,
// subject), which is also what scripts can read; `gitomic restore --from <id> <path>` takes any of
// those ids.
//
// Keys, list pane:
//   j/k, arrows  move   g/G  first/last   PgUp/PgDn  a page   l, Right  open the diff pane
//   v            switch the right pane between the commit's change and "restoring it would change"
//   Enter, r     restore the highlighted version (y/n)         q, Esc  quit
// Keys, diff pane: j/k h/l g/G Ctrl-d/u PgUp/PgDn scroll, Enter/n next (older) commit, N previous,
//   v, r, Esc back to the list.
//
// As in the other screens everything except the terminal is testable: `App` consumes key events and
// reports an `Outcome`, and rendering takes any ratatui backend.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::pick::{style_diff, with_terminal, DiffView, H_STEP, RUN_WINDOW};
use crate::restore::{self, HistRow, Version};
use crate::restore_ui::{civil, short};
use crate::{cherry, git, Res};

// Output above this many bytes is cut, so the change of a huge generated file is never read in full.
const MAX_CHANGE_BYTES: usize = 2 * 1024 * 1024;

pub struct Opts {
    // The file, relative to the directory the command runs in.
    pub path: String,
    pub dry_run: bool,
    pub patch_only: bool,
}

// What `commit` did to `path`: the commit's header and message and its diff restricted to the file.
// Works for a root commit and shows nothing for a merge that left the file as one parent had it.
fn change_text(root: &Path, commit: &str, path: &str) -> Res<String> {
    let literal = format!(":(literal){path}");
    let out = git::run_capped(
        root,
        &[
            "show",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            commit,
            "--",
            &literal,
        ],
        MAX_CHANGE_BYTES,
        None,
    )?;
    let mut text = out.text;
    if out.truncated {
        text.push_str("\n... (truncated)\n");
    }
    Ok(text)
}

// The commits of the checked-out branch that changed `path`, newest first; the flag is true when
// older ones were left out.
fn read_rows(root: &Path, path: &str) -> Res<(Vec<HistRow>, bool)> {
    let head =
        git::rev_parse(root, "HEAD").map_err(|_| "history: the repository has no commit yet")?;
    let tips = [("HEAD".to_string(), Arc::<str>::from(head.as_str()))];
    restore::path_history(root, &tips, path, None)
}

// One line per commit, as printed when there is no terminal.
fn listing(rows: &[HistRow], cut: bool) -> String {
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "{} {}  {}\n",
            short(&r.version.commit),
            civil(r.when),
            subject_of(r)
        ));
    }
    if cut {
        out.push_str("(older commits are not listed)\n");
    }
    out
}

fn subject_of(r: &HistRow) -> &str {
    if r.subject.trim().is_empty() {
        "(no message)"
    } else {
        &r.subject
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    List,
    Diff,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Browse,
    Confirm,
}

#[derive(PartialEq, Debug)]
pub enum Outcome {
    Continue,
    Quit,
    // The version to restore.
    Restore(Version),
}

pub struct App {
    root: PathBuf,
    path: String,
    rows: Vec<HistRow>,
    cut: bool,
    cursor: usize,
    focus: Focus,
    mode: Mode,
    // Show what restoring would change on HEAD instead of the commit's own change.
    vs_head: bool,
    // (commit, vs_head) -> the styled text.
    views: HashMap<(String, bool), DiffView>,
    vscroll: usize,
    hscroll: usize,
    view_h: usize,
    view_w: usize,
    last_h: Option<Instant>,
    status: String,
    list_state: ListState,
}

fn plain(text: &str) -> DiffView {
    style_diff(text)
}

impl App {
    pub fn new(root: &Path, path: &str, rows: Vec<HistRow>, cut: bool) -> App {
        let mut app = App {
            root: root.to_path_buf(),
            path: path.to_string(),
            rows,
            cut,
            cursor: 0,
            focus: Focus::List,
            mode: Mode::Browse,
            vs_head: false,
            views: HashMap::new(),
            vscroll: 0,
            hscroll: 0,
            view_h: 20,
            view_w: 80,
            last_h: None,
            status: String::new(),
            list_state: ListState::default(),
        };
        app.ensure_view();
        app
    }

    fn current(&self) -> Option<&HistRow> {
        self.rows.get(self.cursor)
    }

    fn key(&self) -> Option<(String, bool)> {
        self.current()
            .map(|r| (r.version.commit.to_string(), self.vs_head))
    }

    // Read the text of the highlighted commit unless it is already kept. One git command on one
    // path, so it is read on the spot rather than on a worker.
    fn ensure_view(&mut self) {
        let (Some(key), Some(row)) = (self.key(), self.current()) else {
            return;
        };
        if self.views.contains_key(&key) {
            return;
        }
        let text = if self.vs_head {
            restore::diff_text(&self.root, &row.version, &self.path, None).map(|t| {
                if t.trim().is_empty() {
                    "This version is the same as HEAD's; restoring it changes nothing.".to_string()
                } else {
                    t
                }
            })
        } else {
            change_text(&self.root, &row.version.commit, &self.path).map(|t| {
                if t.trim().is_empty() {
                    "(this commit shows no change to the file; it is a merge)".to_string()
                } else {
                    t
                }
            })
        };
        let view = match text {
            Ok(t) => plain(&t),
            Err(e) => plain(&format!("could not read it: {e}")),
        };
        self.views.insert(key, view);
    }

    fn view(&self) -> Option<&DiffView> {
        self.key().and_then(|k| self.views.get(&k))
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

    fn step(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        let to = (self.cursor as isize + delta).clamp(0, last) as usize;
        if to != self.cursor {
            self.cursor = to;
            self.vscroll = 0;
            self.hscroll = 0;
            self.ensure_view();
        }
    }

    fn page(&self) -> isize {
        self.view_h.max(1) as isize
    }

    fn toggle_view(&mut self) {
        self.vs_head = !self.vs_head;
        self.vscroll = 0;
        self.hscroll = 0;
        self.ensure_view();
    }

    fn ask_restore(&mut self) {
        if self.current().is_some() {
            self.mode = Mode::Confirm;
        }
    }

    // Same rule as the other screens: an h that follows another within RUN_WINDOW is part of one
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

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Quit;
        }
        if self.mode == Mode::Confirm {
            return self.key_confirm(key);
        }
        self.status.clear();
        match self.focus {
            Focus::List => self.key_list(key),
            Focus::Diff => self.key_diff(key, now),
        }
    }

    // Only y and n answer, like the other confirmations.
    fn key_confirm(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => match self.current() {
                Some(r) => Outcome::Restore(r.version.clone()),
                None => Outcome::Continue,
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn key_list(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('g') | KeyCode::Home => self.step(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.step(isize::MAX / 2),
            KeyCode::PageDown => self.step(self.page()),
            KeyCode::PageUp => self.step(-self.page()),
            KeyCode::Char('l') | KeyCode::Right if !self.rows.is_empty() => {
                self.focus = Focus::Diff
            }
            KeyCode::Char('v') => self.toggle_view(),
            KeyCode::Enter | KeyCode::Char('r') => self.ask_restore(),
            KeyCode::Char('q') | KeyCode::Esc => return Outcome::Quit,
            _ => {}
        }
        Outcome::Continue
    }

    fn key_diff(&mut self, key: KeyEvent, now: Instant) -> Outcome {
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
            KeyCode::PageDown => {
                self.vscroll = (self.vscroll + self.view_h.max(1)).min(self.max_v())
            }
            KeyCode::PageUp => self.vscroll = self.vscroll.saturating_sub(self.view_h.max(1)),
            KeyCode::Char('g') | KeyCode::Home => self.vscroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.vscroll = self.max_v(),
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = (self.hscroll + H_STEP).min(self.max_h())
            }
            KeyCode::Char('h') | KeyCode::Left => self.left(now),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => self.step(-1),
            KeyCode::Enter | KeyCode::Char('n') => self.step(1),
            KeyCode::Char('N') => self.step(-1),
            KeyCode::Char('v') => self.toggle_view(),
            KeyCode::Char('r') => self.ask_restore(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Char('q') => return Outcome::Quit,
            _ => {}
        }
        Outcome::Continue
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let left_w = (area.width * 2 / 5).clamp(28, 60).min(area.width / 2);
        let cols =
            Layout::horizontal([Constraint::Length(left_w), Constraint::Min(1)]).split(rows[0]);
        self.render_browse(frame, cols[0], cols[1]);
        self.render_bar(frame, rows[1]);
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
            .rows
            .iter()
            .map(|r| {
                ListItem::new(format!(
                    "{} {}  {}",
                    short(&r.version.commit),
                    civil(r.when),
                    subject_of(r)
                ))
            })
            .collect();
        let more = if self.cut { "+" } else { "" };
        let title = format!(" {}  {}{more} commit(s) ", self.path, self.rows.len());
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
        self.list_state.select(if self.rows.is_empty() {
            None
        } else {
            Some(self.cursor)
        });
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
            None => (vec![Line::from("No commit changed this file.")], 0),
        };
        let what = if self.vs_head {
            "restoring it would change"
        } else {
            "the commit's change"
        };
        let heading = match self.current() {
            Some(r) => format!(
                " {}  {what}  line {}/{} ",
                short(&r.version.commit),
                (self.vscroll + 1).min(total.max(1)),
                total
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

    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        let prompt = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let bar = match self.mode {
            Mode::Confirm => {
                let id = self.current().map_or("", |r| short(&r.version.commit));
                Line::from(Span::styled(
                    format!(
                        " Restore {} as of {id}? The local copy is overwritten. [y/n] ",
                        self.path
                    ),
                    prompt,
                ))
            }
            Mode::Browse if !self.status.is_empty() => Line::from(Span::styled(
                format!(" {} ", self.status),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )),
            Mode::Browse => match self.focus {
                Focus::List => hint(
                    " j/k PgUp/PgDn move  l diff  v change/vs HEAD  Enter restore this version  q quit",
                ),
                Focus::Diff => hint(
                    " j/k h/l PgUp/PgDn scroll  n/N next/prev commit  v change/vs HEAD  r restore  Esc back",
                ),
            },
        };
        frame.render_widget(Paragraph::new(bar), area);
    }
}

fn hint(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

fn event_loop(term: &mut DefaultTerminal, app: &mut App) -> Res<Outcome> {
    loop {
        term.draw(|f| app.render(f))?;
        if !event::poll(Duration::from_secs(3600))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match app.handle_key(key, Instant::now()) {
                Outcome::Continue => {}
                done => return Ok(done),
            }
        }
    }
}

// Entry point for the command.
pub fn run(cwd: &Path, opts: Opts) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let path = restore::from_top(cwd, &opts.path)?;
    if path.is_empty() {
        return Err("history: name a file, not the top of the repository".into());
    }
    let (rows, cut) = read_rows(&root, &path)?;
    if rows.is_empty() {
        return Err(format!("history: no commit on the checked-out branch changed {path}").into());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        print!("{}", listing(&rows, cut));
        return Ok(());
    }
    let mut app = App::new(&root, &path, rows, cut);
    let outcome = with_terminal(|term| event_loop(term, &mut app))?;
    let Outcome::Restore(version) = outcome else {
        println!("gitomic: nothing restored");
        return Ok(());
    };
    let spec = restore::spec_for(&root, &version, &path)?;
    cherry::apply_specs(cwd, vec![spec], opts.dry_run, opts.patch_only)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testrepo::Repo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // f.txt in three versions (v1, v2, v3) with an unrelated commit between the last two.
    fn setup() -> Repo {
        let r = Repo::new();
        r.commit_file("f.txt", "v1\n", "first");
        r.commit_file("f.txt", "v2\n", "second");
        r.commit_file("other.txt", "x\n", "unrelated");
        r.commit_file("f.txt", "v3\n", "third");
        r
    }

    fn app_for(r: &Repo, path: &str) -> App {
        let (rows, cut) = read_rows(&r.0, path).unwrap();
        App::new(&r.0, path, rows, cut)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, code: KeyCode) -> Outcome {
        app.handle_key(key(code), Instant::now())
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

    fn subjects(app: &App) -> Vec<&str> {
        app.rows.iter().map(|r| r.subject.as_str()).collect()
    }

    #[test]
    fn only_the_commits_that_changed_the_file_are_listed_newest_first() {
        let r = setup();
        let app = app_for(&r, "f.txt");
        assert_eq!(subjects(&app), ["third", "second", "first"]);
        assert!(!app.cut);
        let other = app_for(&r, "other.txt");
        assert_eq!(subjects(&other), ["unrelated"]);
    }

    #[test]
    fn a_file_that_matches_head_is_listed_too() {
        // The restore screen lists only files that differ from HEAD; this one has no such rule.
        let r = setup();
        let (rows, _) = read_rows(&r.0, "f.txt").unwrap();
        assert_eq!(rows[0].version.commit.as_ref(), r.head());
    }

    #[test]
    fn a_deleted_file_still_offers_its_earlier_versions() {
        let r = setup();
        r.git(&["rm", "-q", "f.txt"]);
        r.git(&["commit", "-q", "-m", "remove"]);
        let app = app_for(&r, "f.txt");
        assert_eq!(
            subjects(&app),
            ["third", "second", "first"],
            "the deleting commit is not listed"
        );
    }

    #[test]
    fn a_path_nothing_changed_has_no_rows() {
        let r = setup();
        let (rows, _) = read_rows(&r.0, "nope.txt").unwrap();
        assert!(rows.is_empty());
        assert!(run(
            &r.0,
            Opts {
                path: "nope.txt".into(),
                dry_run: false,
                patch_only: false
            }
        )
        .is_err());
    }

    #[test]
    fn the_listing_has_one_line_per_commit_and_names_a_missing_message() {
        let r = setup();
        r.write("f.txt", "v4\n");
        r.git(&["add", "f.txt"]);
        r.git(&["commit", "-q", "--allow-empty-message", "-m", ""]);
        let (rows, cut) = read_rows(&r.0, "f.txt").unwrap();
        let text = listing(&rows, cut);
        assert_eq!(text.lines().count(), 4);
        assert!(
            text.lines().next().unwrap().ends_with("(no message)"),
            "{text}"
        );
        assert!(text.contains("  second"), "{text}");
    }

    #[test]
    fn the_right_pane_shows_what_the_commit_did_to_the_file() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        // Newest commit: v2 -> v3.
        let shown = screen(&mut app, 120, 20);
        assert!(shown.contains("-v2") && shown.contains("+v3"), "{shown}");
        press(&mut app, KeyCode::Char('j'));
        let shown = screen(&mut app, 120, 20);
        assert!(shown.contains("-v1") && shown.contains("+v2"), "{shown}");
        assert!(!shown.contains("+v3"));
        // The first commit created the file.
        press(&mut app, KeyCode::Char('j'));
        let shown = screen(&mut app, 120, 20);
        assert!(
            shown.contains("+v1") && shown.contains("new file"),
            "{shown}"
        );
    }

    #[test]
    fn v_switches_to_what_restoring_would_change_and_back() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        let vs = screen(&mut app, 120, 20);
        assert!(vs.contains("restoring it would change"), "{vs}");
        assert!(vs.contains("-v3") && vs.contains("+v2"), "{vs}");
        press(&mut app, KeyCode::Char('v'));
        let own = screen(&mut app, 120, 20);
        assert!(
            own.contains("the commit's change") && own.contains("+v2"),
            "{own}"
        );
    }

    #[test]
    fn the_newest_version_says_restoring_it_changes_nothing() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::Char('v'));
        let shown = screen(&mut app, 120, 20);
        assert!(shown.contains("restoring it changes nothing"), "{shown}");
    }

    #[test]
    fn movement_stops_at_both_ends_and_pages_move_a_screenful() {
        let r = Repo::new();
        for i in 0..30 {
            r.commit_file("f.txt", &format!("v{i}\n"), &format!("c{i}"));
        }
        let mut app = app_for(&r, "f.txt");
        app.view_h = 10;
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.cursor, 0);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.cursor, 10);
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.cursor, 29, "the end of the list stops the page");
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.cursor, 19);
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.cursor, 0);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.cursor, 29);
    }

    #[test]
    fn enter_asks_and_only_y_restores() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(press(&mut app, KeyCode::Enter), Outcome::Continue);
        assert_eq!(app.mode, Mode::Confirm);
        // Enter and other keys do not answer.
        assert_eq!(press(&mut app, KeyCode::Enter), Outcome::Continue);
        assert_eq!(press(&mut app, KeyCode::Char('x')), Outcome::Continue);
        assert_eq!(app.mode, Mode::Confirm);
        let shown = screen(&mut app, 120, 20);
        assert!(shown.contains("Restore f.txt as of"), "{shown}");
        match press(&mut app, KeyCode::Char('y')) {
            Outcome::Restore(v) => {
                assert_eq!(v.commit.as_ref(), r.git(&["rev-parse", "HEAD~2"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn n_and_escape_cancel_the_confirmation_and_q_quits() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(press(&mut app, KeyCode::Char('n')), Outcome::Continue);
        assert_eq!(app.mode, Mode::Browse);
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(press(&mut app, KeyCode::Esc), Outcome::Continue);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(press(&mut app, KeyCode::Char('q')), Outcome::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(ctrl_c, Instant::now()), Outcome::Quit);
    }

    #[test]
    fn the_diff_pane_moves_between_commits_and_escape_returns() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.focus, Focus::Diff);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.cursor, 1);
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.cursor, 0);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn a_confirmed_version_comes_back_through_the_restore_pipeline() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Enter);
        let Outcome::Restore(v) = press(&mut app, KeyCode::Char('y')) else {
            panic!("expected a version");
        };
        let spec = restore::spec_for(&r.0, &v, "f.txt").unwrap();
        cherry::apply_specs(&r.0, vec![spec], false, false).unwrap();
        assert_eq!(r.read("f.txt"), "v1\n");
        assert_eq!(r.git(&["status", "--porcelain"]), " M f.txt");
    }

    #[test]
    fn tiny_terminals_and_an_empty_list_do_not_panic() {
        let r = setup();
        let mut app = app_for(&r, "f.txt");
        for (w, h) in [(1, 1), (10, 3), (30, 5)] {
            let _ = screen(&mut app, w, h);
        }
        let mut empty = App::new(&r.0, "f.txt", Vec::new(), false);
        let shown = screen(&mut empty, 80, 10);
        assert!(shown.contains("No commit changed this file."), "{shown}");
        press(&mut empty, KeyCode::Char('j'));
        press(&mut empty, KeyCode::Enter);
        assert_eq!(empty.mode, Mode::Browse);
    }
}
