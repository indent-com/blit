//! A client that polls a terminal wants "what is new since I last looked",
//! not the whole grid.  These tests pin down the sequence cursor that answers
//! it: that it never loses a line, never repeats one, survives eviction
//! visibly, and resumes mid-line when the application stopped mid-line.

use blit_alacritty::TerminalDriver;

fn feed_lines(d: &mut TerminalDriver, range: std::ops::Range<usize>) {
    for i in range {
        d.process(format!("line {i}\r\n").as_bytes());
    }
}

#[test]
fn cursor_returns_only_what_was_appended() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..5);

    let (seq, col) = d.cursor_seq();
    feed_lines(&mut d, 5..8);

    // The trailing newline is the empty row the cursor now sits on. Keeping
    // it is what lets successive reads be concatenated without inventing a
    // separator that may or may not belong.
    let read = d.seq_text(seq, col, None, 64 * 1024);
    assert_eq!(read.text, "line 5\nline 6\nline 7\n");
    assert!(!read.truncated);
    assert!(!read.evicted);

    // Reading again from the returned cursor yields nothing new.
    let again = d.seq_text(read.next_seq, read.next_col, None, 64 * 1024);
    assert_eq!(again.text, "");
    assert_eq!(
        (again.next_seq, again.next_col),
        (read.next_seq, read.next_col)
    );
}

#[test]
fn cursor_resumes_inside_a_partial_line() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    d.process(b"Continue? ");

    let (seq, col) = d.cursor_seq();
    let first = d.seq_text(0, 0, None, 64 * 1024);
    assert_eq!(first.text, "Continue?");
    assert_eq!((first.next_seq, first.next_col), (seq, col));

    d.process(b"[y/N] ");
    let rest = d.seq_text(first.next_seq, first.next_col, None, 64 * 1024);
    assert_eq!(
        rest.text, "[y/N]",
        "the resumed read must not repeat the prompt it already returned"
    );
}

#[test]
fn a_prompt_written_without_a_newline_is_visible_immediately() {
    // The reason the cursor line is included rather than withheld until it
    // is complete: an agent waiting on "[y/N]" would otherwise wait forever.
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..3);
    let (seq, col) = d.cursor_seq();

    d.process(b"overwrite? [y/N] ");
    let read = d.seq_text(seq, col, None, 64 * 1024);
    assert_eq!(read.text, "overwrite? [y/N]");
}

#[test]
fn a_bounded_range_reads_one_commands_output() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..3);
    let start = d.cursor_seq().0;
    feed_lines(&mut d, 3..6);
    let end = d.cursor_seq().0;
    feed_lines(&mut d, 6..9);

    let read = d.seq_text(start, 0, Some(end), 64 * 1024);
    assert_eq!(read.text, "line 3\nline 4\nline 5");
    assert_eq!(read.next_seq, end, "a bounded read stops where it was told");
    assert_eq!(read.next_col, 0);
}

#[test]
fn eviction_is_reported_not_silently_misread() {
    let mut d = TerminalDriver::new(10, 40, 20);
    feed_lines(&mut d, 0..200);

    let oldest = d.oldest_seq();
    assert!(oldest > 0, "a 20-line scrollback must have evicted by now");

    let read = d.seq_text(0, 0, None, 64 * 1024);
    assert!(read.evicted, "reading from an evicted start must say so");
    assert_eq!(read.start_seq, oldest);
    assert!(
        !read.text.contains("line 0\n"),
        "evicted text cannot be conjured back"
    );

    let fresh = d.seq_text(oldest, 0, None, 64 * 1024);
    assert!(!fresh.evicted);
}

#[test]
fn truncation_pages_forward_without_losing_a_line() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..40);

    let mut cursor = (0u64, 0u16);
    let mut collected = String::new();
    let mut rounds = 0;
    loop {
        // 20 bytes holds two "line N" rows, so this pages many times.
        let read = d.seq_text(cursor.0, cursor.1, None, 20);
        if !read.text.is_empty() {
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(&read.text);
        }
        let next = (read.next_seq, read.next_col);
        if !read.truncated {
            break;
        }
        assert_ne!(next, cursor, "a truncated page must make progress");
        cursor = next;
        rounds += 1;
        assert!(rounds < 200, "paging failed to terminate");
    }

    for i in 0..40 {
        assert!(
            collected.contains(&format!("line {i}")),
            "paged reads dropped line {i}"
        );
    }
    assert!(rounds > 1, "the budget should have forced several pages");
}

