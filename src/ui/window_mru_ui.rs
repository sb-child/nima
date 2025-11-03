/*
Todo:

- Add test cases
x Animations
  x navigation scrolling
  x thumbnails appearing/disappearing
  x reorganization on scope/filter change
  x animate transition from selecting a thumbnail to the focused window
  ~ Transition when wrapping around during Mru navigation(?) => regular
    transition works fine.
  x UI open/close animation
- shortcut to "summon" a window to the current workspace
x support clicking on the target thumbnail
x add title of the current Mru selection under the thumbnail
x change BakedBuffers to TextureBuffers
x add bindings in the UI to switch to Output or Workspace modes
- add a help panel in the UI listing key bindings (e.g. screenshot UI)
x in UI, left/right should not change the current mode
x "advance" bindings for the MruUI should be copied over from the general
  bindings for the same action.
x support only considering windows from current output/workspace
x support only considering windows from the currently selected application
x support switching navigation modes while the Mru UI is open
x Unfocus the current Tile while the MruUi is up and refocus as necessary when
  the UI is closed.
x Keybindings in the MruUi, e.g. Close window, Quit, Focus selected, prev, next
x Mru list should contain an Option<BakedBuffer> to cache the texture
  once rendered and then reused as needed.
x Transition when opening/closing MruUI
x how to handle overview mode? Inhibit open?
x add config item to disable
x make modifier key configurable
x fix thumbnail close animation (fade out) on MRU UI close.
- support swiping gesture to navigate thumbnails

*/
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::{iter, mem};

use anyhow::ensure;
use niri_config::{
    Action, Bind, CornerRadius, Key, ModKey, Modifiers, MruDirection, MruFilter, MruScope, Trigger,
};
use pango::{Alignment, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::Color32F;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale, Transform};

use crate::animation::{Animation, Clock};
use crate::layout::focus_ring::{FocusRing, FocusRingRenderElement};
use crate::layout::{Layout, LayoutElement, LayoutElementRenderElement, Options};
use crate::niri::Niri;
use crate::niri_render_elements;
use crate::render_helpers::clipped_surface::ClippedSurfaceRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::RenderTarget;
use crate::utils::{
    output_size, round_logical_in_physical, to_physical_precise_round, with_toplevel_role,
};
use crate::window::mapped::MappedId;
use crate::window::Mapped;

// Factor by which to scale window thumbnails
const THUMBNAIL_SCALE: f64 = 0.5;

// Gap between thumbnails.
const GAP: f64 = 50.;

// How much of the next window will always peak from the side of the screen.
const STRUT: f64 = 100.;

// Padding in scope indication panel
const PADDING: i32 = 8;

// Border size of the scope indication panel
const BORDER: i32 = 4;

// Background color for the UI
const BACKGROUND: Color32F = Color32F::new(0., 0., 0., 0.7);

// Font used to render window titles
const FONT: &str = "sans 14px";

#[derive(Debug)]
struct Thumbnail {
    id: MappedId,
    timestamp: Option<Duration>,
    on_current_workspace: bool,
    on_current_output: bool,
    app_id: Option<String>,

    clock: Clock,
    open_animation: Option<Animation>,
    move_animation: Option<MoveAnimation>,
    title_texture: RefCell<TitleTexture>,
}

impl Thumbnail {
    fn are_animations_ongoing(&self) -> bool {
        self.open_animation.is_some() || self.move_animation.is_some()
    }

    fn advance_animations(&mut self) {
        self.open_animation.take_if(|a| a.is_done());
        self.move_animation.take_if(|a| a.anim.is_done());
    }

    /// Animate thumbnail motion from given location.
    fn animate_move_from_with_config(&mut self, from: f64, config: niri_config::Animation) {
        let current_offset = self.render_offset();

        // Preserve the previous config if ongoing.
        let anim = self.move_animation.take().map(|ma| ma.anim);
        let anim = anim
            .map(|anim| anim.restarted(1., 0., 0.))
            .unwrap_or_else(|| Animation::new(self.clock.clone(), 1., 0., 0., config));

        self.move_animation = Some(MoveAnimation {
            anim,
            from: from + current_offset,
        });
    }

    /// Thumbnail offset in the MRU UI view adjusted for animation.
    fn render_offset(&self) -> f64 {
        self.move_animation
            .as_ref()
            .map(|ma| ma.from * ma.anim.value())
            .unwrap_or_default()
    }

    fn title_texture(
        &self,
        renderer: &mut GlesRenderer,
        mapped: &Mapped,
        scale: f64,
    ) -> Option<MruTexture> {
        with_toplevel_role(mapped.toplevel(), |role| {
            role.title
                .as_ref()
                .and_then(|title| self.title_texture.borrow_mut().get(renderer, title, scale))
        })
    }

    fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        mapped: &Mapped,
        thumb_geo: Rectangle<f64, Logical>,
        scale: f64,
        target: RenderTarget,
        focus_ring: Option<&FocusRing>,
    ) -> impl Iterator<Item = WindowMruUiRenderElement<R>> {
        let _span = tracy_client::span!("Thumbnail::render");

        let s = Scale::from(scale);

        let alpha = self
            .open_animation
            .as_ref()
            .map_or(1., |a| a.clamped_value() as f32)
            .clamp(0., 1.);

        // TODO: offscreen
        let elems = mapped
            .render_normal(renderer, Point::new(0., 0.), s, alpha, target)
            .into_iter();

        // Clip thumbnails to their geometry.
        let clip_shader = ClippedSurfaceRenderElement::shader(renderer).cloned();
        let radius = CornerRadius::default();
        let geo = Rectangle::from_size(mapped.size().to_f64());
        let elems = elems.map(move |elem| match elem {
            LayoutElementRenderElement::Wayland(elem) => {
                if let Some(shader) = clip_shader.clone() {
                    if ClippedSurfaceRenderElement::will_clip(&elem, s, geo, radius) {
                        let elem =
                            ClippedSurfaceRenderElement::new(elem, s, geo, shader.clone(), radius);
                        return ThumbnailRenderElement::ClippedSurface(elem);
                    }
                }

                // If we don't have the shader, render it normally.
                let elem = LayoutElementRenderElement::Wayland(elem);
                ThumbnailRenderElement::LayoutElement(elem)
            }
            LayoutElementRenderElement::SolidColor(elem) => {
                // Square radius means we can render it as is.
                LayoutElementRenderElement::SolidColor(elem).into()
            }
        });

        let elems = elems.map(move |elem| {
            let thumb_scale =
                f64::min(thumb_geo.size.w / geo.size.w, thumb_geo.size.h / geo.size.h);
            let offset = Point::new(
                thumb_geo.size.w - (geo.size.w * thumb_scale),
                thumb_geo.size.h - (geo.size.h * thumb_scale),
            )
            .downscale(2.);
            let elem = RescaleRenderElement::from_element(elem, Point::new(0, 0), thumb_scale);
            let elem = RelocateRenderElement::from_element(
                elem,
                (thumb_geo.loc + offset).to_physical_precise_round(scale),
                Relocate::Relative,
            );
            WindowMruUiRenderElement::Thumbnail(elem)
        });

        let title_texture = self.title_texture(renderer.as_gles_renderer(), mapped, scale);
        let title_elems = title_texture.map(|title_texture| {
            let mut title_size = title_texture.logical_size();
            title_size.w = f64::min(title_size.w, thumb_geo.size.w);
            // TODO: fade end if doesn't fit.
            let src = Rectangle::from_size(title_size);

            let loc = thumb_geo.loc
                + Point::new(
                    (thumb_geo.size.w - title_size.w) / 2.,
                    thumb_geo.size.h + 16.,
                );
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);
            let elem = PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                title_texture,
                loc,
                alpha,
                Some(src),
                None,
                Kind::Unspecified,
            ));
            WindowMruUiRenderElement::TextureElement(elem)
        });

        let focus_ring_elems = focus_ring
            .map(move |x| {
                x.render(renderer, thumb_geo.loc)
                    .map(WindowMruUiRenderElement::FocusRing)
            })
            .into_iter()
            .flatten();

        elems.chain(title_elems).chain(focus_ring_elems)
    }
}

