//! The game on an AirPlay display. An app that declares the external display scene role
//! (`UIWindowSceneSessionRoleExternalDisplayNonInteractive` in GameInfo.plist) gets that display as a screen
//! of its own instead of a mirror of the phone: iOS connects a scene to [`register`]'s delegate, which puts a
//! window in it, and [`screen`] hands that window's view to the game for wgpu to draw on. The phone, meanwhile,
//! is covered by a view that says "Playing on AirPlay" and takes its touches: on AirPlay the game is played
//! with a controller.
//!
//! Splitscreen splits the display top and bottom: [`screen`] then hands the top half to the first player and
//! [`second_screen`] the bottom half to the second, whose controller is the phone. The cover then draws the
//! second player's touch controls ([`phone_controls`]) and lets the touches through to the game under it.
//!
//! The game forbids `unsafe`, so the Objective-C lives here. Elsewhere [`screen`] is always `None`.

pub mod audio;

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};

/// A view on the AirPlay display, while one is connected: what a player's renderer draws to instead of the
/// phone's window.
#[derive(Clone, Debug)]
pub struct Screen {
    /// Unique to the view, so a display that went and came back, or a half that was made again, is a new
    /// surface.
    pub id: u64,
    /// The view's size in pixels.
    pub size: (u32, u32),
    #[cfg(target_os = "ios")]
    view: std::sync::Arc<MainThreadRelease<objc2::rc::Retained<objc2_ui_kit::UIView>>>,
}

impl PartialEq for Screen {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.size == other.size
    }
}

impl Eq for Screen {}

/// UIKit objects may outlive their scene because render workers own the surface.
/// Release on the main queue without waiting for it from a render worker.
#[cfg(target_os = "ios")]
#[derive(Debug)]
struct MainThreadRelease<T: 'static>(Option<dispatch2::MainThreadBound<T>>);

#[cfg(target_os = "ios")]
impl<T> MainThreadRelease<T> {
    fn new(value: T, mtm: objc2::MainThreadMarker) -> Self {
        Self(Some(dispatch2::MainThreadBound::new(value, mtm)))
    }

    fn get(&self, mtm: objc2::MainThreadMarker) -> &T {
        self.0.as_ref().expect("value owned until drop").get(mtm)
    }
}

#[cfg(target_os = "ios")]
impl<T> Drop for MainThreadRelease<T> {
    fn drop(&mut self) {
        let value = self.0.take();
        if objc2::MainThreadMarker::new().is_some() {
            drop(value);
        } else {
            dispatch2::DispatchQueue::main().exec_async(move || drop(value));
        }
    }
}

impl HasWindowHandle for Screen {
    #[cfg(target_os = "ios")]
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let mtm = objc2::MainThreadMarker::new().ok_or(HandleError::Unavailable)?;
        let view = self.view.get(mtm);
        if view.window().is_none() {
            return Err(HandleError::Unavailable);
        }
        let handle =
            raw_window_handle::UiKitWindowHandle::new(std::ptr::NonNull::from(&**view).cast());
        // SAFETY: self owns a retain for this view for the lifetime of the handle.
        Ok(unsafe { WindowHandle::borrow_raw(handle.into()) })
    }
    #[cfg(not(target_os = "ios"))]
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasDisplayHandle for Screen {
    #[cfg(target_os = "ios")]
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::uikit())
    }
    #[cfg(not(target_os = "ios"))]
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

/// A touch control drawn on the phone for splitscreen's second player.
#[derive(Clone, Debug, PartialEq)]
pub struct Button {
    pub label: String,
    /// `[x, y, width, height]` in the phone window's points.
    pub rect: [f32; 4],
    /// A finger holds it.
    pub held: bool,
}

/// Make the scene delegate's class known before iOS asks for it by name: call it before the event loop.
pub fn register() {
    #[cfg(target_os = "ios")]
    ios::register();
}

/// The first player's external view while the game is visible, or its top half while split.
pub fn screen(playing: bool, split: bool) -> Option<Screen> {
    #[cfg(target_os = "ios")]
    return ios::screen(playing, split);
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (playing, split);
        None
    }
}

/// Whether an AirPlay scene is connected, independent of its renderer.
pub fn display_connected() -> bool {
    #[cfg(target_os = "ios")]
    return ios::display_connected();
    #[cfg(not(target_os = "ios"))]
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    Detached,
    WaitingForFrame,
    Presented,
}

