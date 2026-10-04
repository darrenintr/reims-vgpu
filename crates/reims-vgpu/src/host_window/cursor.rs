//! The guest's cursor, as the host window shows it ([[host-window]]).
//!
//! # What this carries, and in which direction
//!
//! [`super::input_map`] carries the *host's* pointer to the guest: a position in
//! window pixels becomes a `InputPointerMove`. Nothing carried the guest's
//! pointer *image* the other way, so a window the guest was drawing a cursor
//! for showed the desktop's arrow over a macOS login screen, an I-beam
//! the guest had asked for, and a resize handle it had chosen — the host arrow
//! the whole time. The device already held the glyph; the QEMU console frontends
//! consumed it, and the Rust-owned window did not.
//!
//! The data path is one-way and owned end to end by this module's types:
//!
//! ```text
//! drain (device lock)   publish_window_cursor ──► CursorSlot (own mutex)
//!                                                   │  one WindowWake::Cursor
//! window thread         user_event ◄────────────────┘
//!                       CursorPolicy ──► winit custom cursor / hidden / default
//! ```
//!
//! The window thread never reaches the device. It reads an immutable
//! [`CursorSnapshot`] from a slot that has its own lock, which is what keeps a
//! cursor change from waiting behind a render tranche the way the device lock
//! would make it — and what keeps the pointer at native latency, because the
//! compositor draws the image and nothing here sits on the pointer's path.
//!
//! # Position is the host's
//!
//! The pointer is where the physical pointer is. The guest's own x/y rides the
//! snapshot for diagnosis and is never used to place anything: Wayland has no
//! general pointer warp, and the absolute tablet path the guest listens to
//! already makes the two agree. A snapshot is published when the *image or
//! visibility* changes, never on movement.
//!
//! # The alpha convention, established and not assumed
//!
//! `QEMUCursor` pixels are `0xAARRGGBB` with **straight** (non-premultiplied)
//! alpha, and every consumer of them says so: `ui/cocoa.m` builds the layer image
//! with `kCGBitmapByteOrder32Little | kCGImageAlphaFirst` — alpha-first, *not*
//! `kCGImageAlphaPremultipliedFirst` — and `ui/sdl2.c` hands the pixels to SDL
//! with explicit `0xff0000/0x00ff00/0xff/0xff000000` masks, which SDL reads as
//! straight alpha. The device copies the guest's bytes into that word unchanged
//! (`load_cursor_glyph`: guest `B,G,R,A` to `A<<24|R<<16|G<<8|B`), so the guest's
//! convention is whatever those frontends already assume.
//!
//! winit's `CustomCursor::from_rgba` is documented to take straight alpha too —
//! "the alpha channel is assumed to be **not** premultiplied" — and its Wayland
//! backend premultiplies for `wl_shm` itself. So the whole conversion is a
//! channel reorder. Premultiplying here would darken every soft edge twice, and
//! un-premultiplying would brighten them, and neither can be checked against a
//! glyph this crate does not have. A fully transparent pixel keeps its colour
//! channels for the same reason: they are the guest's, and the compositor
//! decides what they mean at alpha zero.
//!
//! # Which cursor, when
//!
//! `CursorPolicy` is the whole decision, and it is a pure function of two
//! facts: whether the window holds the capture ([`super::keyboard::Keyboard::is_grabbed`]),
//! and the latest snapshot. Only one cursor is ever *asked for*:
//!
//! | captured | snapshot                    | shown                        |
//! |----------|-----------------------------|------------------------------|
//! | no       | any                         | the host's default arrow     |
//! | yes      | none — the guest has said nothing | the host's default arrow |
//! | yes      | hidden                      | no cursor                    |
//! | yes      | visible, no glyph yet       | the host's default arrow     |
//! | yes      | visible, with a glyph       | the guest's glyph            |
//!
//! "The guest has said nothing" and "the guest hid it" are different rows on
//! purpose: the device's `show` starts true, so a window that hid the pointer
//! until the guest spoke would hide it for the whole of firmware boot.
//! Releasing the capture — focus lost, or Ctrl+Alt+Esc — always lands on the
//! first row, so the desktop's pointer comes back, and recapturing re-derives the
//! row from the latest snapshot, so it comes back as the *current* glyph and not
//! whichever one was showing when the capture was lost.

use std::sync::{Arc, Mutex};

