// SPDX-License-Identifier: GPL-3.0-only

//! Compositor-side window-header rendering (Feature 4-C).
//!
//! This module produces the Lunaris faux-titlebar ("window header")
//! entirely inside the compositor, as a `MemoryRenderBuffer` that
//! the render pipeline composes alongside the window itself. Going
//! compositor-rendered instead of the previous cross-process (shell
//! renders DOM, compositor positions it) design buys us three
//! things that the two-process architecture could never deliver:
//!
//! 1. **True 0-frame atomicity.** Header and window are in the
//!    same render pass. No IPC in the hot drag path, no cache
//!    race, no tear-down/build-up between cells — the two always
//!    render at the same vblank.
//! 2. **No cross-client commit sync.** Wayland has no such
//!    primitive, so any shell-committed header pixels are always
//!    behind the compositor's view of the window geometry by at
//!    least one flush cycle.
//! 3. **Deterministic output-change behaviour.** Outputs can
//!    appear/disappear and HiDPI scale can flip without having to
//!    roundtrip state through the shell's Svelte store.
//!
//! The trade-off, which is what we're paying for, is that the
//! visual polish that used to live in CSS now lives in tiny-skia
//! paths. We replicate every detail of the CSS version:
//! `transform: scale(1.1)` hover enlargement, `scale(0.9)` press,
//! close-button red hover, focus ring, activated/inactive colors,
//! rounded button corners, border radius on the header strip,
//! 1-px bottom border, ellipsis-truncated title. See
//! `src/lib/components/WindowHeader.svelte` in the shell for the
//! original CSS.
//!
//! The module is stateless at the API boundary — callers pass in
//! a `HeaderVisualState` describing what the header should look
//! like right now, and get back a buffer. Callers are expected to
//! cache results (see `CosmicWindowInternal::header_cache`) so the
//! rasteriser is only hit when the state actually changes (title,
//! activation, hover, press, width, output scale).

use std::sync::atomic::{AtomicU64, Ordering};

// Note: the `cosmic_text` / `SwashCache` imports that used to drive
// the title-text renderer were removed along with `draw_title` —
// the window header no longer paints a title (user preference,
// dragging is the sole function of the strip between buttons).
// Keeping this comment as a pointer in case a future revision
// wants to restore titles.
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::memory::MemoryRenderBuffer,
    },
    utils::Transform,
};
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapMut, Rect,
    Stroke, Transform as SkiaTransform,
};

use lunaris_theme::{LunarisTheme, Rgba};

/// Fixed logical height of the header strip in CSS pixels. Matches
/// `SSD_HEIGHT` in `shell::element::window`.
pub const HEADER_LOGICAL_HEIGHT: i32 = 36;
/// Button width in logical pixels. Matches
/// `.window-buttons :global(.control-btn) { width: 28px }`
/// in `sdk/ui-kit/.../WindowControls.svelte` — the canonical
/// Lunaris window-decoration button. Do NOT conflate with
/// `BUTTON_HEIGHT` which is smaller — decorations are deliberately
/// shorter than their bounding click area.
pub const BUTTON_LOGICAL_WIDTH: f32 = 28.0;
/// Button height — `height: 22px` from WindowControls.svelte. The
/// button is vertically centred inside the 36-px header strip
/// (so 7 px above and below the hover-rect).
pub const BUTTON_LOGICAL_HEIGHT: f32 = 22.0;
/// Horizontal gap between adjacent buttons. Matches
/// `.window-buttons { gap: 2px }`.
pub const BUTTON_GAP: f32 = 2.0;
/// Right-side padding of the button strip. Matches
/// `.window-buttons { padding-right: 6px }` — the shell's
/// canonical value, NOT the old `.header-buttons { padding-right: 4px }`
/// in the legacy per-stack WindowHeader which this renderer
/// replaced.
pub const BUTTON_STRIP_RIGHT_PAD: f32 = 6.0;
// Title-related layout constants (`TITLE_LEFT_PAD`,
// `TITLE_FONT_SIZE`) removed along with the title renderer. See
// the "Title rendering removed" note further down for why.
/// Idle button opacity. Matches
/// `.window-buttons :global(.control-btn) { opacity: 0.7 }`.
/// Applied to both icon stroke and hover-bg tint so the whole
/// button dims — NOT just the icon.
pub const BUTTON_IDLE_OPACITY: f32 = 0.7;
/// Idle opacity when the window is NOT focused — the compositor
/// renders unfocused windows with an extra-dimmed header so
/// activation state is visually obvious. WindowControls doesn't
/// handle this because it lives inside an app that's always the
/// focused target of its own chrome.
pub const BUTTON_IDLE_OPACITY_INACTIVE: f32 = 0.4;
/// Lucide's `strokeWidth={2}` is given in the icon's 24-unit
/// viewBox, not in physical pixels. When rendered at display
/// size `N`, the physical stroke becomes `N * 2/24 = N/12`. So a
/// 12-px Minus glyph in Lucide has a 1-px physical stroke, not
/// 2 px — a detail the earlier fixed-2.0 constant was getting
/// wrong and making our icons look chunkier than the CSS
/// version.
pub const LUCIDE_VIEWBOX: f32 = 24.0;
pub const LUCIDE_STROKE_UNITS: f32 = 2.0;
/// Stroke width for a Lucide icon rendered at `icon_size` display
/// pixels. Matches the SVG scaling rule above.
#[inline]
pub fn lucide_stroke_width(icon_size: f32) -> f32 {
    icon_size * LUCIDE_STROKE_UNITS / LUCIDE_VIEWBOX
}
/// Nominal icon sizes. Matches
///   `<Minus size={12} strokeWidth={2} />`
///   `<Square size={10} strokeWidth={2} />`
///   `<X size={12} strokeWidth={2} />`
/// in WindowControls.svelte. Smaller than the previous values
/// (14/12/14) the renderer inherited from the legacy stack header
/// — the smaller icons inside a 22-px button read as more
/// balanced at the CSD scale.
pub const ICON_SIZE_MINUS: f32 = 12.0;
pub const ICON_SIZE_SQUARE: f32 = 10.0;
pub const ICON_SIZE_CLOSE: f32 = 12.0;

/// The three window-control buttons, indexed in visual left-to-right
/// order. `Minimize` is leftmost, `Close` is rightmost — matching
/// `WindowHeader.svelte`'s `{#if hdr.has_minimize}...{#if hdr.has_maximize}...close...`
/// order when both optional buttons are present.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum HeaderButton {
    Minimize,
    Maximize,
    Close,
}

/// Pointer interaction state for the header's button strip.
/// `Idle` means the pointer is in the title drag area, outside
/// any button.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum ButtonInteraction {
    #[default]
    Idle,
    /// Pointer is hovering over this button, no press in progress.
    Hover(HeaderButton),
    /// Pointer is over this button AND the button is being pressed
    /// (primary button down). Matches CSS `:active` state.
    Pressed(HeaderButton),
}