impl Presentation {
    pub fn for_frame(attached: bool, presented: bool) -> Self {
        if !attached {
            Self::Detached
        } else if presented {
            Self::Presented
        } else {
            Self::WaitingForFrame
        }
    }
}

#[cfg(any(target_os = "ios", test))]
fn drawable_pixels(width: f64, height: f64, scale: f64) -> Option<(u32, u32)> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let pixels = (width * scale, height * scale);
    let valid = |side: f64| (1.0..=f64::from(u32::MAX)).contains(&side.round());
    (valid(pixels.0) && valid(pixels.1)).then(|| (pixels.0.round() as u32, pixels.1.round() as u32))
}

#[cfg(any(target_os = "ios", test))]
fn split_heights(height: f64, scale: f64) -> (f64, f64) {
    let Some((_, pixels)) = drawable_pixels(height, height, scale) else {
        return (0.0, 0.0);
    };
    // Both views share one pixel-aligned seam, even when the display has an odd height.
    let top = f64::from(pixels / 2) / scale;
    (top, height - top)
}

#[cfg(test)]
mod presentation_tests {
    use super::Presentation;

    #[test]
    fn external_view_requires_drawable_dimensions() {
        assert_eq!(
            super::drawable_pixels(960.0, 540.0, 2.0),
            Some((1920, 1080))
        );
        assert_eq!(super::drawable_pixels(0.0, 540.0, 2.0), None);
        assert_eq!(super::drawable_pixels(960.0, 0.1, 2.0), None);
        assert_eq!(super::drawable_pixels(f64::INFINITY, 540.0, 2.0), None);
    }

    #[test]
    fn split_layout_fills_the_view_when_dimensions_change() {
        assert_eq!(super::split_heights(0.0, 2.0), (0.0, 0.0));
        assert_eq!(super::split_heights(720.0, 2.0), (360.0, 360.0));
        let (top, bottom) = super::split_heights(540.5, 2.0);
        assert_eq!((top, bottom), (270.0, 270.5));
        assert_eq!(top + bottom, 540.5);
    }

    #[test]
    fn attaching_is_not_yet_presenting_and_a_phone_frame_does_not_count() {
        assert_eq!(
            Presentation::for_frame(true, false),
            Presentation::WaitingForFrame
        );
        assert_eq!(Presentation::for_frame(true, true), Presentation::Presented);
        assert_eq!(Presentation::for_frame(false, true), Presentation::Detached);
    }
}

/// Update the covers after evaluating the primary renderer's target and frame.
pub fn rendering(presentation: Presentation) {
    #[cfg(target_os = "ios")]
    ios::rendering(presentation);
    #[cfg(not(target_os = "ios"))]
    let _ = presentation;
}

/// The second player's view: the bottom half, while [`screen`] splits the display.
pub fn second_screen() -> Option<Screen> {
    #[cfg(target_os = "ios")]
    return ios::second_screen();
    #[cfg(not(target_os = "ios"))]
    None
}

/// The phone shows the game under its cover: splitscreen's second player's menus, which they tap there.
/// Called every frame in splitscreen; off again when it ends.
pub fn phone_shows_game(on: bool) {
    #[cfg(target_os = "ios")]
    ios::phone_shows_game(on);
    #[cfg(not(target_os = "ios"))]
    let _ = on;
}

/// The second player's touch controls on the phone, which then lets its touches through to the game; `None`
/// takes them away and the cover takes the touches again.
pub fn phone_controls(buttons: Option<&[Button]>) {
    #[cfg(target_os = "ios")]
    ios::phone_controls(buttons);
    #[cfg(not(target_os = "ios"))]
    let _ = buttons;
}