/// One guest cursor image, immutable once published.
///
/// Shared by `Arc` between the snapshots that carry it, because a show/hide
/// keeps the glyph and the pixels are up to 512x512x4 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorGlyph {
    /// Which glyph the device held: bumped by every glyph the guest sent, even
    /// one identical to the last. What `CursorPolicy` compares to know the
    /// image changed.
    pub serial: u64,
    pub width: u16,
    pub height: u16,
    /// The hotspot in glyph pixels, from the glyph's own top-left.
    pub hot_x: u16,
    pub hot_y: u16,
    /// `0xAARRGGBB`, straight alpha, `width * height` of them, row-major. See the
    /// module documentation for why this is not premultiplied.
    pub argb: Arc<[u32]>,
}

/// Everything the window needs to know about the guest's cursor, at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorSnapshot {
    /// Publication order, from 1. The window ignores a snapshot older than the
    /// one it holds.
    pub seq: u64,
    /// Whether the guest wants a cursor shown. Independent of whether a glyph
    /// exists.
    pub visible: bool,
    /// The glyph, once the guest has sent one. Kept while hidden, so showing
    /// again is not a second copy.
    pub glyph: Option<CursorGlyph>,
    /// The guest's own idea of where the cursor is, as of the publication.
    /// Diagnosis only: it is not republished on movement and places nothing.
    pub guest_pos: (u16, u16),
}

/// The latest cursor snapshot, shared between the device that publishes it and
/// the window thread that reads it.
///
/// Latest-wins and `Arc`-wrapped for the reason [`super::present::FrameSlot`] is:
/// the reader's cost is a refcount bump, and a snapshot superseded before the
/// window looked is a change nobody needed to see. Its own lock — never the
/// device's.
#[derive(Debug, Default)]
pub struct CursorSlot {
    latest: Mutex<Option<Arc<CursorSnapshot>>>,
}

impl CursorSlot {
    /// Replace the latest snapshot. `None` is the guest having forgotten its
    /// cursor, which a reset does.
    pub fn store(&self, snapshot: Option<Arc<CursorSnapshot>>) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = snapshot;
        }
    }

    /// The latest snapshot, if the guest has said anything.
    pub fn load(&self) -> Option<Arc<CursorSnapshot>> {
        self.latest.lock().ok().and_then(|latest| latest.clone())
    }
}

/// A glyph in the layout winit takes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rgba {
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) hot_x: u16,
    pub(super) hot_y: u16,
    /// `R, G, B, A` per pixel, straight alpha.
    pub(super) rgba: Vec<u8>,
}

/// Why a glyph cannot become a cursor. Typed, so the line that names it is one
/// of four and not free text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GlyphRefusal {
    /// A zero dimension.
    Empty,
    /// The pixel count is not `width * height`.
    PixelCount { expected: usize, got: usize },
    /// The hotspot is not inside the image.
    HotspotOutside,
}

impl GlyphRefusal {
    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::Empty => "host_cursor_glyph_empty",
            Self::PixelCount { .. } => "host_cursor_glyph_pixel_count",
            Self::HotspotOutside => "host_cursor_glyph_hotspot_outside",
        }
    }
}

/// Reorder a glyph's `0xAARRGGBB` pixels into `R, G, B, A` bytes.
///
/// A reorder and nothing else: straight alpha in, straight alpha out, every
/// channel of every pixel kept, including the colour of a transparent one. See
/// the module documentation for what establishes that.
pub(super) fn argb_to_rgba(glyph: &CursorGlyph) -> Result<Rgba, GlyphRefusal> {
    if glyph.width == 0 || glyph.height == 0 {
        return Err(GlyphRefusal::Empty);
    }
    let expected = usize::from(glyph.width) * usize::from(glyph.height);
    if glyph.argb.len() != expected {
        return Err(GlyphRefusal::PixelCount {
            expected,
            got: glyph.argb.len(),
        });
    }
    if glyph.hot_x >= glyph.width || glyph.hot_y >= glyph.height {
        return Err(GlyphRefusal::HotspotOutside);
    }
    let mut rgba = Vec::with_capacity(expected * 4);
    for &pixel in glyph.argb.iter() {
        rgba.extend_from_slice(&[
            (pixel >> 16) as u8,
            (pixel >> 8) as u8,
            pixel as u8,
            (pixel >> 24) as u8,
        ]);
    }
    Ok(Rgba {
        width: glyph.width,
        height: glyph.height,
        hot_x: glyph.hot_x,
        hot_y: glyph.hot_y,
        rgba,
    })
}