/// Window MRU traversal context.
#[derive(Debug)]
pub struct WindowMru {
    /// Windows in MRU order.
    thumbnails: Vec<Thumbnail>,

    /// Id of the currently selected window.
    current_id: Option<MappedId>,

    scope: MruScope,
    app_id_filter: Option<String>,
}

impl WindowMru {
    pub fn new(niri: &Niri) -> Self {
        let Some(output) = niri.layout.active_output() else {
            return Self {
                thumbnails: Vec::new(),
                current_id: None,
                scope: MruScope::All,
                app_id_filter: None,
            };
        };

        let mut thumbnails = Vec::new();
        for (mon, ws_idx, ws) in niri.layout.workspaces() {
            let mon = mon.expect("an active output exists so all workspaces have a monitor");
            let on_current_output = mon.output() == output;
            let on_current_workspace = on_current_output && mon.active_workspace_idx() == ws_idx;

            for win in ws.windows() {
                let app_id = with_toplevel_role(win.toplevel(), |role| role.app_id.clone());

                let thumbnail = Thumbnail {
                    id: win.id(),
                    timestamp: win.get_focus_timestamp(),
                    on_current_output,
                    on_current_workspace,
                    app_id,
                    clock: niri.clock.clone(),
                    open_animation: None,
                    move_animation: None,
                    title_texture: Default::default(),
                };
                thumbnails.push(thumbnail);
            }
        }

        thumbnails
            .sort_by(|Thumbnail { timestamp: t1, .. }, Thumbnail { timestamp: t2, .. }| t2.cmp(t1));

        let current_id = thumbnails.first().map(|t| t.id);
        Self {
            thumbnails,
            current_id,
            scope: MruScope::All,
            app_id_filter: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.thumbnails.is_empty()
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        if let Some(id) = self.current_id {
            assert!(
                self.thumbnails().any(|thumbnail| thumbnail.id == id),
                "current_id must be present in the current filtered thumbnail list",
            );
        } else {
            assert!(
                self.thumbnails().next().is_none(),
                "unset current_id must mean that the filtered thumbnail list is empty",
            );
        }
    }

    fn thumbnails(&self) -> impl DoubleEndedIterator<Item = &Thumbnail> {
        let matches = match_filter(self.scope, self.app_id_filter.as_deref());
        self.thumbnails.iter().filter(move |t| matches(t))
    }

    fn thumbnails_with_idx(&self) -> impl DoubleEndedIterator<Item = (usize, &Thumbnail)> {
        let matches = match_filter(self.scope, self.app_id_filter.as_deref());
        self.thumbnails
            .iter()
            .enumerate()
            .filter(move |(_, t)| matches(t))
    }

    fn forward(&mut self) {
        let Some(id) = self.current_id else {
            return;
        };

        let next = self.thumbnails().skip_while(|t| t.id != id).nth(1);
        self.current_id = Some(if let Some(next) = next {
            next.id
        } else {
            // We wrapped around.
            self.thumbnails().next().unwrap().id
        });
    }

    fn backward(&mut self) {
        let Some(id) = self.current_id else {
            return;
        };

        let next = self.thumbnails().rev().skip_while(|t| t.id != id).nth(1);
        self.current_id = Some(if let Some(next) = next {
            next.id
        } else {
            // We wrapped around.
            self.thumbnails().next_back().unwrap().id
        });
    }

    fn set_current(&mut self, id: MappedId) {
        if self.thumbnails().any(|thumbnail| thumbnail.id == id) {
            self.current_id = Some(id);
        }
    }

    fn first_id(&self) -> Option<MappedId> {
        self.thumbnails().next().map(|thumbnail| thumbnail.id)
    }

    fn first(&mut self) {
        self.current_id = self.first_id();
    }

    fn last(&mut self) {
        let id = self.thumbnails().next_back().map(|thumbnail| thumbnail.id);
        self.current_id = id;
    }

    pub fn set_scope_and_filter(&mut self, scope: MruScope, filter: Option<MruFilter>) -> bool {
        let mut changed = self.scope != scope;

        if let Some(id) = self.current_id {
            let (current_idx, current_thumbnail) = self
                .thumbnails_with_idx()
                .find(|(_, thumbnail)| thumbnail.id == id)
                .unwrap();

            if let Some(filter) = filter {
                let filter = match filter {
                    MruFilter::None => None,
                    MruFilter::AppId => current_thumbnail.app_id.clone(),
                };
                changed |= self.app_id_filter != filter;
                self.app_id_filter = filter;
            }
            self.scope = scope;

            // Try to select the same, or the first thumbnail to the left. Failing that, select the
            // first one to the right.
            let mut id = self.first_id();

            for (idx, thumbnail) in self.thumbnails_with_idx() {
                if idx > current_idx {
                    break;
                }
                id = Some(thumbnail.id);
            }
            self.current_id = id;
        } else {
            if filter.is_some() {
                // No current window, can't get app id.
                changed |= self.app_id_filter.is_some();
                self.app_id_filter = None;
            }
            self.scope = scope;
            self.current_id = self.first_id();
        }

        changed
    }

    pub fn set_scope(&mut self, scope: MruScope) {
        self.set_scope_and_filter(scope, None);
    }

    pub fn set_filter(&mut self, filter: MruFilter) {
        self.set_scope_and_filter(self.scope, Some(filter));
    }

    fn remove(&mut self, id: MappedId) -> Option<Thumbnail> {
        let idx = self.thumbnails.iter().position(|t| t.id == id)?;

        // Try to pick a different window when removing the current one.
        if self.current_id == Some(id) {
            self.forward();
        }

        // If we're still on the same window, that means it's the last visible one.
        if self.current_id == Some(id) {
            self.current_id = None;
        }

        Some(self.thumbnails.remove(idx))
    }
}

fn matches(scope: MruScope, app_id_filter: Option<&str>, thumbnail: &Thumbnail) -> bool {
    let x = match scope {
        MruScope::All => true,
        MruScope::Output => thumbnail.on_current_output,
        MruScope::Workspace => thumbnail.on_current_workspace,
    };
    if !x {
        return false;
    }

    if let Some(app_id) = app_id_filter {
        thumbnail.app_id.as_deref() == Some(app_id)
    } else {
        true
    }
}

fn match_filter(scope: MruScope, app_id_filter: Option<&str>) -> impl Fn(&Thumbnail) -> bool + '_ {
    move |thumbnail| matches(scope, app_id_filter, thumbnail)
}