#[test]
fn a_cursor_from_the_future_yields_nothing() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..3);
    let read = d.seq_text(u64::MAX, 0, None, 64 * 1024);
    assert_eq!(read.text, "");
    assert_eq!(read.next_seq, d.cursor_seq().0);
}

#[test]
fn blank_lines_survive_the_round_trip() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    let (seq, col) = d.cursor_seq();
    d.process(b"a\r\n\r\n\r\nb\r\n");
    let read = d.seq_text(seq, col, None, 64 * 1024);
    assert_eq!(read.text, "a\n\n\nb\n", "empty rows are still rows");
}

#[test]
fn soft_wrapped_rows_stay_one_logical_line() {
    let mut d = TerminalDriver::new(10, 10, 1000);
    let (seq, col) = d.cursor_seq();
    d.process(b"aaaaaaaaaabbbbb\r\n");
    let read = d.seq_text(seq, col, None, 64 * 1024);
    assert_eq!(read.text, "aaaaaaaaaabbbbb\n");
}

#[test]
fn the_first_line_keeps_its_sequence_across_the_first_scroll() {
    // The trap this counter exists for: the scroll that creates the
    // scrollback is invisible to a probe that needs a scrollback to arm, so
    // a naive counter renumbers every retained line exactly once.
    let mut d = TerminalDriver::new(4, 40, 1000);
    feed_lines(&mut d, 0..3);
    let before = d.seq_text(0, 0, Some(1), 64 * 1024);
    assert_eq!(before.text, "line 0");

    feed_lines(&mut d, 3..30);

    let after = d.seq_text(0, 0, Some(1), 64 * 1024);
    assert_eq!(
        after.text, "line 0",
        "sequence 0 must still name the first line after the grid scrolled"
    );
    assert_eq!(
        d.oldest_seq(),
        0,
        "nothing was evicted from a 1000-line scrollback"
    );
}

#[test]
fn every_line_is_addressable_by_its_own_sequence() {
    let mut d = TerminalDriver::new(5, 40, 1000);
    feed_lines(&mut d, 0..25);
    for i in 0..25u64 {
        let read = d.seq_text(i, 0, Some(i + 1), 64 * 1024);
        assert_eq!(
            read.text,
            format!("line {i}"),
            "sequence {i} names line {i}"
        );
    }
}

fn named_lines(d: &TerminalDriver, n: u64) -> Vec<String> {
    (0..n)
        .map(|i| d.seq_text(i, 0, Some(i + 1), 64 * 1024).text)
        .collect()
}

#[test]
fn a_taller_resize_keeps_sequence_identity() {
    // Viewport 4, 20 lines of output → 16 lines of history. Growing to 10
    // pulls 6 of those back into the viewport (`grow_lines`); every sequence
    // that already named a line must still name it.
    let mut d = TerminalDriver::new(4, 40, 1000);
    feed_lines(&mut d, 0..20);
    let before = named_lines(&d, 20);
    let cursor = d.cursor_seq();

    d.resize(10, 40);

    assert_eq!(
        named_lines(&d, 20),
        before,
        "grow must not renumber retained lines"
    );
    assert_eq!(
        d.cursor_seq(),
        cursor,
        "cursor_seq must not jump when history is pulled into the viewport"
    );
}

#[test]
fn a_shorter_resize_keeps_sequence_identity() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..20);
    let before = named_lines(&d, 20);
    let cursor = d.cursor_seq();

    d.resize(4, 40);

    assert_eq!(
        named_lines(&d, 20),
        before,
        "shrink must not renumber retained lines"
    );
    assert_eq!(d.cursor_seq(), cursor);
}

#[test]
fn growing_with_no_history_does_not_invent_rotation() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..3);
    let before = named_lines(&d, 3);
    let cursor = d.cursor_seq();
    d.resize(20, 40);
    assert_eq!(named_lines(&d, 3), before);
    assert_eq!(d.cursor_seq(), cursor);
}