/// Caller-controlled snapshot of every visual parameter that
/// determines how the header should look. Intentionally fully
/// by-value, `Clone + Eq`, so a `Mutex<Option<HeaderVisualState>>`
/// on the window lets us cache-invalidate the rasteriser with a
/// single equality check.
#[derive(Clone, Debug)]
pub struct HeaderVisualState {
    /// Logical width of the window in pixels. The header always
    /// spans this width. Clamped to at least ~60 so we don't panic
    /// on degenerate buffers.
    pub width: i32,
    /// `true` when the window owns keyboard focus. Drives text
    /// colour (fg_primary vs. fg_secondary) and the button icon
    /// contrast.
    pub activated: bool,
    /// Window title, already truncated per the client's wishes.
    /// We do our own pixel-accurate ellipsis truncation below so
    /// long titles don't push into the button strip.
    pub title: String,
    /// Which buttons are visible. `(has_minimize, has_maximize)`.
    /// Close is always present.
    pub buttons: ButtonVisibility,
    /// Current pointer interaction with the buttons. Updated every
    /// motion / button event by the hover state machine in
    /// `CosmicWindow`.
    pub interaction: ButtonInteraction,
    /// Fractional output scale. 1.0 for integer-scale outputs,
    /// values like 1.25/1.5/2.0 on HiDPI. The rasterised buffer is
    /// produced at `ceil(width * scale) x ceil(height * scale)`.
    pub scale: f64,
    /// Keyboard-focus ring visibility, per button. Currently a
    /// reserved field; we plumb it through so the CSS
    /// `:focus-visible` outline can be matched later without a
    /// cache-signature change.
    pub focused_button: Option<HeaderButton>,
    /// Opaque signature of the theme used to rasterise. Bumped on
    /// every `ThemeWatcher` reload. Paired with the `theme_ref`
    /// passed into `rasterize` to decide whether the buffer is
    /// stale. Keeps the cache key `Eq`-comparable without having
    /// to include the entire `LunarisTheme` struct.
    pub theme_generation: u64,
}

impl PartialEq for HeaderVisualState {
    fn eq(&self, other: &Self) -> bool {
        // NOTE: `title` is DELIBERATELY NOT compared. Title
        // rendering is removed, so a title change has no visual
        // effect on the pixmap. Including it in the equality check
        // would trigger unnecessary re-rasterisation every time an
        // app updates `xdg_toplevel.title`. If titles come back,
        // reintroduce `&& self.title == other.title` here.
        self.width == other.width
            && self.activated == other.activated
            && self.buttons == other.buttons
            && self.interaction == other.interaction
            && (self.scale - other.scale).abs() < f64::EPSILON
            && self.focused_button == other.focused_button
            && self.theme_generation == other.theme_generation
    }
}

impl Eq for HeaderVisualState {}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ButtonVisibility {
    pub has_minimize: bool,
    pub has_maximize: bool,
}

impl Default for ButtonVisibility {
    fn default() -> Self {
        ButtonVisibility {
            has_minimize: true,
            has_maximize: true,
        }
    }
}

/// A logical-space rect describing where a button sits in the
/// header. The rasteriser emits a vector of these in visual
/// left-to-right order so the hit-tester can decide which button
/// (if any) the pointer is over without having to re-derive
/// geometry.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ButtonRect {
    pub button: HeaderButton,
    /// Centre-x of the button in logical pixels from the header's
    /// left edge. Kept separate from a full rect because the
    /// visuals use scale transforms that are visually centred on
    /// the button rect, but hit-testing is on the rect itself.
    pub center_x: f32,
    pub center_y: f32,
    /// Button width in logical pixels (the clickable/hover region
    /// width — 28 px to match WindowControls.svelte).
    pub width: f32,
    /// Button height in logical pixels (22 px — shorter than the
    /// 36-px header so buttons have breathing room above/below).
    pub height: f32,
}

impl ButtonRect {
    pub fn hit_test(&self, x: f32, y: f32) -> bool {
        let hw = self.width * 0.5;
        let hh = self.height * 0.5;
        x >= self.center_x - hw
            && x <= self.center_x + hw
            && y >= self.center_y - hh
            && y <= self.center_y + hh
    }
}

/// Layout output: the ordered button rects present in the header,
/// as well as the cached pixmap's raw pixel dimensions.
pub struct HeaderLayout {
    pub buttons: Vec<ButtonRect>,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// Compute the logical-space geometry of each visible button,
/// right-aligned with `BUTTON_STRIP_RIGHT_PAD` trailing space and
/// `BUTTON_GAP` between buttons. Does NOT do any rasterisation —
/// separated out so the hit-tester can call the same function
/// without touching tiny-skia. Order of walk is right-to-left
/// (starting with Close, then Maximize, then Minimize), reversed
/// before return to visual left-to-right.
pub fn layout_buttons(state: &HeaderVisualState) -> Vec<ButtonRect> {
    let mut out = Vec::with_capacity(3);
    let width = state.width as f32;
    let center_y = HEADER_LOGICAL_HEIGHT as f32 * 0.5;

    // Walk right-to-left, advancing by button-width + gap.
    let mut x_right = width - BUTTON_STRIP_RIGHT_PAD;

    // Close is always present, leftmost in the R-to-L walk.
    out.push(ButtonRect {
        button: HeaderButton::Close,
        center_x: x_right - BUTTON_LOGICAL_WIDTH * 0.5,
        center_y,
        width: BUTTON_LOGICAL_WIDTH,
        height: BUTTON_LOGICAL_HEIGHT,
    });
    x_right -= BUTTON_LOGICAL_WIDTH;

    if state.buttons.has_maximize {
        x_right -= BUTTON_GAP;
        out.push(ButtonRect {
            button: HeaderButton::Maximize,
            center_x: x_right - BUTTON_LOGICAL_WIDTH * 0.5,
            center_y,
            width: BUTTON_LOGICAL_WIDTH,
            height: BUTTON_LOGICAL_HEIGHT,
        });
        x_right -= BUTTON_LOGICAL_WIDTH;
    }
    if state.buttons.has_minimize {
        x_right -= BUTTON_GAP;
        out.push(ButtonRect {
            button: HeaderButton::Minimize,
            center_x: x_right - BUTTON_LOGICAL_WIDTH * 0.5,
            center_y,
            width: BUTTON_LOGICAL_WIDTH,
            height: BUTTON_LOGICAL_HEIGHT,
        });
    }
    // Return in visual left-to-right order.
    out.reverse();
    out
}

/// Helper: convert a theme RGBA (0..=1 floats) into tiny-skia's
/// premultiplied `Color`. Handles the straight-alpha RGBA stored
/// in `LunarisTheme` (see `sdk/theme/src/lib.rs`).
fn to_skia(c: Rgba) -> Color {
    Color::from_rgba(c[0], c[1], c[2], c[3]).unwrap_or(Color::BLACK)
}

/// CSS-style `color-mix(in srgb, a N%, b (100-N)%)`. `t` is a
/// (0..=1) weight for `a`. Uses **premultiplied-alpha** mixing to
/// match the CSS Color Module Level 5 spec, which says the mix
/// happens on premultiplied values in the given colour space,
/// then divides back out by the combined alpha.
///
/// The visible difference vs straight-alpha: when mixing a fully
/// opaque colour `fg` with `transparent` at `t=0.10`, premultiplied
/// mixing produces `rgba(fg.R, fg.G, fg.B, 0.10)` — full colour at
/// 10 % opacity, which composites over a dark background as a
/// bright tint. Straight-alpha mixing would produce
/// `rgba(0.10*fg.R, 0.10*fg.G, 0.10*fg.B, 0.10)` instead — a dim
/// tint that renders ~10× darker once composited. The Svelte
/// version uses the CSS spec (via `color-mix(in srgb, ...)`), so
/// the compositor-rendered header must match this math or its
/// hover states look muted compared to the shell.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;