type MruTexture = TextureBuffer<GlesTexture>;

pub struct WindowMruUi {
    state: WindowMruUiState,
    cached_mod_key: ModKey,
    cached_bindings: Option<Vec<Bind>>,
    cached_opened_bindings: Option<Vec<Bind>>,
}

pub enum WindowMruUiState {
    Closed {
        /// The MRU UI's closing animation while it is in progress.
        close_animation: Option<Animation>,

        /// Thumbnails to animate while the UI closes.
        closing_thumbnails: Vec<ClosingThumbnail>,

        /// Output on which to display the closing thumbnails
        output: Option<Output>,

        /// Scope used when the UI was last opened
        previous_scope: MruScope,
    },
    Open(Box<Inner>),
}

/// Opaque containing MRU UI state
pub struct Inner {
    /// List of Window Ids to display in the MRU UI.
    wmru: WindowMru,

    /// FocusRing object used for the current MRU UI selection.
    focus_ring: FocusRing,

    /// View offset relative to the currently selected window.
    view_offset: ViewOffset,

    /// Animation clock
    clock: Clock,

    /// Opening Animation for the MruUi itself
    open_animation: Animation,

    open_delay: Animation,

    /// Thumbnails linked to windows that were just closed, or to windows
    /// that no longer match the current MRU filter or scope.
    closing_thumbnails: Vec<ClosingThumbnail>,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Output the UI was opened on
    output: Output,

    /// Scope panel textures for each variant of Scope
    // The array size could be set using std::mem::variant_count, but it
    // is still unstable. For now it is just hard coded.
    scope_panel: RefCell<Option<Vec<MruTexture>>>,
}

#[derive(Debug)]
pub enum ViewOffset {
    /// The view offset is static.
    Static(f64),
    /// The view offset is animating.
    Animation(Animation),
}

// Taken from Tile.rs,
#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
}

impl ViewOffset {
    fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.value(),
        }
    }

    fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.to(),
        }
    }

    fn are_animations_ongoing(&self) -> bool {
        match self {
            ViewOffset::Static(_) => false,
            ViewOffset::Animation(_) => true,
        }
    }

    fn advance_animations(&mut self) {
        if let ViewOffset::Animation(anim) = self {
            if anim.is_done() {
                *self = ViewOffset::Static(anim.to());
            }
        }
    }

    fn animate_from_with_config(
        &mut self,
        from: f64,
        config: niri_config::Animation,
        clock: Clock,
    ) {
        let current = self.current();
        let anim = Animation::new(clock, current + from, current, 0., config);
        *self = ViewOffset::Animation(anim);
    }

    fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(offset) => *offset += delta,
            ViewOffset::Animation(anim) => anim.offset(delta),
        }
    }
}

/// Types for which there is a finite set of values that can be cycled through.
pub trait MruCycle {
    fn cycle(&self, direction: MruDirection) -> Self;
}

/// Reference for how MruScopes an be cycled through, the list must contain MruScope::All
static SCOPE_CYCLE: &[MruScope] = &[MruScope::All, MruScope::Workspace, MruScope::Output];

impl MruCycle for MruScope {
    fn cycle(&self, direction: MruDirection) -> Self {
        *match direction {
            MruDirection::Forward => SCOPE_CYCLE.iter().cycle().skip_while(|s| *s != self).nth(1),
            MruDirection::Backward => SCOPE_CYCLE
                .iter()
                .rev()
                .cycle()
                .skip_while(|s| *s != self)
                .nth(1),
        }
        .unwrap()
    }
}

pub enum MruCloseRequest {
    Cancelled,
    Current,
    Selection(MappedId),
}

niri_render_elements! {
    ThumbnailRenderElement<R> => {
        LayoutElement = LayoutElementRenderElement<R>,
        ClippedSurface = ClippedSurfaceRenderElement<R>,
    }
}

niri_render_elements! {
    WindowMruUiRenderElement<R> => {
        SolidColor = SolidColorRenderElement,
        TextureElement = PrimaryGpuTextureRenderElement,
        FocusRing = FocusRingRenderElement,
        Thumbnail = RelocateRenderElement<RescaleRenderElement<ThumbnailRenderElement<R>>>,
    }
}

impl WindowMruUi {
    pub fn new() -> Self {
        Self {
            // The value here doesn't matter.
            cached_mod_key: ModKey::Alt,
            cached_bindings: None,
            cached_opened_bindings: None,
            state: WindowMruUiState::Closed {
                close_animation: None,
                closing_thumbnails: vec![],
                output: None,
                previous_scope: MruScope::default(),
            },
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, WindowMruUiState::Open { .. })
    }

    pub fn open(
        &mut self,
        layout: &Layout<Mapped>,
        options: Rc<Options>,
        clock: Clock,
        mut wmru: WindowMru,
        dir: MruDirection,
        output: Output,
    ) {
        if self.is_open() {
            return;
        }

        // Each thumbnail is started with an open_animaiton
        wmru.thumbnails.iter_mut().for_each(|t| {
            t.open_animation = Some(Animation::new(
                clock.clone(),
                0.,
                1.,
                0.,
                options.animations.window_open.anim,
            ))
        });

        let open_anim = Animation::new(
            clock.clone(),
            0.,
            1.,
            0.,
            options.animations.window_mru_ui_open_close.0,
        );

        let open_delay = Animation::ease(
            clock.clone(),
            0.,
            1.,
            0.,
            150,
            crate::animation::Curve::Linear,
        );

        let mut inner = Inner {
            wmru,
            focus_ring: FocusRing::new(options.layout.focus_ring),
            options,
            view_offset: ViewOffset::Static(0.),
            closing_thumbnails: vec![],
            open_animation: open_anim,
            open_delay,
            clock,
            output,
            scope_panel: RefCell::new(None),
        };
        inner.view_offset = ViewOffset::Static(inner.compute_view_offset(layout));

        self.state = WindowMruUiState::Open(Box::new(inner));
        self.advance(Some(dir), None, None);
    }

