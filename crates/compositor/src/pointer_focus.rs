/**
 * Which surface the pointer is inside, as a state transition.
 *
 * Three lines of bookkeeping that have now been wrong twice, in ways that both
 * present as "the mouse does nothing at all": first by latching a surface whose
 * client never received the `enter` (so the branch never ran again), then by
 * keeping the *old* surface latched after telling it the pointer had left. Both
 * are transitions between three states — nothing entered, entered here, entered
 * elsewhere — crossed with whether the client can be told. So the decision lives
 * here, apart from the several hundred lines of resource plumbing it used to be
 * embedded in, where it can be enumerated in a test.
 *
 * Generic over the surface id so tests need not mint Wayland objects; `imp.rs`
 * instantiates it with `ObjectId`.
 */
use std::fmt::Debug;

/// What a motion into `hit` should do about pointer focus.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FocusTransition<T> {
    /// Send `wl_pointer.leave` to this surface first.
    pub leave: Option<T>,
    /// The new value of the entered id.
    ///
    /// `None` when the client has no pointer to receive the `enter`: nothing
    /// was entered, so nothing may be recorded. The next motion retries, and
    /// in the meantime no button is dispatched to a surface the pointer is
    /// not over.
    pub entered: Option<T>,
}

/// The transition for a pointer now over `hit`, or `None` when it is already
/// there and only motion is owed.
///
/// `client_has_pointer` is whether any live `wl_pointer` of the hit surface's
/// client exists to receive the `enter`.
pub(crate) fn focus_transition<T: Clone + PartialEq + Debug>(
    entered: Option<&T>,
    hit: &T,
    client_has_pointer: bool,
) -> Option<FocusTransition<T>> {
    if entered == Some(hit) {
        return None;
    }
    Some(FocusTransition {
        leave: entered.cloned(),
        entered: client_has_pointer.then(|| hit.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::{FocusTransition, focus_transition};

    #[test]
    fn already_inside_is_motion_only() {
        assert_eq!(focus_transition(Some(&1), &1, true), None);
    }

    #[test]
    fn crossing_between_surfaces_leaves_then_enters() {
        assert_eq!(
            focus_transition(Some(&1), &2, true),
            Some(FocusTransition {
                leave: Some(1),
                entered: Some(2),
            })
        );
    }

    // The original bug: a client that has mapped a surface but not yet asked
    // for a pointer. Recording it anyway meant the enter branch never ran
    // again, and every later event went to a client that had never been told.
    #[test]
    fn a_surface_whose_client_cannot_be_told_is_not_recorded() {
        assert_eq!(
            focus_transition(None, &2, false),
            Some(FocusTransition {
                leave: None,
                entered: None,
            })
        );
    }

    // The regression on top of it: crossing *out of* a latched surface into
    // one that cannot be told. The old surface is sent its leave either way,
    // so keeping its id would claim a surface the pointer has left.
    #[test]
    fn leaving_for_a_surface_that_cannot_be_told_clears_the_old_one() {
        assert_eq!(
            focus_transition(Some(&1), &2, false),
            Some(FocusTransition {
                leave: Some(1),
                entered: None,
            })
        );
    }

    // What that regression cost, end to end: with the old id kept, returning
    // to it took the already-inside path and it never re-entered — dead mouse
    // on a surface whose client was perfectly able to receive events. Two GUI
    // apps in one compositor is the ordinary case, since every PTY shares it.
    #[test]
    fn returning_from_a_pointerless_surface_re_enters() {
        let away = focus_transition(Some(&1), &2, false).expect("1 -> 2 is a change");
        assert_eq!(away.entered, None, "nothing was entered, so nothing held");

        let back = focus_transition(away.entered.as_ref(), &1, true)
            .expect("2 -> 1 must not be mistaken for already-inside");
        assert_eq!(
            back,
            FocusTransition {
                leave: None,
                entered: Some(1),
            },
            "the surface we left must be entered again, not merely moved over"
        );
    }
}