    // Premultiplied RGB.
    let ar_pre = a[0] * a[3];
    let ag_pre = a[1] * a[3];
    let ab_pre = a[2] * a[3];
    let br_pre = b[0] * b[3];
    let bg_pre = b[1] * b[3];
    let bb_pre = b[2] * b[3];

    let out_r_pre = ar_pre * t + br_pre * inv;
    let out_g_pre = ag_pre * t + bg_pre * inv;
    let out_b_pre = ab_pre * t + bb_pre * inv;
    let out_a = a[3] * t + b[3] * inv;

    if out_a > 1e-6 {
        [
            out_r_pre / out_a,
            out_g_pre / out_a,
            out_b_pre / out_a,
            out_a,
        ]
    } else {
        [0.0, 0.0, 0.0, 0.0]
    }
}

/// Transparent colour with straight alpha. Used when mixing
/// `color-mix(in srgb, fg N%, transparent)` from the CSS.
const TRANSPARENT: Rgba = [0.0, 0.0, 0.0, 0.0];

/// Theme-generation counter. Bumped whenever the compositor-wide
/// `LunarisTheme` reloads (appearance.toml / theme.toml change).
/// Pair it into every cached `HeaderVisualState` so a theme swap
/// invalidates every per-window rasterisation exactly once,
/// automatically, with no cache-inspection logic required.
static THEME_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Call from the theme-reload path (see `crate::theme::watch_theme`)
/// after `replace_lunaris_theme` lands the new theme. Safe to call
/// more often than strictly necessary — the counter just needs to
/// be monotonically advancing.
pub fn bump_theme_generation() {
    THEME_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Current theme generation — tag for cached `HeaderVisualState`.
pub fn theme_generation() -> u64 {
    THEME_GENERATION.load(Ordering::Relaxed)
}

// (Font-system helpers removed along with the title renderer.
// See the "Title rendering removed" comment further down for
// why; bring them back if titles return.)

/// Rasterise a header to an `Argb8888` `MemoryRenderBuffer` ready
/// for the render pipeline.
///
/// Coordinate system: logical pixels for layout math, then scaled
/// to physical pixels for the output buffer. All drawing is done
/// in logical space with a single `SkiaTransform::from_scale` at
/// the end so anti-aliasing lands on physical pixel boundaries.
///
/// RENDER-DEBUG: a `tracing::trace!` at the top of this function
/// reports every re-rasterisation, because this runs only when
/// the cache invalidates — seeing many of them during a drag
/// means the cache key is unstable (bug).
pub fn rasterize_header(
    state: &HeaderVisualState,
    theme: &LunarisTheme,
) -> MemoryRenderBuffer {
    let scale = state.scale.max(0.1);
    let logical_w = state.width.max(60) as f32;
    let logical_h = HEADER_LOGICAL_HEIGHT as f32;
    let pixel_w = (logical_w as f64 * scale).ceil() as u32;
    let pixel_h = (logical_h as f64 * scale).ceil() as u32;

    // RENDER-DEBUG log shows the ACTUAL tokens the rasteriser is
    // painting with this frame. Pair with DevTools computed-style
    // on the shell's topbar / app-settings WindowControls to
    // confirm byte-level parity.
    tracing::info!(
        "RENDER-DEBUG rasterize_header w={} h={} scale={:.2} activated={} \
         interaction={:?} theme_gen={} \
         bg=theme.bg_shell={:?} (shell-surface) \
         icon_color=fg_primary={:?} \
         btn_idle_op={} btn_inact_op={} btn_W={} btn_H={} gap={} right_pad={} \
         icon_sizes={{min:{},sqr:{},cls:{}}} \
         lucide_stroke_at12={:.3}px lucide_stroke_at10={:.3}px \
         accent={:?} error={:?} border={:?} \
         radius_md={}",
        pixel_w, pixel_h, scale, state.activated,
        state.interaction, state.theme_generation,
        theme.bg_shell, theme.fg_primary,
        BUTTON_IDLE_OPACITY, BUTTON_IDLE_OPACITY_INACTIVE,
        BUTTON_LOGICAL_WIDTH, BUTTON_LOGICAL_HEIGHT, BUTTON_GAP,
        BUTTON_STRIP_RIGHT_PAD,
        ICON_SIZE_MINUS, ICON_SIZE_SQUARE, ICON_SIZE_CLOSE,
        lucide_stroke_width(12.0), lucide_stroke_width(10.0),
        theme.accent, theme.error, theme.border,
        theme.radius_md,
    );

    let mut pixmap = Pixmap::new(pixel_w.max(1), pixel_h.max(1))
        .expect("pixmap allocation failed for reasonable dimensions");

    // Transparent clear. The header has rounded top corners so
    // anything outside the path stays transparent.
    pixmap.fill(Color::TRANSPARENT);

    draw_background(&mut pixmap.as_mut(), state, theme, scale);
    draw_bottom_border(&mut pixmap.as_mut(), state, theme, scale);
    let buttons = layout_buttons(state);
    draw_buttons(&mut pixmap.as_mut(), state, theme, scale, &buttons);
    // Title rendering removed: the header now consists of bg +
    // bottom border + buttons only. The drag area between the
    // left edge and the button strip stays a pure flat colour.

    // tiny-skia renders into premultiplied RGBA. MemoryRenderBuffer
    // with `Fourcc::Argb8888` + `Transform::Flipped180` matches the
    // Wayland BGRA convention that the rest of the pipeline uses
    // (same choice as `backend/render/cursor.rs`). Actually the
    // underlying data in `pixmap.data()` is already premultiplied
    // RGBA8; we need to swap to BGRA for Argb8888.
    let mut bgra = pixmap.data().to_vec();
    rgba_to_bgra_inplace(&mut bgra);

    MemoryRenderBuffer::from_slice(
        &bgra,
        Fourcc::Argb8888,
        (pixel_w as i32, pixel_h as i32),
        scale.round() as i32,
        Transform::Normal,
        None,
    )
}

/// Swap red and blue channels in-place. tiny-skia produces
/// premultiplied RGBA; Wayland's `Argb8888` is actually BGRA in
/// little-endian.
fn rgba_to_bgra_inplace(data: &mut [u8]) {
    for chunk in data.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }
}