    pub fn close(&mut self, close_request: MruCloseRequest) -> Option<MappedId> {
        if !self.is_open() {
            return None;
        }
        let state = mem::replace(
            &mut self.state,
            WindowMruUiState::Closed {
                output: None,
                close_animation: None,
                closing_thumbnails: vec![],
                previous_scope: MruScope::default(),
            },
        );
        let WindowMruUiState::Open(inner) = state else {
            unreachable!();
        };

        let response = inner.build_close_response(close_request);

        // Consume Inner
        let Inner {
            clock,
            output,
            wmru,
            mut closing_thumbnails,
            // view_offset,
            options,
            open_delay,
            ..
        } = *inner;

        // let textures = &mut textures.borrow_mut().0;
        let config = options.animations.window_mru_ui_open_close.0;

        // Consume visible thumbnails from Inner to convert them into ClosingThumbnails
        // closing_thumbnails.extend(wmru.thumbnails.into_iter().enumerate().filter_map(
        //     |(idx, thumb)| {
        //         if let Some(texture) = textures[idx].thumbnail.take() {
        //             let anim = Animation::new(
        //                 clock.clone(),
        //                 0.,
        //                 1.,
        //                 0.,
        //                 options.animations.window_close.anim,
        //             );
        //             return ClosingThumbnail::new(thumb, texture, view_offset?, &output, anim);
        //         }
        //         None
        //     },
        // ));

        // Update fields in `self.state` with their final values
        let progress = if open_delay.is_done() { 1. } else { 0. };
        let close_anim = Animation::new(clock.clone(), progress, 0., 0., config);
        if let WindowMruUiState::Closed {
            output: out,
            close_animation: anim,
            closing_thumbnails: thumbs,
            previous_scope: scope,
        } = &mut self.state
        {
            anim.replace(close_anim);
            mem::swap(thumbs, &mut closing_thumbnails);
            out.replace(output);
            *scope = wmru.scope;
        } else {
            unreachable!();
        };
        response
    }

    pub fn advance(
        &mut self,
        dir: Option<MruDirection>,
        scope: Option<MruScope>,
        filter: Option<MruFilter>,
    ) {
        let WindowMruUiState::Open(inner) = &mut self.state else {
            return;
        };

        // TODO: animations

        let new_scope = scope.unwrap_or(inner.wmru.scope);
        let changed = inner.wmru.set_scope_and_filter(new_scope, filter);
        if changed && scope.is_some() {
            // Do not advance when changing the scope.
        } else if let Some(dir) = dir {
            match dir {
                MruDirection::Forward => inner.wmru.forward(),
                MruDirection::Backward => inner.wmru.backward(),
            }
        }
    }

    pub fn set_current(&mut self, id: MappedId) {
        let WindowMruUiState::Open(inner) = &mut self.state else {
            return;
        };
        inner.wmru.set_current(id);
    }

    // - Swap the MRU Ui's WindowMru with the new one,
    // - create a new texture cache initialized with textures that can be reused from the previous
    //   cache
    // - animate thumbnails:
    //   - thumbnails that were in both WindowMru (previous and replacement) change positions with a
    //     move animation
    //   - thumbnails that are no longer present in the replacement WindowMru disappear with a close
    //     animation
    //   - thumbnails that are only in the replacement WindowMru get an open animation
    // let len = wmru.thumbnails.len();

    // Create new empty texture cache
    // let mut textures = Vec::with_capacity(len);
    // textures.resize_with(len, Default::default);

    // Replace the previous texture cache
    // let mut ptextures = inner.textures.replace(TextureCache(textures)).0;
    // let textures = &mut inner.textures.borrow_mut().0;

    // Index in the previous Mru list at which to start looking
    // for thumbnail Ids to match with those from the new Mru list.
    // This just avoids having to go through the entire list each
    // time.
    // let mut start_idx = 0;

    // View offset after the update.
    // It is calculated:
    // - when `dir` is None and the `should_advance` is true, i.e. the current thumbnail is present
    //   in both Mru lists, then the new view_offset is chosen so as to keep that thumbnail in the
    //   same position in the view.
    // - otherwise, the view_offset is chosen to make the first common thumbnail retain its position
    // - if there are no common thumbnails the view_offset eventually defaults to 0.
    // let mut view_offset = {
    //     if let Some(vo) = inner.view_offset {
    //         if let Some((pt, t)) = if should_advance && dir.is_none() {
    //             prev_wmru
    //                 .current()
    //                 .and_then(|pt| wmru.current().map(|t| (pt, t)))
    //         } else {
    //             // look for the first visible thumbnail present in both lists
    //             prev_wmru
    //                 .thumbnails
    //                 .iter()
    //                 .filter(|pt| pt.offset + pt.size.w >= vo)
    //                 .filter_map(|pt| {
    //                     wmru.thumbnails
    //                         .iter()
    //                         .find(|t| t.id == pt.id)
    //                         .map(|t| (pt, t))
    //                 })
    //                 .next()
    //         } {
    //             Some(t.offset - pt.offset + vo)
    //         } else {
    //             None
    //         }
    //     } else {
    //         None
    //     }
    // };

    // wmru.thumbnails.iter_mut().enumerate().for_each(|(idx, t)| {
    //     match prev_wmru
    //         .thumbnails
    //         .iter()
    //         .enumerate()
    //         .skip(start_idx)
    //         .try_for_each(|(pidx, pt)| {
    //             if pt.timestamp < t.timestamp {
    //                 return ControlFlow::Break(None);
    //             }
    //             start_idx = pidx + 1;
    //             if t.id == pt.id {
    //                 ControlFlow::Break(Some(pidx))
    //             } else {
    //                 ControlFlow::Continue(())
    //             }
    //         }) {
    //         ControlFlow::Break(Some(pidx)) => {
    //             // The thumbnail is present in the previous and
    //             // replacement Mru list.
    //             let pt = &prev_wmru.thumbnails[pidx];
    //
    //             // If the view_offset hasn't yet been determined, derive
    //             // it by matching the thumbnail's position in the previous
    //             // view and the new one.
    //             if view_offset.is_none() && inner.view_offset.is_some() {
    //                 view_offset.replace(t.offset - pt.offset + inner.view_offset.unwrap());
    //             };
    //
    //             // Animate the new thumbnail so that it appears to move
    //             // from the corresponding one's former position.
    //             // The previous position needs to be projected into the
    //             // updated view's referential.
    //             if let Some(view_offset) = view_offset {
    //                 if let Some(prev_view_offset) = inner.view_offset {
    //                     t.animate_move_from_with_config(
    //                         (pt.offset - prev_view_offset) - (t.offset - view_offset),
    //                         inner.options.animations.window_movement.0,
    //                     );
    //                 }
    //             }
    //
    //             // Retain the previous thumbnail's textures by
    //             // transfering it to the new texture cache.
    //             // mem::swap(&mut ptextures[pidx], &mut textures[idx]);
    //         }
    //         _ => {
    //             // The new thumbnail wasn't in the previous Mru list.
    //
    //             // Schedule an open animation for it.
    //             t.open_animation = Some(Animation::new(
    //                 t.clock.clone(),
    //                 0.,
    //                 1.,
    //                 0.,
    //                 inner.options.animations.window_open.anim,
    //             ))
    //         }
    //     }
    // });

    // Replace the UI's WindowMru.
    // let prev_wmru = std::mem::replace(prev_wmru, wmru);

