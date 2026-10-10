//! Scaffolding for eat's alt-screen viewer: the key classification --
//! which key means what, including the vi-style aliases and the platform
//! primary modifier -- and the terminal session. The view in
//! [`super::views`] keeps the interpretation.

use crate::helpers::CoordType;
use crate::input::{InputKey, kbmod, vk};

/// Visual columns per Left/Right press when wrap is off. Matches less's
/// default horizontal scroll step.
pub(crate) const H_SCROLL_STEP: CoordType = 8;

/// What a keypress means to a viewer.
///
/// Deliberately describes intent rather than an action to perform: the
/// two views implement several of these differently, and flattening that
/// into one shared handler is what would put the duplication back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewerKey {
    /// `q` or Escape, plus the editor's exit chords Ctrl+Q and Ctrl+W.
    Quit,
    /// Copy the selection. The textarea is mounted unfocused, so its own
    /// chord never fires and the view has to do this itself.
    Copy,
    SelectAll,
    /// Bare `w` -- toggle word wrap.
    ToggleWrap,
    /// `r` -- load the source again.
    Reload,
    /// Signed line count: negative is up.
    ScrollLines(CoordType),
    /// Signed page count: negative is up. The view scales by its own
    /// body height, which only it knows.
    ScrollPages(CoordType),
    /// Signed column count: negative is left.
    ScrollColumns(CoordType),
    /// Home, or `g`.
    ToTop,
    /// End, or `G`.
    ToBottom,
}

/// The platform's primary chord modifier: Cmd on macOS, Ctrl elsewhere.
fn primary_modifier() -> crate::input::InputKeyMod {
    if cfg!(any(target_os = "macos", target_os = "ios")) { kbmod::CMD } else { kbmod::CTRL }
}

/// Map a keypress to viewer intent, or `None` to leave it unhandled.
///
/// vi aliases mirror less: `j`/`k` for down/up, `h`/`l` for left/right,
/// `g`/`G` for top/bottom. Shift is only consulted where it changes
/// meaning (`g` vs `G`), so the lowercase arms explicitly reject it
/// rather than matching both.
pub(crate) fn classify(k: InputKey) -> Option<ViewerKey> {
    let bare = k.key();
    let shifted = k.modifiers_contains(kbmod::SHIFT);
    let primary = k.modifiers_contains(primary_modifier());
    let ctrl = k.modifiers_contains(kbmod::CTRL);

    if bare == vk::Q || bare == vk::ESCAPE || (bare == vk::W && ctrl) {
        Some(ViewerKey::Quit)
    } else if bare == vk::C && primary {
        Some(ViewerKey::Copy)
    } else if bare == vk::A && primary {
        Some(ViewerKey::SelectAll)
    } else if bare == vk::W {
        Some(ViewerKey::ToggleWrap)
    } else if bare == vk::R {
        Some(ViewerKey::Reload)
    } else if bare == vk::UP || (bare == vk::K && !shifted) {
        Some(ViewerKey::ScrollLines(-1))
    } else if bare == vk::DOWN || (bare == vk::J && !shifted) {
        Some(ViewerKey::ScrollLines(1))
    } else if bare == vk::PRIOR {
        Some(ViewerKey::ScrollPages(-1))
    } else if bare == vk::NEXT {
        Some(ViewerKey::ScrollPages(1))
    } else if bare == vk::LEFT || (bare == vk::H && !shifted) {
        Some(ViewerKey::ScrollColumns(-H_SCROLL_STEP))
    } else if bare == vk::RIGHT || (bare == vk::L && !shifted) {
        Some(ViewerKey::ScrollColumns(H_SCROLL_STEP))
    } else if bare == vk::HOME || (bare == vk::G && !shifted) {
        Some(ViewerKey::ToTop)
    } else if bare == vk::END || (bare == vk::G && shifted) {
        Some(ViewerKey::ToBottom)
    } else {
        None
    }
}

