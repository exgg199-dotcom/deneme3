//! Input belongs to geometry the renderer has actually presented.
use super::*;

#[cfg(feature = "test-support")]
#[path = "presentation/probe.rs"]
pub(super) mod probe;

impl Game<'_> {
    /// No renderer means a headless control session, not an unpresented window.
    pub(super) fn presentation_stale(&self) -> bool {
        #[cfg(feature = "test-support")]
        if self.presentation_test_bypass {
            return false;
        }
        self.renderer
            .as_ref()
            .is_some_and(|r| !r.presentation_current())
    }

    pub(super) fn menu_input_blocked(&self) -> bool {
        self.reload_blocks_input() || self.presentation_stale()
    }

    /// Retire geometry-dependent gestures once per unpresented interval. Do not
    /// release keyboard movement: rotating a world is not opening a new screen.
    pub(super) fn sync_presentation_input(&mut self, event_loop: &dyn EventLoopCtl) {
        let stale = self.presentation_stale();
        if !stale {
            self.presentation_input_blocked = false;
            return;
        }
        if std::mem::replace(&mut self.presentation_input_blocked, true) {
            return;
        }
        self.cancel_pointer_drags();
        self.cancel_touch_and_release_mouse();
        self.cancel_emote_controller();
        self.pad_cancel_inputs(event_loop);
        self.suspend_system_keyboard();
        self.capture(false);
    }

    /// Finish slider edits and cancel pointer drags without activating controls.
    pub(super) fn cancel_pointer_drags(&mut self) {
        self.drag_button = None;
        self.dragging = false;
        self.inv_drag = None;
        // The drag has already applied its value. Save it without a synthetic click.
        let mut events = self.ui.options.release();
        events.retain(|event| !matches!(event, horizon_client::options_ui::Event::ButtonClick));
        if let Err(error) = self.handle_option_events(events) {
            log::warn!("Horizon: finishing options after a pointer interruption: {error}");
        }
        if let Some(title) = &mut self.title {
            title.release();
            title.release_field();
        }
        self.hud_editor_release();
    }

    /// Cancel touches after an interruption and release any mouse mappings they held.
    pub(super) fn cancel_touch_and_release_mouse(&mut self) {
        let touch_buttons = [MouseButton::Left, MouseButton::Right]
            .map(|button| (button, self.touch_holds_mouse_mapping(button)));
        self.cancel_touch_for_reload();
        for (button, held) in touch_buttons {
            if held && let Err(error) = self.mouse_mapping(button, false, Source::Touch) {
                log::warn!("Horizon: retiring touch after geometry change: {error}");
            }
        }
    }

    /// A finger captured by an old menu must not send a release into its replacement.
    pub(super) fn discard_stale_touch_menu(&mut self) -> bool {
        let stale = self
            .touch
            .menu
            .as_ref()
            .is_some_and(|finger| finger.screen != self.key_screen());
        if stale {
            self.touch.menu = None;
            self.touch.window_buttons.remove(&MouseButton::Left);
            self.touch_drag_button = None;
        }
        stale
    }
}