    // Whatever textures remain in the previous texture cache should be
    // used to trigger close animations for the corresponding thumbnails.
    // if let Some(prev_view_offset) = inner.view_offset {
    //     prev_wmru
    //         .thumbnails
    //         .into_iter()
    //         .enumerate()
    //         .for_each(|(idx, thumb)| {
    //             if let Some(texture) = ptextures[idx].thumbnail.take() {
    //                 let anim = Animation::new(
    //                     inner.clock.clone(),
    //                     0.,
    //                     1.,
    //                     0.,
    //                     inner.options.animations.window_close.anim,
    //                 );
    //                 if let Some(closing) = ClosingThumbnail::new(
    //                     thumb,
    //                     texture,
    //                     prev_view_offset,
    //                     &inner.output,
    //                     anim,
    //                 ) {
    //                     inner.closing_thumbnails.push(closing);
    //                 }
    //             }
    //         });
    // }

    pub fn first(&mut self) {
        let WindowMruUiState::Open(ref mut inner) = self.state else {
            return;
        };
        inner.wmru.first();
    }

    pub fn last(&mut self) {
        let WindowMruUiState::Open(ref mut inner) = self.state else {
            return;
        };
        inner.wmru.last();
    }

    pub fn scope(&self) -> MruScope {
        match &self.state {
            WindowMruUiState::Closed { previous_scope, .. } => *previous_scope,
            WindowMruUiState::Open(inner) => inner.wmru.scope,
        }
    }

    pub fn current_window_id(&self) -> Option<MappedId> {
        let WindowMruUiState::Open(inner) = &self.state else {
            return None;
        };
        inner.wmru.current_id
    }

    pub fn remove_window(&mut self, id: MappedId) {
        let WindowMruUiState::Open(inner) = &mut self.state else {
            return;
        };
        let wmru = &mut inner.wmru;

        let Some(thumbnail) = wmru.remove(id) else {
            return;
        };

        // TODO: animate close and move.

        if wmru.thumbnails.is_empty() {
            self.close(MruCloseRequest::Cancelled);
        }

        // if let Some(idx) = wmru.thumbnails.iter().position(|t| t.id == id) {
        // Remove the thumbnail and the cached texture.
        // let thumb = wmru.thumbnails.remove(idx);
        // if wmru.current >= wmru.thumbnails.len() {
        //     wmru.current = wmru.current.saturating_sub(1);
        // }
        // Update the offset of all thumbnails that follow the removed
        // thumbnail.
        // wmru.thumbnails.iter_mut().skip(idx).for_each(|t| {
        //     let offset_delta = thumb.size.w + GAP;
        //     t.animate_move_from_with_config(
        //         offset_delta,
        //         inner.options.animations.window_movement.0,
        //     );
        //     t.offset -= offset_delta;
        // });
        // If there is a cached texture, the thumbnail may be visible
        // so schedule a closing animation.
        // if let Some(texture) = inner.textures.borrow_mut().0.remove(idx).thumbnail.take() {
        //     let anim = Animation::new(
        //         inner.clock.clone(),
        //         0.,
        //         1.,
        //         0.,
        //         inner.options.animations.window_close.anim,
        //     );
        //     if let Some(view_offset) = inner.view_offset {
        //         if let Some(closing) =
        //             ClosingThumbnail::new(thumb, texture, view_offset, &inner.output, anim)
        //         {
        //             inner.closing_thumbnails.push(closing);
        //         }
        //     }
        // }
        // }
    }

    pub fn update_render_elements(&mut self, layout: &Layout<Mapped>, output: &Output) {
        let WindowMruUiState::Open(ref mut inner) = self.state else {
            return;
        };

        let scale = output.current_scale().fractional_scale();

        if let Some(id) = inner.wmru.current_id {
            // TODO: copy color logic from the tab indicator.
            // TODO: no need to walk the positions here.
            let x = inner.thumbnails(layout).find_map(|(thumbnail, _, geo)| {
                (thumbnail.id == id).then(move || {
                    let alpha = thumbnail
                        .open_animation
                        .as_ref()
                        .map_or(1., |a| a.clamped_value() as f32)
                        .clamp(0., 1.);
                    (geo.size, alpha)
                })
            });
            if let Some((size, alpha)) = x {
                // let rules = mapped.rules();
                let draw_border_with_background = false;
                // let draw_border_with_background = rules
                //     .draw_border_with_background
                //     .unwrap_or_else(|| !mapped.has_ssd());

                // TODO round width to physical
                inner.focus_ring.update_render_elements(
                    size,
                    true,
                    !draw_border_with_background,
                    false,
                    Rectangle::default(), // TODO for gradients
                    CornerRadius::default(),
                    scale,
                    alpha,
                );
            } else {
                error!("window in the MRU must be present in the layout");
            }
        }
    }

    pub fn render_output<R: NiriRenderer>(
        &self,
        niri: &Niri,
        output: &Output,
        renderer: &mut R,
        target: RenderTarget,
    ) -> Vec<WindowMruUiRenderElement<R>> {
        let mut rv = Vec::new();
        let output_size = output_size(output);

        let progress = match &self.state {
            WindowMruUiState::Closed {
                close_animation: None,
                ..
            } => return vec![],
            WindowMruUiState::Closed {
                close_animation: Some(ref close_animation),
                closing_thumbnails,
                output: closing_output,
                ..
            } => {
                if let Some(closing_output) = closing_output {
                    if closing_output == output {
                        rv.extend(
                            closing_thumbnails
                                .iter()
                                .rev()
                                .map(|closing| closing.render().into()),
                        );
                    }
                }
                close_animation.clamped_value()
            }
            WindowMruUiState::Open(ref inner) => {
                if inner.open_delay.is_done() {
                    if *output == inner.output {
                        rv.extend(inner.render(niri, renderer, output, target));
                    }
                    1.
                } else {
                    return rv;
                }
            }
        };

        let progress = progress.clamp(0., 1.) as f32;

        // Put a panel above the current desktop view to contrast the thumbnails
        let buffer = SolidColorBuffer::new(output_size, BACKGROUND);

        rv.push(
            SolidColorRenderElement::from_buffer(
                &buffer,
                Point::default(),
                progress,
                Kind::Unspecified,
            )
            .into(),
        );

        rv
    }

    pub fn are_animations_ongoing(&self) -> bool {
        match self.state {
            WindowMruUiState::Open(ref inner) => inner.are_animations_ongoing(),
            WindowMruUiState::Closed {
                ref close_animation,
                ref closing_thumbnails,
                ..
            } => {
                close_animation.is_some()
                    || closing_thumbnails
                        .iter()
                        .any(|closing| closing.are_animations_ongoing())
            }
        }
    }

    pub fn advance_animations(&mut self, layout: &Layout<Mapped>) {
        match &mut self.state {
            WindowMruUiState::Open(inner) => inner.advance_animations(layout),
            WindowMruUiState::Closed {
                close_animation,
                closing_thumbnails,
                ..
            } => {
                close_animation.take_if(|a| a.is_done());
                closing_thumbnails.retain(|closing| closing.are_animations_ongoing());
            }
        }
    }