/// Terminal setup shared by both views, unwound in reverse on drop.
///
/// Animations go off for the lifetime of a viewer: with a poll interval
/// in the hundreds of milliseconds the textarea's ~60ms scroll lerp lands
/// between ticks, so the viewport sits still and then jumps, which reads
/// as stutter rather than motion. Snapping per tick is what gives the
/// steady cadence. The editor's own animations are unaffected -- the
/// previous value is restored here.
pub(crate) struct ViewerSession {
    prev_no_animations: bool,
    _deinit: crate::sys::Deinit,
}

impl ViewerSession {
    pub fn begin() -> std::io::Result<Self> {
        let _deinit = crate::sys::init();
        crate::sys::switch_modes()?;
        let prev_no_animations = crate::glyphs::no_animations();
        crate::glyphs::set_no_animations(true);
        Ok(Self { prev_no_animations, _deinit })
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        crate::glyphs::set_no_animations(self.prev_no_animations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: InputKey) -> Option<ViewerKey> {
        classify(k)
    }

    #[test]
    fn quit_on_q_or_escape() {
        assert_eq!(key(vk::Q), Some(ViewerKey::Quit));
        assert_eq!(key(vk::ESCAPE), Some(ViewerKey::Quit));
    }

    #[test]
    fn ctrl_w_quits_but_bare_w_wraps() {
        assert_eq!(key(kbmod::CTRL | vk::Q), Some(ViewerKey::Quit));
        assert_eq!(key(kbmod::CTRL | vk::W), Some(ViewerKey::Quit));
        assert_eq!(key(vk::W), Some(ViewerKey::ToggleWrap));
    }

    #[test]
    fn vi_aliases_match_their_arrow_keys() {
        assert_eq!(key(vk::K), key(vk::UP));
        assert_eq!(key(vk::J), key(vk::DOWN));
        assert_eq!(key(vk::H), key(vk::LEFT));
        assert_eq!(key(vk::L), key(vk::RIGHT));
        assert_eq!(key(vk::G), key(vk::HOME));
    }

    #[test]
    fn shift_g_is_bottom_but_plain_g_is_top() {
        assert_eq!(key(vk::G), Some(ViewerKey::ToTop));
        assert_eq!(key(vk::G.with_modifiers(kbmod::SHIFT)), Some(ViewerKey::ToBottom));
        assert_eq!(key(vk::END), Some(ViewerKey::ToBottom));
    }

    #[test]
    fn shift_does_not_smuggle_in_the_lowercase_aliases() {
        // Shift+J is not "scroll down" -- only the bare key is.
        for k in [vk::J, vk::K, vk::H, vk::L] {
            assert_eq!(key(k.with_modifiers(kbmod::SHIFT)), None, "{k:?} with shift");
        }
    }

    #[test]
    fn copy_and_select_all_need_the_primary_modifier() {
        assert_eq!(key(vk::C), None, "bare c is not copy");
        assert_eq!(key(vk::A), None, "bare a is not select-all");
        let primary = if cfg!(any(target_os = "macos", target_os = "ios")) {
            kbmod::CMD
        } else {
            kbmod::CTRL
        };
        assert_eq!(key(vk::C.with_modifiers(primary)), Some(ViewerKey::Copy));
        assert_eq!(key(vk::A.with_modifiers(primary)), Some(ViewerKey::SelectAll));
    }

    #[test]
    fn paging_is_signed_and_unscaled() {
        // The view multiplies by its own body height.
        assert_eq!(key(vk::PRIOR), Some(ViewerKey::ScrollPages(-1)));
        assert_eq!(key(vk::NEXT), Some(ViewerKey::ScrollPages(1)));
    }

    #[test]
    fn unknown_keys_are_left_alone() {
        assert_eq!(key(vk::F1), None);
        assert_eq!(key(vk::TAB), None);
    }
}
