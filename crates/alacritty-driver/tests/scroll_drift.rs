//! A viewer parked in the scrollback names its position as a distance from
//! the live bottom, so the driver has to say how far the content moved for
//! that distance to be re-anchored.  These tests pin that report down.

use blit_alacritty::TerminalDriver;
use blit_remote::FrameState;

fn row_text(frame: &FrameState, row: u16) -> String {
    (0..frame.cols())
        .map(|c| frame.cell_content(row, c))
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn feed_lines(d: &mut TerminalDriver, range: std::ops::Range<usize>) {
    for i in range {
        d.process(format!("line {i}\r\n").as_bytes());
    }
}

#[test]
fn scrolled_lines_re_anchors_a_parked_viewer() {
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..50);

    let before = d.scrolled_lines();
    let parked = row_text(&d.scrollback_frame(5), 0);

    feed_lines(&mut d, 50..53);

    let moved = (d.scrolled_lines() - before) as usize;
    assert_eq!(moved, 3, "three lines of output, three lines of movement");
    assert_eq!(
        row_text(&d.scrollback_frame(5 + moved), 0),
        parked,
        "re-anchored viewer should still be on the same line"
    );
    assert_ne!(
        row_text(&d.scrollback_frame(5), 0),
        parked,
        "the un-compensated offset is the drift this guards against"
    );
}

#[test]
fn scrolled_lines_counts_past_a_full_scrollback() {
    // Scrollback full: `scrollback_lines` stops growing, but the content a
    // parked viewer is reading still moves, so the count has to keep going.
    let mut d = TerminalDriver::new(10, 40, 20);
    feed_lines(&mut d, 0..100);

    let before = d.scrolled_lines();
    let parked = row_text(&d.scrollback_frame(5), 0);
    feed_lines(&mut d, 100..102);
    let moved = (d.scrolled_lines() - before) as usize;

    assert_eq!(moved, 2);
    assert_eq!(row_text(&d.scrollback_frame(5 + moved), 0), parked);
}

#[test]
fn scrolled_lines_is_still_when_the_app_repaints_in_place() {
    // The Claude Code shape: redraw the bottom rows without pushing lines.
    let mut d = TerminalDriver::new(10, 40, 1000);
    feed_lines(&mut d, 0..50);

    let before = d.scrolled_lines();
    for i in 0..20 {
        d.process(format!("\x1b[3A\x1b[Jspinner {i}\r\n\r\n\r\n").as_bytes());
    }
    assert_eq!(
        d.scrolled_lines(),
        before,
        "an in-place repaint must not move a parked viewer"
    );
}