    pub fn bindings(&mut self, mod_key: ModKey) -> impl Iterator<Item = &Bind> {
        if self.cached_mod_key != mod_key {
            self.cached_mod_key = mod_key;
            self.cached_bindings = None;
            self.cached_opened_bindings = None;
        }

        let modifiers = mod_key.to_modifiers();
        let apply_modkey = move |mut bind: Bind| {
            bind.key.modifiers |= modifiers;
            bind
        };

        let is_open = self.is_open();

        let bindings = self
            .cached_bindings
            .get_or_insert(MRU_UI_BINDINGS.iter().cloned().map(apply_modkey).collect());

        let opened_bindings = self.cached_opened_bindings.get_or_insert(
            MRU_UI_OPENED_BINDINGS
                .iter()
                .cloned()
                .map(apply_modkey)
                .collect(),
        );

        bindings.iter().chain(
            is_open
                .then_some(opened_bindings.iter())
                .into_iter()
                .flatten(),
        )
    }

    pub fn output(&self) -> Option<&Output> {
        match self.state {
            WindowMruUiState::Open(ref inner) => Some(&inner.output),
            _ => None,
        }
    }

    pub fn thumbnail_under(&self, niri: &Niri, pos: Point<f64, Logical>) -> Option<MappedId> {
        let WindowMruUiState::Open(inner) = &self.state else {
            return None;
        };

        inner.thumbnail_under(niri, pos)
    }
}

fn compute_view_offset(cur_x: f64, working_width: f64, new_col_x: f64, new_col_width: f64) -> f64 {
    let new_x = new_col_x;
    let new_right_x = new_col_x + new_col_width;

    // If the column is already fully visible, leave the view as is.
    if cur_x <= new_x && new_right_x <= cur_x + working_width {
        return -(new_col_x - cur_x);
    }

    // Otherwise, prefer the alignment that results in less motion from the current position.
    let dist_to_left = (cur_x - new_x).abs();
    let dist_to_right = ((cur_x + working_width) - new_right_x).abs();
    if dist_to_left <= dist_to_right {
        0.
    } else {
        -(working_width - new_col_width)
    }
}

impl Inner {
    fn are_animations_ongoing(&self) -> bool {
        (!self.open_animation.is_done())
            || self
                .wmru
                .thumbnails
                .iter()
                .any(|t| t.are_animations_ongoing())
            || self.view_offset.are_animations_ongoing()
            || !self.closing_thumbnails.is_empty()
    }

    fn advance_animations(&mut self, layout: &Layout<Mapped>) {
        self.view_offset.advance_animations();
        self.closing_thumbnails
            .retain_mut(|closing| closing.are_animations_ongoing());
        self.wmru
            .thumbnails
            .iter_mut()
            .for_each(|t| t.advance_animations());

        let new_view_offset = self.compute_view_offset(layout);

        let delta = new_view_offset - self.view_offset.target();
        let pixel = 1. / self.output.current_scale().fractional_scale();
        if delta.abs() > pixel {
            self.animate_view_offset_from(-delta);
        }
        self.view_offset.offset(delta);
    }

    fn animate_view_offset_from(&mut self, from: f64) {
        self.view_offset.animate_from_with_config(
            from,
            self.options.animations.window_movement.0,
            self.clock.clone(),
        );
    }

    fn compute_view_offset(&self, layout: &Layout<Mapped>) -> f64 {
        let Some(current_id) = self.wmru.current_id else {
            return 0.;
        };

        let output_size = output_size(&self.output);

        let working_x = STRUT + GAP;
        let working_width = (output_size.w - working_x * 2.).max(0.);

        let mut current_geo = Rectangle::default();
        let mut strip_width = 0.;
        for (thumbnail, _, geo) in self.thumbnails(layout) {
            if thumbnail.id == current_id {
                current_geo = geo;
            }
            strip_width = geo.loc.x + geo.size.w;

            // If we found current_geo, and the strip width is already bigger than the working
            // width, no need to compute further.
            if current_geo.size.w != 0. && strip_width > working_width {
                break;
            }
        }

        // If the whole strip fits on screen, center it.
        if strip_width <= working_width {
            return -(output_size.w - strip_width) / 2.;
        }

        compute_view_offset(
            self.view_offset.target() + working_x,
            working_width,
            current_geo.loc.x,
            current_geo.size.w,
        ) + current_geo.loc.x
            - working_x
    }

    /// Generate a response to an MruCloseRequest
    fn build_close_response(&self, close_request: MruCloseRequest) -> Option<MappedId> {
        let Inner { wmru, .. } = self;

        match close_request {
            MruCloseRequest::Cancelled => None,
            MruCloseRequest::Current => wmru.current_id,
            MruCloseRequest::Selection(id) => Some(id),
        }
    }

    fn thumbnails<'a>(
        &'a self,
        layout: &'a Layout<Mapped>,
    ) -> impl Iterator<Item = (&'a Thumbnail, &'a Mapped, Rectangle<f64, Logical>)> {
        let output_size = output_size(&self.output);
        let scale = self.output.current_scale().fractional_scale();
        let round = move |logical: f64| round_logical_in_physical(scale, logical);

        let gap = round(GAP);

        let mut x = 0.;
        self.wmru.thumbnails().filter_map(move |thumbnail| {
            let id = thumbnail.id;
            let Some((_, mapped)) = layout.windows().find(|(_, mapped)| mapped.id() == id) else {
                error!("window in the MRU must be present in the layout");
                return None;
            };

            let max_height = 480.;
            let max_height = f64::min(max_height, output_size.h * THUMBNAIL_SCALE);
            let output_ratio = output_size.w / output_size.h;
            let max_width = max_height * output_ratio;

            let size = mapped.size().to_f64();
            let thumb_scale = f64::min(max_width / size.w, max_height / size.h);
            let thumb_scale = f64::min(THUMBNAIL_SCALE, thumb_scale);
            let size = size.to_f64().upscale(thumb_scale);
            // Round to physical pixels.
            let size = size.to_physical_precise_round(scale).to_logical(scale);

            let y = round((output_size.h - size.h) / 2.);

            let loc = Point::new(x, y);
            x += size.w + gap;

            let geo = Rectangle::new(loc, size);
            Some((thumbnail, mapped, geo))
        })
    }

    fn thumbnails_in_view<'a>(
        &'a self,
        layout: &'a Layout<Mapped>,
    ) -> impl Iterator<Item = (&'a Thumbnail, &'a Mapped, Rectangle<f64, Logical>)> {
        let output_size = output_size(&self.output);
        let scale = self.output.current_scale().fractional_scale();
        let round = |logical: f64| round_logical_in_physical(scale, logical);

        let view_pos = round(self.view_offset.current());

        let leftmost = view_pos;
        let rightmost = view_pos + output_size.w;

        self.thumbnails(layout)
            .skip_while(move |(_, _, geo)| geo.loc.x + geo.size.w <= leftmost)
            .map_while(move |(thumbnail, mapped, mut geo)| {
                if rightmost <= geo.loc.x {
                    return None;
                }

                geo.loc.x -= view_pos;
                Some((thumbnail, mapped, geo))
            })
    }

    fn render<R: NiriRenderer>(
        &self,
        niri: &Niri,
        renderer: &mut R,
        output: &Output,
        target: RenderTarget,
    ) -> impl Iterator<Item = WindowMruUiRenderElement<R>> {
        let mut rv = Vec::new();

        let output_size = output_size(output);
        let scale = output.current_scale().fractional_scale();

        // render the scope indicator
        if self.scope_panel.borrow().is_none() {
            if let Ok(panels) = make_scope_panels(renderer.as_gles_renderer(), scale) {
                let _ = self.scope_panel.borrow_mut().insert(panels);
            }
        }
        if let Some(texture) = self
            .scope_panel
            .borrow()
            .as_ref()
            .and_then(|p| p.get(self.wmru.scope as usize))
        {
            let texture_sz = texture.logical_size();
            let location = Point::<f64, Logical>::from((
                (output_size.w - texture_sz.w) / 2.,
                GAP + texture_sz.h / 2.,
            ));
            let elem = PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                texture.clone(),
                location,
                1.,
                None,
                None,
                Kind::Unspecified,
            ));
            rv.push(elem.into());
        }

        // As with tiles, render thumbnails for closing windows on top of
        // others.
        for closing in self.closing_thumbnails.iter().rev() {
            let elem = closing.render();
            rv.push(elem.into());
        }

        let Some(current_id) = self.wmru.current_id else {
            return rv.into_iter();
        };

        for (thumbnail, mapped, geo) in self.thumbnails_in_view(&niri.layout) {
            let focus_ring = (thumbnail.id == current_id).then_some(&self.focus_ring);
            let elems = thumbnail.render(renderer, mapped, geo, scale, target, focus_ring);
            rv.extend(elems);
        }

        rv.into_iter()
    }

    fn thumbnail_under(&self, niri: &Niri, pos: Point<f64, Logical>) -> Option<MappedId> {
        for (thumbnail, _, geo) in self.thumbnails_in_view(&niri.layout) {
            if geo.contains(pos) {
                return Some(thumbnail.id);
            }
        }

        None
    }
}

