// Interactive commit picker for `gitomic drop` when no hash is given (issue #12, part 2). Two
// panes: the eligible commits on the left, the highlighted commit's diff on the right. The
// operator marks commits for removal and submits; the marked hashes are then handed to the
// ordinary drop path, so the picker adds no rewrite logic of its own.
//
// The module is split so that everything except the terminal itself is testable. `App` is a
// plain state machine that consumes key events and reports an `Outcome`; the two operations that
// touch git (loading a diff, pre-checking that a selection can be dropped) sit behind the `Source`
// trait, and rendering takes any ratatui backend.
//
// Keys, list pane:
//   j/k, arrows  move             g/G  first/last        space  toggle the mark
//   l, Right     open the diff    Enter  submit the marked commits (after a y/n confirmation)
//   q, Esc       quit (confirmed first when commits are marked)
// Keys, diff pane:
//   j/k          scroll vertically       h/l  scroll horizontally     g/G  top/bottom
//   Ctrl-d/u     half a page            space  toggle the mark
//   Enter, n     next commit down       Shift-Enter, N  next commit up
//   h at the left edge, Esc  back to the list        q  quit
// Scrolling stops at every edge. Leaving the diff with h needs a press that is not part of a run of
// h presses that was still scrolling, so holding h to reach the left edge cannot overshoot into the
// list.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::{commands, Res};

// Columns moved by one horizontal scroll key press.
const H_STEP: usize = 8;
// Two h presses closer together than this are treated as one continuous run (key auto-repeat or a
// held key), so the second cannot switch panes if the first was still scrolling.
const RUN_WINDOW: Duration = Duration::from_millis(200);
// Diffs longer than this are cut off, so that a commit touching a generated file cannot stall the
// picker or exhaust memory. The cut is announced on the final line.
const MAX_DIFF_LINES: usize = 20_000;

// The two operations the picker needs from git.
pub trait Source {
    // The text shown for one commit: header, stat, and patch.
    fn diff(&mut self, commit: &str) -> Result<String, String>;
    // Verify that the given commits can be dropped together; returns the number of later commits
    // that would be re-applied, or the reason the drop is not possible.
    fn check(&mut self, picked: &[String]) -> Result<usize, String>;
}

// The real source: git for diffs and the drop planner for pre-checks.
struct GitSource {
    cwd: PathBuf,
    root: PathBuf,
}

impl Source for GitSource {
    fn diff(&mut self, commit: &str) -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "show",
                "--no-color",
                "--stat",
                "--patch",
                "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
                commit,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn check(&mut self, picked: &[String]) -> Result<usize, String> {
        commands::check_drop(&self.cwd, picked).map_err(|e| e.to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Focus {
    List,
    Diff,
}

// What the picker is currently waiting for. Only `Browse` interprets navigation keys; the two
// prompts accept y or n and ignore everything else, Enter included, so a stray Return can never
// confirm.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Browse,
    ConfirmDrop { replayed: usize },
    ConfirmQuit,
}

// The result of handling one key event.
#[derive(PartialEq, Debug)]
pub enum Outcome {
    Continue,
    Quit,
    Submit(Vec<String>),
}

struct Entry {
    full: String,
    subject: String,
    marked: bool,
}

// A commit's text, pre-styled once, with the width of its widest line for horizontal scroll limits.
struct DiffView {
    lines: Vec<Line<'static>>,
    max_width: usize,
}

pub struct App {
    entries: Vec<Entry>,
    cursor: usize,
    focus: Focus,
    mode: Mode,
    views: HashMap<String, DiffView>,
    vscroll: usize,
    hscroll: usize,
    // Size of the diff viewport, refreshed on every render and used to bound scrolling.
    view_h: usize,
    view_w: usize,
    last_h: Option<Instant>,
    status: String,
    status_is_error: bool,
    list_state: ListState,
}

impl App {
    // `choices` is (full id, subject) newest first, as `commands::drop_choices` returns it.
    pub fn new(choices: Vec<(String, String)>) -> App {
        App {
            entries: choices
                .into_iter()
                .map(|(full, subject)| Entry {
                    full,
                    subject,
                    marked: false,
                })
                .collect(),
            cursor: 0,
            focus: Focus::List,
            mode: Mode::Browse,
            views: HashMap::new(),
            vscroll: 0,
            hscroll: 0,
            view_h: 20,
            view_w: 80,
            last_h: None,
            status: String::new(),
            status_is_error: false,
            list_state: ListState::default(),
        }
    }

    fn marked_count(&self) -> usize {
        self.entries.iter().filter(|e| e.marked).count()
    }

    fn marked_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.marked)
            .map(|e| e.full.clone())
            .collect()
    }

