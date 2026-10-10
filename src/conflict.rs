// Conflict hunks of a text file (issue #13). `git merge-tree` leaves a conflicted file in the
// result tree with conflict markers in place; this module splits such a file into plain text and
// conflict hunks, records a per-hunk decision, and renders the file again once every hunk has one.
// It has no dependency on git or the terminal, so the whole module is exercised by ordinary unit
// tests.
//
// Terminology follows the picker: side A is the copy already in the tree (the checked-out branch),
// side B is the version carried by the commit being picked.

// The decision for one conflict hunk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    // Keep the tree copy.
    A,
    // Take the picked commit's version.
    B,
    // Keep both, tree copy first.
    Both,
}

impl Side {
    // One-letter form used in patch headers and on screen.
    pub fn letter(self) -> char {
        match self {
            Side::A => 'A',
            Side::B => 'B',
            Side::Both => 'C',
        }
    }

    #[allow(dead_code)]
    pub fn from_letter(c: char) -> Option<Side> {
        match c {
            'A' => Some(Side::A),
            'B' => Some(Side::B),
            'C' => Some(Side::Both),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk {
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
    pub choice: Option<Side>,
}

impl Hunk {
    // The bytes this hunk contributes under its current decision, or None while undecided.
    fn chosen(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        match self.choice? {
            Side::A => out.extend_from_slice(&self.ours),
            Side::B => out.extend_from_slice(&self.theirs),
            Side::Both => {
                out.extend_from_slice(&self.ours);
                out.extend_from_slice(&self.theirs);
            }
        }
        Some(out)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Segment {
    Text(Vec<u8>),
    Hunk(Hunk),
}

// Length of a marker line's run of marker characters. Git lengthens the run only when the file
// itself contains such lines; that case fails the parse below and falls back to a whole-file
// decision, which is always correct if less fine grained.
const MARKER: usize = 7;

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

// True when `line` is a marker made of `ch`, optionally followed by a space and a label.
fn is_marker(line: &[u8], ch: u8) -> bool {
    let body = strip_eol(line);
    body.len() >= MARKER
        && body[..MARKER].iter().all(|&b| b == ch)
        && (body.len() == MARKER || body[MARKER] == b' ')
}

#[derive(PartialEq)]
enum State {
    Text,
    Ours,
    Base,
    Theirs,
}

// Split `content` into text and conflict hunks. Returns None when the file contains no complete
// conflict, or when the markers are not well formed (a marker inside a hunk, a missing separator or
// terminator), so a caller never guesses at a structure it cannot verify. A diff3-style base
// section is recognised and discarded; only the two sides are kept.
pub fn parse(content: &[u8]) -> Option<Vec<Segment>> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut text: Vec<u8> = Vec::new();
    let mut ours: Vec<u8> = Vec::new();
    let mut theirs: Vec<u8> = Vec::new();
    let mut state = State::Text;
    let mut hunks = 0usize;

    for line in content.split_inclusive(|&b| b == b'\n') {
        match state {
            State::Text => {
                if is_marker(line, b'<') {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    state = State::Ours;
                } else {
                    // A lone separator outside a hunk is ordinary text (a setext heading, for
                    // instance), not a sign of a malformed conflict.
                    text.extend_from_slice(line);
                }
            }
            State::Ours => {
                if is_marker(line, b'|') {
                    state = State::Base;
                } else if is_marker(line, b'=') {
                    state = State::Theirs;
                } else if is_marker(line, b'<') || is_marker(line, b'>') {
                    return None;
                } else {
                    ours.extend_from_slice(line);
                }
            }
            State::Base => {
                if is_marker(line, b'=') {
                    state = State::Theirs;
                } else if is_marker(line, b'<') || is_marker(line, b'>') {
                    return None;
                }
            }
            State::Theirs => {
                if is_marker(line, b'>') {
                    segments.push(Segment::Hunk(Hunk {
                        ours: std::mem::take(&mut ours),
                        theirs: std::mem::take(&mut theirs),
                        choice: None,
                    }));
                    hunks += 1;
                    state = State::Text;
                } else if is_marker(line, b'<') || is_marker(line, b'=') || is_marker(line, b'|') {
                    return None;
                } else {
                    theirs.extend_from_slice(line);
                }
            }
        }
    }
    if state != State::Text || hunks == 0 {
        return None;
    }
    if !text.is_empty() {
        segments.push(Segment::Text(text));
    }
    Some(segments)
}

pub fn hunk_count(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|s| matches!(s, Segment::Hunk(_)))
        .count()
}

pub fn unresolved_count(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|s| matches!(s, Segment::Hunk(h) if h.choice.is_none()))
        .count()
}

// The conflict hunks of a segment list, in file order. Callers index decisions by position among hunks
// rather than among segments, since the plain text between them is not something to decide about.
pub fn hunks(segments: &[Segment]) -> Vec<&Hunk> {
    segments
        .iter()
        .filter_map(|s| match s {
            Segment::Hunk(h) => Some(h),
            Segment::Text(_) => None,
        })
        .collect()
}

// Record (or, with None, withdraw) the decision for the `n`th hunk. Out-of-range indices are ignored rather
// than panicking, so a stale selection carried across a reparse cannot abort the process.
pub fn decide(segments: &mut [Segment], n: usize, choice: Option<Side>) {
    if let Some(h) = segments
        .iter_mut()
        .filter_map(|s| match s {
            Segment::Hunk(h) => Some(h),
            Segment::Text(_) => None,
        })
        .nth(n)
    {
        h.choice = choice;
    }
}

// Answer every hunk the same way. Backs the non-interactive whole-side resolution.
pub fn decide_all(segments: &mut [Segment], choice: Side) {
    for s in segments.iter_mut() {
        if let Segment::Hunk(h) = s {
            h.choice = Some(choice);
        }
    }
}

// The file as it stands under the current decisions, or None while any hunk is undecided.
pub fn render(segments: &[Segment]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for seg in segments {
        match seg {
            Segment::Text(t) => out.extend_from_slice(t),
            Segment::Hunk(h) => out.extend_from_slice(&h.chosen()?),
        }
    }
    Some(out)
}

// Largest product of line counts, after the common head and tail are set aside, for which the exact
// longest-common-subsequence table is built (4 bytes a cell). Beyond it the differing middle becomes one
// hunk: coarser to decide, but bounded in memory and time.
const DIFF_CELL_LIMIT: usize = 4_000_000;

// The line-level difference between two versions of one file, as the segments the decision screen works on.
// `a` is the copy to keep by default (the work tree), `b` the copy offered (a stashed or otherwise saved
// version). Lines both share are plain text; every maximal run of lines that differ is one hunk whose `ours`
// holds the lines only `a` has and whose `theirs` holds the lines only `b` has, so an insertion in `b` is a
// hunk with an empty A side and a deletion a hunk with an empty B side. Deciding every hunk for A renders
// `a` again byte for byte; deciding every hunk for B renders `b`. A final line without a terminator is a
// line of its own and differs from the same text with one, which keeps that distinction decidable too.
// Identical inputs give no hunk.
pub fn diff_segments(a: &[u8], b: &[u8]) -> Vec<Segment> {
    let la: Vec<&[u8]> = a.split_inclusive(|&c| c == b'\n').collect();
    let lb: Vec<&[u8]> = b.split_inclusive(|&c| c == b'\n').collect();
    let head = la.iter().zip(&lb).take_while(|(x, y)| x == y).count();
    let tail = la[head..]
        .iter()
        .rev()
        .zip(lb[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let ma = &la[head..la.len() - tail];
    let mb = &lb[head..lb.len() - tail];

    let mut out = Out::default();
    out.same(&la[..head]);
    if ma.len().saturating_mul(mb.len()) > DIFF_CELL_LIMIT {
        out.ours(ma);
        out.theirs(mb);
    } else {
        // lcs[i][j]: length of the longest common subsequence of ma[i..] and mb[j..].
        let w = mb.len() + 1;
        let mut lcs = vec![0u32; (ma.len() + 1) * w];
        for i in (0..ma.len()).rev() {
            for j in (0..mb.len()).rev() {
                lcs[i * w + j] = if ma[i] == mb[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < ma.len() && j < mb.len() {
            if ma[i] == mb[j] {
                out.same(&ma[i..=i]);
                i += 1;
                j += 1;
            } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
                out.ours(&ma[i..=i]);
                i += 1;
            } else {
                out.theirs(&mb[j..=j]);
                j += 1;
            }
        }
        out.ours(&ma[i..]);
        out.theirs(&mb[j..]);
    }
    out.same(&la[la.len() - tail..]);
    out.finish()
}

// Accumulator for `diff_segments`: shared lines extend the current text run, differing lines extend the
// current hunk, and a shared line after a hunk closes it.
#[derive(Default)]
struct Out {
    segments: Vec<Segment>,
    text: Vec<u8>,
    ours: Vec<u8>,
    theirs: Vec<u8>,
}

impl Out {
    fn close(&mut self) {
        if !self.ours.is_empty() || !self.theirs.is_empty() {
            self.segments.push(Segment::Hunk(Hunk {
                ours: std::mem::take(&mut self.ours),
                theirs: std::mem::take(&mut self.theirs),
                choice: None,
            }));
        }
    }

    fn same(&mut self, lines: &[&[u8]]) {
        if lines.is_empty() {
            return;
        }
        self.close();
        for l in lines {
            self.text.extend_from_slice(l);
        }
    }

    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.segments
                .push(Segment::Text(std::mem::take(&mut self.text)));
        }
    }

    fn ours(&mut self, lines: &[&[u8]]) {
        if !lines.is_empty() {
            self.flush_text();
        }
        for l in lines {
            self.ours.extend_from_slice(l);
        }
    }

    fn theirs(&mut self, lines: &[&[u8]]) {
        if !lines.is_empty() {
            self.flush_text();
        }
        for l in lines {
            self.theirs.extend_from_slice(l);
        }
    }

    fn finish(mut self) -> Vec<Segment> {
        self.close();
        self.flush_text();
        self.segments
    }
}

// A stable identity for a conflict, derived from where it is and what both sides hold (FNV-1a, 64
// bit). The same conflict met again while a patch is recomputed against a newer HEAD hashes to the
// same value, so an earlier decision can be reused; a conflict whose content changed does not, and
// is asked about afresh.
pub fn fingerprint(path: &str, ours: &[u8], theirs: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        // A separator no field can produce keeps ("ab", "c") distinct from ("a", "bc").
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    };
    feed(path.as_bytes());
    feed(ours);
    feed(theirs);
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(ours: &str, theirs: &str) -> Segment {
        Segment::Hunk(Hunk {
            ours: ours.as_bytes().to_vec(),
            theirs: theirs.as_bytes().to_vec(),
            choice: None,
        })
    }

    fn text(t: &str) -> Segment {
        Segment::Text(t.as_bytes().to_vec())
    }

    #[test]
    fn parses_text_and_hunks_in_order() {
        let src = "l1\n<<<<<<< HEAD\nMAIN\n=======\nFEAT\n>>>>>>> abc\nl3\n";
        let segs = parse(src.as_bytes()).unwrap();
        assert_eq!(
            segs,
            vec![text("l1\n"), hunk("MAIN\n", "FEAT\n"), text("l3\n")]
        );
        assert_eq!(hunk_count(&segs), 1);
        assert_eq!(unresolved_count(&segs), 1);
    }

    #[test]
    fn parses_several_hunks() {
        let src = "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n";
        let segs = parse(src.as_bytes()).unwrap();
        assert_eq!(hunk_count(&segs), 2);
        assert_eq!(segs.len(), 3);
    }

    #[test]
    fn discards_a_diff3_base_section() {
        let src = "<<<<<<< HEAD\nours\n||||||| base\nold\n=======\ntheirs\n>>>>>>> x\n";
        let segs = parse(src.as_bytes()).unwrap();
        assert_eq!(segs, vec![hunk("ours\n", "theirs\n")]);
    }

    #[test]
    fn keeps_carriage_returns_inside_sides() {
        let src = "<<<<<<< HEAD\r\nA\r\n=======\r\nB\r\n>>>>>>> x\r\n";
        let mut segs = parse(src.as_bytes()).unwrap();
        assert_eq!(segs, vec![hunk("A\r\n", "B\r\n")]);
        if let Segment::Hunk(h) = &mut segs[0] {
            h.choice = Some(Side::B);
        }
        assert_eq!(render(&segs).unwrap(), b"B\r\n");
    }

    #[test]
    fn empty_side_is_a_valid_hunk() {
        let src = "<<<<<<< HEAD\n=======\nadded\n>>>>>>> x\n";
        let segs = parse(src.as_bytes()).unwrap();
        assert_eq!(segs, vec![hunk("", "added\n")]);
    }

    #[test]
    fn rejects_files_without_a_complete_conflict() {
        assert!(parse(b"plain\ntext\n").is_none());
        assert!(parse(b"<<<<<<< HEAD\nours\n=======\ntheirs\n").is_none());
        assert!(parse(b"<<<<<<< HEAD\nours\n>>>>>>> x\n").is_none());
        assert!(parse(b"=======\n").is_none());
        assert!(parse(b"<<<<<<< a\n<<<<<<< b\n=======\n>>>>>>> c\n").is_none());
        assert!(parse(b"").is_none());
    }

    #[test]
    fn longer_marker_runs_and_prose_are_not_markers() {
        // Eight characters is not a marker of length seven: git lengthens markers when the file
        // holds such lines, and this parser declines those files.
        assert!(parse(b"<<<<<<<< x\na\n========\nb\n>>>>>>>> y\n").is_none());
        let segs = parse(b"<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n=== not a marker\n").unwrap();
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn render_waits_for_every_decision() {
        let mut segs = parse(b"a\n<<<<<<< x\n1\n=======\n2\n>>>>>>> y\nb\n").unwrap();
        assert!(render(&segs).is_none());
        if let Segment::Hunk(h) = &mut segs[1] {
            h.choice = Some(Side::A);
        }
        assert_eq!(render(&segs).unwrap(), b"a\n1\nb\n");
        assert_eq!(unresolved_count(&segs), 0);
    }

    #[test]
    fn each_side_renders_as_chosen() {
        for (side, want) in [(Side::A, "1\n"), (Side::B, "2\n"), (Side::Both, "1\n2\n")] {
            let mut segs = parse(b"<<<<<<< x\n1\n=======\n2\n>>>>>>> y\n").unwrap();
            if let Segment::Hunk(h) = &mut segs[0] {
                h.choice = Some(side);
            }
            assert_eq!(render(&segs).unwrap(), want.as_bytes());
        }
    }

    #[test]
    fn fingerprint_depends_on_every_field_and_their_boundaries() {
        let base = fingerprint("f", b"ab", b"c");
        assert_eq!(base, fingerprint("f", b"ab", b"c"));
        assert_ne!(base, fingerprint("g", b"ab", b"c"));
        assert_ne!(base, fingerprint("f", b"a", b"bc"));
        assert_ne!(base, fingerprint("f", b"ab", b"d"));
        assert_eq!(base.len(), 16);
    }

    fn rendered(segs: &mut [Segment], side: Side) -> Vec<u8> {
        decide_all(segs, side);
        render(segs).unwrap()
    }

    #[test]
    fn identical_inputs_have_no_hunk() {
        assert_eq!(hunk_count(&diff_segments(b"a\nb\n", b"a\nb\n")), 0);
        assert_eq!(hunk_count(&diff_segments(b"", b"")), 0);
    }

    #[test]
    fn each_separate_change_is_its_own_hunk() {
        let a = b"1\n2\n3\n4\n5\n6\n7\n";
        let b = b"1\nTWO\n3\n4\n5\n6\n7\nEIGHT\n";
        let mut segs = diff_segments(a, b);
        assert_eq!(
            segs,
            vec![
                text("1\n"),
                hunk("2\n", "TWO\n"),
                text("3\n4\n5\n6\n7\n"),
                hunk("", "EIGHT\n"),
            ]
        );
        assert_eq!(rendered(&mut segs.clone(), Side::A), a);
        assert_eq!(rendered(&mut segs, Side::B), b);
    }

    #[test]
    fn one_hunk_can_be_taken_while_another_is_kept() {
        let mut segs = diff_segments(b"a\nx\nb\ny\nc\n", b"a\nX\nb\nY\nc\n");
        decide(&mut segs, 0, Some(Side::B));
        decide(&mut segs, 1, Some(Side::A));
        assert_eq!(render(&segs).unwrap(), b"a\nX\nb\ny\nc\n");
    }

    #[test]
    fn a_deletion_and_an_insertion_are_decidable() {
        let segs = diff_segments(b"a\ngone\nb\n", b"a\nb\nnew\n");
        assert_eq!(
            segs,
            vec![
                text("a\n"),
                hunk("gone\n", ""),
                text("b\n"),
                hunk("", "new\n")
            ]
        );
    }

    #[test]
    fn a_missing_final_newline_is_a_difference_of_its_own() {
        let a = b"a\nb";
        let b = b"a\nb\n";
        let mut segs = diff_segments(a, b);
        assert_eq!(hunk_count(&segs), 1);
        assert_eq!(rendered(&mut segs.clone(), Side::A), a);
        assert_eq!(rendered(&mut segs, Side::B), b);
    }

    #[test]
    fn crlf_lines_are_compared_with_their_terminators() {
        let mut segs = diff_segments(b"a\r\nb\r\n", b"a\r\nB\r\n");
        assert_eq!(segs, vec![text("a\r\n"), hunk("b\r\n", "B\r\n")]);
        assert_eq!(rendered(&mut segs, Side::B), b"a\r\nB\r\n");
    }

    #[test]
    fn a_file_with_nothing_in_common_is_one_hunk() {
        let segs = diff_segments(b"a\nb\n", b"c\n");
        assert_eq!(segs, vec![hunk("a\nb\n", "c\n")]);
        let segs = diff_segments(b"", b"c\n");
        assert_eq!(segs, vec![hunk("", "c\n")]);
    }

    #[test]
    fn inputs_past_the_table_limit_fall_back_to_one_hunk_and_still_render_both_sides() {
        let n = 2200;
        let a: String = (0..n).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..n).map(|i| format!("b{i}\n")).collect();
        let mut segs = diff_segments(a.as_bytes(), b.as_bytes());
        assert_eq!(hunk_count(&segs), 1);
        assert_eq!(rendered(&mut segs.clone(), Side::A), a.as_bytes());
        assert_eq!(rendered(&mut segs, Side::B), b.as_bytes());
    }

    #[test]
    fn side_letters_round_trip() {
        for s in [Side::A, Side::B, Side::Both] {
            assert_eq!(Side::from_letter(s.letter()), Some(s));
        }
        assert_eq!(Side::from_letter('z'), None);
    }
}