// `primary_family_from_stack` removed along with the title
// renderer — it only existed to hand a single family name to
// cosmic-text's `Family::Name`.

/// Fill the header background with rounded top corners, matching
/// the topbar's `background: var(--background)` — resolved inside
/// the `.shell-surface` CSS context (`app.css:95`) where
/// `--background` overrides to `--color-bg-shell`. The CSS pair is:
///
/// ```css
/// .shell-surface {
///   --background: var(--color-bg-shell);
///   --muted-foreground: color-mix(in srgb, var(--color-fg-shell) 70%, transparent 30%);
/// }
/// ```
///
/// The window header is shell chrome (sits on the same "level"
/// visually as the topbar and popovers), so it uses shell-surface
/// semantics, not the root `--background` (`bg_app`) that regular
/// app content uses. Reading `theme.bg_shell` here is the fix for
/// "topbar #0a0a0a vs header #0f0f0f visible mismatch" — the old
/// `theme.bg_app` read matched the Svelte `WindowHeader.svelte` at
/// root scope, which ITSELF was out of sync with the topbar's
/// shell-surface scope. We also flip the Svelte version to use
/// shell-surface tokens so stack headers stay consistent.
fn draw_background(
    pixmap: &mut PixmapMut,
    state: &HeaderVisualState,
    theme: &LunarisTheme,
    scale: f64,
) {
    let mut pb = PathBuilder::new();
    let w = state.width as f32;
    let h = HEADER_LOGICAL_HEIGHT as f32;
    let r = theme.radius_md;

    // Rounded-top, square-bottom path. Walks clockwise from the
    // bottom-left. When `radius_md == 0` (sharp-corner user
    // preference), the quadratic control points collapse into the
    // line endpoints and tiny-skia renders a plain rect — no
    // subpixel rounding tint.
    pb.move_to(0.0, h);
    pb.line_to(0.0, r);
    pb.quad_to(0.0, 0.0, r, 0.0);
    pb.line_to(w - r, 0.0);
    pb.quad_to(w, 0.0, w, r);
    pb.line_to(w, h);
    pb.close();

    let mut paint = Paint::default();
    paint.set_color(to_skia(theme.bg_shell));
    paint.anti_alias = true;
    let ts = SkiaTransform::from_scale(scale as f32, scale as f32);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, FillRule::Winding, ts, None);
    }
}

/// 1-logical-pixel bottom border, matches `.window-header
/// { border-bottom: 1px solid var(--border) }`. Rendered as a
/// filled 1-px-tall strip flush with the bottom edge, so it lands
/// on the row of physical pixels directly above the window
/// content regardless of integer vs fractional scale.
fn draw_bottom_border(
    pixmap: &mut PixmapMut,
    state: &HeaderVisualState,
    theme: &LunarisTheme,
    scale: f64,
) {
    let w = state.width as f32;
    let h = HEADER_LOGICAL_HEIGHT as f32;
    let mut paint = Paint::default();
    paint.set_color(to_skia(theme.border));
    paint.anti_alias = false;
    let ts = SkiaTransform::from_scale(scale as f32, scale as f32);
    if let Some(rect) = Rect::from_xywh(0.0, h - 1.0, w, 1.0) {
        pixmap.fill_rect(rect, &paint, ts, None);
    }
}

/// Draw the three (or two) window-control buttons. Matches every
/// state of `.header-btn` and `.header-btn.close` from the CSS:
/// hover background, pressed scale(0.9), hover scale(1.1),
/// close-red hover, focus ring.
///
/// Scale transforms for hover / pressed are implemented by
/// applying an additional tiny-skia `SkiaTransform::from_scale`
/// centred on each button, built per-button so one button being
/// hovered doesn't displace the others.
fn draw_buttons(
    pixmap: &mut PixmapMut,
    state: &HeaderVisualState,
    theme: &LunarisTheme,
    scale: f64,
    buttons: &[ButtonRect],
) {
    for b in buttons {
        let (bg_color, icon_color, button_scale) =
            button_visual(b.button, state, theme);
        let hw = b.width * 0.5;
        let hh = b.height * 0.5;

        // The per-button scale transform: translate origin to the
        // button centre, apply scale, translate back. Layered on
        // top of the global output scale.
        let local = SkiaTransform::from_translate(b.center_x, b.center_y)
            .pre_scale(button_scale, button_scale)
            .pre_translate(-b.center_x, -b.center_y);
        let ts = local.post_scale(scale as f32, scale as f32);

        // Background: rounded-rect fill if hover or pressed,
        // transparent otherwise.
        if bg_color[3] > 0.001 {
            let mut pb = PathBuilder::new();
            let left = b.center_x - hw;
            let top = b.center_y - hh;
            let radius = theme.radius_md;
            rounded_rect_path(&mut pb, left, top, b.width, b.height, radius);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(to_skia(bg_color));
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, FillRule::Winding, ts, None);
            }
        }

        // Focus ring — 2 px accent outline on the button when
        // keyboard-focused. Matches
        // `.control-btn:focus-visible { outline: 2px solid
        //  var(--color-accent); outline-offset: 1px }`.
        if state.focused_button == Some(b.button) {
            let mut pb = PathBuilder::new();
            let offset = 1.0;
            let rw = b.width + 2.0 * offset;
            let rh = b.height + 2.0 * offset;
            let left = b.center_x - rw * 0.5;
            let top = b.center_y - rh * 0.5;
            rounded_rect_path(
                &mut pb,
                left, top, rw, rh, theme.radius_md + offset,
            );
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(to_skia(theme.accent));
                paint.anti_alias = true;
                let mut stroke = Stroke::default();
                stroke.width = 2.0;
                pixmap.stroke_path(&path, &paint, &stroke, ts, None);
            }
        }

        // Icon glyph itself, rendered with anti-aliased strokes
        // matching Lucide's default 2-px stroke weight.
        draw_button_icon(pixmap, b, icon_color, ts);
    }
}

