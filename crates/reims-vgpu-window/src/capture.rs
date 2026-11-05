//! Input capture: whether the guest or the host gets a key, and where the host's pointer may go.
//!
//! A click into the image captures the keyboard and the pointer together: from then on every key
//! goes to the guest, ⌘ shortcuts included (and the host's system keys, which the AppKit side
//! turns off while captured), and the host's pointer is held over the image
//! ([`Capture::pointer_confinement`]): it cannot leave for the title bar or the host's other
//! windows, and every click is the guest's. The keyboard is never captured without the pointer,
//! nor the pointer without the keyboard. Pressing ⌃⌥⌘ together releases both, and so does the
//! window losing the keyboard. Uncaptured, a key with ⌘ held drives the host's menu and every
//! other key goes to the guest, and the pointer goes where the person moves it.
//!
//! The click that makes the window key only brings it forward: the view does not take a first
//! click (`acceptsFirstMouse:`), so that click neither captures nor reaches the guest. Clicks
//! after it capture, and are sent to the guest.

use crate::geometry::Rect;
use crate::keys::Modifiers;

/// What to do with a click into the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickAction {
    /// Whether the click goes to the guest.
    pub forward: bool,
}

/// Who a key goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The guest's keyboard.
    Guest,
    /// The host: the app's menu and AppKit's own handling.
    Host,
}

/// Whether input, the keyboard and the pointer, is captured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// Captured.
    captured: bool,
}

/// What the title says while captured.
const CAPTURED_SUFFIX: &str =
    " \u{2014} input captured (press \u{2303}\u{2325}\u{2318} to release)";

/// What the title says while not captured, on every OS: that a click captures.
pub(crate) const UNCAPTURED_SUFFIX: &str = " \u{2014} click to capture input";

impl Capture {
    /// Not captured.
    pub fn new() -> Capture {
        Capture::default()
    }

    /// Whether input, the keyboard and the pointer, is captured.
    pub fn is_captured(&self) -> bool {
        self.captured
    }

    /// A click landed in the image: the keyboard and the pointer are captured. `window_was_key`
    /// is whether the window already had the keyboard before the click; a click that made it
    /// key, where one reaches the input, is not forwarded.
    pub fn click_in_image(&mut self, window_was_key: bool) -> ClickAction {
        self.captured = true;
        ClickAction {
            forward: window_was_key,
        }
    }

    /// Releases the keyboard and the pointer. Whether they were captured.
    pub fn release(&mut self) -> bool {
        std::mem::replace(&mut self.captured, false)
    }

    /// The window lost the keyboard: capture is released. Whether it was captured, so that the
    /// host's system keys are given back.
    pub fn resign_key(&mut self) -> bool {
        self.release()
    }

    /// Where the host's pointer is held, `image` the image's rectangle in the window's points:
    /// over the image while captured, where it has an area. `None` while not captured, and over
    /// an image of no area, which would hold the pointer still: the pointer then goes where the
    /// person moves it.
    pub fn pointer_confinement(&self, image: Rect) -> Option<Rect> {
        (self.captured && image.width > 0.0 && image.height > 0.0).then_some(image)
    }

    /// Who a key goes to, `command_held` whether ⌘ is down: the guest while captured; the host
    /// for a ⌘ key while not; else the guest.
    pub fn route(&self, command_held: bool) -> Route {
        if !self.captured && command_held {
            Route::Host
        } else {
            Route::Guest
        }
    }

    /// Whether `held` is the release chord, ⌃⌥⌘ all down (Shift either way), while captured.
    /// The caller then sends the ups of the keys down ([`crate::keys::KeyState::release_all`])
    /// and releases capture ([`Capture::release`]).
    pub fn release_chord(&self, held: Modifiers) -> bool {
        self.captured && held.control && held.option && held.command
    }

