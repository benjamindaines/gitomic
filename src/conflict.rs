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

    #[test]
    fn side_letters_round_trip() {
        for s in [Side::A, Side::B, Side::Both] {
            assert_eq!(Side::from_letter(s.letter()), Some(s));
        }
        assert_eq!(Side::from_letter('z'), None);
    }
}
