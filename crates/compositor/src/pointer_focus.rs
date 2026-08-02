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

/// What to do with a button event while a popup grab may be open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ButtonRouting {
    /// Send it to the entered surface.
    Deliver,
    /// Swallow it: it is the click that broke a popup grab, or that click's
    /// release.
    Consume,
}

/// Route a button, and say what to remember for the next one.
///
/// `dismissed_grab` is whether this press just closed a popup chain. Such a
/// click is spent on closing it — delivered as well, it presses whatever the
/// menu was covering. Its release has to go too: a client that never saw the
/// press must not see the release, so the button is remembered until then.
pub(crate) fn button_routing(
    pressed: bool,
    button: u32,
    dismissed_grab: bool,
    swallowing: Option<u32>,
) -> (ButtonRouting, Option<u32>) {
    if dismissed_grab {
        return (ButtonRouting::Consume, Some(button));
    }
    if pressed {
        // A fresh press ends any earlier gesture, so a release that never
        // arrived cannot leave the swallow armed to eat an unrelated one.
        return (ButtonRouting::Deliver, None);
    }
    if swallowing == Some(button) {
        return (ButtonRouting::Consume, None);
    }
    (ButtonRouting::Deliver, swallowing)
}

/// Who should hold the keyboard once `closing` goes away.
///
/// `holder` is the popup that has it, if a popup does; `remaining` is the
/// grab stack with `closing` already removed, outermost first.
///
/// `None` means leave focus alone — that popup was not holding it, so a
/// menu closing elsewhere in the tree changes nothing. `Some(None)` means
/// hand it back to the focused toplevel. `Some(Some(id))` means hand it to
/// the popup still grabbing underneath: closing a submenu returns to its
/// parent menu, not past both to the page.
pub(crate) fn keyboard_focus_after_popup_close<T: Clone + PartialEq>(
    closing: &T,
    holder: Option<&T>,
    remaining: &[T],
) -> Option<Option<T>> {
    if holder != Some(closing) {
        return None;
    }
    Some(remaining.last().cloned())
}

#[cfg(test)]
mod tests {
    use super::{
        ButtonRouting, FocusTransition, button_routing, focus_transition,
        keyboard_focus_after_popup_close,
    };

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

    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;

    #[test]
    fn an_ordinary_click_is_delivered() {
        assert_eq!(
            button_routing(true, BTN_LEFT, false, None),
            (ButtonRouting::Deliver, None)
        );
        assert_eq!(
            button_routing(false, BTN_LEFT, false, None),
            (ButtonRouting::Deliver, None)
        );
    }

    // The bug: the click that closed a menu was also delivered underneath, so
    // dismissing a context menu pressed the link or button behind it.
    #[test]
    fn the_click_that_closes_a_menu_is_spent_on_closing_it() {
        let (routing, swallow) = button_routing(true, BTN_LEFT, true, None);
        assert_eq!(routing, ButtonRouting::Consume);
        assert_eq!(swallow, Some(BTN_LEFT), "its release must go too");

        assert_eq!(
            button_routing(false, BTN_LEFT, false, swallow),
            (ButtonRouting::Consume, None),
            "a client that never saw the press must not see the release"
        );
    }

    #[test]
    fn another_buttons_release_is_untouched_while_armed() {
        assert_eq!(
            button_routing(false, BTN_RIGHT, false, Some(BTN_LEFT)),
            (ButtonRouting::Deliver, Some(BTN_LEFT))
        );
    }

    // A release that never arrives (the pointer leaves mid-click) must not
    // leave the swallow armed to eat an unrelated one later.
    #[test]
    fn a_new_press_disarms_a_stale_swallow() {
        assert_eq!(
            button_routing(true, BTN_LEFT, false, Some(BTN_LEFT)),
            (ButtonRouting::Deliver, None)
        );
    }

    // A menu closing somewhere else in the tree must not yank the keyboard
    // from whoever has it.
    #[test]
    fn closing_a_popup_that_lacks_focus_changes_nothing() {
        assert_eq!(keyboard_focus_after_popup_close(&7, Some(&9), &[9]), None);
        assert_eq!(keyboard_focus_after_popup_close(&7, None, &[]), None);
    }

    #[test]
    fn closing_the_last_menu_returns_the_keyboard_to_the_window() {
        assert_eq!(
            keyboard_focus_after_popup_close(&7, Some(&7), &[]),
            Some(None)
        );
    }

    // Nested menus: dismissing a submenu goes back to its parent menu, not
    // past both to the page underneath.
    #[test]
    fn closing_a_submenu_returns_to_the_menu_beneath_it() {
        assert_eq!(
            keyboard_focus_after_popup_close(&8, Some(&8), &[7]),
            Some(Some(7))
        );
    }
}