#[cfg(target_os = "ios")]
mod ios {
    use super::{Button, MainThreadRelease, Presentation, Screen};
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSObject, NSObjectProtocol, NSString};
    use objc2_ui_kit::{
        NSTextAlignment, UIApplication, UIColor, UIFont, UILabel, UIScene, UISceneActivationState,
        UISceneConnectionOptions, UISceneDelegate, UISceneSession, UIView, UIViewAutoresizing,
        UIViewController, UIWindow, UIWindowScene, UIWindowSceneDelegate,
        UIWindowSceneSessionRoleApplication,
    };
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    /// A view with the id its surfaces know it by.
    struct View {
        id: u64,
        view: Retained<UIView>,
        handle: Arc<MainThreadRelease<Retained<UIView>>>,
    }

    impl View {
        fn new(view: Retained<UIView>, mtm: MainThreadMarker) -> Self {
            Self {
                id: next_id(),
                handle: Arc::new(MainThreadRelease::new(view.clone(), mtm)),
                view,
            }
        }
    }

    struct Cover {
        view: Retained<UIView>,
        label: Retained<UILabel>,
        /// The second player's controls, over the cover's black.
        pad: Retained<UIView>,
        drawn: Vec<Button>,
    }

    struct State {
        scene: Retained<UIWindowScene>,
        /// Kept: a window nothing retains leaves the display.
        _window: Retained<UIWindow>,
        view: View,
        /// The top and bottom halves, while split.
        halves: Option<(View, View)>,
        cover: Option<Cover>,
        idle: Retained<UILabel>,
        playing: bool,
        target: Option<(u64, (u32, u32))>,
        presentation: Presentation,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
        static IDS: Cell<u64> = const { Cell::new(0) };
        /// `phone_shows_game`: the cover stands aside.
        static PHONE_GAME: Cell<bool> = const { Cell::new(false) };
    }

    fn next_id() -> u64 {
        IDS.with(|c| {
            c.set(c.get() + 1);
            c.get()
        })
    }

    define_class!(
        // The name GameInfo.plist's external display configuration gives.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "HorizonAirPlayDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UISceneDelegate for Delegate {
            #[unsafe(method(scene:willConnectToSession:options:))]
            fn will_connect(
                &self,
                scene: &UIScene,
                _session: &UISceneSession,
                _options: &UISceneConnectionOptions,
            ) {
                connect(scene);
            }

            #[unsafe(method(sceneDidDisconnect:))]
            fn did_disconnect(&self, scene: &UIScene) {
                disconnect(scene);
            }
        }

        unsafe impl UIWindowSceneDelegate for Delegate {}
    );

    pub(super) fn register() {
        let _ = <Delegate as objc2::ClassType>::class();
    }

    fn flexible() -> UIViewAutoresizing {
        UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight
    }

    fn connect(scene: &UIScene) {
        let Some(scene) = scene.downcast_ref::<UIWindowScene>() else {
            return;
        };
        let mtm = MainThreadMarker::from(scene);
        let window = UIWindow::initWithWindowScene(UIWindow::alloc(mtm), scene);
        let controller = UIViewController::new(mtm);
        let view = controller.view().unwrap_or_else(|| UIView::new(mtm));
        view.setBackgroundColor(Some(&UIColor::blackColor()));
        window.setRootViewController(Some(&controller));
        window.setHidden(false);
        let idle = label(mtm, view.bounds(), "Game is not running", 28.0);
        idle.setAutoresizingMask(flexible());
        idle.setBackgroundColor(Some(&UIColor::blackColor()));
        view.addSubview(&idle);
        install_state(State {
            scene: scene.retain(),
            _window: window,
            view: View::new(view, mtm),
            halves: None,
            cover: None,
            idle,
            playing: false,
            target: None,
            presentation: Presentation::Detached,
        });
    }

    fn remove_cover(state: Option<State>) {
        if let Some(cover) = state.and_then(|state| state.cover) {
            cover.view.removeFromSuperview();
        }
    }

    fn install_state(state: State) {
        let previous = STATE.with(|slot| slot.borrow_mut().replace(state));
        // The old window can outlive the external scene.
        remove_cover(previous);
    }

    fn disconnect(scene: &UIScene) {
        let disconnected = STATE.with(|s| {
            let mut state = s.borrow_mut();
            if state.as_ref().is_some_and(|st| {
                std::ptr::eq(&*st.scene as *const UIWindowScene as *const UIScene, scene)
            }) {
                state.take()
            } else {
                None
            }
        });
        remove_cover(disconnected);
    }

    /// Find the visible window in the application scene, preferring the active key window.
    fn phone_window(mtm: MainThreadMarker) -> Option<Retained<UIWindow>> {
        let app = UIApplication::sharedApplication(mtm);
        // SAFETY: a UIKit constant, read on the main thread.
        let role = unsafe { UIWindowSceneSessionRoleApplication };
        let mut best = None;
        for scene in app.connectedScenes().iter() {
            let Ok(scene) = scene.downcast::<UIWindowScene>() else {
                continue;
            };
            if *scene.session().role() != *role {
                continue;
            }
            let active = scene.activationState() == UISceneActivationState::ForegroundActive;
            for window in scene.windows().iter() {
                if window.isHidden() {
                    continue;
                }
                let priority = (active, window.isKeyWindow());
                if best.as_ref().is_none_or(|(current, _)| priority > *current) {
                    best = Some((priority, window));
                }
            }
        }
        best.map(|(_, window)| window)
    }

    fn label(mtm: MainThreadMarker, frame: CGRect, text: &str, size: f64) -> Retained<UILabel> {
        let label = UILabel::initWithFrame(UILabel::alloc(mtm), frame);
        label.setText(Some(&NSString::from_str(text)));
        // SAFETY: a label made here, on the main thread.
        unsafe { label.setTextColor(Some(&UIColor::whiteColor())) };
        label.setTextAlignment(NSTextAlignment::Center);
        unsafe { label.setFont(Some(&UIFont::boldSystemFontOfSize(size))) };
        label
    }

    /// A black view over the phone's window that says where the game is, and takes the touches the hidden
    /// controls would have. Made when first shown: the phone's window may not exist yet when a display that
    /// was already connected joins at launch.
    fn cover_phone(mtm: MainThreadMarker, window: &UIWindow) -> Cover {
        let bounds: CGRect = window.bounds();
        let view = UIView::initWithFrame(UIView::alloc(mtm), bounds);
        view.setAutoresizingMask(flexible());
        view.setBackgroundColor(Some(&UIColor::blackColor()));
        let text = label(mtm, bounds, "Playing on AirPlay", 22.0);
        text.setAutoresizingMask(flexible());
        view.addSubview(&text);
        let pad = UIView::initWithFrame(UIView::alloc(mtm), bounds);
        pad.setAutoresizingMask(flexible());
        view.addSubview(&pad);
        window.addSubview(&view);
        Cover {
            view,
            label: text,
            pad,
            drawn: Vec::new(),
        }
    }

    fn screen_of(state: &State, view: &View) -> Option<Screen> {
        let bounds = view.view.bounds();
        let scale = state.scene.screen().scale();
        let size = super::drawable_pixels(bounds.size.width, bounds.size.height, scale)?;
        Some(Screen {
            id: view.id,
            size,
            view: view.handle.clone(),
        })
    }

    /// Make new target views when split-screen starts.
    fn halves(state: &State) -> (View, View) {
        let mtm = MainThreadMarker::from(&*state.view.view);
        let make = || {
            let view = UIView::new(mtm);
            view.setBackgroundColor(Some(&UIColor::blackColor()));
            state.view.view.addSubview(&view);
            View::new(view, mtm)
        };
        (make(), make())
    }

    fn layout_halves(state: &State) {
        let Some((top, bottom)) = &state.halves else {
            return;
        };
        let bounds = state.view.view.bounds();
        let scale = state.scene.screen().scale();
        let (top_height, bottom_height) = super::split_heights(bounds.size.height, scale);
        for (view, y, height) in [
            (top, bounds.origin.y, top_height),
            (bottom, bounds.origin.y + top_height, bottom_height),
        ] {
            let frame = CGRect::new(
                CGPoint::new(bounds.origin.x, y),
                CGSize::new(bounds.size.width, height),
            );
            if view.view.frame() != frame {
                view.view.setFrame(frame);
            }
        }
    }

    fn update_covers(state: &mut State) {
        // Wait for an external frame before covering the phone.
        let attached = state.playing && state.presentation != Presentation::Detached;
        let presented = state.playing && state.presentation == Presentation::Presented;
        state.idle.setHidden(attached);
        if !attached {
            state.view.view.bringSubviewToFront(&state.idle);
        }
        if presented
            && state.cover.as_ref().is_none_or(|cover| {
                cover.view.window().is_none_or(|window| {
                    window.isHidden()
                        || !window.isKeyWindow()
                        || window.windowScene().is_none_or(|scene| {
                            scene.activationState() != UISceneActivationState::ForegroundActive
                        })
                })
            })
        {
            let mtm = MainThreadMarker::from(&*state.view.view);
            if let Some(window) = phone_window(mtm) {
                if let Some(cover) = &state.cover {
                    // Reattach when UIKit replaces the phone window.
                    if cover
                        .view
                        .window()
                        .is_none_or(|old| !std::ptr::eq(&*old, &*window))
                    {
                        cover.view.removeFromSuperview();
                        cover.view.setFrame(window.bounds());
                        window.addSubview(&cover.view);
                    }
                } else {
                    state.cover = Some(cover_phone(mtm, &window));
                }
            }
        }
        if let Some(cover) = &state.cover {
            let shown = presented && !PHONE_GAME.with(Cell::get);
            cover.view.setHidden(!shown);
            if shown && let Some(window) = cover.view.superview() {
                window.bringSubviewToFront(&cover.view);
            }
        }
    }

    pub(super) fn rendering(presentation: Presentation) {
        STATE.with(|s| {
            let mut state = s.borrow_mut();
            if let Some(state) = state.as_mut()
                && state.presentation != presentation
            {
                state.presentation = presentation;
                update_covers(state);
            }
        });
    }

    pub(super) fn screen(playing: bool, split: bool) -> Option<Screen> {
        STATE.with(|s| {
            let mut state = s.borrow_mut();
            let state = state.as_mut()?;
            if state.playing != playing {
                state.playing = playing;
                let message = if playing {
                    "Game is playing on your iPhone or iPad"
                } else {
                    "Game is not running"
                };
                state.idle.setText(Some(&NSString::from_str(message)));
            }
            let split = playing && split;
            if split && state.halves.is_none() {
                state.halves = Some(halves(state));
            } else if !split && let Some((top, bottom)) = state.halves.take() {
                top.view.removeFromSuperview();
                bottom.view.removeFromSuperview();
            }
            if !playing || !split {
                PHONE_GAME.with(|phone| phone.set(false));
                if let Some(cover) = &state.cover {
                    show_guest_controls(cover, false);
                }
            }
            layout_halves(state);
            let view = state.halves.as_ref().map_or(&state.view, |(top, _)| top);
            let screen = if playing { screen_of(state, view) } else { None };
            let target = screen.as_ref().map(|s| (s.id, s.size));
            // A previous frame cannot justify covering the phone on a new target.
            if !playing || state.target != target {
                state.target = target;
                state.presentation = Presentation::Detached;
            }
            update_covers(state);
            screen
        })
    }

    pub(super) fn display_connected() -> bool {
        STATE.with(|state| state.borrow().is_some())
    }

    pub(super) fn second_screen() -> Option<Screen> {
        STATE.with(|s| {
            let state = s.borrow();
            let state = state.as_ref().filter(|state| state.playing)?;
            layout_halves(state);
            let (_, bottom) = state.halves.as_ref()?;
            screen_of(state, bottom)
        })
    }

    // A visibility change applies even if no new frame is presented.
    pub(super) fn phone_shows_game(on: bool) {
        if PHONE_GAME.with(|value| value.replace(on)) != on {
            STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    update_covers(state);
                }
            });
        }
    }

    fn show_guest_controls(cover: &Cover, show: bool) {
        // With guest controls, touches go through to the game underneath.
        cover.view.setUserInteractionEnabled(!show);
        cover.label.setHidden(show);
        cover.pad.setHidden(!show);
    }

    pub(super) fn phone_controls(buttons: Option<&[Button]>) {
        STATE.with(|s| {
            let mut state = s.borrow_mut();
            let Some(state) = state.as_mut() else {
                return;
            };
            let buttons = buttons.filter(|_| state.playing && state.halves.is_some());
            let Some(cover) = state.cover.as_mut() else {
                return;
            };
            show_guest_controls(cover, buttons.is_some());
            let buttons = buttons.unwrap_or_default();
            if cover.drawn == buttons {
                return;
            }
            let mtm = MainThreadMarker::from(&*cover.pad);
            for old in cover.pad.subviews().iter() {
                old.removeFromSuperview();
            }
            for b in buttons {
                let [x, y, width, height] = b.rect.map(f64::from);
                let frame = CGRect::new(CGPoint::new(x, y), CGSize::new(width, height));
                let view = UIView::initWithFrame(UIView::alloc(mtm), frame);
                let alpha = if b.held { 0.55 } else { 0.25 };
                view.setBackgroundColor(Some(&UIColor::colorWithWhite_alpha(1.0, alpha)));
                let size = (frame.size.height * 0.45).clamp(9.0, 24.0);
                let text = label(
                    mtm,
                    CGRect::new(CGPoint::new(0.0, 0.0), frame.size),
                    &b.label,
                    size,
                );
                text.setAutoresizingMask(flexible());
                view.addSubview(&text);
                cover.pad.addSubview(&view);
            }
            cover.drawn = buttons.to_vec();
        });
    }
}
