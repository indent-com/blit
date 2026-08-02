/**
 * Where a surface accepts pointer input.
 *
 * A `wl_region` is not a rectangle but a set built by adding and subtracting
 * them in order, so membership cannot be answered by a bounds check — it has
 * to replay the ops. The case that matters most is the degenerate one: a
 * region with no rectangles at all is a surface that takes no input, which is
 * how a client says "the pointer belongs to whatever is behind me". Firefox
 * puts its rendering in a subsurface covering the whole window and sets
 * exactly that, so ignoring input regions sent every event to a surface with
 * no input handler and made the mouse look dead.
 */
/// One rectangle added to or subtracted from a `wl_region`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegionOp {
    pub add: bool,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Whether `ops` cover the surface-local point.
///
/// The last rectangle covering the point decides, since a later `subtract`
/// cuts a hole a previous `add` made and a later `add` fills it back in.
pub(crate) fn contains(ops: &[RegionOp], x: f64, y: f64) -> bool {
    let mut inside = false;
    for op in ops {
        if x >= op.x as f64
            && y >= op.y as f64
            && x < (op.x + op.w) as f64
            && y < (op.y + op.h) as f64
        {
            inside = op.add;
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::{RegionOp, contains};

    fn add(x: i32, y: i32, w: i32, h: i32) -> RegionOp {
        RegionOp {
            add: true,
            x,
            y,
            w,
            h,
        }
    }

    fn sub(x: i32, y: i32, w: i32, h: i32) -> RegionOp {
        RegionOp {
            add: false,
            x,
            y,
            w,
            h,
        }
    }

    // The Firefox case: a region built and never added to.
    #[test]
    fn an_empty_region_takes_no_input_anywhere() {
        assert!(!contains(&[], 0.0, 0.0));
        assert!(!contains(&[], 700.0, 500.0));
    }

    #[test]
    fn a_rectangle_takes_input_inside_its_bounds() {
        let r = [add(10, 10, 100, 50)];
        assert!(contains(&r, 10.0, 10.0), "top-left corner is inside");
        assert!(contains(&r, 109.9, 59.9));
        assert!(!contains(&r, 9.9, 30.0));
        assert!(!contains(&r, 110.0, 30.0), "right edge is exclusive");
        assert!(!contains(&r, 30.0, 60.0), "bottom edge is exclusive");
    }

    #[test]
    fn subtract_punches_a_hole() {
        let r = [add(0, 0, 100, 100), sub(40, 40, 20, 20)];
        assert!(contains(&r, 10.0, 10.0));
        assert!(!contains(&r, 50.0, 50.0), "inside the hole");
        assert!(contains(&r, 65.0, 50.0), "past the hole");
    }

    // Order is what makes this a replay rather than a pair of bounds checks.
    #[test]
    fn a_later_add_fills_an_earlier_hole_back_in() {
        let r = [add(0, 0, 100, 100), sub(40, 40, 20, 20), add(45, 45, 5, 5)];
        assert!(contains(&r, 46.0, 46.0), "refilled");
        assert!(!contains(&r, 55.0, 55.0), "still a hole");
    }

    #[test]
    fn disjoint_rectangles_both_count() {
        let r = [add(0, 0, 10, 10), add(100, 100, 10, 10)];
        assert!(contains(&r, 5.0, 5.0));
        assert!(contains(&r, 105.0, 105.0));
        assert!(!contains(&r, 50.0, 50.0), "the gap between them");
    }
}
