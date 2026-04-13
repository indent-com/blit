#[derive(Clone, Copy)]
pub(crate) struct PositionerGeometry {
    pub(crate) size: (i32, i32),
    pub(crate) anchor_rect: (i32, i32, i32, i32),
    pub(crate) anchor: u32,
    pub(crate) gravity: u32,
    pub(crate) constraint_adjustment: u32,
    pub(crate) offset: (i32, i32),
}

// xdg_positioner constraint adjustment flags.
pub(crate) const CONSTRAINT_SLIDE_X: u32 = 1;
pub(crate) const CONSTRAINT_SLIDE_Y: u32 = 2;
pub(crate) const CONSTRAINT_FLIP_X: u32 = 4;
pub(crate) const CONSTRAINT_FLIP_Y: u32 = 8;
pub(crate) const CONSTRAINT_RESIZE_X: u32 = 16;
pub(crate) const CONSTRAINT_RESIZE_Y: u32 = 32;

impl PositionerGeometry {
    /// Anchor reference point for the given anchor enum value.
    fn anchor_point(&self, anchor: u32) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        match anchor {
            1 => (ax + aw / 2, ay),
            2 => (ax + aw / 2, ay + ah),
            3 => (ax, ay + ah / 2),
            4 => (ax + aw, ay + ah / 2),
            5 => (ax, ay),
            6 => (ax, ay + ah),
            7 => (ax + aw, ay),
            8 => (ax + aw, ay + ah),
            _ => (ax + aw / 2, ay + ah / 2),
        }
    }

    /// Gravity offset for the given gravity enum value and popup size.
    fn gravity_offset(gravity: u32, pw: i32, ph: i32) -> (i32, i32) {
        match gravity {
            1 => (-(pw / 2), -ph),
            2 => (-(pw / 2), 0),
            3 => (-pw, -(ph / 2)),
            4 => (0, -(ph / 2)),
            5 => (-pw, -ph),
            6 => (-pw, 0),
            7 => (0, -ph),
            8 => (0, 0),
            _ => (-(pw / 2), -(ph / 2)),
        }
    }

    fn flip_anchor_x(anchor: u32) -> u32 {
        match anchor {
            3 => 4,
            4 => 3,
            5 => 7,
            7 => 5,
            6 => 8,
            8 => 6,
            other => other,
        }
    }

    fn flip_anchor_y(anchor: u32) -> u32 {
        match anchor {
            1 => 2,
            2 => 1,
            5 => 6,
            6 => 5,
            7 => 8,
            8 => 7,
            other => other,
        }
    }

    fn flip_gravity_x(gravity: u32) -> u32 {
        Self::flip_anchor_x(gravity)
    }

    fn flip_gravity_y(gravity: u32) -> u32 {
        Self::flip_anchor_y(gravity)
    }

    fn raw_position(&self, anchor: u32, gravity: u32) -> (i32, i32, i32, i32) {
        let (ref_x, ref_y) = self.anchor_point(anchor);
        let (pw, ph) = self.size;
        let (gx, gy) = Self::gravity_offset(gravity, pw, ph);
        (
            ref_x + gx + self.offset.0,
            ref_y + gy + self.offset.1,
            pw.max(1),
            ph.max(1),
        )
    }

    /// Compute popup position/size in parent-window-geometry coordinates.
    pub(crate) fn compute_position(
        &self,
        parent_abs: (i32, i32),
        bounds: (i32, i32, i32, i32),
    ) -> (i32, i32, i32, i32) {
        let ca = self.constraint_adjustment;
        let (bx, by, bw, bh) = bounds;
        let bounds_right = bx + bw;
        let bounds_bottom = by + bh;
        let (pax, pay) = parent_abs;

        let fits = |x: i32, y: i32, w: i32, h: i32| -> bool {
            let abs_x = pax + x;
            let abs_y = pay + y;
            abs_x >= bx && abs_y >= by && abs_x + w <= bounds_right && abs_y + h <= bounds_bottom
        };

        let (mut x, mut y, mut w, mut h) = self.raw_position(self.anchor, self.gravity);
        if fits(x, y, w, h) || ca == 0 {
            return (x, y, w, h);
        }

        let mut cur_anchor = self.anchor;
        let mut cur_gravity = self.gravity;
        if ca & CONSTRAINT_FLIP_X != 0 {
            let fa = Self::flip_anchor_x(cur_anchor);
            let fg = Self::flip_gravity_x(cur_gravity);
            let (fx, fy, fw, fh) = self.raw_position(fa, fg);
            if fits(fx, fy, fw, fh) {
                return (fx, fy, fw, fh);
            }
            let orig_abs_x = pax + x;
            let flip_abs_x = pax + fx;
            let orig_overflow_x = (bx - orig_abs_x).max(0) + (orig_abs_x + w - bounds_right).max(0);
            let flip_overflow_x =
                (bx - flip_abs_x).max(0) + (flip_abs_x + fw - bounds_right).max(0);
            if flip_overflow_x < orig_overflow_x {
                x = fx;
                y = fy;
                w = fw;
                h = fh;
                cur_anchor = fa;
                cur_gravity = fg;
            }
        }

        if ca & CONSTRAINT_FLIP_Y != 0 {
            let fa = Self::flip_anchor_y(cur_anchor);
            let fg = Self::flip_gravity_y(cur_gravity);
            let (fx, fy, fw, fh) = self.raw_position(fa, fg);
            if fits(fx, fy, fw, fh) {
                return (fx, fy, fw, fh);
            }
            let orig_abs_y = pay + y;
            let flip_abs_y = pay + fy;
            let orig_overflow_y =
                (by - orig_abs_y).max(0) + (orig_abs_y + h - bounds_bottom).max(0);
            let flip_overflow_y =
                (by - flip_abs_y).max(0) + (flip_abs_y + fh - bounds_bottom).max(0);
            if flip_overflow_y < orig_overflow_y {
                x = fx;
                y = fy;
                w = fw;
                h = fh;
            }
        }

        if ca & CONSTRAINT_SLIDE_X != 0 {
            let abs_x = pax + x;
            if abs_x + w > bounds_right {
                x -= (abs_x + w) - bounds_right;
            }
            let abs_x = pax + x;
            if abs_x < bx {
                x += bx - abs_x;
            }
        }

        if ca & CONSTRAINT_SLIDE_Y != 0 {
            let abs_y = pay + y;
            if abs_y + h > bounds_bottom {
                y -= (abs_y + h) - bounds_bottom;
            }
            let abs_y = pay + y;
            if abs_y < by {
                y += by - abs_y;
            }
        }

        if ca & CONSTRAINT_RESIZE_X != 0 {
            let abs_x = pax + x;
            if abs_x < bx {
                let delta = bx - abs_x;
                w -= delta;
                x += delta;
            }
            if pax + x + w > bounds_right {
                w = bounds_right - (pax + x);
            }
            w = w.max(1);
        }

        if ca & CONSTRAINT_RESIZE_Y != 0 {
            let abs_y = pay + y;
            if abs_y < by {
                let delta = by - abs_y;
                h -= delta;
                y += delta;
            }
            if pay + y + h > bounds_bottom {
                h = bounds_bottom - (pay + y);
            }
            h = h.max(1);
        }

        (x, y, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::{CONSTRAINT_SLIDE_X, CONSTRAINT_SLIDE_Y, PositionerGeometry};

    fn test_geometry() -> PositionerGeometry {
        PositionerGeometry {
            size: (40, 20),
            anchor_rect: (0, 0, 0, 0),
            anchor: 0,
            gravity: 0,
            constraint_adjustment: 0,
            offset: (0, 0),
        }
    }

    #[test]
    fn compute_position_respects_nonzero_bounds_origin() {
        let mut geometry = test_geometry();
        geometry.constraint_adjustment = CONSTRAINT_SLIDE_X | CONSTRAINT_SLIDE_Y;

        let (x, y, w, h) = geometry.compute_position((8, 30), (8, 30, 1000, 700));

        assert_eq!((x, y, w, h), (0, 0, 40, 20));
    }

    #[test]
    fn compute_position_uses_geometry_right_edge_for_sliding() {
        let mut geometry = test_geometry();
        geometry.size = (120, 60);
        geometry.constraint_adjustment = CONSTRAINT_SLIDE_X;
        geometry.offset = (950, 100);

        let (x, y, w, h) = geometry.compute_position((8, 30), (8, 30, 1000, 700));

        assert_eq!((x, y, w, h), (880, 70, 120, 60));
    }
}