    // Load the highlighted commit's diff if it is not cached yet. Called before each draw, so the
    // scroll limits used while handling a key always describe what is on screen.
    pub fn ensure_diff(&mut self, src: &mut dyn Source) {
        let Some(entry) = self.entries.get(self.cursor) else {
            return;
        };
        if self.views.contains_key(&entry.full) {
            return;
        }
        let full = entry.full.clone();
        let view = match src.diff(&full) {
            Ok(text) => style_diff(&text),
            Err(e) => style_diff(&format!("could not show {full}: {e}")),
        };
        self.views.insert(full, view);
    }

    fn view(&self) -> Option<&DiffView> {
        self.entries
            .get(self.cursor)
            .and_then(|e| self.views.get(&e.full))
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

    // Move the highlighted commit by `delta`, stopping at either end, and show it from its top
    // left.
    fn step(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = self.cursor.saturating_add_signed(delta).min(last);
        self.jump(next);
    }

    fn jump(&mut self, index: usize) {
        if index != self.cursor {
            self.cursor = index;
            self.vscroll = 0;
            self.hscroll = 0;
        }
    }

    fn toggle(&mut self) {
        if let Some(e) = self.entries.get_mut(self.cursor) {
            e.marked = !e.marked;
        }
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
        match self.mode {
            Mode::ConfirmDrop { .. } => return self.key_confirm_drop(key),
            Mode::ConfirmQuit => return self.key_confirm_quit(key),
            Mode::Browse => {}
        }
        self.status.clear();
        match self.focus {
            Focus::List => self.key_list(key, src),
            Focus::Diff => self.key_diff(key, now),
        }
    }

    fn key_list(&mut self, key: KeyEvent, src: &mut dyn Source) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('g') | KeyCode::Home => self.jump(0),
            KeyCode::Char('G') | KeyCode::End => self.jump(self.entries.len().saturating_sub(1)),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Diff,
            KeyCode::Enter => self.submit(src),
            KeyCode::Char('q') | KeyCode::Esc => return self.request_quit(),
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
            KeyCode::Char('g') | KeyCode::Home => self.vscroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.vscroll = self.max_v(),
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = (self.hscroll + H_STEP).min(self.max_h())
            }
            KeyCode::Char('h') | KeyCode::Left => self.left(now),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => self.step(-1),
            KeyCode::Enter | KeyCode::Char('n') => self.step(1),
            KeyCode::Char('N') => self.step(-1),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Char('q') => return self.request_quit(),
            _ => {}
        }
        Outcome::Continue
    }

    // Horizontal scroll to the left, or back to the list pane once already at the left edge. A
    // press that follows another h press within RUN_WINDOW is part of the same run and never
    // switches panes, so a held key stops at the edge; a fresh press after a pause does switch.
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

    // Enter in the list pane. Pre-checks the selection so that a conflict is reported here, with
    // the selection still editable, rather than after the terminal has been handed back.
    fn submit(&mut self, src: &mut dyn Source) {
        let picked = self.marked_ids();
        if picked.is_empty() {
            self.say("nothing is marked; press space on a commit first", true);
            return;
        }
        match src.check(&picked) {
            Ok(replayed) => self.mode = Mode::ConfirmDrop { replayed },
            Err(reason) => self.say(reason, true),
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

    // Only y and n answer a prompt. Every other key, Enter included, is ignored.
    fn key_confirm_drop(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Submit(self.marked_ids()),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.say("cancelled; selection kept", false);
                Outcome::Continue
            }
            _ => Outcome::Continue,
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

        let focused = self.focus;
        let focus_style = |f: Focus| {
            if focused == f {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new().fg(Color::DarkGray)
            }
        };

        // Left pane: one row per commit, red when marked for removal.
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                let subject = if e.subject.is_empty() {
                    "(no message)"
                } else {
                    &e.subject
                };
                let mark = if e.marked { 'x' } else { ' ' };
                let text = format!("[{mark}] {} {subject}", &e.full[..e.full.len().min(8)]);
                let style = if e.marked {
                    Style::new().fg(Color::Red)
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
        let title = format!(
            " Commits  {} of {} marked ",
            self.marked_count(),
            self.entries.len()
        );
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(focus_style(Focus::List)),
            )
            .highlight_style(cursor_style);
        self.list_state.select(Some(self.cursor));
        frame.render_stateful_widget(list, cols[0], &mut self.list_state);

        // Right pane: only the visible rows are cloned into the widget, so a long diff costs no
        // more than a short one per frame.
        let inner_h = cols[1].height.saturating_sub(2) as usize;
        let inner_w = cols[1].width.saturating_sub(2) as usize;
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
            None => (Vec::new(), 0),
        };
        let heading = match self.entries.get(self.cursor) {
            Some(e) => format!(
                " {}  line {}/{}  col {} ",
                &e.full[..e.full.len().min(8)],
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
        frame.render_widget(diff, cols[1]);

        // Bottom line: a prompt, a message, or the key hints for the focused pane.
        let bar = match self.mode {
            Mode::ConfirmDrop { replayed } => Line::from(Span::styled(
                format!(
                    " Drop {} marked commit(s)? {replayed} later commit(s) will be re-applied. \
                     [y/n] ",
                    self.marked_count()
                ),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Mode::ConfirmQuit => Line::from(Span::styled(
                format!(" Quit and discard {} mark(s)? [y/n] ", self.marked_count()),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Mode::Browse if !self.status.is_empty() => {
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
            Mode::Browse => Line::from(Span::styled(
                match self.focus {
                    Focus::List => " j/k move  space mark  l diff  Enter drop marked  q quit ",
                    Focus::Diff => {
                        " j/k h/l scroll  Enter/n next  S-Enter/N prev  space mark  h list  q quit "
                    }
                },
                Style::new().fg(Color::DarkGray),
            )),
        };
        frame.render_widget(Paragraph::new(bar), rows[1]);
    }
}

// Turn git's text into styled lines. Tabs are expanded and control characters replaced, since a raw
// escape sequence in a diffed file would otherwise be interpreted by the terminal. Patch lines are
// coloured only after the first `diff --git` line, so a commit message that happens to begin with
// '+' or '-' keeps the default style.
fn style_diff(text: &str) -> DiffView {
    let mut lines = Vec::new();
    let mut max_width = 0;
    let mut in_patch = false;
    for (n, raw) in text.lines().enumerate() {
        if n >= MAX_DIFF_LINES {
            let more = text.lines().count() - MAX_DIFF_LINES;
            lines.push(Line::from(Span::styled(
                format!("... {more} more line(s) not shown"),
                Style::new().fg(Color::Yellow),
            )));
            break;
        }
        let clean: String = raw
            .chars()
            .flat_map(|c| match c {
                '\t' => vec![' '; 4],
                c if c.is_control() => vec!['\u{b7}'],
                c => vec![c],
            })
            .collect();
        if clean.starts_with("diff --git") {
            in_patch = true;
        }
        let style = if in_patch {
            if clean.starts_with("+++") || clean.starts_with("---") {
                Style::new().add_modifier(Modifier::BOLD)
            } else if clean.starts_with('+') {
                Style::new().fg(Color::Green)
            } else if clean.starts_with('-') {
                Style::new().fg(Color::Red)
            } else if clean.starts_with("@@") {
                Style::new().fg(Color::Cyan)
            } else if !clean.starts_with(' ') && !clean.is_empty() {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            }
        } else if n < 3 {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        let line = Line::from(Span::styled(clean, style));
        max_width = max_width.max(line.width());
        lines.push(line);
    }
    DiffView { lines, max_width }
}

// Run the picker on the real terminal. Returns the full ids of the commits the operator marked and
// confirmed, or None when the picker was left without confirming anything. Errors when there is no
// terminal to draw on or no commit that could be dropped.
pub fn run(cwd: &Path) -> Res<Option<Vec<String>>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("drop: no commit given, and the interactive picker needs a terminal".into());
    }
    let choices = commands::drop_choices(cwd)?;
    if choices.is_empty() {
        return Err("drop: no commit is eligible to be dropped".into());
    }
    let root = crate::git::work_tree(cwd)?;
    let mut src = GitSource {
        cwd: cwd.to_path_buf(),
        root,
    };
    let mut app = App::new(choices);

    let mut terminal = ratatui::try_init()?;
    // Terminals that speak the kitty keyboard protocol then report Shift-Enter as such; others send
    // a plain Return for it, and N covers that case. The flag is pushed without first asking the
    // terminal whether it is supported: the query blocks until the terminal answers, and a terminal
    // that does not know the sequence ignores the push and the matching pop.
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let result = event_loop(&mut terminal, &mut app, &mut src);
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    ratatui::restore();

    match result? {
        Outcome::Submit(ids) => Ok(Some(ids)),
        _ => Ok(None),
    }
}

fn event_loop(term: &mut DefaultTerminal, app: &mut App, src: &mut dyn Source) -> Res<Outcome> {
    loop {
        app.ensure_diff(src);
        term.draw(|f| app.render(f))?;
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // A source that serves canned diffs and records what it was asked to check.
    struct Fake {
        check_result: Result<usize, String>,
        checked: Vec<Vec<String>>,
    }

    impl Fake {
        fn ok() -> Fake {
            Fake {
                check_result: Ok(2),
                checked: Vec::new(),
            }
        }
    }

    impl Source for Fake {
        fn diff(&mut self, commit: &str) -> Result<String, String> {
            let mut text =
                format!("commit {commit}\nAuthor: T <t@e.invalid>\nDate:   now\n\n    msg\n");
            text.push_str(
                "\n a.txt | 2 +-\n\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n",
            );
            text.push_str("@@ -1,2 +1,2 @@\n context\n-old line\n");
            text.push_str(&format!("+{}\n", "w".repeat(200)));
            for i in 0..30 {
                text.push_str(&format!(" filler {i}\n"));
            }
            Ok(text)
        }

        fn check(&mut self, picked: &[String]) -> Result<usize, String> {
            self.checked.push(picked.to_vec());
            self.check_result.clone()
        }
    }

    fn id(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn app() -> (App, Fake) {
        let choices = vec![
            (id('a'), "newest".to_string()),
            (id('b'), String::new()),
            (id('c'), "oldest".to_string()),
        ];
        let mut app = App::new(choices);
        app.view_h = 10;
        app.view_w = 40;
        let mut fake = Fake::ok();
        app.ensure_diff(&mut fake);
        (app, fake)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, fake: &mut Fake, code: KeyCode, at: Instant) -> Outcome {
        let out = app.handle_key(key(code), at, fake);
        app.ensure_diff(fake);
        out
    }

    fn ch(c: char) -> KeyCode {
        KeyCode::Char(c)
    }

    #[test]
    fn list_moves_and_stops_at_both_ends() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('k'), t);
        assert_eq!(a.cursor, 0);
        for _ in 0..5 {
            press(&mut a, &mut f, ch('j'), t);
        }
        assert_eq!(a.cursor, 2);
        press(&mut a, &mut f, ch('g'), t);
        assert_eq!(a.cursor, 0);
        press(&mut a, &mut f, ch('G'), t);
        assert_eq!(a.cursor, 2);
    }

    #[test]
    fn space_toggles_the_mark_in_both_panes() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch(' '), t);
        assert!(a.entries[0].marked);
        press(&mut a, &mut f, ch(' '), t);
        assert!(!a.entries[0].marked);
        press(&mut a, &mut f, ch('l'), t);
        press(&mut a, &mut f, ch(' '), t);
        assert!(a.entries[0].marked);
        assert_eq!(a.focus, Focus::Diff);
    }

    #[test]
    fn l_enters_the_diff_and_a_lone_h_returns() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('l'), t);
        assert_eq!(a.focus, Focus::Diff);
        press(&mut a, &mut f, ch('h'), t);
        assert_eq!(a.focus, Focus::List);
    }

    #[test]
    fn diff_scrolling_stops_at_the_bottom_and_never_changes_pane() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('l'), t);
        for _ in 0..500 {
            press(&mut a, &mut f, ch('j'), t);
        }
        assert_eq!(a.vscroll, a.max_v());
        assert!(a.vscroll > 0);
        assert_eq!(a.focus, Focus::Diff);
        assert_eq!(a.cursor, 0);
        for _ in 0..500 {
            press(&mut a, &mut f, ch('k'), t);
        }
        assert_eq!(a.vscroll, 0);
        assert_eq!(a.focus, Focus::Diff);
    }

    #[test]
    fn horizontal_scroll_stops_at_the_right_edge_and_stays_in_the_diff() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('l'), t);
        for _ in 0..100 {
            press(&mut a, &mut f, ch('l'), t);
        }
        assert_eq!(a.hscroll, a.max_h());
        assert!(a.hscroll > 0);
        assert_eq!(a.focus, Focus::Diff);
    }

    #[test]
    fn a_run_of_h_presses_stops_at_the_left_edge_but_a_later_press_leaves() {
        let (mut a, mut f) = app();
        let t0 = Instant::now();
        press(&mut a, &mut f, ch('l'), t0);
        for _ in 0..3 {
            press(&mut a, &mut f, ch('l'), t0);
        }
        assert_eq!(a.hscroll, 3 * H_STEP);
        // Auto-repeat: every press 30 ms after the previous one.
        let mut t = t0 + Duration::from_secs(1);
        for _ in 0..10 {
            press(&mut a, &mut f, ch('h'), t);
            t += Duration::from_millis(30);
        }
        assert_eq!(a.hscroll, 0);
        assert_eq!(
            a.focus,
            Focus::Diff,
            "a held key must not overshoot into the list"
        );
        // A fresh press after a pause is a deliberate one.
        t += Duration::from_millis(600);
        press(&mut a, &mut f, ch('h'), t);
        assert_eq!(a.focus, Focus::List);
    }

    #[test]
    fn enter_in_the_diff_moves_down_and_shift_enter_or_n_moves_up() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('l'), t);
        press(&mut a, &mut f, ch('j'), t);
        press(&mut a, &mut f, ch('l'), t);
        assert!(a.vscroll > 0 || a.hscroll > 0);
        press(&mut a, &mut f, KeyCode::Enter, t);
        assert_eq!(a.cursor, 1);
        assert_eq!(
            (a.vscroll, a.hscroll),
            (0, 0),
            "a new commit shows from the top left"
        );
        assert_eq!(a.focus, Focus::Diff);
        press(&mut a, &mut f, KeyCode::Enter, t);
        press(&mut a, &mut f, KeyCode::Enter, t);
        assert_eq!(a.cursor, 2, "stops at the last commit");
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        a.handle_key(shift, t, &mut f);
        assert_eq!(a.cursor, 1);
        press(&mut a, &mut f, ch('N'), t);
        assert_eq!(a.cursor, 0);
        press(&mut a, &mut f, ch('N'), t);
        assert_eq!(a.cursor, 0);
        press(&mut a, &mut f, ch('n'), t);
        assert_eq!(a.cursor, 1);
    }

    #[test]
    fn enter_with_nothing_marked_asks_for_a_mark_and_checks_nothing() {
        let (mut a, mut f) = app();
        let out = press(&mut a, &mut f, KeyCode::Enter, Instant::now());
        assert_eq!(out, Outcome::Continue);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("nothing is marked"));
        assert!(f.checked.is_empty());
    }

    #[test]
    fn submit_needs_a_y_and_ignores_enter_and_other_keys() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch(' '), t);
        press(&mut a, &mut f, ch('G'), t);
        press(&mut a, &mut f, ch(' '), t);
        assert_eq!(press(&mut a, &mut f, KeyCode::Enter, t), Outcome::Continue);
        assert_eq!(a.mode, Mode::ConfirmDrop { replayed: 2 });
        assert_eq!(f.checked, vec![vec![id('a'), id('c')]]);
        for code in [
            KeyCode::Enter,
            ch(' '),
            ch('j'),
            ch('q'),
            ch('x'),
            KeyCode::Tab,
        ] {
            assert_eq!(press(&mut a, &mut f, code, t), Outcome::Continue);
            assert_eq!(
                a.mode,
                Mode::ConfirmDrop { replayed: 2 },
                "{code:?} must not answer"
            );
        }
        assert_eq!(
            press(&mut a, &mut f, ch('y'), t),
            Outcome::Submit(vec![id('a'), id('c')])
        );
    }

    #[test]
    fn declining_the_confirmation_keeps_the_selection() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch(' '), t);
        press(&mut a, &mut f, KeyCode::Enter, t);
        assert_eq!(press(&mut a, &mut f, ch('n'), t), Outcome::Continue);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.entries[0].marked);
        press(&mut a, &mut f, KeyCode::Enter, t);
        assert_eq!(press(&mut a, &mut f, KeyCode::Esc, t), Outcome::Continue);
        assert_eq!(a.mode, Mode::Browse);
    }

    #[test]
    fn a_failed_pre_check_reports_and_leaves_the_selection_editable() {
        let (mut a, mut f) = app();
        f.check_result = Err("drop: bbbbbbbbbbbb does not apply".to_string());
        let t = Instant::now();
        press(&mut a, &mut f, ch(' '), t);
        press(&mut a, &mut f, KeyCode::Enter, t);
        assert_eq!(a.mode, Mode::Browse);
        assert!(a.status.contains("does not apply"));
        assert!(a.status_is_error);
        assert!(a.entries[0].marked);
    }

    #[test]
    fn quitting_is_immediate_without_marks_and_confirmed_with_them() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        assert_eq!(press(&mut a, &mut f, ch('q'), t), Outcome::Quit);

        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '), t);
        assert_eq!(press(&mut a, &mut f, ch('q'), t), Outcome::Continue);
        assert_eq!(a.mode, Mode::ConfirmQuit);
        assert_eq!(press(&mut a, &mut f, KeyCode::Enter, t), Outcome::Continue);
        assert_eq!(a.mode, Mode::ConfirmQuit);
        assert_eq!(press(&mut a, &mut f, ch('n'), t), Outcome::Continue);
        assert_eq!(a.mode, Mode::Browse);
        press(&mut a, &mut f, ch('q'), t);
        assert_eq!(press(&mut a, &mut f, ch('y'), t), Outcome::Quit);
    }

    #[test]
    fn ctrl_c_always_quits() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '), Instant::now());
        let ctrl_c = KeyEvent::new(ch('c'), KeyModifiers::CONTROL);
        assert_eq!(a.handle_key(ctrl_c, Instant::now(), &mut f), Outcome::Quit);
    }

    #[test]
    fn key_release_events_are_ignored() {
        let (mut a, mut f) = app();
        let mut ev = key(ch('j'));
        ev.kind = KeyEventKind::Release;
        a.handle_key(ev, Instant::now(), &mut f);
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn diff_text_is_sanitised_and_colours_only_patch_lines() {
        let text = "commit x\nAuthor: a\nDate: d\n\n+ not a patch line\n\
                    diff --git a/f b/f\n+add\n-del\n\tTab\x1b[31m\n";
        let v = style_diff(text);
        let header = Style::new().fg(Color::Yellow);
        assert_eq!(v.lines[0].spans[0].style, header);
        assert_eq!(
            v.lines[4].spans[0].style,
            Style::new(),
            "message text before the patch"
        );
        assert_eq!(v.lines[6].spans[0].style, Style::new().fg(Color::Green));
        assert_eq!(v.lines[7].spans[0].style, Style::new().fg(Color::Red));
        let last: String = v.lines[8]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!last.contains('\x1b') && !last.contains('\t'), "{last:?}");
    }

    #[test]
    fn overlong_diffs_are_cut_with_a_notice() {
        let text = "x\n".repeat(MAX_DIFF_LINES + 5);
        let v = style_diff(&text);
        assert_eq!(v.lines.len(), MAX_DIFF_LINES + 1);
        let last: String = v.lines[MAX_DIFF_LINES]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("5 more line(s) not shown"), "{last}");
    }

    // Render to a test backend and return the screen as text.
    fn screen(app: &mut App, fake: &mut Fake, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        app.ensure_diff(fake);
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
    fn render_shows_hashes_marks_diff_and_hints() {
        let (mut a, mut f) = app();
        press(&mut a, &mut f, ch(' '), Instant::now());
        let s = screen(&mut a, &mut f, 100, 20);
        assert!(s.contains("[x] aaaaaaaa newest"), "{s}");
        assert!(s.contains("[ ] bbbbbbbb (no message)"), "{s}");
        assert!(s.contains("[ ] cccccccc oldest"), "{s}");
        assert!(s.contains("1 of 3 marked"), "{s}");
        assert!(s.contains("+++ b/a.txt"), "{s}");
        assert!(s.contains("Enter drop marked"), "{s}");
    }

    #[test]
    fn render_shows_the_confirmation_prompt() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch(' '), t);
        press(&mut a, &mut f, KeyCode::Enter, t);
        let s = screen(&mut a, &mut f, 100, 20);
        assert!(
            s.contains("Drop 1 marked commit(s)? 2 later commit(s) will be re-applied. [y/n]"),
            "{s}"
        );
    }

    #[test]
    fn render_survives_tiny_terminals() {
        let (mut a, mut f) = app();
        for (w, h) in [(1, 1), (10, 3), (30, 5), (200, 3)] {
            screen(&mut a, &mut f, w, h);
        }
    }

    #[test]
    fn horizontal_scroll_moves_the_rendered_text() {
        let (mut a, mut f) = app();
        let t = Instant::now();
        press(&mut a, &mut f, ch('l'), t);
        press(&mut a, &mut f, ch('l'), t);
        let s = screen(&mut a, &mut f, 100, 20);
        assert!(s.contains("col 9"), "{s}");
        assert!(
            !s.contains("+++ b/a.txt"),
            "scrolled past the start of that line: {s}"
        );
    }
}