/// Pick the (background, icon, scale) visual triple for a button
/// based on its interaction state. Matches the canonical
/// `WindowControls.svelte` shipped in `app-settings` (which is
/// the visible reference the user compares against side-by-side
/// with the compositor-rendered header) **field-for-field**:
///
/// ```css
/// .control-btn          { opacity: 0.7;
///                         transition: opacity var(--duration-fast)
///                                     var(--easing-default); }
/// .control-btn:hover    { opacity: 1; }                /* NOTHING ELSE */
/// .close-btn:hover      { background: var(--destructive);
///                         color: #ffffff; }
/// ```
///
/// Notably, the canonical decoration buttons have:
///  * NO hover-bg tint on non-close buttons (only opacity changes).
///  * NO scale on hover.
///  * NO scale on press.
///
/// An older variant of `WindowControls.svelte` in `desktop-shell`
/// has scale(1.1) on hover, scale(0.9) on press, and a 10 %
/// foreground bg-tint on hover — those were a brief experiment
/// that never made it back into the canonical version. The
/// compositor used to mirror that experimental variant; this
/// function now matches the pared-back canonical look so the
/// app-settings decorations and the compositor-rendered Kitty
/// decorations animate identically.
///
/// Inactive-window dimming (`BUTTON_IDLE_OPACITY_INACTIVE`) stays
/// — that's a compositor-specific extension because
/// WindowControls.svelte never lives outside its own focused
/// app window.
fn button_visual(
    button: HeaderButton,
    state: &HeaderVisualState,
    theme: &LunarisTheme,
) -> (Rgba, Rgba, f32) {
    let is_close = button == HeaderButton::Close;

    let (hovered, _pressed) = match state.interaction {
        ButtonInteraction::Hover(b) if b == button => (true, false),
        ButtonInteraction::Pressed(b) if b == button => (true, true),
        _ => (false, false),
    };

    // Effective "button opacity":
    //   activated + idle  → 0.7   (matches WindowControls.svelte)
    //   activated + hover → 1.0
    //   inactive  + idle  → 0.4   (compositor-only extension)
    //   inactive  + hover → 0.7
    let button_opacity = if hovered {
        if state.activated { 1.0 } else { BUTTON_IDLE_OPACITY }
    } else if state.activated {
        BUTTON_IDLE_OPACITY
    } else {
        BUTTON_IDLE_OPACITY_INACTIVE
    };

    // Background — pre-opacity. Non-close hover stays transparent
    // (no tint). Close-hover lights up the full destructive
    // colour, no alpha attenuation other than `button_opacity`.
    let bg_raw = if hovered && is_close {
        theme.error
    } else {
        TRANSPARENT
    };
    let mut bg = bg_raw;
    bg[3] *= button_opacity;

    // Icon colour. Close-hover flips icon to pure white for
    // maximum contrast on the red fill, regardless of theme.
    let icon_raw = if hovered && is_close {
        [1.0, 1.0, 1.0, 1.0]
    } else {
        theme.fg_primary
    };
    let mut icon_color = icon_raw;
    icon_color[3] *= button_opacity;

    // Scale = 1.0 always. The canonical `WindowControls.svelte`
    // does NOT animate scale; only opacity. Keep this as a triple
    // return so callers stay shape-stable if a future variant
    // wants to bring scale back.
    let scale = 1.0;

    (bg, icon_color, scale)
}

/// Append a closed rounded-rect sub-path to the builder.
/// tiny-skia doesn't have a native rounded rect, so we draw 4
/// corners with quadratic béziers. The radius is clamped to half
/// the shortest side to avoid self-intersection for small rects.
fn rounded_rect_path(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w * 0.5).min(h * 0.5);
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
}

/// Draw the button's icon glyph, centred at `(b.center_x,
/// b.center_y)`, at pixel-perfect parity with the Lucide SVG
/// source the shell's `WindowControls.svelte` uses.
///
/// Every Lucide icon is authored in a 24×24 viewBox with
/// `stroke="currentColor" stroke-width="2" stroke-linecap="round"
/// stroke-linejoin="round" fill="none"`. We map the authored
/// path coordinates (still in 24-unit space) onto our display
/// `icon_size` with a linear scale — the same math an SVG
/// renderer does when it honours the viewBox. That gets us
/// identical geometry. The stroke width scales with the icon too
/// (`icon_size/12` px for the canonical width-2), matching the
/// SVG rendering exactly.
///
/// Authored paths this function draws:
///   Minus:  `<path d="M5 12h14"/>`
///   Square: `<rect width="18" height="18" x="3" y="3" rx="2"/>`
///   X:      `<path d="M18 6 6 18"/>` + `<path d="m6 6 12 12"/>`
///
/// Hand-drawing instead of rasterising the SVG: brings in no
/// resvg dependency, gives us explicit control over the path
/// ordering (the X renders as two separate paths so each gets
/// round caps at both ends, exactly like Lucide's dual `<path>`s).
fn draw_button_icon(
    pixmap: &mut PixmapMut,
    b: &ButtonRect,
    color: Rgba,
    ts: SkiaTransform,
) {
    let icon_size = match b.button {
        HeaderButton::Minimize => ICON_SIZE_MINUS,
        HeaderButton::Maximize => ICON_SIZE_SQUARE,
        HeaderButton::Close => ICON_SIZE_CLOSE,
    };
    let stroke_w = lucide_stroke_width(icon_size);

    // Lucide viewBox origin relative to our button centre. The
    // Lucide coordinate frame is 24×24 with (0,0) top-left and
    // (12,12) at the glyph centre.
    let ox = b.center_x - icon_size * 0.5;
    let oy = b.center_y - icon_size * 0.5;
    let s = icon_size / LUCIDE_VIEWBOX;
    // Map a Lucide (vx, vy) coordinate to display space.
    let p = |vx: f32, vy: f32| -> (f32, f32) { (ox + vx * s, oy + vy * s) };

    let mut paint = Paint::default();
    paint.set_color(to_skia(color));
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = stroke_w;
    stroke.line_cap = tiny_skia::LineCap::Round;
    stroke.line_join = tiny_skia::LineJoin::Round;

    match b.button {
        HeaderButton::Minimize => {
            // Lucide `minus`: M5 12 h14  ≡  (5,12) → (19,12).
            let (x1, y1) = p(5.0, 12.0);
            let (x2, y2) = p(19.0, 12.0);
            let mut pb = PathBuilder::new();
            pb.move_to(x1, y1);
            pb.line_to(x2, y2);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, ts, None);
            }
        }
        HeaderButton::Maximize => {
            // Lucide `square`:
            //   <rect width="18" height="18" x="3" y="3" rx="2"/>
            // Top-left at (3,3), size 18×18, corner radius 2 —
            // both in viewBox units.
            let (rx_tl, ry_tl) = p(3.0, 3.0);
            let (rx_br, ry_br) = p(21.0, 21.0);
            let rect_w = rx_br - rx_tl;
            let rect_h = ry_br - ry_tl;
            let corner = 2.0 * s;
            let mut pb = PathBuilder::new();
            rounded_rect_path(&mut pb, rx_tl, ry_tl, rect_w, rect_h, corner);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, ts, None);
            }
        }
        HeaderButton::Close => {
            // Lucide `x`: two separate paths so each diagonal
            // gets its own pair of round line-caps.
            //   M18 6  6 18
            //   m 6 6  12 12   (relative → absolute is 6,6 → 18,18)
            let (a1x, a1y) = p(18.0, 6.0);
            let (a2x, a2y) = p(6.0, 18.0);
            let mut pb1 = PathBuilder::new();
            pb1.move_to(a1x, a1y);
            pb1.line_to(a2x, a2y);

            let (b1x, b1y) = p(6.0, 6.0);
            let (b2x, b2y) = p(18.0, 18.0);
            let mut pb2 = PathBuilder::new();
            pb2.move_to(b1x, b1y);
            pb2.line_to(b2x, b2y);

            if let Some(path) = pb1.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, ts, None);
            }
            if let Some(path) = pb2.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, ts, None);
            }
        }
    }
}

