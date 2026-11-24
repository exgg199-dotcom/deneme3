//! The game on an AirPlay display (iPhone and iPad; Horizon's own): while one is connected, the renderer draws
//! on it at its own size, the touch controls are put away, and the phone says "Playing on AirPlay" over
//! everything (crate `horizon-airplay`). It is played with a controller meanwhile. In splitscreen the display's
//! top half is this game's and its bottom half the second player's (game/splitscreen.rs). Both the game and
//! the loading screen before it call [`sync`] every frame; everywhere but iOS it finds no display and does
//! nothing.
use super::Game;
use horizon_airplay::{Presentation, Screen};
use horizon_client::gfx::Renderer;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use winit::window::Window;

const RETRY_DELAY: Duration = Duration::from_secs(2);

/// The latest requested display, with a bounded retry after a failed handoff.
#[derive(Default)]
pub(super) struct Shown {
    requested: Option<(u64, (u32, u32))>,
    retry_at: Option<Instant>,
}

/// Move the renderer to `screen`, a view of the AirPlay display, or back to `window` without one, when that
/// changed since `shown`.
pub(super) fn sync(
    renderer: Option<&mut Renderer>,
    window: Option<&Arc<Window>>,
    shown: &mut Shown,
    screen: Option<Screen>,
) {
    let Some(renderer) = renderer else {
        return;
    };
    let want = screen.as_ref().map(|s| (s.id, s.size));
    let now = Instant::now();
    if want == shown.requested
        && shown.retry_at.map_or_else(
            || renderer.matches_external_target(want),
            |retry| now < retry,
        )
    {
        return;
    }
    let phone = window.map_or((renderer.config.width, renderer.config.height), |w| {
        let size = w.inner_size();
        (size.width, size.height)
    });
    let result = match screen {
        Some(screen) => {
            let size = screen.size;
            log::info!("Horizon: AirPlay display at {}x{}", size.0, size.1);
            renderer.set_target(Some((screen.id, Arc::new(screen))), size)
        }
        None => {
            log::info!("Horizon: drawing on the phone");
            renderer.set_target(None, phone)
        }
    };
    shown.requested = want;
    shown.retry_at = None;
    if let Err(error) = result {
        log::warn!("Horizon: AirPlay display: {error}");
        if want.is_some()
            && let Err(error) = renderer.set_target(None, phone)
        {
            log::error!("Horizon: back on the phone after AirPlay: {error}");
        }
        // A connected view can be briefly unavailable during scene setup.
        // Retry even if UIKit keeps its id and size, without doing so per frame.
        shown.retry_at = Some(Instant::now() + RETRY_DELAY);
    }
}

/// Follow the main game or loading screen. The phone cover is shown only after the
/// renderer presents a frame to its external display.
pub(super) fn sync_primary(
    mut renderer: Option<&mut Renderer>,
    window: Option<&Arc<Window>>,
    shown: &mut Shown,
    playing: bool,
    split: bool,
) {
    let screen = horizon_airplay::screen(playing, split);
    sync(renderer.as_deref_mut(), window, shown, screen);
    horizon_airplay::rendering(presentation(renderer.as_deref().filter(|_| playing)));
}

pub(super) fn presentation(renderer: Option<&Renderer>) -> Presentation {
    renderer.map_or(Presentation::Detached, |renderer| {
        Presentation::for_frame(renderer.targeted(), renderer.presentation_current())
    })
}

impl Game<'_> {
    /// The game draws on an AirPlay display, so the phone's window is not where it is played.
    pub(super) fn on_airplay(&self) -> bool {
        self.renderer.as_ref().is_some_and(|r| r.targeted())
    }

    pub(super) fn sync_airplay(&mut self) {
        if self.splitscreen.guest {
            return;
        }
        sync_primary(
            self.renderer.as_mut(),
            self.window.as_ref(),
            &mut self.airplay,
            !horizon_ios_launcher::covered(),
            self.splitscreen.active,
        );
    }
}