/// What the window is showing as its pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    /// The platform's default arrow.
    Host,
    /// No pointer at all.
    Hidden,
    /// The guest's glyph, by serial.
    Guest { serial: u64 },
}

impl Shown {
    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Hidden => "hidden",
            Self::Guest { .. } => "guest",
        }
    }
}

/// Why the pointer changed, for the one line that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    /// The guest sent an image.
    Glyph,
    /// The guest showed or hid its cursor.
    Visibility,
    /// The window took or gave up the capture.
    Capture,
}

impl Reason {
    pub(super) const fn slug(self) -> &'static str {
        match self {
            Self::Glyph => "glyph",
            Self::Visibility => "visibility",
            Self::Capture => "capture",
        }
    }
}

/// A change of pointer, with the facts the line about it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Transition {
    pub(super) shown: Shown,
    pub(super) reason: Reason,
    /// The snapshot the decision read, or `None` if the guest had said nothing.
    pub(super) snapshot: Option<Arc<CursorSnapshot>>,
    pub(super) grabbed: bool,
}

impl Transition {
    /// `host_window_cursor`, one line per change. Never per move: nothing here
    /// is called on a pointer event.
    pub(super) fn line(&self) -> String {
        let (seq, visible, size, hot) = match self.snapshot.as_deref() {
            Some(snapshot) => (
                snapshot.seq,
                u8::from(snapshot.visible),
                snapshot
                    .glyph
                    .as_ref()
                    .map_or((0, 0), |g| (g.width, g.height)),
                snapshot
                    .glyph
                    .as_ref()
                    .map_or((0, 0), |g| (g.hot_x, g.hot_y)),
            ),
            None => (0, 0, (0, 0), (0, 0)),
        };
        format!(
            "host_window_cursor seq={seq} visible={visible} width={} height={} hot={},{} \
             reason={} shown={} grabbed={}",
            size.0,
            size.1,
            hot.0,
            hot.1,
            self.reason.slug(),
            self.shown.slug(),
            u8::from(self.grabbed),
        )
    }
}

/// Which pointer the window shows, from the capture and the latest snapshot.
///
/// Holds no window: it says what should be shown and what is, and the caller
/// does the showing and reports back through [`Self::applied`]. That keeps the
/// decision testable with no display, and keeps a refused glyph — one winit
/// would not take — from being recorded as shown when it is not.
#[derive(Debug)]
pub(super) struct CursorPolicy {
    grabbed: bool,
    latest: Option<Arc<CursorSnapshot>>,
    shown: Shown,
}

impl CursorPolicy {
    /// A window that has applied nothing: the platform's arrow, uncaptured.
    pub(super) fn new() -> Self {
        Self {
            grabbed: false,
            latest: None,
            shown: Shown::Host,
        }
    }

    /// What the policy believes is on the window. For the tests: production
    /// reads it only through the [`Transition`]s it is handed.
    #[cfg(test)]
    pub(super) fn shown(&self) -> Shown {
        self.shown
    }

    /// The table in the module documentation.
    fn wanted(&self) -> Shown {
        if !self.grabbed {
            return Shown::Host;
        }
        match self.latest.as_deref() {
            None => Shown::Host,
            Some(snapshot) if !snapshot.visible => Shown::Hidden,
            Some(snapshot) => match snapshot.glyph.as_ref() {
                Some(glyph) => Shown::Guest {
                    serial: glyph.serial,
                },
                None => Shown::Host,
            },
        }
    }

    fn settle(&mut self, reason: Reason) -> Option<Transition> {
        let wanted = self.wanted();
        if wanted == self.shown {
            return None;
        }
        self.shown = wanted;
        Some(Transition {
            shown: wanted,
            reason,
            snapshot: self.latest.clone(),
            grabbed: self.grabbed,
        })
    }

    /// The capture changed hands.
    pub(super) fn set_grabbed(&mut self, grabbed: bool) -> Option<Transition> {
        if self.grabbed == grabbed {
            return None;
        }
        self.grabbed = grabbed;
        self.settle(Reason::Capture)
    }

