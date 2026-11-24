//! PNG encoding follows the owning-thread screenshot hook, never the frame thread.
use super::Game;
use horizon_client::gfx::{PendingScreenshot, ScreenshotSave};
use horizon_client::text::Text;
use std::path::{Path, PathBuf};

/// A capture can retain a full RGBA image and an HDR copy while it encodes.
const MAX_PLAYER_SCREENSHOTS_IN_FLIGHT: usize = 2;

pub(super) fn can_queue_player_screenshot(
    outstanding: &[PathBuf],
    replacing: Option<&Path>,
) -> bool {
    outstanding.len() < MAX_PLAYER_SCREENSHOTS_IN_FLIGHT
        || replacing.is_some_and(|path| outstanding.iter().any(|pending| pending == path))
}

pub struct ScreenshotWrite {
    save: ScreenshotSave,
    message: Option<Text>,
    chat: bool,
}

impl ScreenshotWrite {
    pub(in crate::game) fn targets(&self, path: &Path) -> bool {
        self.save.original == path
    }

    pub(super) fn start(
        pending: PendingScreenshot,
        destination: Option<(PathBuf, String)>,
        message: Option<Text>,
        chat: bool,
    ) -> Self {
        Self {
            save: pending.save_background(destination),
            message,
            chat,
        }
    }
}

/// Release names whose readbacks disappeared. One representative path is
/// enough to report the failure, even when a reset dropped several requests.
fn discard_abandoned_names(
    names: &mut Vec<PathBuf>,
    mut still_writing: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let mut first = None;
    names.retain(|path| {
        if still_writing(path) {
            return true;
        }
        if first.is_none() {
            first = Some(path.clone());
        }
        false
    });
    first
}

impl Game<'_> {
    /// Finish captures on the game thread even when the window skips redraws.
    pub(super) fn poll_screenshot_capture(&mut self) {
        self.poll_screenshot_writes();
        // A sequence consumes the readback for its own last frame.
        if self.screenshot_sequence.is_some() {
            return;
        }
        let mut consumed = false;
        let finished = self
            .renderer
            .as_mut()
            .and_then(|renderer| renderer.take_screenshot_result())
            .and_then(|result| match result {
                Ok(pending) if self.pending_screenshot.is_some() => {
                    Some(pending.save().map_err(|error| error.to_string()))
                }
                Ok(pending) => self.save_screenshot(pending),
                Err(error) => {
                    let result = Err(error.message);
                    if self.pending_screenshot.is_none() {
                        consumed = self.release_screenshot_name(&error.path);
                        if consumed {
                            self.finish_screenshot(&error.path, &result, None);
                        }
                    }
                    Some(result)
                }
            });
        if let Some(result) = &finished
            && let Some(reply) = self.pending_screenshot.take()
        {
            let _ = reply.send(
                match result {
                    Ok([w, h]) => {
                        serde_json::json!({ "ok": true, "width": w, "height": h })
                    }
                    Err(e) => serde_json::json!({ "error": e }),
                }
                .to_string(),
            );
            consumed = true;
        }
        self.fail_abandoned_screenshot();
        // A result nobody asked for was the world icon's
        // (`tick_world_icon`): a failure tries again a second
        // later, as `tryTakeScreenshotIfNeeded` does.
        if !consumed && let Some(Err(error)) = finished {
            log::warn!("Horizon: couldn't save the world icon: {error}");
        }
    }

    pub(in crate::game) fn fail_unwritten_screenshots(&mut self, message: &str) {
        let writes = &self.ui.screenshot_writes;
        let abandoned = discard_abandoned_names(&mut self.ui.screenshot_messages, |path| {
            writes.iter().any(|write| write.chat && write.targets(path))
        });
        if let Some(path) = abandoned {
            self.finish_screenshot(&path, &Err(message.to_owned()), None);
        }
    }

    /// Consume completed writes without waiting. Returns true when teardown
    /// can drop the queue without joining an active encoder on the UI thread.
    pub(super) fn poll_screenshot_writes(&mut self) -> bool {
        let mut index = 0;
        while index < self.ui.screenshot_writes.len() {
            let Some((path, result)) = self.ui.screenshot_writes[index].save.try_complete() else {
                index += 1;
                continue;
            };
            let write = self.ui.screenshot_writes.remove(index);
            if write.chat {
                self.release_screenshot_name(&write.save.original);
                let message = write.message.or_else(|| {
                    let hdr = result.as_ref().ok()?.hdr_path.as_deref()?;
                    Some(super::chat::screenshot_saved_line(&path, Some(hdr)))
                });
                self.finish_screenshot(&path, &result.map(|saved| saved.size), message);
            } else if let Err(error) = result {
                log::warn!("Horizon: couldn't save the world icon: {error}");
            }
        }
        self.ui.screenshot_writes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_captures_wait_for_a_slot_but_can_replace_an_undrawn_request() {
        let captured = PathBuf::from("encoding.png");
        let queued = PathBuf::from("undrawn.png");
        let mut pending = vec![captured.clone(), queued.clone()];
        assert!(!can_queue_player_screenshot(&pending, None));
        assert!(!can_queue_player_screenshot(
            &pending,
            Some(Path::new("world-icon.png"))
        ));
        assert!(can_queue_player_screenshot(&pending, Some(&queued)));

        pending.remove(0);
        assert!(can_queue_player_screenshot(&pending, None));
    }

    #[test]
    fn abandoned_player_reservations_are_released_without_touching_encoders() {
        let mut names = ["dropped.png", "encoding.png", "also-dropped.png"]
            .map(PathBuf::from)
            .to_vec();
        let writing = |path: &Path| path == Path::new("encoding.png");
        assert_eq!(
            discard_abandoned_names(&mut names, writing),
            Some(PathBuf::from("dropped.png"))
        );
        assert_eq!(names, [PathBuf::from("encoding.png")]);
        assert_eq!(discard_abandoned_names(&mut names, writing), None);
    }
}