/// Cached title texture.
#[derive(Debug, Default)]
struct TitleTexture {
    title: String,
    scale: f64,
    texture: Option<Option<MruTexture>>,
}

impl TitleTexture {
    fn get(&mut self, renderer: &mut GlesRenderer, title: &str, scale: f64) -> Option<MruTexture> {
        if self.title != title || self.scale != scale {
            self.texture = None;
            self.title = title.to_owned();
            self.scale = scale;
        }

        self.texture
            .get_or_insert_with(|| generate_title_texture(renderer, title, scale).ok())
            .clone()
    }
}

fn generate_title_texture(
    renderer: &mut GlesRenderer,
    title: &str,
    scale: f64,
) -> anyhow::Result<MruTexture> {
    let _span = tracy_client::span!("window_mru_ui::generate_title_texture");

    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_text(title);

    let (width, height) = layout.pixel_size();
    ensure!(width > 0 && height > 0);

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_text(title);

    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &layout);

    // if apply_gradient {
    //     let gradient = cairo::LinearGradient::new(0., 0., width as f64, 0.);
    //     gradient.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 1.0); // fully opaque
    //     gradient.add_color_stop_rgba(0.9, 1.0, 1.0, 1.0, 1.0); // fully opaque
    //     gradient.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0); // fade to transparent
    //
    //     // Use destination-in to mask the content with the gradient
    //     cr.set_operator(cairo::Operator::DestIn);
    //     cr.rectangle(0., 0., width as f64, height as f64);
    //     cr.set_source(&gradient)?;
    //     cr.fill()?;
    // }

    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}

fn make_scope_panels(renderer: &mut GlesRenderer, scale: f64) -> anyhow::Result<Vec<MruTexture>> {
    fn make_panel_text(idx: usize) -> String {
        let span_unselected = "<span fgcolor='#555555'>";
        let span_end = "</span>";
        // Using a hair-space or thin-space doesn't seem to make a difference
        // so for now don't add a space around shortcut keys.
        // let span_shortcut = "<span face='mono' bgcolor='#2C2C2C'>\u{200a}";
        // let span_shortcut_end = format!("\u{200a}{span_end}");
        let span_shortcut = "<span face='mono' bgcolor='#2C2C2C' letter_spacing='5000'>";
        let span_shortcut_end = span_end;
        iter::once(format!(
            " {span_unselected}{span_shortcut}S{span_shortcut_end}cope:{span_end}"
        ))
        .chain(SCOPE_CYCLE.iter().map(|s| {
            let mut t = match s {
                MruScope::All => format!("{span_shortcut}A{span_shortcut_end}ll"),
                MruScope::Output => format!("{span_shortcut}O{span_shortcut_end}utput"),
                MruScope::Workspace => format!("{span_shortcut}W{span_shortcut_end}orkspace"),
            };
            if *s as usize != idx {
                t = format!("{span_unselected}{t}{span_end}")
            }
            t
        }))
        .collect::<Vec<_>>()
        .join("  ")
    }

    (0..SCOPE_CYCLE.len())
        .map(make_panel_text)
        .map(|text| make_panel(renderer, scale, &text))
        .collect()
}

// This is a copy of screenshot_ui's render_panel
fn make_panel(renderer: &mut GlesRenderer, scale: f64, text: &str) -> anyhow::Result<MruTexture> {
    let font = FontDescription::from_string(FONT);
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let border_width = (f64::from(BORDER) / 2. * scale).round() * 2.;
    let half_border_width = (border_width / 2.) as i32;
    let spacing = to_physical_precise_round::<i32>(scale, 2) * 1024;

    // Render `scope_text` to a dummy surface to determine its size
    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&font));
    layout.set_markup(text);
    layout.set_spacing(spacing);
    let (mut width, mut height) = layout.pixel_size();

    // Setup the final surface
    width += 2 * padding + half_border_width;
    height += padding * 2;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;

    let padding = f64::from(padding);
    let half_border_width = f64::from(half_border_width);

    cr.move_to(padding + half_border_width, padding);

    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Left);
    layout.set_markup(text);
    layout.set_spacing(spacing);

    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &layout);

    cr.move_to(0., 0.);
    cr.line_to(width.into(), 0.);
    cr.line_to(width.into(), height.into());
    cr.line_to(0., height.into());
    cr.line_to(0., 0.);
    cr.set_source_rgb(0.3, 0.3, 0.3);
    cr.set_line_width(border_width);
    cr.stroke()?;
    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}

#[derive(Debug)]
/// A visible Thumbnail that is in the process of being dismissed.
/// This can happen if the corresponding window was closed or if the
/// window ceases to match the current MRU filter or scope.
pub struct ClosingThumbnail {
    texture: MruTexture,
    /// Position relative to the Output
    location: Point<f64, Logical>,
    anim: Animation,
}