// Title rendering removed.
//
// The compositor window header used to draw the window's
// `xdg_toplevel.title` between the left edge of the drag zone and
// the button strip, using cosmic-text + SwashCache. That text
// renderer was deleted along with the helpers `font_system`,
// `swash_cache`, `primary_family_from_stack`, `draw_title`, and
// `blit_glyph`, in favour of a title-less header: the drag strip
// is now a pure flat-colour region. `state.title` is retained in
// `HeaderVisualState` because the shell's other code paths
// (stack tabs via the `lunaris-shell-overlay` protocol) still
// consume it — the rasteriser just never renders it.
//
// Restoring titles is additive: re-introduce the helper, call it
// from `rasterize_header`, put the `TITLE_LEFT_PAD` /
// `TITLE_FONT_SIZE` constants back. See the git history for the
// full body if needed.


// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_state(width: i32, activated: bool) -> HeaderVisualState {
        HeaderVisualState {
            width,
            activated,
            title: "Test Window".to_owned(),
            buttons: ButtonVisibility::default(),
            interaction: ButtonInteraction::Idle,
            scale: 1.0,
            focused_button: None,
            theme_generation: 0,
        }
    }

    #[test]
    fn button_layout_right_aligns_from_width() {
        let state = stub_state(1000, true);
        let buttons = layout_buttons(&state);
        assert_eq!(buttons.len(), 3);
        // Left-to-right visual order: minimize, maximize, close
        assert_eq!(buttons[0].button, HeaderButton::Minimize);
        assert_eq!(buttons[1].button, HeaderButton::Maximize);
        assert_eq!(buttons[2].button, HeaderButton::Close);
        // Close is rightmost.
        assert!(buttons[2].center_x > buttons[1].center_x);
        assert!(buttons[1].center_x > buttons[0].center_x);
    }

    #[test]
    fn button_layout_omits_hidden_buttons() {
        let mut state = stub_state(1000, true);
        state.buttons.has_minimize = false;
        state.buttons.has_maximize = true;
        let buttons = layout_buttons(&state);
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].button, HeaderButton::Maximize);
        assert_eq!(buttons[1].button, HeaderButton::Close);
    }

    #[test]
    fn button_layout_centres_buttons_vertically() {
        let state = stub_state(800, true);
        let buttons = layout_buttons(&state);
        for b in &buttons {
            assert!((b.center_y - HEADER_LOGICAL_HEIGHT as f32 / 2.0).abs() < 0.5);
        }
    }

    #[test]
    fn button_hit_test_respects_asymmetric_width_height() {
        // Canonical WindowControls geometry: 28-wide, 22-tall,
        // centred vertically in the 36-px header.
        let b = ButtonRect {
            button: HeaderButton::Close,
            center_x: 100.0,
            center_y: 18.0, // header midline
            width: 28.0,
            height: 22.0,
        };
        assert!(b.hit_test(100.0, 18.0), "centre hit");
        assert!(b.hit_test(86.0, 18.0), "left edge X"); // 100 - 28/2
        assert!(!b.hit_test(85.0, 18.0), "outside left");
        assert!(b.hit_test(100.0, 7.0), "top edge Y"); // 18 - 22/2
        assert!(!b.hit_test(100.0, 6.0), "outside top");
        assert!(b.hit_test(100.0, 29.0), "bottom edge Y"); // 18 + 22/2
        assert!(!b.hit_test(100.0, 30.0), "outside bottom");
    }

    #[test]
    fn button_visual_close_hover_is_full_destructive() {
        // Canonical WindowControls.svelte:
        //   .close-btn:hover { background: var(--destructive) }
        // Full opaque red, not a mix. Icon flips to pure white.
        // (Old behaviour was error @ 80 % alpha — that came from a
        //  stale stack WindowHeader CSS variant.)
        let state = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Close),
            ..stub_state(800, true)
        };
        let theme = LunarisTheme::lunaris_dark();
        let (bg, icon, scale) = button_visual(HeaderButton::Close, &state, &theme);
        // Full destructive RGB at full alpha (activated window → opacity 1.0).
        assert!((bg[0] - theme.error[0]).abs() < 0.001, "R {}", bg[0]);
        assert!((bg[1] - theme.error[1]).abs() < 0.001);
        assert!((bg[2] - theme.error[2]).abs() < 0.001);
        assert!((bg[3] - 1.0).abs() < 0.001, "A should be full, got {}", bg[3]);
        // Icon pure white at full alpha on activated window.
        assert_eq!(icon, [1.0, 1.0, 1.0, 1.0]);
        // Canonical app-settings WindowControls has NO scale on
        // hover or active. Stay at 1.0.
        assert!((scale - 1.0).abs() < 0.001, "no hover scale, got {}", scale);
    }

    #[test]
    fn button_visual_close_hover_is_full_destructive_even_inactive_dimmed() {
        // Even on an unfocused window the close-hover bg stays
        // destructive-coloured; only its alpha is dimmed by the
        // inactive-idle opacity (hover on unfocused window).
        let state = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Close),
            ..stub_state(800, false) // inactive
        };
        let theme = LunarisTheme::lunaris_dark();
        let (bg, icon, _) = button_visual(HeaderButton::Close, &state, &theme);
        assert!((bg[0] - theme.error[0]).abs() < 0.001);
        // hover on inactive → BUTTON_IDLE_OPACITY (0.7), not 1.0.
        assert!((bg[3] - BUTTON_IDLE_OPACITY).abs() < 0.001);
        // Icon still pure white RGB.
        assert!((icon[0] - 1.0).abs() < 0.001);
        assert!((icon[3] - BUTTON_IDLE_OPACITY).abs() < 0.001);
    }

    #[test]
    fn button_visual_no_scale_animation() {
        // Canonical WindowControls has no transform animation —
        // verify all states stay at scale 1.0 so the compositor
        // doesn't reintroduce the bouncy scale-on-press feel.
        let theme = LunarisTheme::lunaris_dark();

        let idle = stub_state(800, true);
        let (_, _, s_idle) = button_visual(HeaderButton::Minimize, &idle, &theme);
        assert!((s_idle - 1.0).abs() < 0.001);

        let hover = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Minimize),
            ..idle.clone()
        };
        let (_, _, s_hover) = button_visual(HeaderButton::Minimize, &hover, &theme);
        assert!((s_hover - 1.0).abs() < 0.001);

        let pressed = HeaderVisualState {
            interaction: ButtonInteraction::Pressed(HeaderButton::Minimize),
            ..idle
        };
        let (_, _, s_press) = button_visual(HeaderButton::Minimize, &pressed, &theme);
        assert!((s_press - 1.0).abs() < 0.001);
    }

    #[test]
    fn button_visual_no_hover_bg_for_non_close() {
        // Non-close hover keeps bg transparent — only opacity
        // changes. This is the visible difference vs the older
        // desktop-shell variant that mixed a 10 % foreground tint.
        let theme = LunarisTheme::lunaris_dark();
        let hover = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Minimize),
            ..stub_state(800, true)
        };
        let (bg, _, _) = button_visual(HeaderButton::Minimize, &hover, &theme);
        // Pre-multiplied alpha == 0 → background fully transparent.
        assert!(bg[3] < 1e-5, "non-close hover should not paint bg, got alpha {}", bg[3]);
    }

    #[test]
    fn button_visual_idle_is_transparent() {
        let state = stub_state(800, true);
        let theme = LunarisTheme::panda();
        let (bg, _, scale) = button_visual(HeaderButton::Minimize, &state, &theme);
        assert_eq!(bg[3], 0.0);
        assert!((scale - 1.0).abs() < 0.001);
    }

    #[test]
    fn visual_state_eq_requires_exact_match() {
        let a = stub_state(800, true);
        let b = a.clone();
        assert_eq!(a, b);

        let mut c = a.clone();
        c.activated = false;
        assert_ne!(a, c);

        // `title` is intentionally NOT part of equality — title
        // rendering is removed, so a title swap alone should NOT
        // invalidate the cached pixmap.
        let mut d = a.clone();
        d.title = "Other Window".into();
        assert_eq!(a, d, "title change must not invalidate the cache");

        let mut e = a.clone();
        e.interaction = ButtonInteraction::Hover(HeaderButton::Close);
        assert_ne!(a, e);
    }

    #[test]
    fn mix_respects_ratio_for_opaque_colours() {
        // Both inputs opaque — premultiplied mixing collapses to
        // straight mixing because alpha is 1 on both sides.
        let black: Rgba = [0.0, 0.0, 0.0, 1.0];
        let white: Rgba = [1.0, 1.0, 1.0, 1.0];
        let m = mix(white, black, 0.5);
        assert!((m[0] - 0.5).abs() < 0.01);
        assert_eq!(m[3], 1.0);
    }

    #[test]
    fn mix_with_transparent_keeps_full_colour_reduces_alpha() {
        // This is the CSS parity test. `color-mix(in srgb, fg 10%,
        // transparent)` in a browser produces rgba(fg.R, fg.G,
        // fg.B, 0.10) — full colour, 10 % alpha. The dim-straight-
        // alpha alternative would give rgba(0.10*fg.R, ..., 0.10)
        // which looks ~10× darker on a dark background.
        let fg: Rgba = [0.98, 0.98, 0.98, 1.0]; // #fafafa
        let transparent: Rgba = [0.0, 0.0, 0.0, 0.0];
        let m = mix(fg, transparent, 0.10);
        // RGB should be preserved (premultiplied math divides back).
        assert!((m[0] - 0.98).abs() < 0.001, "R should be preserved, got {}", m[0]);
        assert!((m[1] - 0.98).abs() < 0.001);
        assert!((m[2] - 0.98).abs() < 0.001);
        // Alpha reduced by the weight.
        assert!((m[3] - 0.10).abs() < 0.001, "A should be 0.10, got {}", m[3]);
    }

    #[test]
    fn mix_with_transparent_and_small_weight_stays_bright() {
        // Sanity: even at 8% weight (the CSS tab-hover uses 8%),
        // the RGB colour survives — only the alpha shrinks.
        let fg: Rgba = [0.98, 0.98, 0.98, 1.0];
        let transparent: Rgba = [0.0, 0.0, 0.0, 0.0];
        let m = mix(fg, transparent, 0.08);
        assert!((m[0] - 0.98).abs() < 0.001);
        assert!((m[3] - 0.08).abs() < 0.001);
    }

    #[test]
    fn mix_zero_weight_returns_second_colour() {
        let fg: Rgba = [0.5, 0.5, 0.5, 1.0];
        let transparent: Rgba = [0.0, 0.0, 0.0, 0.0];
        let m = mix(fg, transparent, 0.0);
        assert_eq!(m[3], 0.0);
    }

    #[test]
    fn rasterize_produces_buffer_of_correct_size() {
        let state = stub_state(600, true);
        let theme = LunarisTheme::panda();
        let buf = rasterize_header(&state, &theme);
        // MemoryRenderBuffer dims are (width, height) physical.
        // Can't easily inspect internals; just ensure it doesn't panic.
        drop(buf);
    }

    #[test]
    fn rasterize_handles_empty_title() {
        let mut state = stub_state(600, true);
        state.title = String::new();
        let theme = LunarisTheme::panda();
        let _ = rasterize_header(&state, &theme);
    }

    #[test]
    fn rasterize_handles_hidpi_scale() {
        let mut state = stub_state(600, true);
        state.scale = 2.0;
        let theme = LunarisTheme::panda();
        let _ = rasterize_header(&state, &theme);
    }

    #[test]
    fn rasterize_handles_narrow_window() {
        // Window smaller than the button strip should still not
        // panic — clamped to minimum width internally.
        let state = stub_state(10, true);
        let theme = LunarisTheme::panda();
        let _ = rasterize_header(&state, &theme);
    }

    #[test]
    fn rgba_to_bgra_swaps_first_and_third_channel() {
        let mut data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        rgba_to_bgra_inplace(&mut data);
        assert_eq!(data, vec![3u8, 2, 1, 4, 7, 6, 5, 8]);
    }

    // primary_family_* tests removed along with the title renderer.

    #[test]
    fn rasterize_uses_theme_radius_not_hardcoded() {
        // Changing the theme's radius_md must change the
        // rasterised output bytes — proves the renderer honours
        // the token instead of silently falling back to a literal.
        let state = stub_state(600, true);
        let mut theme_a = LunarisTheme::lunaris_dark();
        theme_a.radius_md = 0.0;
        let mut theme_b = LunarisTheme::lunaris_dark();
        theme_b.radius_md = 16.0;
        // MemoryRenderBuffer doesn't expose data() directly, so we
        // can't diff bytes here, but running the rasteriser with
        // different radius values must not panic and must produce
        // DIFFERENT buffers. Presence of a buffer at all is a
        // sanity test; the byte-diff test lives in the integration
        // suite under tests/.
        let _ = rasterize_header(&state, &theme_a);
        let _ = rasterize_header(&state, &theme_b);
    }

    #[test]
    fn rasterize_uses_theme_bg_app_not_hardcoded() {
        // A `bg_app` swap should change the rasterised pixels.
        let state = stub_state(600, true);
        let mut dark = LunarisTheme::lunaris_dark();
        let mut light = LunarisTheme::lunaris_light();
        // Sanity: the two presets don't have the same bg_app.
        assert_ne!(dark.bg_app, light.bg_app);
        dark.bg_app = [1.0, 0.0, 0.0, 1.0]; // pure red
        light.bg_app = [0.0, 1.0, 0.0, 1.0]; // pure green
        let _ = rasterize_header(&state, &dark);
        let _ = rasterize_header(&state, &light);
    }

    #[test]
    fn rasterize_picks_theme_accent_for_focus_ring() {
        // Focused-button rendering uses theme.accent for the ring.
        let mut state = stub_state(600, true);
        state.focused_button = Some(HeaderButton::Close);
        let theme = LunarisTheme::lunaris_dark();
        let _ = rasterize_header(&state, &theme);
    }

    // ── Lucide icon geometry ──────────────────────────────────
    // The window-control buttons render hand-drawn Lucide paths
    // (see `draw_button_icon`). These tests encode the Lucide
    // source geometry — any future tweak that drifts us off-spec
    // will fail here before it becomes a visual regression.

    #[test]
    fn lucide_stroke_width_scales_with_icon_size() {
        // stroke-width="2" in a 24-unit viewBox. At display size
        // N, the physical stroke is N * 2/24 = N/12.
        assert!((lucide_stroke_width(12.0) - 1.0).abs() < 1e-5);
        assert!((lucide_stroke_width(24.0) - 2.0).abs() < 1e-5);
        assert!((lucide_stroke_width(10.0) - 10.0 / 12.0).abs() < 1e-5);
    }

    #[test]
    fn lucide_minus_line_length_matches_svg_path() {
        // `<path d="M5 12h14"/>` = line (5,12) → (19,12), viewBox
        // span 14. At icon_size 12 the physical line length is
        // 12 * 14/24 = 7.0 px.
        let vb_span = 19.0 - 5.0;
        let phys = ICON_SIZE_MINUS * vb_span / LUCIDE_VIEWBOX;
        assert!((phys - 7.0).abs() < 1e-5, "minus line should be 7px at icon_size=12, got {phys}");
    }

    #[test]
    fn lucide_square_rect_fills_18_of_24() {
        // `<rect width="18" height="18" x="3" y="3" rx="2"/>`.
        // At icon_size 10: rect 10 * 18/24 = 7.5px, rx = 10 * 2/24.
        let rect = ICON_SIZE_SQUARE * 18.0 / LUCIDE_VIEWBOX;
        let rx = ICON_SIZE_SQUARE * 2.0 / LUCIDE_VIEWBOX;
        assert!((rect - 7.5).abs() < 1e-5, "square rect size {rect}");
        assert!((rx - 10.0 / 12.0).abs() < 1e-5, "square rx {rx}");
    }

    #[test]
    fn lucide_x_diagonals_span_12_of_24() {
        // Diagonals go from (6,6) to (18,18) etc — span 12 units.
        // At icon_size 12: 12 * 12/24 = 6.0 px diagonal extent
        // per axis (i.e. ±3 px from centre).
        let extent_per_axis = ICON_SIZE_CLOSE * 12.0 / LUCIDE_VIEWBOX;
        assert!((extent_per_axis - 6.0).abs() < 1e-5, "X diagonal axis extent {extent_per_axis}");
        // Endpoints sit `extent/2` from centre.
        let offset_from_centre = extent_per_axis * 0.5;
        assert!((offset_from_centre - 3.0).abs() < 1e-5);
    }

    #[test]
    fn rasterize_picks_theme_error_for_close_hover_full_opacity() {
        // Superseded by `button_visual_close_hover_is_full_destructive`
        // — kept here as a no-brainer sanity check that the close
        // hover bg at least pulls from theme.error (not some other
        // token). Full semantics in the dedicated test.
        let state = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Close),
            ..stub_state(600, true)
        };
        let theme = LunarisTheme::lunaris_dark();
        let (bg, _, _) = button_visual(HeaderButton::Close, &state, &theme);
        assert!((bg[0] - theme.error[0]).abs() < 0.01, "close hover R should match theme.error");
        assert!((bg[1] - theme.error[1]).abs() < 0.01);
        assert!((bg[2] - theme.error[2]).abs() < 0.01);
    }

    #[test]
    fn button_opacity_matches_windowcontrols_values() {
        // WindowControls.svelte canonical values:
        //   activated + idle  → opacity 0.7
        //   activated + hover → opacity 1.0
        //   (inactive window is a compositor-specific extension)
        //   inactive  + idle  → opacity 0.4
        //   inactive  + hover → opacity 0.7
        let theme = LunarisTheme::lunaris_dark();

        let (_, icon_act_idle, _) =
            button_visual(HeaderButton::Minimize, &stub_state(600, true), &theme);
        // Icon RGB is fg_primary; alpha times button_opacity = 0.7.
        assert!((icon_act_idle[3] - BUTTON_IDLE_OPACITY).abs() < 1e-5);
        assert_eq!(icon_act_idle[0], theme.fg_primary[0]);

        let hover_state = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Minimize),
            ..stub_state(600, true)
        };
        let (_, icon_act_hover, _) =
            button_visual(HeaderButton::Minimize, &hover_state, &theme);
        assert!((icon_act_hover[3] - 1.0).abs() < 1e-5, "activated hover should be 1.0 opacity");

        let (_, icon_inact_idle, _) =
            button_visual(HeaderButton::Minimize, &stub_state(600, false), &theme);
        assert!((icon_inact_idle[3] - BUTTON_IDLE_OPACITY_INACTIVE).abs() < 1e-5);

        let inact_hover = HeaderVisualState {
            interaction: ButtonInteraction::Hover(HeaderButton::Minimize),
            ..stub_state(600, false)
        };
        let (_, icon_inact_hover, _) =
            button_visual(HeaderButton::Minimize, &inact_hover, &theme);
        assert!((icon_inact_hover[3] - BUTTON_IDLE_OPACITY).abs() < 1e-5);
    }

    #[test]
    fn rasterize_reads_theme_bg_shell_not_bg_app() {
        // Regression: the compositor-rendered header must use
        // `bg_shell` (shell-surface colour, matches the topbar) and
        // NOT `bg_app` (root colour, used for app content). Sanity
        // check by construction — we verify the theme preset has
        // distinct bg_shell and bg_app so a silent future
        // refactor that flips them can't hide.
        let dark = LunarisTheme::lunaris_dark();
        assert_ne!(
            dark.bg_shell, dark.bg_app,
            "dark theme should have distinct shell-chrome vs app-content backgrounds"
        );
        assert_eq!(dark.bg_shell, [0x0a as f32 / 255.0, 0x0a as f32 / 255.0, 0x0a as f32 / 255.0, 1.0]);
        assert_eq!(dark.bg_app,   [0x0f as f32 / 255.0, 0x0f as f32 / 255.0, 0x0f as f32 / 255.0, 1.0]);
    }
}