    /// The text the window's title ends with: what capture is on and how to release it while
    /// captured, and that a click captures while not.
    pub fn title_suffix(&self) -> &'static str {
        if self.captured {
            CAPTURED_SUFFIX
        } else {
            UNCAPTURED_SUFFIX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHORD: Modifiers = Modifiers {
        control: true,
        option: true,
        command: true,
        shift: false,
    };

    /// A click into the image captures; the click that makes the window key is not forwarded
    #[test]
    fn a_click_in_the_image_captures() {
        let mut capture = Capture::new();
        assert!(!capture.is_captured());
        assert_eq!(
            capture.click_in_image(false),
            ClickAction { forward: false }
        );
        assert!(capture.is_captured());
        assert_eq!(capture.click_in_image(true), ClickAction { forward: true });
        assert!(capture.is_captured());
        // Released, the window still key: the next click captures and is forwarded.
        assert!(capture.release());
        assert_eq!(capture.click_in_image(true), ClickAction { forward: true });
        assert!(capture.is_captured());
    }

    /// Captured, every key goes to the guest, ⌘ shortcuts included
    #[test]
    fn captured_keys_go_to_the_guest() {
        let mut capture = Capture::new();
        capture.click_in_image(true);
        assert_eq!(capture.route(true), Route::Guest);
        assert_eq!(capture.route(false), Route::Guest);
    }

    /// Uncaptured, ⌘ keys go to the host's menu and other keys to the guest
    #[test]
    fn uncaptured_command_goes_to_the_host() {
        let capture = Capture::new();
        assert_eq!(capture.route(true), Route::Host);
        assert_eq!(capture.route(false), Route::Guest);
    }

    /// ⌃⌥⌘ together releases capture, with or without Shift; fewer do not
    #[test]
    fn control_option_command_releases() {
        let mut capture = Capture::new();
        assert!(!capture.release_chord(CHORD), "not captured");
        capture.click_in_image(true);
        assert!(capture.release_chord(CHORD));
        assert!(capture.release_chord(Modifiers {
            shift: true,
            ..CHORD
        }));
        for held in [
            Modifiers {
                control: false,
                ..CHORD
            },
            Modifiers {
                option: false,
                ..CHORD
            },
            Modifiers {
                command: false,
                ..CHORD
            },
            Modifiers::default(),
        ] {
            assert!(!capture.release_chord(held), "{held:?}");
        }
        assert!(capture.release());
        assert!(!capture.is_captured());
        assert!(!capture.release());
    }

    /// Losing the keyboard releases capture, and says whether it was captured
    #[test]
    fn losing_focus_releases() {
        let mut capture = Capture::new();
        capture.click_in_image(false);
        assert!(capture.resign_key());
        assert!(!capture.is_captured());
        assert_eq!(capture.route(true), Route::Host);
        assert!(!capture.resign_key());
    }

    /// Captured, the pointer is held over the image; released, by the chord or by losing the
    /// keyboard, it is free; an image of no area holds nothing
    #[test]
    fn captured_input_holds_the_pointer_over_the_image() {
        let image = Rect {
            x: 100.0,
            y: 50.5,
            width: 960.0,
            height: 600.0,
        };
        let mut capture = Capture::new();
        assert_eq!(capture.pointer_confinement(image), None);
        capture.click_in_image(false);
        assert_eq!(capture.pointer_confinement(image), Some(image));
        for empty in [
            Rect::default(),
            Rect {
                width: 0.0,
                ..image
            },
            Rect {
                height: 0.0,
                ..image
            },
        ] {
            assert_eq!(capture.pointer_confinement(empty), None, "{empty:?}");
        }
        capture.release();
        assert_eq!(capture.pointer_confinement(image), None);
        capture.click_in_image(true);
        capture.resign_key();
        assert_eq!(capture.pointer_confinement(image), None);
    }

    /// The title says a click captures while not captured, and how to release while captured
    #[test]
    fn the_title_says_whether_input_is_captured() {
        let mut capture = Capture::new();
        assert_eq!(capture.title_suffix(), " \u{2014} click to capture input");
        capture.click_in_image(true);
        assert!(capture.title_suffix().contains("captured"));
        assert!(capture.title_suffix().contains("\u{2303}\u{2325}\u{2318}"));
        capture.release();
        assert_eq!(capture.title_suffix(), UNCAPTURED_SUFFIX);
        capture.click_in_image(false);
        capture.resign_key();
        assert_eq!(capture.title_suffix(), UNCAPTURED_SUFFIX);
    }
}