impl ClosingThumbnail {
    /// Convert a visible [Thumbnail] into its "closing" counterpart.
    /// Visibility is determined based on the givan [Output] and [view_offset].
    /// Returns an [Option<ClosingThumbnail>] depending on visbility.
    fn new(
        thumb: Thumbnail,
        texture: MruTexture,
        view_offset: f64,
        output: &Output,
        anim: Animation,
    ) -> Option<Self> {
        todo!()
        // let offset = thumb.offset - view_offset;
        // let output_size = output_size(output);
        // let thumb_visible = offset + thumb.size.w >= 0. || offset <= output_size.w;
        // if !thumb_visible {
        //     return None;
        // }
        // let location = Point::from((offset, (output_size.h - texture.logical_size().h) / 2.));
        //
        // Some(Self {
        //     texture,
        //     location,
        //     anim,
        // })
    }

    pub fn render(&self) -> PrimaryGpuTextureRenderElement {
        PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
            self.texture.clone(),
            self.location,
            (1. - self.anim.value()) as f32,
            None,
            None,
            Kind::Unspecified,
        ))
    }

    fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }
}

/// Key bindings available when the MRU UI is open.
/// Because the UI is closed when the Alt key is released, all bindings
/// have the ALT modifier.
static MRU_UI_OPENED_BINDINGS: &[Bind] = &[
    // Escape just closes the MRU UI
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Escape),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruCancel,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    // Left and Right can also be used when the UI is open
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Right),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Forward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Left),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Backward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    // j and k can be used as well
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::j),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Forward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::k),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Backward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    // and so can h and l can be used as well
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::l),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Forward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::h),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Backward, None, None),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    // And q can be used to close windows during navigation
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::q),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruCloseCurrent,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Return),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruClose,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Home),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruFirst,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::End),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruLast,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::a),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruChangeScope(MruScope::All),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::w),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruChangeScope(MruScope::Workspace),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::o),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruChangeScope(MruScope::Output),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::s),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruCycleScope(MruDirection::Forward),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::s),
            modifiers: Modifiers::SHIFT,
        },
        action: Action::MruCycleScope(MruDirection::Backward),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
];

/// Key bindings that are available both when the MRU UI is opened or closed
static MRU_UI_BINDINGS: &[Bind] = &[
    // The following two bindings cover MRU window navigation. They are
    // preset because the `Alt` key is treated specially in `on_keyboard`.
    // When it is released the active MRU traversal is considered to have
    // completed. If the user were allowed to change the MRU bindings
    // below, the navigation mechanism would no longer work as intended.
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Tab),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Forward, None, Some(MruFilter::None)),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::Tab),
            modifiers: Modifiers::SHIFT,
        },
        action: Action::MruAdvance(MruDirection::Backward, None, Some(MruFilter::None)),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    // forward/backward bind actions for AppId navigation
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::grave),
            modifiers: Modifiers::empty(),
        },
        action: Action::MruAdvance(MruDirection::Forward, None, Some(MruFilter::AppId)),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
    Bind {
        key: Key {
            trigger: Trigger::Keysym(Keysym::grave),
            modifiers: Modifiers::SHIFT,
        },
        action: Action::MruAdvance(MruDirection::Backward, None, Some(MruFilter::AppId)),
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    },
];

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest_derive::Arbitrary;

    use super::*;

    #[test]
    fn remove_last_window_out_of_two() {
        let ops = [Op::Backward, Op::Remove(1)];

        let thumbnails = vec![
            Thumbnail {
                id: MappedId::next(),
                timestamp: None,
                on_current_workspace: false,
                on_current_output: false,
                app_id: None,
                clock: Clock::with_time(Duration::ZERO),
                open_animation: None,
                move_animation: None,
                title_texture: Default::default(),
            },
            Thumbnail {
                id: MappedId::next(),
                timestamp: None,
                on_current_workspace: false,
                on_current_output: false,
                app_id: None,
                clock: Clock::with_time(Duration::ZERO),
                open_animation: None,
                move_animation: None,
                title_texture: Default::default(),
            },
        ];
        let current_id = thumbnails.first().map(|t| t.id);
        let mut mru = WindowMru {
            thumbnails,
            current_id,
            scope: MruScope::All,
            app_id_filter: None,
        };

        check_ops(&mut mru, &ops);
    }

    fn arbitrary_scope() -> impl Strategy<Value = MruScope> {
        prop_oneof![
            Just(MruScope::All),
            Just(MruScope::Output),
            Just(MruScope::Workspace),
        ]
    }

    fn arbitrary_filter() -> impl Strategy<Value = MruFilter> {
        prop_oneof![Just(MruFilter::None), Just(MruFilter::AppId)]
    }

    fn arbitrary_app_id() -> impl Strategy<Value = Option<String>> {
        prop_oneof![Just(None), Just(Some(1)), Just(Some(2))]
            .prop_map(|id| id.map(|id| format!("app-{id}")))
    }

    prop_compose! {
        fn arbitrary_thumbnail()(
            timestamp: Option<Duration>,
            on_current_output: bool,
            on_current_workspace: bool,
            app_id in arbitrary_app_id(),
        ) -> Thumbnail {
            Thumbnail {
                id: MappedId::next(),
                timestamp,
                on_current_workspace,
                on_current_output,
                app_id,
                clock: Clock::with_time(Duration::ZERO),
                open_animation: None,
                move_animation: None,
                title_texture: Default::default(),
            }
        }
    }

    prop_compose! {
        fn arbitrary_mru()(
            thumbnails in proptest::collection::vec(arbitrary_thumbnail(), 1..10),
        ) -> WindowMru {
            let current_id = thumbnails.first().map(|t| t.id);
            WindowMru {
                thumbnails,
                current_id,
                scope: MruScope::All,
                app_id_filter: None,
            }
        }
    }

    #[derive(Debug, Clone, Arbitrary)]
    enum Op {
        Forward,
        Backward,
        First,
        Last,
        SetScope(#[proptest(strategy = "arbitrary_scope()")] MruScope),
        SetFilter(#[proptest(strategy = "arbitrary_filter()")] MruFilter),
        Remove(#[proptest(strategy = "1..10usize")] usize),
    }

    impl Op {
        fn apply(&self, mru: &mut WindowMru) {
            match self {
                Op::Forward => mru.forward(),
                Op::Backward => mru.backward(),
                Op::First => mru.first(),
                Op::Last => mru.last(),
                Op::SetScope(scope) => mru.set_scope(*scope),
                Op::SetFilter(filter) => mru.set_filter(*filter),
                Op::Remove(idx) => {
                    if let Some(thumbnail) = mru.thumbnails.get(*idx) {
                        let id = thumbnail.id;
                        mru.remove(id);
                    }
                }
            }
        }
    }

    fn check_ops(mru: &mut WindowMru, ops: &[Op]) {
        for op in ops {
            op.apply(mru);
            mru.verify_invariants();
        }
    }

    proptest! {
        #[test]
        fn random_operations_dont_panic(
            mut mru in arbitrary_mru(),
            ops: Vec<Op>,
        ) {
            check_ops(&mut mru, &ops);
        }
    }
}