    /// A snapshot was read from the slot.
    ///
    /// A snapshot older than the one held is ignored: the slot is latest-wins,
    /// but a reader that raced a publisher could still be handed an old one, and
    /// applying it would put a superseded glyph on screen.
    pub(super) fn set_snapshot(
        &mut self,
        snapshot: Option<Arc<CursorSnapshot>>,
    ) -> Option<Transition> {
        if let (Some(held), Some(new)) = (self.latest.as_deref(), snapshot.as_deref()) {
            if new.seq < held.seq {
                return None;
            }
        }
        let reason = match (self.latest.as_deref(), snapshot.as_deref()) {
            (Some(old), Some(new)) if old.visible != new.visible => Reason::Visibility,
            (None, Some(new)) if !new.visible => Reason::Visibility,
            (Some(_), None) => Reason::Visibility,
            _ => Reason::Glyph,
        };
        self.latest = snapshot;
        self.settle(reason)
    }

    /// Record what was actually applied, when it was not what was asked for — a
    /// glyph the toolkit refused falls back to the host's arrow, and the policy
    /// must not believe the guest's was shown.
    pub(super) fn applied(&mut self, shown: Shown) {
        self.shown = shown;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_window::keyboard::Keyboard;

    fn glyph(serial: u64, width: u16, height: u16, hot: (u16, u16), fill: u32) -> CursorGlyph {
        CursorGlyph {
            serial,
            width,
            height,
            hot_x: hot.0,
            hot_y: hot.1,
            argb: vec![fill; usize::from(width) * usize::from(height)].into(),
        }
    }

    fn snapshot(seq: u64, visible: bool, glyph: Option<CursorGlyph>) -> Arc<CursorSnapshot> {
        Arc::new(CursorSnapshot {
            seq,
            visible,
            glyph,
            guest_pos: (0, 0),
        })
    }

    /// The four glyphs a guest walks through in the order a pointer crosses a
    /// window edge: arrow, I-beam, a resize handle, and the arrow again. Distinct
    /// sizes and hotspots, so a replacement that kept any of the old geometry
    /// would show.
    fn arrow(serial: u64) -> CursorGlyph {
        glyph(serial, 12, 19, (0, 0), 0xFF00_0000)
    }
    fn ibeam(serial: u64) -> CursorGlyph {
        glyph(serial, 9, 17, (4, 8), 0xFFFF_FFFF)
    }
    fn resize(serial: u64) -> CursorGlyph {
        glyph(serial, 16, 16, (8, 8), 0xFF12_3456)
    }

    // ---- conversion --------------------------------------------------------

    /// Known pixels, byte for byte. The channel order is the whole of the
    /// conversion, so each pixel is chosen to make a wrong order visible: every
    /// channel differs, and the alpha is neither 0 nor 255.
    #[test]
    fn argb_pixels_become_straight_alpha_rgba_bytes() {
        let source = CursorGlyph {
            serial: 1,
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 0,
            argb: vec![
                0x80FF_8040, // A=0x80 R=0xFF G=0x80 B=0x40
                0xFF01_0203, // opaque, every channel distinct
                0x0012_3456, // transparent, colour present
                0x7F7F_7F7F, // mid grey, mid alpha
            ]
            .into(),
        };
        let rgba = argb_to_rgba(&source).expect("a well-formed glyph");
        assert_eq!(
            rgba.rgba,
            vec![
                0xFF, 0x80, 0x40, 0x80, // R G B A
                0x01, 0x02, 0x03, 0xFF, //
                0x12, 0x34, 0x56, 0x00, //
                0x7F, 0x7F, 0x7F, 0x7F,
            ]
        );
    }

    /// The alpha convention, pinned: nothing multiplies or divides by alpha.
    ///
    /// A premultiplied conversion would turn `0x80FF8040`'s red into `0x80` and a
    /// transparent pixel's colour into zero; an un-premultiplying one would
    /// saturate. Either would be a visible fringe on every soft edge, and neither
    /// is detectable without this.
    #[test]
    fn alpha_is_neither_premultiplied_nor_undone() {
        let half = glyph(1, 1, 1, (0, 0), 0x80C8_6432);
        assert_eq!(
            argb_to_rgba(&half).expect("glyph").rgba,
            vec![0xC8, 0x64, 0x32, 0x80]
        );
        let clear = glyph(1, 1, 1, (0, 0), 0x00C8_6432);
        assert_eq!(
            argb_to_rgba(&clear).expect("glyph").rgba,
            vec![0xC8, 0x64, 0x32, 0x00],
            "a transparent pixel keeps its colour: it is the guest's"
        );
    }

    /// Width, height and hotspot are the guest's, exactly.
    #[test]
    fn size_and_hotspot_survive_the_conversion_exactly() {
        for (width, height, hot) in [(12, 19, (0, 0)), (9, 17, (4, 8)), (512, 512, (511, 511))] {
            let source = glyph(7, width, height, hot, 0xFF00_00FF);
            let rgba = argb_to_rgba(&source).expect("glyph");
            assert_eq!((rgba.width, rgba.height), (width, height));
            assert_eq!((rgba.hot_x, rgba.hot_y), hot);
            assert_eq!(
                rgba.rgba.len(),
                usize::from(width) * usize::from(height) * 4
            );
        }
    }

    /// What this module hands winit is what winit accepts: its own constructor
    /// checks the byte count, the size bound and the hotspot, so a conversion that
    /// disagreed with it fails here rather than on a live window.
    #[test]
    fn winit_accepts_the_largest_glyph_the_device_will_publish() {
        let source = glyph(
            1,
            crate::model::CURSOR_MAX_DIM as u16,
            crate::model::CURSOR_MAX_DIM as u16,
            (3, 5),
            0xFFAA_BBCC,
        );
        let rgba = argb_to_rgba(&source).expect("glyph");
        assert!(winit::window::CustomCursor::from_rgba(
            rgba.rgba,
            rgba.width,
            rgba.height,
            rgba.hot_x,
            rgba.hot_y
        )
        .is_ok());
    }

    #[test]
    fn a_malformed_glyph_is_refused_by_name_and_never_guessed_at() {
        assert_eq!(
            argb_to_rgba(&glyph(1, 0, 4, (0, 0), 0)),
            Err(GlyphRefusal::Empty)
        );
        let mut short = glyph(1, 4, 4, (0, 0), 0);
        short.argb = vec![0u32; 15].into();
        assert_eq!(
            argb_to_rgba(&short),
            Err(GlyphRefusal::PixelCount {
                expected: 16,
                got: 15
            })
        );
        assert_eq!(
            argb_to_rgba(&glyph(1, 4, 4, (4, 0), 0)),
            Err(GlyphRefusal::HotspotOutside)
        );
        assert_ne!(
            GlyphRefusal::Empty.slug(),
            GlyphRefusal::HotspotOutside.slug()
        );
    }

    // ---- which cursor ------------------------------------------------------

    fn captured_with(snapshot: Option<Arc<CursorSnapshot>>) -> CursorPolicy {
        let mut policy = CursorPolicy::new();
        policy.set_snapshot(snapshot);
        policy.set_grabbed(true);
        policy
    }

    /// The guest hides its cursor and shows it again, with the glyph kept.
    #[test]
    fn guest_hide_hides_and_guest_show_shows_the_same_glyph() {
        let mut policy = captured_with(Some(snapshot(1, true, Some(arrow(1)))));
        assert_eq!(policy.shown(), Shown::Guest { serial: 1 });

        let hide = policy
            .set_snapshot(Some(snapshot(2, false, Some(arrow(1)))))
            .expect("a visible cursor was hidden");
        assert_eq!(hide.shown, Shown::Hidden);
        assert_eq!(hide.reason, Reason::Visibility);

        let show = policy
            .set_snapshot(Some(snapshot(3, true, Some(arrow(1)))))
            .expect("a hidden cursor was shown");
        assert_eq!(show.shown, Shown::Guest { serial: 1 });
        assert_eq!(show.reason, Reason::Visibility);
    }

    /// A hide the guest sends before it has ever sent a glyph is still a hide.
    #[test]
    fn a_hide_with_no_glyph_yet_is_still_a_hide() {
        let mut policy = captured_with(None);
        assert_eq!(policy.shown(), Shown::Host, "the guest has said nothing");
        let hide = policy
            .set_snapshot(Some(snapshot(1, false, None)))
            .expect("the guest hid it");
        assert_eq!(hide.shown, Shown::Hidden);
    }

    /// A cursor the guest wants shown and has no image for is the host's arrow,
    /// and the arrow is then replaced, not stacked, when the image arrives.
    #[test]
    fn a_visible_cursor_with_no_glyph_yet_is_the_host_arrow_until_one_arrives() {
        let mut policy = captured_with(Some(snapshot(1, true, None)));
        assert_eq!(policy.shown(), Shown::Host);
        let arrived = policy
            .set_snapshot(Some(snapshot(2, true, Some(arrow(1)))))
            .expect("the glyph arrived");
        assert_eq!(arrived.shown, Shown::Guest { serial: 1 });
        assert_eq!(arrived.reason, Reason::Glyph);
    }

    /// Arrow, I-beam, resize, arrow: each is a transition naming a new serial
    /// with reason `glyph`, and the last one is a new serial even though its image
    /// equals the first — the device bumps the serial per glyph sent.
    #[test]
    fn glyph_replacement_walks_arrow_ibeam_resize_arrow() {
        let mut policy = captured_with(Some(snapshot(1, true, Some(arrow(1)))));
        let mut shown = vec![policy.shown()];
        for (seq, glyph) in [(2, ibeam(2)), (3, resize(3)), (4, arrow(4))] {
            let transition = policy
                .set_snapshot(Some(snapshot(seq, true, Some(glyph))))
                .expect("a new glyph is a transition");
            assert_eq!(transition.reason, Reason::Glyph);
            shown.push(transition.shown);
        }
        assert_eq!(
            shown,
            vec![
                Shown::Guest { serial: 1 },
                Shown::Guest { serial: 2 },
                Shown::Guest { serial: 3 },
                Shown::Guest { serial: 4 },
            ]
        );
        // The line for each carries the glyph's own geometry and hotspot.
        let last = policy
            .set_snapshot(Some(snapshot(5, true, Some(ibeam(5)))))
            .expect("transition");
        let line = last.line();
        assert!(line.contains("width=9 height=17 hot=4,8"), "{line}");
        assert!(
            line.contains("reason=glyph shown=guest grabbed=1"),
            "{line}"
        );
    }

    /// The same snapshot twice is nothing: no transition, so no line and no call
    /// into the window.
    #[test]
    fn an_unchanged_snapshot_is_not_a_transition() {
        let mut policy = captured_with(Some(snapshot(1, true, Some(arrow(1)))));
        assert_eq!(
            policy.set_snapshot(Some(snapshot(2, true, Some(arrow(1))))),
            None
        );
    }

    /// A reader handed an older snapshot than it holds keeps the newer one.
    #[test]
    fn a_superseded_snapshot_is_ignored() {
        let mut policy = captured_with(Some(snapshot(5, true, Some(ibeam(5)))));
        assert_eq!(
            policy.set_snapshot(Some(snapshot(4, true, Some(arrow(4))))),
            None
        );
        assert_eq!(policy.shown(), Shown::Guest { serial: 5 });
    }

    /// The whole capture story against the real keyboard machine: focus takes the
    /// guest cursor, Ctrl+Alt+Esc gives the desktop its arrow back, and the
    /// recapture that follows focus returning shows the glyph the guest chose
    /// *while* the capture was released, not the one showing when it was lost.
    #[test]
    fn releasing_capture_restores_the_host_cursor_and_recapture_restores_the_latest_glyph() {
        const ESC: u32 = 1;
        const LEFTCTRL: u32 = 29;
        const LEFTALT: u32 = 56;

        let mut keyboard = Keyboard::new();
        let mut policy = CursorPolicy::new();
        policy.set_snapshot(Some(snapshot(1, true, Some(arrow(1)))));
        assert_eq!(
            policy.shown(),
            Shown::Host,
            "an uncaptured window shows the desktop's pointer whatever the guest chose"
        );

        // Focus gained: captured, and the guest's glyph is immediate.
        keyboard.focus(true);
        let captured = policy
            .set_grabbed(keyboard.is_grabbed())
            .expect("capture engaged");
        assert_eq!(captured.shown, Shown::Guest { serial: 1 });
        assert_eq!(captured.reason, Reason::Capture);

        // Ctrl+Alt+Esc: released while still focused.
        keyboard.key(LEFTCTRL, true);
        keyboard.key(LEFTALT, true);
        keyboard.key(ESC, true);
        let released = policy
            .set_grabbed(keyboard.is_grabbed())
            .expect("the chord released the capture");
        assert_eq!(released.shown, Shown::Host);
        assert_eq!(released.reason, Reason::Capture);

        // The guest changes its cursor while the desktop owns the pointer. It is
        // remembered and shows nothing.
        assert_eq!(
            policy.set_snapshot(Some(snapshot(2, true, Some(resize(2))))),
            None
        );
        assert_eq!(policy.shown(), Shown::Host);

        // Leave and come back: the latch clears on focus loss, so focus gain
        // recaptures.
        keyboard.focus(false);
        assert_eq!(policy.set_grabbed(keyboard.is_grabbed()), None);
        keyboard.focus(true);
        let recaptured = policy
            .set_grabbed(keyboard.is_grabbed())
            .expect("capture re-armed");
        assert_eq!(
            recaptured.shown,
            Shown::Guest { serial: 2 },
            "the latest glyph, not the one showing when the capture was lost"
        );
    }

    /// Losing focus is a release even with no chord, and a hidden guest cursor
    /// does not follow the pointer out of the window: the desktop's comes back.
    #[test]
    fn focus_loss_never_leaves_the_desktop_without_a_pointer() {
        let mut policy = captured_with(Some(snapshot(1, false, Some(arrow(1)))));
        assert_eq!(policy.shown(), Shown::Hidden);
        let lost = policy.set_grabbed(false).expect("capture released");
        assert_eq!(lost.shown, Shown::Host);
        assert_ne!(lost.shown, Shown::Hidden);
    }

    /// A guest that forgets its cursor — a reset — leaves the window on the
    /// host's arrow rather than on whatever it last showed.
    #[test]
    fn a_reset_guest_falls_back_to_the_host_arrow() {
        let mut policy = captured_with(Some(snapshot(1, true, Some(arrow(1)))));
        let reset = policy.set_snapshot(None).expect("the snapshot was cleared");
        assert_eq!(reset.shown, Shown::Host);
    }

    /// A glyph winit refused is reported back, so the policy does not believe the
    /// guest's was shown and will try again on the next glyph rather than skip it
    /// as unchanged.
    #[test]
    fn a_refused_glyph_is_recorded_as_the_host_arrow() {
        let mut policy = CursorPolicy::new();
        policy.set_snapshot(Some(snapshot(1, true, Some(arrow(1)))));
        let transition = policy.set_grabbed(true).expect("captured");
        assert_eq!(transition.shown, Shown::Guest { serial: 1 });
        policy.applied(Shown::Host);
        assert_eq!(policy.shown(), Shown::Host);
        // The next glyph is a transition again, because `shown` is not it.
        assert!(policy
            .set_snapshot(Some(snapshot(2, true, Some(ibeam(2)))))
            .is_some());
    }

    /// The slot is latest-wins and starts empty.
    #[test]
    fn the_slot_holds_the_latest_snapshot() {
        let slot = CursorSlot::default();
        assert!(slot.load().is_none());
        slot.store(Some(snapshot(1, true, None)));
        slot.store(Some(snapshot(2, false, None)));
        assert_eq!(slot.load().expect("stored").seq, 2);
        slot.store(None);
        assert!(slot.load().is_none());
    }

    /// The window thread must not reach the device.
    ///
    /// The structural claim, checked as source: nothing in `host_window` outside
    /// its own tests names the device registry, a device slot, or the locks that
    /// guard device state. The reader's only inputs are the [`CursorSlot`] and the
    /// wake, each behind its own mutex, which is what stops a cursor change
    /// from waiting behind a render tranche — the device lock is held for a whole
    /// tranche, measured at 935-979 ms. A test that took the device lock and
    /// watched the reader proceed would prove nothing, because the reader has no
    /// way to name it; this proves it still has none.
    #[test]
    fn the_window_thread_names_no_device_state() {
        const FORBIDDEN: [&str; 7] = [
            "device_slot",
            "DEVICES",
            "BoundDevice",
            "DeviceInner",
            "lock_for_drain",
            "lock_device_for_vcpu",
            "crate::device::",
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/host_window");
        for entry in std::fs::read_dir(&dir).expect("host_window source directory") {
            let path = entry.expect("directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable source");
            // Everything before the file's own test module.
            let production = source.split("#[cfg(test)]").next().unwrap_or("");
            for line in production.lines() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for word in FORBIDDEN {
                    assert!(
                        !code.contains(word),
                        "{} reaches the device through `{word}`: {code}",
                        path.display()
                    );
                }
            }
        }
    }
}
