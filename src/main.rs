//! Shuffle — a fast, snappy file manager for Apple Silicon Macs.
//!
//! Milestone 2: a left sidebar of shortcuts.
//! - Recent: directories visited recently (tracked + persisted, most-recent first).
//! - Bookmarks: user-pinned directories (the "+" pins the current folder).
//! - Favorites: Applications, Pictures, Documents, Downloads.
//! - Locations: Macintosh HD (/) and the current user's home directory.
//! Clicking any item navigates the main listing. The active location is
//! highlighted. State lives in ~/Library/Application Support/Shuffle/.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local};
use gpui::{
    actions, anchored, div, ease_out_quint, img, point, prelude::*, px, relative, rgb, rgba, size,
    uniform_list, Animation, AnimationExt, AnyElement, App,
    Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, ElementId, ExternalPaths, FocusHandle, FontWeight, ImageSource,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    PathPromptOptions, Rgba,
    RenderImage, ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace, NSDraggingContext,
    NSDraggingSession, NSDraggingSource, NSDragOperation, NSGraphicsContext, NSImage,
    NSPDFImageRep, NSWorkspace,
};
use objc2_foundation::{NSData, NSFileManager, NSObject, NSString, NSURL};
use rayon::prelude::*;

const RECENTS_CAP: usize = 12;

// Menu-bar actions. OpenSettings/Quit are app-level; the rest are dispatched to
// the focused explorer window and handled in `Shuffle::render`'s root.
actions!(shuffle, [OpenSettings, Quit]);
actions!(
    shuffle,
    [
        NewTab,
        NewFolder,
        CloseTab,
        MoveToTrash,
        ViewList,
        ViewIcons,
        ViewColumns,
        ViewGallery,
        ToggleSidebar,
        GoBack,
        GoForward,
        GoHome,
        GoApplications,
        GoComputer,
        FocusSearch,
    ]
);

// ----- theming ---------------------------------------------------------------

/// A complete color theme. Every field is a 0xRRGGBB color.
#[derive(Clone, Copy, PartialEq)]
struct Theme {
    bg: u32,            // app background
    sidebar: u32,       // sidebar background
    surface: u32,       // elevated surfaces (menus, panels, active nav)
    hover: u32,         // row / item hover background (the "mouseover" color)
    selected: u32,      // selected / highlighted background
    border: u32,        // hairline borders
    border_strong: u32, // stronger dividers
    text: u32,          // primary text
    text_muted: u32,    // secondary text (kind / date / size)
    text_dim: u32,      // section headers, placeholders
    accent: u32,        // folders, carets, active highlights
}

impl Theme {
    /// A translucent variant of a base color, for floating panels. `a` is 0..=255.
    fn alpha(color: u32, a: u32) -> Rgba {
        rgba((color << 8) | (a & 0xff))
    }
}

impl Default for Theme {
    fn default() -> Self {
        PRESETS[0].1
    }
}

/// Clamp a channel value to 0..=255.
const fn clamp8(x: u32) -> u32 {
    if x > 0xff {
        0xff
    } else {
        x
    }
}

/// Scale every channel of an 0xRRGGBB color by `num/den` (lighten if >1, darken
/// if <1), clamping each channel. Used to derive coherent shades from one base.
const fn scale(c: u32, num: u32, den: u32) -> u32 {
    let r = clamp8(((c >> 16) & 0xff) * num / den);
    let g = clamp8(((c >> 8) & 0xff) * num / den);
    let b = clamp8((c & 0xff) * num / den);
    (r << 16) | (g << 8) | b
}

/// Build a coherent DARK theme from a background, text, and accent color.
/// Surfaces are derived lighter than the background; muted/dim text darker.
const fn dark_theme(bg: u32, text: u32, accent: u32) -> Theme {
    Theme {
        bg,
        sidebar: scale(bg, 8, 10),
        surface: scale(bg, 15, 10),
        hover: scale(bg, 13, 10),
        selected: scale(bg, 19, 10),
        border: scale(bg, 14, 10),
        border_strong: scale(bg, 20, 10),
        text,
        text_muted: scale(text, 7, 10),
        text_dim: scale(text, 5, 10),
        accent,
    }
}

/// Build a coherent LIGHT theme. Surfaces are derived darker than the (light)
/// background; muted/dim text lighter so it recedes.
const fn light_theme(bg: u32, text: u32, accent: u32) -> Theme {
    Theme {
        bg,
        sidebar: scale(bg, 95, 100),
        surface: scale(bg, 90, 100),
        hover: scale(bg, 93, 100),
        selected: scale(bg, 84, 100),
        border: scale(bg, 88, 100),
        border_strong: scale(bg, 80, 100),
        text,
        text_muted: scale(text, 13, 10),
        text_dim: scale(text, 16, 10),
        accent,
    }
}

/// Built-in palettes shown in Settings — a broad spread across hues, dark and
/// light. (name, theme)
const PRESETS: &[(&str, Theme)] = &[
    // --- Hand-tuned signatures ---
    (
        "Shuffle Dark",
        Theme {
            bg: 0x1e1e22,
            sidebar: 0x17171a,
            surface: 0x2f2f3a,
            hover: 0x2a2a30,
            selected: 0x33334a,
            border: 0x303036,
            border_strong: 0x3a3a44,
            text: 0xf0f0f4,
            text_muted: 0x8a8a92,
            text_dim: 0x6b6b73,
            accent: 0x7aa2f7,
        },
    ),
    (
        "Catppuccin Mocha",
        Theme {
            bg: 0x1e1e2e,
            sidebar: 0x181825,
            surface: 0x313244,
            hover: 0x2a2b3c,
            selected: 0x45475a,
            border: 0x313244,
            border_strong: 0x45475a,
            text: 0xcdd6f4,
            text_muted: 0xa6adc8,
            text_dim: 0x6c7086,
            accent: 0x89b4fa,
        },
    ),
    (
        "Catppuccin Macchiato",
        Theme {
            bg: 0x24273a,
            sidebar: 0x1e2030,
            surface: 0x363a4f,
            hover: 0x2e3148,
            selected: 0x494d64,
            border: 0x363a4f,
            border_strong: 0x494d64,
            text: 0xcad3f5,
            text_muted: 0xa5adcb,
            text_dim: 0x6e738d,
            accent: 0x8aadf4,
        },
    ),
    (
        "Catppuccin Frappé",
        Theme {
            bg: 0x303446,
            sidebar: 0x292c3c,
            surface: 0x414559,
            hover: 0x3a3e52,
            selected: 0x51576d,
            border: 0x414559,
            border_strong: 0x51576d,
            text: 0xc6d0f5,
            text_muted: 0xa5adce,
            text_dim: 0x737994,
            accent: 0x8caaee,
        },
    ),
    (
        "Catppuccin Latte",
        Theme {
            bg: 0xeff1f5,
            sidebar: 0xe6e9ef,
            surface: 0xccd0da,
            hover: 0xdce0e8,
            selected: 0xbcc0cc,
            border: 0xccd0da,
            border_strong: 0xbcc0cc,
            text: 0x4c4f69,
            text_muted: 0x6c6f85,
            text_dim: 0x9ca0b0,
            accent: 0x1e66f5,
        },
    ),
    // --- Popular dark themes (varied accent hues) ---
    ("Dracula", dark_theme(0x282a36, 0xf8f8f2, 0xbd93f9)),
    ("Nord", dark_theme(0x2e3440, 0xe5e9f0, 0x88c0d0)),
    ("Tokyo Night", dark_theme(0x1a1b26, 0xc0caf5, 0x7aa2f7)),
    ("Gruvbox Dark", dark_theme(0x282828, 0xebdbb2, 0xfe8019)),
    ("One Dark", dark_theme(0x282c34, 0xabb2bf, 0x61afef)),
    ("Solarized Dark", dark_theme(0x002b36, 0x93a1a1, 0x268bd2)),
    ("Monokai", dark_theme(0x272822, 0xf8f8f2, 0xa6e22e)),
    ("Everforest", dark_theme(0x2d353b, 0xd3c6aa, 0xa7c080)),
    ("Rosé Pine", dark_theme(0x191724, 0xe0def4, 0xeb6f92)),
    // --- Bold single-hue darks (green / red / blue / …) ---
    ("Forest", dark_theme(0x10211a, 0xd6f5e3, 0x4ade80)),
    ("Crimson", dark_theme(0x211315, 0xf8dcdc, 0xf87171)),
    ("Ocean", dark_theme(0x0e1a26, 0xd6ecff, 0x38bdf8)),
    ("Grape", dark_theme(0x1a1326, 0xece0f8, 0xc084fc)),
    ("Amber", dark_theme(0x221a10, 0xf8ecd6, 0xfbbf24)),
    ("Rose", dark_theme(0x241620, 0xf8dcee, 0xf472b6)),
    ("Teal", dark_theme(0x0e201e, 0xd6f5f0, 0x2dd4bf)),
    ("Sunset", dark_theme(0x241712, 0xfae5d8, 0xfb7185)),
    // --- Light themes ---
    ("Solarized Light", light_theme(0xfdf6e3, 0x586e75, 0x268bd2)),
    ("GitHub Light", light_theme(0xffffff, 0x24292f, 0x0969da)),
    ("Rosé Pine Dawn", light_theme(0xfaf4ed, 0x575279, 0xd7827e)),
    ("Mint", light_theme(0xf0faf4, 0x14532d, 0x16a34a)),
    ("Sky", light_theme(0xeff6ff, 0x1e3a5f, 0x2563eb)),
    ("Lavender", light_theme(0xf6f2fe, 0x4c3a66, 0x8b5cf6)),
];

/// Curated per-property options shown in Settings alongside the presets.
/// Convert HSL (h in 0..360, s/l in 0..1) to a 0xRRGGBB color.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m).clamp(0.0, 1.0)) * 255.0).round() as u32;
    (to(r1) << 16) | (to(g1) << 8) | to(b1)
}

/// A broad, ordered swatch palette: a grayscale ramp plus a hue × lightness grid
/// — enough variety to set any reasonable background/text/accent color.
fn palette_colors() -> Vec<u32> {
    let mut out = Vec::new();
    // Grayscale ramp (black → white).
    for i in 0..13 {
        let v = (i as f32 / 12.0 * 255.0).round() as u32;
        out.push((v << 16) | (v << 8) | v);
    }
    // Hues across the wheel, each from dark to light.
    let lights = [0.18f32, 0.30, 0.42, 0.55, 0.68, 0.82];
    for hue in (0..360).step_by(30) {
        for &l in &lights {
            out.push(hsl_to_rgb(hue as f32, 0.65, l));
        }
    }
    out
}

/// Wrapper so the theme lives in the GPUI global store and notifies observers.
#[derive(Clone, Copy)]
struct ThemeGlobal(Theme);
impl gpui::Global for ThemeGlobal {}

thread_local! {
    /// The render-side copy of the active theme, read by all draw code on the
    /// main thread without needing an `App` handle.
    static ACTIVE_THEME: RefCell<Theme> = RefCell::new(Theme::default());
}

/// The active theme. Read this anywhere in render code.
fn theme() -> Theme {
    ACTIVE_THEME.with(|t| *t.borrow())
}

fn set_active_theme(t: Theme) {
    ACTIVE_THEME.with(|c| *c.borrow_mut() = t);
}

/// Apply a theme everywhere: update the render-side copy, persist it, and store
/// it in the global so observers (the main window) repaint.
fn apply_theme(t: Theme, cx: &mut App) {
    set_active_theme(t);
    save_theme(&t);
    cx.set_global(ThemeGlobal(t));
    // Keep menus following the theme unless the user chose menu colors.
    let mut m = menu_style();
    if !m.custom {
        m.follow_theme(&t);
        set_active_menu(m);
        save_menu_style(&m);
        cx.set_global(MenuStyleGlobal(m));
    }
    cx.refresh_windows();
}

// ----- feature preferences (the General tab toggles) -------------------------

/// User-toggleable features.
#[derive(Clone, Copy)]
struct Prefs {
    /// Show a terminal-style command input at the bottom of the explorer.
    terminal: bool,
    /// Also show a scrollback of what you've typed / command output.
    term_history: bool,
    /// Show a file preview in the inspector when a file is selected.
    preview: bool,
    /// Show ‹ › page arrows on multi-page PDF previews in the inspector.
    preview_pages: bool,
    /// Show file information in the inspector when a file is selected.
    info: bool,
    /// Show the leading ".." row that goes up one level.
    show_parent: bool,
    /// Collapse the left sidebar to an icon-only rail.
    sidebar_collapsed: bool,
    /// How many Recents to show in the sidebar (0 hides the section).
    recent_limit: usize,
    /// Give the command palette (Cmd+P) its own Up/Down query history.
    palette_history: bool,
    /// Enable custom sidebar "groups" of files/folders.
    groups_enabled: bool,
    /// Show the always-on "Filter" pill in the bottom-right (/ still works).
    show_filter_button: bool,
    /// Show a live frame-rate meter in the top-right (debug; costs some CPU).
    show_fps: bool,
    /// Run user shell-script actions from the Scripts folder in menus.
    script_actions: bool,
    /// SFTP auth: true = delegate to the user's ~/.ssh (config/keys/known_hosts);
    /// false = use the explicit key path saved per server.
    ssh_use_system: bool,
    /// Whether the user has made the first-run SFTP auth choice yet.
    ssh_configured: bool,
    /// Show the expandable "waterfall" folder tree at the bottom of the sidebar.
    waterfall: bool,
    /// Command palette / search window opacity, as a percent (20–100). Lower is
    /// more see-through; the default is nearly opaque so it doesn't read as a
    /// confusing floating pane over the explorer.
    palette_opacity: u8,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            terminal: false,
            term_history: false,
            preview: false,
            preview_pages: true,
            info: false,
            show_parent: true,
            sidebar_collapsed: false,
            recent_limit: 3,
            palette_history: false,
            groups_enabled: false,
            show_filter_button: true,
            show_fps: false,
            script_actions: false,
            ssh_use_system: true,
            ssh_configured: false,
            waterfall: false,
            palette_opacity: 92,
        }
    }
}

#[derive(Clone, Copy)]
struct PrefsGlobal(Prefs);
impl gpui::Global for PrefsGlobal {}

thread_local! {
    static ACTIVE_PREFS: RefCell<Prefs> = const { RefCell::new(Prefs {
        terminal: false,
        term_history: false,
        preview: false,
        preview_pages: true,
        info: false,
        show_parent: true,
        sidebar_collapsed: false,
        recent_limit: 3,
        palette_history: false,
        groups_enabled: false,
        show_filter_button: true,
        show_fps: false,
        script_actions: false,
        ssh_use_system: true,
        ssh_configured: false,
        waterfall: false,
        palette_opacity: 92,
    }) };
}

/// The active preferences. Read this anywhere in render code.
fn prefs() -> Prefs {
    ACTIVE_PREFS.with(|p| *p.borrow())
}

/// The command palette's background alpha (0–255) from the opacity pref.
fn palette_alpha() -> u32 {
    let pct = prefs().palette_opacity.clamp(20, 100) as u32;
    pct * 255 / 100
}

fn set_active_prefs(p: Prefs) {
    ACTIVE_PREFS.with(|c| *c.borrow_mut() = p);
}

/// Apply prefs everywhere: update the render copy, persist, store in the global.
fn apply_prefs(p: Prefs, cx: &mut App) {
    set_active_prefs(p);
    save_prefs(&p);
    cx.set_global(PrefsGlobal(p));
    cx.refresh_windows();
}

// ----- icon packs ------------------------------------------------------------

/// The active icon pack: a folder of images named `folder.png`, `file.png`, and
/// per-extension (e.g. `pdf.png`, `png.png`) that override macOS icons.
#[derive(Clone)]
struct IconPackGlobal(Option<PathBuf>);
impl gpui::Global for IconPackGlobal {}

thread_local! {
    static ACTIVE_ICON_PACK: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn icon_pack() -> Option<PathBuf> {
    ACTIVE_ICON_PACK.with(|p| p.borrow().clone())
}

fn set_active_icon_pack(p: Option<PathBuf>) {
    ACTIVE_ICON_PACK.with(|c| *c.borrow_mut() = p);
}

/// Apply an icon pack (or `None` to revert to macOS icons): persist, rebuild
/// icons, and notify the explorer window.
fn apply_icon_pack(p: Option<PathBuf>, cx: &mut App) {
    set_active_icon_pack(p.clone());
    save_icon_pack(&p);
    cx.set_global(IconPackGlobal(p));
    cx.refresh_windows();
}

// ----- menu style (right-click / dropdown menu appearance) -------------------

/// Customizable look of pop-up menus (right-click menu, dropdowns).
#[derive(Clone, Copy, PartialEq)]
struct MenuStyle {
    /// Menu background color.
    bg: u32,
    /// Menu text ("letter") color.
    text: u32,
    /// Background opacity, 0..=100 percent.
    opacity: u8,
    /// Menu font size in pixels.
    font_px: f32,
    /// True once the user picks explicit menu colors. While false, `bg`/`text`
    /// follow the active theme (so switching to a light theme lightens menus
    /// too); opacity and size are always user-controlled.
    custom: bool,
}

impl Default for MenuStyle {
    fn default() -> Self {
        let d = Theme::default();
        MenuStyle { bg: d.surface, text: d.text, opacity: 100, font_px: 14.0, custom: false }
    }
}

impl MenuStyle {
    /// Re-derive `bg`/`text` from the theme unless the user set explicit colors.
    fn follow_theme(&mut self, t: &Theme) {
        if !self.custom {
            self.bg = t.surface;
            self.text = t.text;
        }
    }
}

impl MenuStyle {
    /// The background as an rgba value with the configured opacity applied.
    fn bg_rgba(&self) -> Rgba {
        let a = (self.opacity.min(100) as u32 * 255) / 100;
        Theme::alpha(self.bg, a)
    }
}

#[derive(Clone, Copy)]
struct MenuStyleGlobal(MenuStyle);
impl gpui::Global for MenuStyleGlobal {}

thread_local! {
    static ACTIVE_MENU: RefCell<MenuStyle> = RefCell::new(MenuStyle::default());
}

fn menu_style() -> MenuStyle {
    ACTIVE_MENU.with(|m| *m.borrow())
}

fn set_active_menu(m: MenuStyle) {
    ACTIVE_MENU.with(|c| *c.borrow_mut() = m);
}

/// Apply a menu style everywhere: render copy, persist, global, repaint.
fn apply_menu_style(m: MenuStyle, cx: &mut App) {
    set_active_menu(m);
    save_menu_style(&m);
    cx.set_global(MenuStyleGlobal(m));
    cx.refresh_windows();
}

// ----- app icon background (the Dock icon) -----------------------------------

/// The default app icon (the exact bundle icon: blue folder on a light rounded
/// square with the standard macOS margin). We recolor only its background so a
/// customized icon keeps the identical size, shape, margin, and logo.
const ICON_BASE_PNG: &[u8] = include_bytes!("../icon_base.png");

/// How the app icon's background is drawn behind the logo.
#[derive(Clone, PartialEq)]
enum IconBg {
    /// Keep the built-in bundle icon (light) as-is.
    Default,
    /// Recolor the background to a solid color.
    Color(u32),
    /// Fill the background with a user-supplied image (copied to the config dir).
    Image(PathBuf),
}

thread_local! {
    static ACTIVE_ICON_BG: RefCell<IconBg> = const { RefCell::new(IconBg::Default) };
    /// Cached decoded preview icon: (config it was built for, decoded image).
    static ICON_PREVIEW: RefCell<Option<(IconBg, Option<Arc<RenderImage>>)>> =
        const { RefCell::new(None) };
}

fn icon_bg() -> IconBg {
    ACTIVE_ICON_BG.with(|b| b.borrow().clone())
}

fn set_active_icon_bg(b: IconBg) {
    ACTIVE_ICON_BG.with(|c| *c.borrow_mut() = b);
}

/// The decoded icon for the settings preview (recolored base, cached per config).
fn preview_icon_render() -> Option<Arc<RenderImage>> {
    let bg = icon_bg();
    ICON_PREVIEW.with(|c| {
        {
            let cache = c.borrow();
            if let Some((cached, render)) = cache.as_ref() {
                if *cached == bg {
                    return render.clone();
                }
            }
        }
        let render = match &bg {
            IconBg::Default => decode_icon(ICON_BASE_PNG),
            _ => compose_icon_png(&bg).and_then(|png| decode_icon(&png)),
        };
        *c.borrow_mut() = Some((bg, render.clone()));
        render
    })
}

/// Apply the icon background: update the render copy, persist, redraw the Dock
/// icon, and repaint (so the Settings preview updates).
fn apply_icon_bg(bg: IconBg, cx: &mut App) {
    set_active_icon_bg(bg.clone());
    save_icon_bg(&bg);
    refresh_dock_icon(&bg);
    cx.refresh_windows();
}

/// Recolor the base icon per `bg` and hand it to the Dock. `Default` leaves the
/// bundle's own icon untouched.
fn refresh_dock_icon(bg: &IconBg) {
    if matches!(bg, IconBg::Default) {
        return; // keep the built-in AppIcon.icns
    }
    if let Some(png) = compose_icon_png(bg) {
        set_dock_icon(&png);
    }
}

/// Whether a base-icon pixel belongs to the (light, low-saturation) background
/// rather than the (saturated blue) folder logo.
fn is_icon_background(r: u8, g: u8, b: u8) -> bool {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    mx >= 200 && (mx - mn) <= 26
}

/// Build the customized icon PNG by recoloring ONLY the base icon's background,
/// preserving its exact shape, margin, corner radius, logo, and shading.
fn compose_icon_png(bg: &IconBg) -> Option<Vec<u8>> {
    use image::imageops;
    let mut base = image::load_from_memory(ICON_BASE_PNG).ok()?.to_rgba8();
    let (w, h) = base.dimensions();

    // For an image background, cover-fit it to the icon so each background pixel
    // has a source color.
    let bg_img = match bg {
        IconBg::Image(path) => {
            let img = image::open(path).ok()?.to_rgba8();
            let (iw, ih) = img.dimensions();
            let scale = (w as f32 / iw as f32).max(h as f32 / ih as f32);
            let (nw, nh) = ((iw as f32 * scale) as u32, (ih as f32 * scale) as u32);
            Some(imageops::resize(&img, nw.max(1), nh.max(1), imageops::FilterType::Lanczos3))
        }
        _ => None,
    };
    let color = match bg {
        IconBg::Color(c) => Some([(c >> 16) as u8, (c >> 8) as u8, *c as u8]),
        _ => None,
    };

    for (x, y, px) in base.enumerate_pixels_mut() {
        let [r, g, b, a] = px.0;
        if a == 0 || !is_icon_background(r, g, b) {
            continue; // transparent margin or the logo → leave untouched
        }
        if let Some(img) = &bg_img {
            let (bw, bh) = img.dimensions();
            let sp = img.get_pixel(x.min(bw - 1), y.min(bh - 1)).0;
            px.0 = [sp[0], sp[1], sp[2], a];
        } else if let Some([cr, cg, cb]) = color {
            // Preserve the base's subtle gradient by scaling the color by this
            // pixel's brightness (near-white → full color, slightly darker →
            // slightly darker color).
            let lum = r.max(g).max(b) as f32 / 255.0;
            px.0 = [
                (cr as f32 * lum) as u8,
                (cg as f32 * lum) as u8,
                (cb as f32 * lum) as u8,
                a,
            ];
        }
    }

    let mut out = Vec::new();
    base.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Hand a PNG to the running app as its Dock / cmd-tab icon.
fn set_dock_icon(png: &[u8]) {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let data = objc2_foundation::NSData::with_bytes(png);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

/// The pack image overriding the icon for `key` (a file extension, FOLDER_KEY,
/// or FILE_KEY), if the active pack provides one.
fn pack_icon_path(key: &str) -> Option<PathBuf> {
    let pack = icon_pack()?;
    let name = if key == FOLDER_KEY {
        "folder"
    } else if key == FILE_KEY {
        "file"
    } else if let Some(rest) = key.strip_prefix("fav:") {
        // Favorite/location icons (e.g. "fav:documents") map to <name>.png so a
        // pack can override sidebar icons just like file/folder icons.
        rest
    } else {
        key
    };
    for ext in ["png", "jpg", "jpeg", "gif", "tiff", "bmp", "webp"] {
        let p = pack.join(format!("{name}.{ext}"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Decode an image file from disk into a 128px GPUI icon (for pack icons).
fn decode_image_file(path: &Path) -> Option<Arc<RenderImage>> {
    let bytes = fs::read(path).ok()?;
    decode_icon(&bytes)
}

fn clear_icon_cache() {
    ICON_CACHE.with(|c| c.borrow_mut().clear());
}

// ----- keybindings -----------------------------------------------------------

/// Every rebindable action.
#[derive(Clone, Copy, PartialEq)]
enum KeyAction {
    CommandPalette,
    NewTab,
    CloseTab,
    Find,
    SelectAll,
    NewFile,
    NewFolder,
    Rename,
    CopyPath,
    Duplicate,
    MakeAlias,
    Compress,
    MoveToTrash,
    RevealInFinder,
    Open,
    // Command-palette (Cmd+P) text editing. These act only while the palette is
    // open, so they're excluded from the normal (global) key dispatch.
    PaletteCursorStart,
    PaletteCursorEnd,
    PaletteSelectAll,
    PaletteDeleteToStart,
    PaletteHistoryPrev,
    PaletteHistoryNext,
}

impl KeyAction {
    const ALL: &'static [KeyAction] = &[
        KeyAction::CommandPalette,
        KeyAction::NewTab,
        KeyAction::CloseTab,
        KeyAction::Find,
        KeyAction::SelectAll,
        KeyAction::NewFile,
        KeyAction::NewFolder,
        KeyAction::Rename,
        KeyAction::CopyPath,
        KeyAction::Duplicate,
        KeyAction::MakeAlias,
        KeyAction::Compress,
        KeyAction::MoveToTrash,
        KeyAction::RevealInFinder,
        KeyAction::Open,
        KeyAction::PaletteCursorStart,
        KeyAction::PaletteCursorEnd,
        KeyAction::PaletteSelectAll,
        KeyAction::PaletteDeleteToStart,
        KeyAction::PaletteHistoryPrev,
        KeyAction::PaletteHistoryNext,
    ];

    /// Palette actions apply only inside the Cmd+P palette (skipped by the
    /// normal global key dispatch).
    fn is_palette(self) -> bool {
        matches!(
            self,
            KeyAction::PaletteCursorStart
                | KeyAction::PaletteCursorEnd
                | KeyAction::PaletteSelectAll
                | KeyAction::PaletteDeleteToStart
                | KeyAction::PaletteHistoryPrev
                | KeyAction::PaletteHistoryNext
        )
    }

    fn id(self) -> usize {
        Self::ALL.iter().position(|a| *a == self).unwrap()
    }

    /// Stable key used for persistence.
    fn key(self) -> &'static str {
        match self {
            KeyAction::CommandPalette => "command_palette",
            KeyAction::NewTab => "new_tab",
            KeyAction::CloseTab => "close_tab",
            KeyAction::Find => "find",
            KeyAction::SelectAll => "select_all",
            KeyAction::NewFile => "new_file",
            KeyAction::NewFolder => "new_folder",
            KeyAction::Rename => "rename",
            KeyAction::CopyPath => "copy_path",
            KeyAction::Duplicate => "duplicate",
            KeyAction::MakeAlias => "make_alias",
            KeyAction::Compress => "compress",
            KeyAction::MoveToTrash => "move_to_trash",
            KeyAction::RevealInFinder => "reveal_in_finder",
            KeyAction::Open => "open",
            KeyAction::PaletteCursorStart => "palette_cursor_start",
            KeyAction::PaletteCursorEnd => "palette_cursor_end",
            KeyAction::PaletteSelectAll => "palette_select_all",
            KeyAction::PaletteDeleteToStart => "palette_delete_to_start",
            KeyAction::PaletteHistoryPrev => "palette_history_prev",
            KeyAction::PaletteHistoryNext => "palette_history_next",
        }
    }

    /// Human label shown in Settings.
    fn label(self) -> &'static str {
        match self {
            KeyAction::CommandPalette => "Command palette",
            KeyAction::NewTab => "New tab",
            KeyAction::CloseTab => "Close tab",
            KeyAction::Find => "Filter current folder",
            KeyAction::SelectAll => "Select all",
            KeyAction::NewFile => "New file",
            KeyAction::NewFolder => "New folder",
            KeyAction::Rename => "Rename",
            KeyAction::CopyPath => "Copy path",
            KeyAction::Duplicate => "Duplicate",
            KeyAction::MakeAlias => "Make alias",
            KeyAction::Compress => "Compress",
            KeyAction::MoveToTrash => "Move to Trash",
            KeyAction::RevealInFinder => "Reveal in Finder",
            KeyAction::Open => "Open",
            KeyAction::PaletteCursorStart => "Palette: cursor to start",
            KeyAction::PaletteCursorEnd => "Palette: cursor to end",
            KeyAction::PaletteSelectAll => "Palette: select all",
            KeyAction::PaletteDeleteToStart => "Palette: delete to start",
            KeyAction::PaletteHistoryPrev => "Palette: previous history",
            KeyAction::PaletteHistoryNext => "Palette: next history",
        }
    }

    /// Default keystroke (some actions are unbound by default).
    fn default_binding(self) -> Option<&'static str> {
        match self {
            KeyAction::CommandPalette => Some("cmd-p"),
            KeyAction::NewTab => Some("cmd-t"),
            KeyAction::CloseTab => Some("cmd-w"),
            KeyAction::Find => Some("/"),
            KeyAction::SelectAll => Some("cmd-a"),
            KeyAction::PaletteCursorStart => Some("cmd-left"),
            KeyAction::PaletteCursorEnd => Some("cmd-right"),
            KeyAction::PaletteSelectAll => Some("cmd-a"),
            KeyAction::PaletteDeleteToStart => Some("ctrl-u"),
            KeyAction::PaletteHistoryPrev => Some("up"),
            KeyAction::PaletteHistoryNext => Some("down"),
            _ => None,
        }
    }
}

/// The active key bindings (one optional keystroke per action).
#[derive(Clone)]
struct Keymap {
    binds: Vec<Option<String>>,
}

impl Keymap {
    fn defaults() -> Self {
        Keymap {
            binds: KeyAction::ALL
                .iter()
                .map(|a| a.default_binding().map(String::from))
                .collect(),
        }
    }
    fn get(&self, a: KeyAction) -> Option<&str> {
        self.binds[a.id()].as_deref()
    }
    fn set(&mut self, a: KeyAction, b: Option<String>) {
        self.binds[a.id()] = b;
    }
    /// The action bound to keystroke `ks`, if any.
    fn action_for(&self, ks: &str) -> Option<KeyAction> {
        KeyAction::ALL.iter().copied().find(|a| self.get(*a) == Some(ks))
    }
}

#[derive(Clone)]
struct KeymapGlobal(Keymap);
impl gpui::Global for KeymapGlobal {}

thread_local! {
    static ACTIVE_KEYMAP: RefCell<Keymap> = RefCell::new(Keymap::defaults());
}

fn keymap() -> Keymap {
    ACTIVE_KEYMAP.with(|k| k.borrow().clone())
}

fn set_active_keymap(k: Keymap) {
    ACTIVE_KEYMAP.with(|c| *c.borrow_mut() = k);
}

fn apply_keymap(k: Keymap, cx: &mut App) {
    set_active_keymap(k.clone());
    save_keymap(&k);
    cx.set_global(KeymapGlobal(k));
    cx.refresh_windows();
}

/// Whether `c` counts as part of a "word" for Option+Arrow navigation.
/// Only alphanumerics are word characters, so separators like `_`, `-`, `.`,
/// `/` and spaces are boundaries — e.g. in "helix_vault" a word jump lands on
/// "vault".
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Char index of the word boundary to the LEFT of `cursor` in `s`: skip any
/// separators immediately left of the cursor, then skip the word before them.
fn prev_word_boundary(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor.min(chars.len());
    while i > 0 && !is_word_char(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// Char index of the word boundary to the RIGHT of `cursor` in `s`: skip any
/// separators at the cursor, then skip the following word.
fn next_word_boundary(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = cursor.min(len);
    while i < len && !is_word_char(chars[i]) {
        i += 1;
    }
    while i < len && is_word_char(chars[i]) {
        i += 1;
    }
    i
}

/// Byte offset of char index `i` in `s` (or `s.len()` if past the end).
fn char_byte(s: &str, i: usize) -> usize {
    s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
}

/// Canonical string for a keystroke, e.g. "cmd-shift-p" or "/".
fn canon_keystroke(ks: &gpui::Keystroke) -> String {
    let m = &ks.modifiers;
    let mut s = String::new();
    if m.platform {
        s.push_str("cmd-");
    }
    if m.control {
        s.push_str("ctrl-");
    }
    if m.alt {
        s.push_str("alt-");
    }
    if m.shift {
        s.push_str("shift-");
    }
    s.push_str(&ks.key);
    s
}

/// Which theme color a hex field edits.
#[derive(Clone, Copy, PartialEq)]
enum ColorTarget {
    Bg,
    Text,
    Hover,
    MenuBg,
    MenuText,
}

/// The Settings window: a tabbed customization surface.
struct Settings {
    tab: usize,
    /// The section within the active tab the rail last jumped to (highlighted
    /// in the nested sidebar list).
    section: usize,
    /// Scrolls the content pane so a rail sub-item can jump to its section.
    content_scroll: ScrollHandle,
    focus: FocusHandle,
    /// The action whose keystroke is currently being recorded, if any.
    recording: Option<KeyAction>,
    /// In-progress hex color entry: (which color, typed hex digits).
    color_edit: Option<(ColorTarget, String)>,
    /// In-progress typed entry for the search-window opacity (the digits shown
    /// while the value field is focused). `None` when not editing.
    opacity_edit: Option<String>,
}

/// Section names shown as nested rail items for a settings tab (and the order
/// they appear in the content pane, after the page header at child index 0).
fn tab_sections(tab: usize) -> &'static [&'static str] {
    match tab {
        0 => &[
            "Explorer",
            "Inspector",
            "Command Palette",
            "Sidebar",
            "Connections",
            "Script Actions",
            "Software Update",
            "Developer",
        ],
        2 => &["Theme", "Colors", "Menu", "App Icon", "Icon Pack"],
        _ => &[],
    }
}

impl Settings {
    fn new(cx: &mut Context<Self>) -> Self {
        // Repaint when the shared update-check state changes (the check runs
        // async; the main window may also drive it).
        cx.observe_global::<UpdateCheckGlobal>(|_, cx| cx.notify()).detach();
        Settings {
            tab: 0,
            section: 0,
            content_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            recording: None,
            color_edit: None,
            opacity_edit: None,
        }
    }

    /// Kick off a GitHub check from the Settings "Updates" row.
    fn start_update_check(&self, cx: &mut Context<Self>) {
        cx.set_global(UpdateCheckGlobal(UpdateCheck::Checking));
        cx.spawn(async move |_, cx| {
            let tag = cx.background_spawn(async move { fetch_latest_tag() }).await;
            let next = match tag {
                None => UpdateCheck::Failed,
                Some(tag) => {
                    if parse_version(&tag) > parse_version(env!("CARGO_PKG_VERSION")) {
                        UpdateCheck::Available(tag)
                    } else {
                        UpdateCheck::UpToDate
                    }
                }
            };
            let _ = cx.update(|cx| cx.set_global(UpdateCheckGlobal(next)));
        })
        .detach();
    }

    /// Route key events: opacity/hex entry first, then keybind recording.
    fn handle_settings_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.opacity_edit.is_some() {
            self.handle_opacity_key(ev, cx);
        } else if self.color_edit.is_some() {
            self.handle_hex_key(ev, cx);
        } else if self.recording.is_some() {
            self.handle_keybind_key(ev, cx);
        }
    }

    /// Capture typed digits for the search-window opacity field; each edit
    /// applies live (clamped 40–100), Enter/Esc finish. Typing "30" sets 30%.
    fn handle_opacity_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let apply = |s: &str, cx: &mut Context<Self>| {
            if let Ok(n) = s.parse::<u8>() {
                let mut np = prefs();
                np.palette_opacity = n.clamp(20, 100);
                apply_prefs(np, cx);
            }
        };
        match ev.keystroke.key.as_str() {
            "escape" | "enter" | "tab" => {
                // Commit whatever's typed (clamped), then leave edit mode.
                if let Some(s) = self.opacity_edit.take() {
                    if !s.is_empty() {
                        apply(&s, cx);
                    }
                }
                cx.notify();
            }
            "backspace" => {
                if let Some(s) = self.opacity_edit.as_mut() {
                    s.pop();
                }
                if let Some(s) = self.opacity_edit.clone() {
                    if !s.is_empty() {
                        apply(&s, cx);
                    }
                }
                cx.notify();
            }
            _ => {
                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                    if let Some(s) = self.opacity_edit.as_mut() {
                        for c in ch.chars() {
                            // Up to 3 digits ("100"); ignore anything else.
                            if c.is_ascii_digit() && s.len() < 3 {
                                s.push(c);
                            }
                        }
                    }
                    // Apply live as they type so the preview updates.
                    if let Some(s) = self.opacity_edit.clone() {
                        if !s.is_empty() {
                            apply(&s, cx);
                        }
                    }
                    cx.notify();
                }
            }
        }
    }

    /// Capture typed hex digits for a color field; Enter applies, Esc cancels.
    fn handle_hex_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some((target, _)) = self.color_edit.clone() else {
            return;
        };
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.color_edit = None;
                cx.notify();
            }
            "backspace" => {
                if let Some((_, s)) = self.color_edit.as_mut() {
                    s.pop();
                }
                cx.notify();
            }
            "enter" => {
                if let Some((_, s)) = &self.color_edit {
                    if let Ok(c) = u32::from_str_radix(s, 16) {
                        match target {
                            ColorTarget::Bg => {
                                let mut nt = theme();
                                nt.bg = c;
                                apply_theme(nt, cx);
                            }
                            ColorTarget::Text => {
                                let mut nt = theme();
                                nt.text = c;
                                apply_theme(nt, cx);
                            }
                            ColorTarget::Hover => {
                                let mut nt = theme();
                                nt.hover = c;
                                apply_theme(nt, cx);
                            }
                            ColorTarget::MenuBg => {
                                let mut nm = menu_style();
                                nm.bg = c;
                                nm.custom = true;
                                apply_menu_style(nm, cx);
                            }
                            ColorTarget::MenuText => {
                                let mut nm = menu_style();
                                nm.text = c;
                                nm.custom = true;
                                apply_menu_style(nm, cx);
                            }
                        }
                    }
                }
                self.color_edit = None;
                cx.notify();
            }
            _ => {
                if let Some(ch) = ev.keystroke.key_char.as_ref() {
                    // Accept up to 6 hex digits.
                    if let Some((_, s)) = self.color_edit.as_mut() {
                        for c in ch.chars() {
                            if c.is_ascii_hexdigit() && s.len() < 6 {
                                s.push(c.to_ascii_lowercase());
                            }
                        }
                    }
                    cx.notify();
                }
            }
        }
    }

    /// Capture the next keystroke for the action being rebound.
    fn handle_keybind_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(action) = self.recording else {
            return;
        };
        let ks = &ev.keystroke;
        let key = ks.key.as_str();
        match key {
            "escape" => {}
            "backspace" | "delete" => {
                let mut km = keymap();
                km.set(action, None);
                apply_keymap(km, cx);
            }
            // Ignore lone modifier presses; wait for a real key.
            "cmd" | "ctrl" | "alt" | "shift" | "function" => return,
            _ => {
                let mut km = keymap();
                km.set(action, Some(canon_keystroke(ks)));
                apply_keymap(km, cx);
            }
        }
        self.recording = None;
        cx.notify();
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let tabs = [("General", "\u{2699}"), ("Keybinds", "\u{2318}"), ("Customization", "\u{25d0}")];

        // Left tab rail.
        let mut tab_items: Vec<AnyElement> = Vec::new();
        for (i, (name, glyph)) in tabs.iter().enumerate() {
            let active = i == self.tab;
            tab_items.push(
                div()
                    .id(("tab", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .mx_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if active { rgb(t.text) } else { rgb(t.text_muted) })
                    .when(active, |s| s.bg(rgb(t.surface)))
                    .when(!active, |s| s.hover(|s| s.bg(rgb(t.hover))))
                    .child(
                        div()
                            .w(px(16.0))
                            .flex()
                            .justify_center()
                            .text_color(if active { rgb(t.accent) } else { rgb(t.text_dim) })
                            .child(glyph.to_string()),
                    )
                    .child(name.to_string())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.tab = i;
                        this.section = 0;
                        this.content_scroll.set_offset(point(px(0.0), px(0.0)));
                        cx.notify();
                    }))
                    .into_any_element(),
            );

            // Nested section links under the active tab — click to jump.
            if i == self.tab {
                for (si, sname) in tab_sections(i).iter().enumerate() {
                    let active_sec = si == self.section;
                    tab_items.push(
                        div()
                            .id(("sec", i * 100 + si))
                            .flex()
                            .items_center()
                            .py_1()
                            .mx_2()
                            .pl(px(34.0))
                            .pr_2()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(if active_sec { rgb(t.text) } else { rgb(t.text_muted) })
                            .when(active_sec, |s| s.bg(rgb(t.hover)))
                            .when(!active_sec, |s| s.hover(|s| s.bg(rgb(t.hover))))
                            .child(sname.to_string())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.section = si;
                                this.content_scroll.scroll_to_top_of_item(si + 1);
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        let rail = div()
            .flex_none()
            .w(px(184.0))
            .h_full()
            .pt_3()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(t.sidebar))
            .border_r_1()
            .border_color(rgb(t.border))
            .child(
                div()
                    .px_4()
                    .pb_2()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(t.text))
                    .child("Settings"),
            )
            .children(tab_items);

        let children = match self.tab {
            0 => self.render_general(cx),
            1 => self.render_keybinds(cx),
            _ => self.render_customization(cx),
        };

        div()
            .flex()
            .size_full()
            .bg(rgb(t.bg))
            .text_sm()
            .text_color(rgb(t.text))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                this.handle_settings_key(ev, cx);
            }))
            .child(rail)
            .child(
                // Relative wrapper so the scrollbar thumb can overlay the
                // scrolling content at the right edge.
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        // The section blocks are direct children so a rail link
                        // can scroll_to_top_of_item(section_index + 1) to jump.
                        div()
                            .id("settings-content")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.content_scroll)
                            .flex()
                            .flex_col()
                            .gap_4()
                            .p_5()
                            .children(children),
                    )
                    .children(static_scrollbar_thumb(&self.content_scroll)),
            )
    }
}

impl Settings {
    /// The search-window-opacity row: title/description on the left, and on the
    /// right a −/+ stepper around a **typable** value. Click the number to edit
    /// it and just type (e.g. "30" → 30%); Enter/Esc/click-away commit.
    fn render_opacity_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme();
        let pct = prefs().palette_opacity;
        let editing = self.opacity_edit.clone();

        let step_btn = |bid: &'static str, glyph: &'static str, delta: i16| {
            div()
                .id(bid)
                .flex_none()
                .w(px(24.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .bg(rgb(t.surface))
                .text_color(rgb(t.text))
                .hover(|s| s.bg(rgb(t.hover)))
                .child(glyph)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    // Stepping also leaves the typing field, committing nothing extra.
                    this.opacity_edit = None;
                    let mut np = prefs();
                    let next = (np.palette_opacity as i16 + delta).clamp(20, 100);
                    np.palette_opacity = next as u8;
                    apply_prefs(np, cx);
                    cx.notify();
                }))
        };

        // The value: an editable field when focused, else a clickable label.
        let value: AnyElement = match &editing {
            Some(s) => div()
                .id("opacity-value")
                .flex_none()
                .min_w(px(52.0))
                .px_2()
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(rgb(t.bg))
                .border_1()
                .border_color(rgb(t.accent))
                .text_color(rgb(t.text))
                .child(s.clone())
                // A caret so it reads as an active text field.
                .child(div().w(px(1.5)).h(px(13.0)).ml(px(1.0)).bg(rgb(t.text)))
                .child("%")
                .into_any_element(),
            None => div()
                .id("opacity-value")
                .flex_none()
                .min_w(px(52.0))
                .px_2()
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_text()
                .text_color(rgb(t.text))
                .hover(|s| s.bg(rgb(t.hover)))
                .child(format!("{pct}%"))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    // Start empty so the user can just type the number they want.
                    this.opacity_edit = Some(String::new());
                    window.focus(&this.focus);
                    cx.notify();
                }))
                .into_any_element(),
        };

        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(rgb(t.text)).child("Search window opacity"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(t.text_muted))
                            .child("How opaque the Cmd+P search window is. Click the number to type a value (20–100). Lower lets the explorer show through."),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(step_btn("opacity-dec", "\u{2212}", -1))
                    .child(value)
                    .child(step_btn("opacity-inc", "+", 1)),
            )
            .into_any_element()
    }

    /// The General tab as an ordered list of blocks: page header, then one card
    /// per section (matching `tab_sections(0)`), then the reset button. Returned
    /// as separate children so the rail can scroll to any section.
    fn render_general(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = prefs();
        let t = theme();
        let script_count = discover_script_actions().len();

        let scripts_folder_row = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_color(rgb(t.text)).child("Scripts folder"))
                    .child(div().text_xs().text_color(rgb(t.text_muted)).child(format!(
                        "{} script{} loaded — the README there explains the format.",
                        script_count,
                        if script_count == 1 { "" } else { "s" }
                    ))),
            )
            .child(
                div()
                    .id("open-scripts")
                    .flex_none()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(t.surface))
                    .text_color(rgb(t.text))
                    .hover(|s| s.bg(rgb(t.hover)))
                    .active(|s| s.bg(rgb(t.selected)))
                    .child("Open Scripts Folder")
                    .on_click(cx.listener(|_, _: &ClickEvent, _, _| {
                        if let Some(dir) = ensure_scripts_dir() {
                            let _ = Command::new("open").arg(&dir).spawn();
                        }
                    })),
            )
            .into_any_element();

        // Connections (SFTP) section rows: the auth-mode choice, then the saved
        // servers with a Remove button each, then how to add more.
        let mut connection_rows: Vec<AnyElement> = Vec::new();
        connection_rows.push(
            toggle_row(
                "tg-ssh-system",
                "Use my ~/.ssh configuration",
                "On: connect using your existing SSH keys, ~/.ssh/config aliases, and \
                 known_hosts — nothing is stored in the app. Off: use the private-key \
                 path you set per server.",
                p.ssh_use_system,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.ssh_use_system = !np.ssh_use_system;
                    np.ssh_configured = true;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        let servers = sftp_servers();
        if servers.is_empty() {
            connection_rows.push(
                div()
                    .text_xs()
                    .text_color(rgb(t.text_dim))
                    .child(
                        "No saved servers yet. Add one from the sidebar: Connect to \
                         Server\u{2026} then enter sftp://user@host (or an ~/.ssh alias).",
                    )
                    .into_any_element(),
            );
        } else {
            for (i, s) in servers.iter().enumerate() {
                let srv = s.clone();
                connection_rows.push(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_color(rgb(t.text)).child(s.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(t.text_muted))
                                        .child(format!("sftp://{}", s.display())),
                                ),
                        )
                        .child(
                            div()
                                .id(("sftp-remove", i))
                                .flex_none()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgb(t.hover))
                                .text_color(rgb(t.text))
                                .hover(|s| s.border_color(rgb(0xd9544f)).text_color(rgb(0xd9544f)))
                                .border_1()
                                .border_color(rgb(t.border))
                                .child("Remove")
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    if srv.use_password {
                                        keychain_delete_password(&srv);
                                    }
                                    let list: Vec<SftpServer> = sftp_servers()
                                        .into_iter()
                                        .filter(|x| x.display() != srv.display())
                                        .collect();
                                    apply_sftp_servers(list, cx);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }

        vec![
            settings_header(
                "General",
                "How the explorer behaves and what the side panels show.",
            )
            .into_any_element(),
            settings_section(
                "Explorer",
                Some("Navigation and the file list."),
                vec![
                    toggle_row(
                        "tg-show-parent",
                        "Show \u{201c}..\u{201d} (up one level)",
                        "Show the leading \u{201c}..\u{201d} entry in list view that navigates to the parent folder.",
                        p.show_parent,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.show_parent = !np.show_parent;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-filter-button",
                        "Filter button",
                        "Show the always-on \u{201c}Filter\u{201d} button in the bottom-right corner. Pressing / still opens the filter either way.",
                        p.show_filter_button,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.show_filter_button = !np.show_filter_button;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-terminal",
                        "Terminal mode",
                        "Show a command input at the bottom to move through the explorer like a terminal (with path/command autocomplete).",
                        p.terminal,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.terminal = !np.terminal;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-term-history",
                        "Terminal history",
                        "Show a scrollback of what you've typed and command output above the input (otherwise it's just the input line).",
                        p.term_history,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.term_history = !np.term_history;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ],
            ),
            settings_section(
                "Inspector",
                Some("The side panel shown when you click a file."),
                vec![
                    toggle_row(
                        "tg-preview",
                        "Preview",
                        "Click a file once to preview it (images, PDFs, documents, \u{2026}) in the inspector panel.",
                        p.preview,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.preview = !np.preview;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-preview-pages",
                        "Preview page controls",
                        "Show \u{2039} \u{203a} arrows under multi-page PDF previews to flip through the pages without opening the file.",
                        p.preview_pages,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.preview_pages = !np.preview_pages;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-info",
                        "Information",
                        "Click a file once to see its details (kind, size, dates, dimensions, color space, and more) in the inspector panel.",
                        p.info,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.info = !np.info;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ],
            ),
            settings_section(
                "Command Palette",
                Some("The Cmd+P search and actions."),
                vec![
                    toggle_row(
                        "tg-palette-history",
                        "Command palette history",
                        "Give Cmd+P its own query history: press Up/Down in the palette to cycle through your previous searches.",
                        p.palette_history,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.palette_history = !np.palette_history;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    self.render_opacity_row(cx),
                    // Live sample so the opacity change is visible immediately.
                    palette_opacity_preview(),
                ],
            ),
            settings_section(
                "Sidebar",
                None,
                vec![
                    toggle_row(
                        "tg-groups",
                        "Sidebar groups",
                        "Create custom groups of files/folders in the sidebar. Right-click the sidebar to make a group, then right-click any item to add it.",
                        p.groups_enabled,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.groups_enabled = !np.groups_enabled;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    toggle_row(
                        "tg-waterfall",
                        "Waterfall folder tree",
                        "Show an expandable tree of the current folder's subfolders at the bottom of the sidebar. Click a triangle to drill in, click a name to open it in the pane.",
                        p.waterfall,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.waterfall = !np.waterfall;
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    stepper_row(
                        "st-recents",
                        "Recent folders",
                        "How many recently-visited folders to show in the sidebar (0 hides the Recents section).",
                        if p.recent_limit == 0 {
                            "Off".to_string()
                        } else {
                            p.recent_limit.to_string()
                        },
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.recent_limit = np.recent_limit.saturating_sub(1);
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.recent_limit = (np.recent_limit + 1).min(RECENTS_CAP);
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ],
            ),
            settings_section(
                "Connections",
                Some("SFTP servers — browse remote hosts over SSH from the sidebar."),
                connection_rows,
            ),
            settings_section(
                "Script Actions",
                Some("Run your own shell scripts from the right-click menu."),
                vec![
                    toggle_row(
                        "tg-script-actions",
                        "Run script actions",
                        "Show your own shell scripts (from the Scripts folder) in the right-click menu for matching files, and run them with the selected paths. Only scripts you put there run \u{2014} treat it like your ~/bin.",
                        p.script_actions,
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut np = prefs();
                            np.script_actions = !np.script_actions;
                            if np.script_actions {
                                ensure_scripts_dir();
                            }
                            apply_prefs(np, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    scripts_folder_row,
                ],
            ),
            settings_section(
                "Software Update",
                None,
                vec![self.render_update_row(cx)],
            ),
            settings_section(
                "Developer",
                Some("Diagnostics for checking performance."),
                vec![toggle_row(
                    "tg-show-fps",
                    "Frame rate meter",
                    "Show a live FPS readout in the top-right of the explorer window. Forces continuous redraws to measure them, so it uses some CPU while on \u{2014} meant for checking smoothness, not everyday use.",
                    p.show_fps,
                    cx.listener(|_, _: &ClickEvent, _, cx| {
                        let mut np = prefs();
                        np.show_fps = !np.show_fps;
                        apply_prefs(np, cx);
                        cx.notify();
                    }),
                )
                .into_any_element()],
            ),
            div()
                .pt_1()
                .child(reset_button(
                    "reset-general",
                    "Reset General to Default",
                    cx.listener(|_, _: &ClickEvent, _, cx| {
                        apply_prefs(Prefs::default(), cx);
                        cx.notify();
                    }),
                ))
                .into_any_element(),
        ]
    }

    /// The Software-Update row: current version plus a Check / Install &
    /// Relaunch button, driven by the shared UpdateCheck state.
    fn render_update_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme();
        let state = cx.global::<UpdateCheckGlobal>().0.clone();
        let status: Option<String> = match &state {
            UpdateCheck::Idle => None,
            UpdateCheck::Checking => Some("Checking\u{2026}".into()),
            UpdateCheck::UpToDate => Some("You\u{2019}re up to date.".into()),
            UpdateCheck::Available(v) => Some(format!("{v} is available.")),
            UpdateCheck::Failed => Some("Couldn\u{2019}t reach GitHub \u{2014} try again.".into()),
            UpdateCheck::Install(v) => Some(format!("Installing {v}\u{2026} the app will relaunch.")),
        };
        let button: Option<&'static str> = match &state {
            UpdateCheck::Idle => Some("Check for Updates"),
            UpdateCheck::UpToDate => Some("Check Again"),
            UpdateCheck::Failed => Some("Try Again"),
            UpdateCheck::Available(_) => Some("Install & Relaunch"),
            UpdateCheck::Checking | UpdateCheck::Install(_) => None,
        };
        let avail = matches!(state, UpdateCheck::Available(_));
        let avail_tag = match &state {
            UpdateCheck::Available(v) => Some(v.clone()),
            _ => None,
        };
        let mut right = div().flex_none().flex().items_center().gap_2();
        if let Some(s) = status {
            right = right.child(
                div()
                    .text_xs()
                    .text_color(rgb(if avail { t.accent } else { t.text_muted }))
                    .child(s),
            );
        }
        if let Some(label) = button {
            right = right.child(
                div()
                    .id("update-check-btn")
                    .flex_none()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if avail { rgb(t.accent) } else { rgb(t.surface) })
                    .text_color(if avail { rgb(0xffffff) } else { rgb(t.text) })
                    .hover(|s| s.bg(rgb(t.hover)))
                    .active(|s| s.bg(rgb(t.selected)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(v) = avail_tag.clone() {
                            cx.set_global(UpdateCheckGlobal(UpdateCheck::Install(v)));
                        } else {
                            this.start_update_check(cx);
                        }
                        cx.notify();
                    })),
            );
        }
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_color(rgb(t.text))
                            .child(format!(
                                "Version {} ({})",
                                env!("CARGO_PKG_VERSION"),
                                // Build stamp (git short SHA) baked in at compile
                                // time so "which build am I actually running?"
                                // is answerable from Settings. "dev" when built
                                // without the stamp.
                                option_env!("SHUFFLE_BUILD_SHA").unwrap_or("dev")
                            )),
                    )
                    .child(div().text_xs().text_color(rgb(t.text_muted)).child(
                        "Check GitHub for a newer release and install it in place \u{2014} the app swaps itself and relaunches.",
                    )),
            )
            .child(right)
            .into_any_element()
    }

    fn render_keybinds(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme();
        let km = keymap();
        let mut rows: Vec<AnyElement> = Vec::new();
        for action in KeyAction::ALL.iter().copied() {
            let recording = self.recording == Some(action);
            let binding = km.get(action);
            // The binding chip: shows the keystroke, "Unbound", or a recording hint.
            let (chip_text, chip_dim) = if recording {
                ("Press keys… (⌫ clear, esc cancel)".to_string(), false)
            } else {
                match binding {
                    Some(b) => (pretty_keystroke(b), false),
                    None => ("Unbound".to_string(), true),
                }
            };
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .py_1()
                    .child(div().text_color(rgb(t.text)).child(action.label()))
                    .child(
                        div()
                            .id(("kb", action.id()))
                            .flex_none()
                            .min_w(px(150.0))
                            .px_2()
                            .py(px(2.0))
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .bg(rgb(t.surface))
                            .border_1()
                            .border_color(if recording { rgb(t.accent) } else { rgb(t.border) })
                            .text_color(if chip_dim { rgb(t.text_dim) } else { rgb(t.text) })
                            .hover(|s| s.border_color(rgb(t.accent)))
                            .child(chip_text)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.recording = Some(action);
                                window.focus(&this.focus);
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        vec![
            settings_header(
                "Keybinds",
                "Click a shortcut, then press the keys. \u{232b} clears it; Esc cancels.",
            )
            .into_any_element(),
            settings_section("Shortcuts", None, rows),
            div()
                .pt_1()
                .child(reset_button(
                    "reset-keybinds",
                    "Reset Shortcuts to Default",
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.recording = None;
                        apply_keymap(Keymap::defaults(), cx);
                        cx.notify();
                    }),
                ))
                .into_any_element(),
        ]
    }

    fn render_customization(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme();

        // Preset palette cards.
        let mut presets: Vec<AnyElement> = Vec::new();
        for (i, (name, preset)) in PRESETS.iter().enumerate() {
            let preset = *preset;
            let selected = preset == t;
            presets.push(
                div()
                    .id(("preset", i))
                    .w(px(150.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected { rgb(t.accent) } else { rgb(t.border) })
                    .bg(rgb(preset.bg))
                    .hover(|s| s.border_color(rgb(t.accent)))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(swatch_dot(preset.sidebar))
                            .child(swatch_dot(preset.surface))
                            .child(swatch_dot(preset.accent))
                            .child(swatch_dot(preset.text)),
                    )
                    .child(div().text_color(rgb(preset.text)).child(name.to_string()))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        apply_theme(preset, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let presets_grid = div()
            .flex()
            .flex_wrap()
            .gap_3()
            .children(presets)
            .into_any_element();

        // A color control: name + description on the left, hex field on the
        // right, and the swatch spectrum below.
        let color_block = |title: &str,
                           desc: &str,
                           target: ColorTarget,
                           current: u32,
                           spectrum: AnyElement,
                           hex: AnyElement| {
            let t = theme();
            let _ = (target, current);
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_color(rgb(t.text)).child(title.to_string()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(t.text_muted))
                                        .child(desc.to_string()),
                                ),
                        )
                        .child(hex),
                )
                .child(spectrum)
                .into_any_element()
        };

        // Menu preview + caption.
        let menu_preview_row = div()
            .flex()
            .items_center()
            .gap_4()
            .child(self.menu_preview())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(t.text_muted))
                    .child("Live preview of the right-click menu."),
            )
            .into_any_element();

        // App-icon preview + upload/reset controls.
        let app_icon_row = div()
            .flex()
            .items_center()
            .gap_4()
            .child(self.app_icon_preview())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(t.text_muted))
                            .child("Pick a color below or upload an image (PNG or JPG; square looks best)."),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("icon-bg-upload")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(rgb(t.surface))
                                    .border_1()
                                    .border_color(rgb(t.border))
                                    .hover(|s| s.border_color(rgb(t.accent)))
                                    .child("Upload Background\u{2026}")
                                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                        let rx = cx.prompt_for_paths(PathPromptOptions {
                                            files: true,
                                            directories: false,
                                            multiple: false,
                                            prompt: Some("Choose Background Image".into()),
                                        });
                                        cx.spawn(async move |_, cx| {
                                            if let Ok(Ok(Some(paths))) = rx.await {
                                                if let Some(p) = paths.into_iter().next() {
                                                    if let Some(dest) = store_icon_bg_image(&p) {
                                                        let _ = cx.update(|cx| {
                                                            apply_icon_bg(IconBg::Image(dest), cx)
                                                        });
                                                    }
                                                }
                                            }
                                        })
                                        .detach();
                                    })),
                            )
                            .child(
                                div()
                                    .id("icon-bg-reset")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(rgb(t.hover))
                                    .hover(|s| s.bg(rgb(t.selected)))
                                    .child("Reset")
                                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                        apply_icon_bg(IconBg::Default, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element();

        // Icon-pack status + choose/reset + note.
        let icon_pack_row = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(rgb(t.text))
                    .child(match icon_pack() {
                        Some(p) => format!("Current pack: {}", path_label(&p)),
                        None => "Using macOS icons".to_string(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("pack-choose")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(t.surface))
                            .border_1()
                            .border_color(rgb(t.border))
                            .hover(|s| s.border_color(rgb(t.accent)))
                            .child("Choose Folder\u{2026}")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                let rx = cx.prompt_for_paths(PathPromptOptions {
                                    files: false,
                                    directories: true,
                                    multiple: false,
                                    prompt: Some("Choose Icon Pack".into()),
                                });
                                cx.spawn(async move |this, cx| {
                                    if let Ok(Ok(Some(paths))) = rx.await {
                                        if let Some(p) = paths.into_iter().next() {
                                            let _ = this
                                                .update(cx, |_, cx| apply_icon_pack(Some(p), cx));
                                        }
                                    }
                                })
                                .detach();
                            })),
                    )
                    .child(
                        div()
                            .id("pack-reset")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(t.hover))
                            .hover(|s| s.bg(rgb(t.selected)))
                            .child("Use macOS icons")
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                apply_icon_pack(None, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(t.text_dim))
                    .child(
                        "A folder of images named folder.png, file.png, and per-extension \
                         (e.g. pdf.png, png.png). PNG with transparency works best.",
                    ),
            )
            .into_any_element();

        vec![
            settings_header(
                "Customization",
                "Make Shuffle yours \u{2014} palette, menus, and app icon.",
            )
            .into_any_element(),
            settings_section(
                "Theme",
                Some("Pick a starting palette, then fine-tune the colors below."),
                vec![presets_grid],
            ),
            settings_section(
                "Colors",
                Some("Core interface colors. Type a hex value or pick from the swatches."),
                vec![
                    color_block(
                        "Background",
                        "The main window and file list.",
                        ColorTarget::Bg,
                        t.bg,
                        self.color_row("bg", t.bg, |t, c| t.bg = c, cx).into_any_element(),
                        self.hex_field(ColorTarget::Bg, t.bg, cx).into_any_element(),
                    ),
                    color_block(
                        "Text",
                        "Primary text throughout the app.",
                        ColorTarget::Text,
                        t.text,
                        self.color_row("text", t.text, |t, c| t.text = c, cx).into_any_element(),
                        self.hex_field(ColorTarget::Text, t.text, cx).into_any_element(),
                    ),
                    color_block(
                        "Mouseover",
                        "The highlight behind hovered rows and buttons.",
                        ColorTarget::Hover,
                        t.hover,
                        self.color_row("hover", t.hover, |t, c| t.hover = c, cx).into_any_element(),
                        self.hex_field(ColorTarget::Hover, t.hover, cx).into_any_element(),
                    ),
                ],
            ),
            settings_section(
                "Menu",
                Some("The right-click / dropdown menu appearance."),
                vec![
                    menu_preview_row,
                    color_block(
                        "Menu color",
                        "The menu background.",
                        ColorTarget::MenuBg,
                        menu_style().bg,
                        self.menu_color_row("menubg", menu_style().bg, |m, c| m.bg = c, cx)
                            .into_any_element(),
                        self.hex_field(ColorTarget::MenuBg, menu_style().bg, cx).into_any_element(),
                    ),
                    color_block(
                        "Letter color",
                        "The menu text.",
                        ColorTarget::MenuText,
                        menu_style().text,
                        self.menu_color_row("menutext", menu_style().text, |m, c| m.text = c, cx)
                            .into_any_element(),
                        self.hex_field(ColorTarget::MenuText, menu_style().text, cx)
                            .into_any_element(),
                    ),
                    stepper_row(
                        "st-menu-opacity",
                        "Opacity",
                        "Menu background opacity (lower is more see-through).",
                        format!("{}%", menu_style().opacity),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut m = menu_style();
                            m.opacity = m.opacity.saturating_sub(10);
                            apply_menu_style(m, cx);
                            cx.notify();
                        }),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut m = menu_style();
                            m.opacity = (m.opacity + 10).min(100);
                            apply_menu_style(m, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                    stepper_row(
                        "st-menu-size",
                        "Text size",
                        "Menu font size in pixels.",
                        format!("{}px", menu_style().font_px as i32),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut m = menu_style();
                            m.font_px = (m.font_px - 1.0).max(9.0);
                            apply_menu_style(m, cx);
                            cx.notify();
                        }),
                        cx.listener(|_, _: &ClickEvent, _, cx| {
                            let mut m = menu_style();
                            m.font_px = (m.font_px + 1.0).min(24.0);
                            apply_menu_style(m, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
                ],
            ),
            settings_section(
                "App Icon",
                Some("The Dock and Finder icon."),
                vec![app_icon_row, self.icon_color_row(cx).into_any_element()],
            ),
            settings_section(
                "Icon Pack",
                Some("Replace macOS file icons with your own image set."),
                vec![icon_pack_row],
            ),
            div()
                .pt_1()
                .child(reset_button(
                    "reset-customization",
                    "Reset Customization to Default",
                    cx.listener(|_, _: &ClickEvent, _, cx| {
                        apply_theme(Theme::default(), cx);
                        apply_menu_style(MenuStyle::default(), cx);
                        apply_icon_bg(IconBg::Default, cx);
                        apply_icon_pack(None, cx);
                        cx.notify();
                    }),
                ))
                .into_any_element(),
        ]
    }

    /// A grid of color swatches; clicking one sets a single theme field via
    /// `set`. `tag` keeps element ids unique across rows that share colors.
    fn color_row(
        &self,
        tag: &'static str,
        current: u32,
        set: fn(&mut Theme, u32),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme();
        let mut swatches: Vec<AnyElement> = Vec::new();
        for c in palette_colors() {
            let selected = c == current;
            swatches.push(
                div()
                    .id((tag, c as usize))
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(c))
                    .border_2()
                    .border_color(if selected { rgb(t.accent) } else { rgb(t.border) })
                    .hover(|s| s.border_color(rgb(t.accent)))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        let mut nt = theme();
                        set(&mut nt, c);
                        apply_theme(nt, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div().flex().flex_wrap().gap_1().children(swatches)
    }

    /// Like [`color_row`] but sets a field on the menu style.
    fn menu_color_row(
        &self,
        tag: &'static str,
        current: u32,
        set: fn(&mut MenuStyle, u32),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme();
        let mut swatches: Vec<AnyElement> = Vec::new();
        for c in palette_colors() {
            let selected = c == current;
            swatches.push(
                div()
                    .id((tag, c as usize))
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(c))
                    .border_2()
                    .border_color(if selected { rgb(t.accent) } else { rgb(t.border) })
                    .hover(|s| s.border_color(rgb(t.accent)))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        let mut nm = menu_style();
                        set(&mut nm, c);
                        nm.custom = true;
                        apply_menu_style(nm, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div().flex().flex_wrap().gap_1().children(swatches)
    }

    /// A live sample pop-up menu showing the current menu style.
    fn menu_preview(&self) -> impl IntoElement {
        let m = menu_style();
        let t = theme();
        let row = |label: &str| {
            div()
                .mx_1()
                .px_3()
                .py_1()
                .rounded_md()
                .child(label.to_string())
        };
        div()
            .min_w(px(200.0))
            .py_1()
            .bg(m.bg_rgba())
            .text_color(rgb(m.text))
            .text_size(px(m.font_px))
            .rounded_md()
            .border_1()
            .border_color(rgb(t.border_strong))
            .shadow_lg()
            .child(row("Open"))
            .child(row("Rename"))
            .child(row("Add to Bookmarks"))
            .child(div().my_1().mx_2().h(px(1.0)).bg(rgb(t.border_strong)))
            .child(row("Move to Trash"))
    }

    /// A live preview of the app icon (the recolored base — matches the Dock).
    fn app_icon_preview(&self) -> impl IntoElement {
        let mut base = div()
            .flex_none()
            .w(px(80.0))
            .h(px(80.0))
            .flex()
            .items_center()
            .justify_center();
        if let Some(r) = preview_icon_render() {
            base = base.child(img(ImageSource::Render(r)).w(px(80.0)).h(px(80.0)));
        }
        base
    }

    /// Color swatches that set the app-icon background color.
    fn icon_color_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let cur = match icon_bg() {
            IconBg::Color(c) => Some(c),
            _ => None,
        };
        let mut swatches: Vec<AnyElement> = Vec::new();
        for c in palette_colors() {
            let selected = cur == Some(c);
            swatches.push(
                div()
                    .id(("iconbg", c as usize))
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(c))
                    .border_2()
                    .border_color(if selected { rgb(t.accent) } else { rgb(t.border) })
                    .hover(|s| s.border_color(rgb(t.accent)))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        apply_icon_bg(IconBg::Color(c), cx);
                    }))
                    .into_any_element(),
            );
        }
        div().flex().flex_wrap().gap_1().children(swatches)
    }

    /// A small "current value + editable hex" control for a theme color.
    fn hex_field(
        &self,
        target: ColorTarget,
        current: u32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme();
        let editing = self.color_edit.as_ref().filter(|(tt, _)| *tt == target);
        let text = match editing {
            Some((_, s)) => format!("#{s}\u{2502}"),
            None => format!("#{current:06x}"),
        };
        div()
            .id(("hex", target as usize))
            .flex_none()
            .px_2()
            .py(px(2.0))
            .rounded_md()
            .cursor_text()
            .bg(rgb(t.surface))
            .border_1()
            .border_color(if editing.is_some() { rgb(t.accent) } else { rgb(t.border) })
            .text_xs()
            .text_color(rgb(t.text))
            .child(text)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.color_edit = Some((target, String::new()));
                window.focus(&this.focus);
                cx.notify();
            }))
    }
}

/// A small color dot used in preset previews.
fn swatch_dot(color: u32) -> impl IntoElement {
    div().w(px(14.0)).h(px(14.0)).rounded_full().bg(rgb(color))
}

/// A label/value line in the Information inspector.
fn info_row(label: &str, value: &str) -> impl IntoElement {
    let t = theme();
    div()
        .flex()
        .justify_between()
        .gap_3()
        .text_xs()
        .child(
            div()
                .flex_none()
                .text_color(rgb(t.text_dim))
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(rgb(t.text))
                .child(value.to_string()),
        )
}

/// Pretty-print a stored keystroke string ("cmd-shift-p") with Mac symbols.
fn pretty_keystroke(s: &str) -> String {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 1 {
        return key_symbol(parts[0]);
    }
    let mut out = String::new();
    for m in &parts[..parts.len() - 1] {
        out.push_str(match *m {
            "cmd" => "⌘",
            "ctrl" => "⌃",
            "alt" => "⌥",
            "shift" => "⇧",
            o => o,
        });
    }
    out.push_str(&key_symbol(parts[parts.len() - 1]));
    out
}

fn key_symbol(k: &str) -> String {
    match k {
        "enter" => "↩".into(),
        "escape" => "⎋".into(),
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "space" => "␣".into(),
        "tab" => "⇥".into(),
        o => o.to_uppercase(),
    }
}

/// A section heading inside Settings.
fn settings_title(text: &str) -> impl IntoElement {
    div()
        .text_color(rgb(theme().text_muted))
        .text_xs()
        .child(text.to_uppercase())
}

/// The large title + one-line description shown at the top of a settings tab.
fn settings_header(title: &str, subtitle: &str) -> impl IntoElement {
    let t = theme();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .pb_1()
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(t.text))
                .child(title.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(t.text_muted))
                .child(subtitle.to_string()),
        )
}

/// A hairline divider used inside settings cards.
fn settings_divider() -> impl IntoElement {
    div().h(px(1.0)).bg(rgb(theme().border))
}

/// A settings "card": a titled, bordered container grouping related rows, each
/// row padded and separated by a hairline. This is the core of the settings
/// look — related controls read as one unit instead of a flat list.
fn settings_section(title: &str, subtitle: Option<&str>, rows: Vec<AnyElement>) -> AnyElement {
    let t = theme();
    let mut header = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .pt_3()
        .pb_3()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(t.text))
                .child(title.to_string()),
        );
    if let Some(s) = subtitle {
        header = header.child(
            div()
                .text_xs()
                .text_color(rgb(t.text_dim))
                .child(s.to_string()),
        );
    }

    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .rounded_lg()
        .border_1()
        .border_color(rgb(t.border))
        .bg(Theme::alpha(t.surface, 0x33))
        .child(header)
        .child(settings_divider());

    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            card = card.child(settings_divider());
        }
        card = card.child(div().px_4().py_3().child(row));
    }
    card.into_any_element()
}

/// A "Reset to Default" button (used once per settings tab).
fn reset_button(
    id: &'static str,
    label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    div()
        .flex()
        .pt_2()
        .child(
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .bg(rgb(t.hover))
                .text_color(rgb(t.text))
                .border_1()
                .border_color(rgb(t.border))
                .hover(|s| s.border_color(rgb(0xd9544f)).text_color(rgb(0xd9544f)))
                .child(label.to_string())
                .on_click(on_click),
        )
}

/// A live sample of the command palette for the opacity setting: a mock
/// explorer backdrop with a scaled-down palette floating over it, using the
/// same background alpha as the real palette. As the opacity stepper changes,
/// this repaints so the user sees exactly how see-through the search window
/// will be over their files.
fn palette_opacity_preview() -> AnyElement {
    let t = theme();
    // Backdrop: a few faux "file rows" (icon chip + name/detail bars) so the
    // transparency is visible — a solid backdrop would hide the effect.
    let faux_row = |icon: u32, w1: f32, w2: f32| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(22.0))
            .child(div().flex_none().w(px(12.0)).h(px(12.0)).rounded_sm().bg(rgb(icon)))
            .child(div().flex_none().w(px(w1)).h(px(7.0)).rounded_full().bg(Theme::alpha(t.text, 0x44)))
            .child(div().flex_none().w(px(w2)).h(px(7.0)).rounded_full().bg(Theme::alpha(t.text, 0x22)))
    };
    // In-flow, fills the fixed-height box (`size_full`). An outer box with only
    // absolutely-positioned children collapses to zero — so the backdrop must be
    // in-flow to reliably paint.
    let backdrop = div()
        .size_full()
        .bg(rgb(t.bg))
        .flex()
        .flex_col()
        .py_2()
        .child(faux_row(t.accent, 90.0, 40.0))
        .child(faux_row(t.accent, 120.0, 30.0))
        .child(faux_row(0xd9844f, 70.0, 55.0))
        .child(faux_row(t.accent, 140.0, 25.0))
        .child(faux_row(0x4faf7a, 60.0, 45.0))
        .child(faux_row(t.accent, 100.0, 35.0))
        .child(faux_row(0xd9544f, 110.0, 30.0));

    // A miniature palette, styled exactly like the real one (same alpha).
    let hint = |k: &'static str| {
        div()
            .px_1()
            .rounded_sm()
            .bg(Theme::alpha(t.text_dim, 0x22))
            .text_color(rgb(t.text_muted))
            .child(k)
    };
    let sample = div()
        .absolute()
        .top(px(14.0))
        .left(px(28.0))
        .right(px(28.0))
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(Theme::alpha(t.surface, palette_alpha()))
        .rounded_lg()
        .border_1()
        .border_color(rgb(t.border_strong))
        .shadow_lg()
        // Input line.
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(t.border_strong))
                .child(div().flex_none().text_color(rgb(t.accent)).child("›"))
                .child(div().text_color(rgb(t.text)).child("Documents")),
        )
        // A selected result + a plain one.
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .h(px(24.0))
                .bg(rgb(t.selected))
                .child(div().flex_none().w(px(12.0)).h(px(12.0)).rounded_sm().bg(rgb(t.accent)))
                .child(div().text_color(rgb(t.text)).child("Documents"))
                .child(div().text_xs().text_color(rgb(t.text_muted)).child("~/Documents")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .h(px(24.0))
                .child(div().flex_none().w(px(12.0)).h(px(12.0)).rounded_sm().bg(rgb(t.accent)))
                .child(div().text_color(rgb(t.text)).child("Downloads"))
                .child(div().text_xs().text_color(rgb(t.text_muted)).child("~/Downloads")),
        )
        // Footer hints.
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(rgb(t.border_strong))
                .text_xs()
                .text_color(rgb(t.text_dim))
                .child(hint("↩"))
                .child("Open")
                .child(hint("esc"))
                .child("Close"),
        );

    div()
        .relative()
        .w_full()
        .h(px(178.0))
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(rgb(t.border))
        // Fixed height + in-flow backdrop; the sample palette floats over it.
        .child(backdrop)
        .child(sample)
        .into_any_element()
}

/// A labelled on/off toggle row used in the General settings tab. `id` must be
/// unique per row, or only the first switch receives clicks.
fn toggle_row(
    id: &'static str,
    title: &str,
    desc: &str,
    on: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    // The pill switch.
    let knob = div()
        .absolute()
        .top(px(2.0))
        .left(if on { px(20.0) } else { px(2.0) })
        .w(px(16.0))
        .h(px(16.0))
        .rounded_full()
        .bg(rgb(0xffffff));
    let switch = div()
        .id(id)
        .flex_none()
        .relative()
        .w(px(38.0))
        .h(px(20.0))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { rgb(t.accent) } else { rgb(t.surface) })
        .child(knob)
        .on_click(on_click);

    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(rgb(t.text)).child(title.to_string()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(t.text_muted))
                        .child(desc.to_string()),
                ),
        )
        .child(switch)
}

/// A settings row with a −/+ stepper and a value label (used for numeric prefs).
fn stepper_row(
    id: &'static str,
    title: &str,
    desc: &str,
    value_label: String,
    on_dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let button = |bid: SharedString, glyph: &str| {
        div()
            .id(bid)
            .flex_none()
            .w(px(24.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .bg(rgb(t.surface))
            .text_color(rgb(t.text))
            .hover(|s| s.bg(rgb(t.hover)))
            .child(glyph.to_string())
    };

    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(rgb(t.text)).child(title.to_string()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(t.text_muted))
                        .child(desc.to_string()),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_2()
                .child(button(SharedString::from(format!("{id}-dec")), "−").on_click(on_dec))
                .child(
                    div()
                        .w(px(40.0))
                        .flex()
                        .justify_center()
                        .text_color(rgb(t.text))
                        .child(value_label),
                )
                .child(button(SharedString::from(format!("{id}-inc")), "+").on_click(on_inc)),
        )
}

// Default column widths for the main listing; all are user-resizable.
const ICON_W: f32 = 18.0;
const MIN_COL_W: f32 = 50.0;

// Command-palette result row height, and how many show before scrolling.
const PALETTE_ROW_H: f32 = 26.0;
const PALETTE_MAX_ROWS: usize = 7;
/// Sidebar width; also the left edge of the content/canvas area.
const SIDEBAR_W: f32 = 220.0;
/// Sidebar width when collapsed to an icon-only rail.
const SIDEBAR_COLLAPSED_W: f32 = 52.0;
/// Height of the custom titlebar strip (the OS titlebar is transparent, so this
/// colored bar sits behind the traffic lights).
const TITLEBAR_H: f32 = 34.0;
/// Tab strip row height.
const TAB_H: f32 = 30.0;
/// Fixed list-row height (so marquee selection can map y-coordinates to rows).
const ROW_H: f32 = 24.0;
/// Overlay scrollbar: stays solid this long after the last scroll…
const SCROLLBAR_LINGER: f32 = 1.0;
/// …then fades out over this long (seconds).
const SCROLLBAR_FADE: f32 = 0.35;
/// Duration of the pane split open/close animation.
const SPLIT_ANIM_MS: u64 = 220;
/// Red used for an invalid rename (field border/text + the reason pill).
const RENAME_ERR_COLOR: u32 = 0xef4444;

/// True while a *tab* is being dragged (gpui only exposes "some drag is
/// active", and the file-drop highlight must not appear during tab drags).
/// Set by the chip's drag constructor; cleared on drop or when the drag ends.
static TAB_DRAG_LIVE: AtomicBool = AtomicBool::new(false);

/// The four resizable columns of the main listing.
#[derive(Clone, Copy, PartialEq)]
enum Column {
    Name,
    Kind,
    Date,
    Size,
}

impl Column {
    fn key(self) -> usize {
        match self {
            Column::Name => 0,
            Column::Kind => 1,
            Column::Date => 2,
            Column::Size => 3,
        }
    }
}

/// Current pixel widths of each column.
#[derive(Clone, Copy)]
struct ColumnWidths {
    name: f32,
    kind: f32,
    date: f32,
    size: f32,
}

impl Default for ColumnWidths {
    fn default() -> Self {
        Self {
            name: 320.0,
            kind: 165.0,
            date: 185.0,
            size: 90.0,
        }
    }
}

impl ColumnWidths {
    fn get(&self, col: Column) -> f32 {
        match col {
            Column::Name => self.name,
            Column::Kind => self.kind,
            Column::Date => self.date,
            Column::Size => self.size,
        }
    }

    fn set(&mut self, col: Column, w: f32) {
        let w = w.max(MIN_COL_W);
        match col {
            Column::Name => self.name = w,
            Column::Kind => self.kind = w,
            Column::Date => self.date = w,
            Column::Size => self.size = w,
        }
    }
}

/// An in-progress column drag.
#[derive(Clone, Copy)]
struct Resize {
    col: Column,
    start_x: f32,
    start_w: f32,
}

/// An in-progress scrollbar-thumb drag (which pane's list is being scrolled).
#[derive(Clone, Copy)]
struct ScrollDrag {
    pane: usize,
    start_y: f32,
    start_scrolled: f32,
}

/// Reserved cache keys for the shared generic folder and file icons.
const FOLDER_KEY: &str = "\u{0}folder";
const FILE_KEY: &str = "\u{0}file";

/// What activating a command-palette item does.
/// One entry in the palette's ⌘K actions panel.
#[derive(Clone, Copy, PartialEq)]
enum PaletteAction {
    /// Open the item (navigate into a dir / launch a file's default app).
    Open,
    /// Navigate to the file's enclosing folder and select it.
    RevealShuffle,
    /// Open the folder (or the file's folder) in a new tab.
    OpenNewTab,
    /// Reveal in Finder.
    RevealFinder,
    /// Copy the full path to the clipboard.
    CopyPath,
}

#[derive(Clone)]
enum Action {
    /// Open a path (navigate into it if a dir, else open the file).
    Open(PathBuf, bool),
    /// Copy the current directory's path to the clipboard.
    CopyDir,
    /// Open the Settings window.
    OpenSettings,
    /// Inert (e.g. "path not found").
    None,
}

/// An open right-click context menu: where it sits, and the entry it targets
/// (None when invoked on empty space).
struct ContextMenu {
    x: f32,
    y: f32,
    /// The pane whose active tab this menu acts on (for refresh after FS ops).
    pane: usize,
    target: Option<(PathBuf, bool)>,
    /// Which level of the menu is showing (root, or a drilled-in submenu).
    view: MenuView,
}

/// In-progress inline rename of a file/folder.
struct Rename {
    pane: usize,
    path: PathBuf,
    text: String,
    /// Text cursor (char index) within `text`.
    cursor: usize,
    /// Selection anchor (char index); `Some` and different from `cursor` means
    /// that range is selected (starts as the whole name, like Finder).
    anchor: Option<usize>,
}

/// The current level shown in the context menu.
#[derive(Clone, Copy, PartialEq)]
enum MenuView {
    Root,
    OpenWith,
    Tags,
    QuickActions,
    Services,
    AddToGroup,
}

/// A user-defined sidebar group: a named collection of files/folders.
#[derive(Clone)]
struct Group {
    name: String,
    paths: Vec<PathBuf>,
}

/// What a right-click in the sidebar targeted (drives its context menu).
#[derive(Clone)]
enum SidebarTarget {
    /// Empty sidebar space → offer "New Group".
    Empty,
    /// A pinned bookmark path → offer "Remove Bookmark".
    Bookmark(PathBuf),
    /// A group's header (by index) → offer "Delete Group".
    GroupHeader(usize),
    /// A member of a group: (group index, path) → offer "Remove from Group".
    GroupMember(usize, PathBuf),
    /// A saved SFTP server → offer "Edit…" / "Remove".
    Sftp(SftpServer),
    /// A tab chip: (pane, tab index) → New Tab / Duplicate / Close / Close Others.
    Tab(usize, usize),
    /// The nav/path bar of a pane → New Tab / Copy Path / Open in Terminal.
    NavBar(usize),
}

/// One row in the command palette: a title, a gray subtitle (full path), and
/// what happens on activation.
struct PaletteItem {
    title: String,
    subtitle: String,
    action: Action,
    is_dir: bool,
}

/// One row in the main listing, with the metadata we display.
#[derive(Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    /// Whether size/dates have been read (the fast first pass leaves them empty).
    loaded: bool,
}

/// A file's cloud-storage materialization state. Detected from the kernel's
/// `SF_DATALESS` flag, which Finder itself uses — set on iCloud and File
/// Provider (Dropbox, Google Drive, OneDrive, …) placeholders whose bytes live
/// in the cloud, not on disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CloudSync {
    /// Not a cloud placeholder: an ordinary local file, or a fully downloaded
    /// cloud file. No badge.
    Local,
    /// Online-only: content is in the cloud, not materialized on disk.
    OnlineOnly,
    /// Actively downloading (we kicked off a materialize; clears once local).
    Syncing,
}

/// `SF_DATALESS` (super-user file flag): the bytes aren't on disk — a cloud
/// placeholder. See `chflags(2)` / `<sys/stat.h>`.
const SF_DATALESS: u32 = 0x4000_0000;

/// How the listing is sorted. `None` is the default (folders first, by name).
#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    None,
    Name,
    Kind,
    Modified,
    Created,
    Size,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::None => "None",
            SortKey::Name => "Name",
            SortKey::Kind => "Kind",
            SortKey::Modified => "Date Modified",
            SortKey::Created => "Date Created",
            SortKey::Size => "Size",
        }
    }
}

/// How items are displayed in a pane.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    List,
    Icons,
    Columns,
    Gallery,
}

/// One open tab: an independent directory view with its own history, scroll,
/// find, and path-edit state.
struct Tab {
    current_dir: PathBuf,
    entries: Vec<Entry>,
    /// Back/forward navigation history and our position within it.
    history: Vec<PathBuf>,
    hist_pos: usize,
    /// Deepest directory visited along the current lineage. When we move up to
    /// an ancestor, the breadcrumb keeps showing this path's trailing segments
    /// (grayed out) so the user can click forward into them again.
    deepest: Option<PathBuf>,
    /// When `Some`, the path bar is an editable text field holding this string.
    editing_path: Option<String>,
    /// Text-cursor position (char index) within `editing_path`.
    path_cursor: usize,
    /// Selection anchor (char index) within `editing_path`; `Some` and different
    /// from `path_cursor` means a range is selected (Option+Shift+Arrow, Cmd+A).
    path_anchor: Option<usize>,
    /// When `Some`, an in-directory "find" filter is active (opened by `/`).
    /// `find_results` holds the matching `entries` indices, best match first.
    find_query: Option<String>,
    find_results: Vec<usize>,
    /// Text cursor (char index) within `find_query`.
    find_cursor: usize,
    /// Selection anchor (char index) within `find_query`; `Some` and different
    /// from `find_cursor` means a range is selected (Option+Shift+Arrow, Cmd+A).
    find_anchor: Option<usize>,
    /// Result set of the active `content:` Spotlight search (absolute paths in
    /// this folder), and the term it corresponds to. `None` = no content
    /// search resolved yet.
    content_hits: Option<HashSet<PathBuf>>,
    content_for: Option<String>,
    /// Bumps each time a content search is kicked off, so a stale mdfind result
    /// is discarded.
    content_gen: u64,
    scroll_handle: UniformListScrollHandle,
    /// Horizontal scroll of the columns (when they're wider than the pane).
    h_scroll: ScrollHandle,
    /// The set of selected items.
    selection: HashSet<PathBuf>,
    /// The focused item (last clicked): used for shift-range and the inspector.
    anchor: Option<PathBuf>,
    /// Sort criterion and direction for this tab's listing.
    sort_key: SortKey,
    sort_asc: bool,
    /// How items are displayed.
    view: ViewMode,
    /// Column view: the chain of folders selected, one per cascading column.
    col_chain: Vec<PathBuf>,
    /// Column view: which column currently has keyboard focus.
    col_active: usize,
    /// Generation of the latest load, so a stale background metadata fill is
    /// discarded if the user navigated away.
    load_gen: u64,
    /// Last time this tab's list scrolled; drives the scrollbar's fade-out.
    last_scroll: Instant,
    /// Bumped per scroll so the fade-out animation restarts from opaque.
    scroll_epoch: u64,
    /// The directory's mtime as of the last load; the watcher reloads the tab
    /// when the folder changes underneath us (new download, deletion, …).
    dir_mtime: Option<SystemTime>,
    /// When `Some`, this tab browses a remote SFTP server; `current_dir` is a
    /// remote absolute path on that host. `None` = a normal local tab.
    remote: Option<SftpServer>,
}

impl Tab {
    fn new(dir: PathBuf) -> Self {
        // Fast first paint; full metadata is filled in from the background.
        let entries = read_entries_fast(&dir);
        Tab {
            current_dir: dir.clone(),
            entries,
            history: vec![dir.clone()],
            hist_pos: 0,
            deepest: Some(dir),
            editing_path: None,
            path_cursor: 0,
            path_anchor: None,
            find_query: None,
            find_results: Vec::new(),
            find_cursor: 0,
            find_anchor: None,
            content_hits: None,
            content_for: None,
            content_gen: 0,
            scroll_handle: UniformListScrollHandle::new(),
            h_scroll: ScrollHandle::new(),
            selection: HashSet::new(),
            anchor: None,
            sort_key: SortKey::None,
            sort_asc: true,
            view: ViewMode::List,
            col_chain: Vec::new(),
            col_active: 0,
            load_gen: 0,
            last_scroll: Instant::now(),
            scroll_epoch: 0,
            dir_mtime: None,
            remote: None,
        }
    }
}

/// A pane: a column in the canvas holding a stack of tabs.
struct Pane {
    tabs: Vec<Tab>,
    active: usize,
}

impl Pane {
    fn new(dir: PathBuf) -> Self {
        Pane {
            tabs: vec![Tab::new(dir)],
            active: 0,
        }
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
}

/// Payload for a tab drag: which pane/tab is being dragged.
#[derive(Clone, Copy)]
struct TabDrag {
    pane: usize,
    tab: usize,
}

/// A small floating tooltip (used by the collapsed sidebar to show a row's
/// name/path on hover).
struct TooltipView {
    text: String,
}

impl Render for TooltipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(Theme::alpha(t.surface, 0xf2))
            .border_1()
            .border_color(rgb(t.border))
            .text_color(rgb(t.text))
            .text_xs()
            .shadow_lg()
            .child(self.text.clone())
    }
}

/// The floating preview rendered under the cursor while dragging a tab.
struct TabDragPreview {
    label: String,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(Theme::alpha(t.surface, 0xf2))
            .border_1()
            .border_color(rgb(t.accent))
            .text_color(rgb(t.text))
            .text_sm()
            .shadow_lg()
            .child(self.label.clone())
    }
}

/// The root view: a workspace of one or two side-by-side panes.
struct Shuffle {
    panes: Vec<Pane>,
    active_pane: usize,
    /// Left pane's width fraction when two panes are shown (0.2..0.8).
    split_ratio: f32,
    /// In-progress divider drag: (cursor x at grab, split_ratio at grab).
    divider_drag: Option<(f32, f32)>,
    /// Bumped whenever the pane layout changes (split created or collapsed) so
    /// the one-shot open/close width animations restart.
    split_epoch: usize,
    /// Set when the split collapses back to one pane: the surviving pane's
    /// starting width fraction and whether it's anchored to the right edge
    /// (i.e. the *left* pane was the one removed). Drives the grow animation.
    collapse_anim: Option<(f32, bool)>,
    /// True while the scrollbar-fade repaint ticker task is alive.
    fade_ticker: bool,
    recents: Vec<PathBuf>,
    bookmarks: Vec<PathBuf>,
    /// User-defined sidebar groups (when the feature is enabled).
    groups: Vec<Group>,
    /// Open sidebar context menu: (x, y, what was clicked).
    sidebar_menu: Option<(f32, f32, SidebarTarget)>,
    /// Open "New Group" naming dialog: the name being typed (None = closed).
    group_dialog: Option<String>,
    /// Sidebar section titles the user has collapsed (hidden their items).
    collapsed_sections: HashSet<String>,
    widths: ColumnWidths,
    resize: Option<Resize>,
    scroll_drag: Option<ScrollDrag>,
    // Command palette (Cmd+P).
    focus: FocusHandle,
    palette_open: bool,
    query: String,
    /// Text-cursor position in `query`, as a char index (0..=char count).
    query_cursor: usize,
    /// Selection anchor (char index). When `Some` and different from the cursor,
    /// the range between them is selected; Cmd+A selects all, Option+Shift+Arrow
    /// extends by word. `None` means no selection.
    query_anchor: Option<usize>,
    /// Past palette queries (newest last), when the history setting is on.
    palette_hist: Vec<String>,
    /// Which history entry is being browsed with Up/Down (None = live query).
    palette_hist_pos: Option<usize>,
    palette_items: Vec<PaletteItem>,
    selected: usize,
    /// The palette's ⌘K actions panel: selected action index (None = closed).
    palette_actions: Option<usize>,
    search_gen: u64,
    palette_scroll: ScrollHandle,
    /// In-memory fuzzy index of ~/ (None until the background build finishes).
    index: Option<Arc<FileIndex>>,
    context_menu: Option<ContextMenu>,
    /// In-progress inline rename, if any.
    rename: Option<Rename>,
    /// Open "Sort By" dropdown: (pane, x, y) in window coords.
    sort_menu: Option<(usize, f32, f32)>,
    /// Pending "move to Trash" confirmation: (pane, paths).
    confirm_delete: Option<(usize, Vec<PathBuf>)>,
    /// Monotonic counter tagging each directory load (for stale-result guards).
    next_load_gen: u64,
    /// In-progress marquee (box) selection: (pane, start, current) window coords.
    marquee: Option<(usize, (f32, f32), (f32, f32))>,
    /// A left-press on a draggable row: (pane, path, press position). Promoted to
    /// a native OS drag once the cursor moves past a small threshold.
    drag_candidate: Option<(usize, PathBuf, (f32, f32))>,
    /// Open "Connect to Server" dialog: the URL being typed (None = closed).
    server_dialog: Option<ServerForm>,
    /// First-run SSH prompt (choose ~/.ssh vs. credentials) — shown once.
    ssh_ask: bool,
    /// Recently-connected server URLs (most recent first).
    server_history: Vec<String>,
    // Terminal mode (the bottom command bar).
    term_input: String,
    term_output: Vec<String>,
    term_focused: bool,
    term_scroll: ScrollHandle,
    /// State of the self-updater's banner. `None` = up to date / not checked /
    /// dismissed.
    update: Option<UpdateStatus>,
    /// Current page of the inspector's PDF preview (0-based; reset on focus).
    preview_page: usize,
    /// The row the mouse is currently over: (pane, path). Enter renames it
    /// even when nothing is selected (point-and-rename).
    hovered: Option<(usize, PathBuf)>,
    /// Where a file drag would land right now: (pane, Some(display row)) for
    /// a folder row, (pane, None) for the pane's own directory. Computed from
    /// the drag position on every synthesized drag-move — one source of truth,
    /// so the highlight can't flicker between panes.
    drop_hover: Option<(usize, Option<usize>)>,
    /// Last SFTP error (connection/listing), shown as a banner until cleared.
    remote_error: Option<String>,
    /// Waterfall sidebar tree: which folders are expanded, their cached child
    /// subfolders, and which are being loaded (to avoid duplicate reads).
    waterfall_expanded: HashSet<PathBuf>,
    waterfall_children: HashMap<PathBuf, Vec<PathBuf>>,
    waterfall_pending: HashSet<PathBuf>,
    /// Each cached waterfall folder's mtime, so the folder watcher can spot an
    /// outside change and invalidate just that folder for a live refresh.
    waterfall_mtime: HashMap<PathBuf, SystemTime>,
    /// Cloud files with a download/evict in flight — shown with the syncing
    /// badge until the operation finishes and the listing is re-read.
    cloud_busy: HashSet<PathBuf>,
}

/// Which cloud store a path belongs to, for routing download/evict.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloudKind {
    /// iCloud Drive — full download + evict via Foundation.
    ICloud,
    /// A third-party File Provider store (Dropbox, Google Drive, OneDrive, …) —
    /// download works when the provider is running; evict is provider-driven.
    Provider,
}

/// Update-check state shared between the Settings window (which drives the
/// "Check for Updates" button) and the main window (which shows the banner
/// and performs the install).
#[derive(Clone, PartialEq, Default)]
enum UpdateCheck {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// A newer tag (like "v0.2.7") is available to install.
    Available(String),
    /// The GitHub query failed (offline, rate limit, …).
    Failed,
    /// Settings asked the main window to download + install this tag now.
    Install(String),
}

#[derive(Clone, Default)]
struct UpdateCheckGlobal(UpdateCheck);
impl gpui::Global for UpdateCheckGlobal {}

/// Drives the update banner under the titlebar.
#[derive(Clone)]
enum UpdateStatus {
    /// A newer release (tag like "v0.2.4") is available to install.
    Available(String),
    /// The user clicked Update; we're downloading + verifying it now.
    Downloading(String),
    /// The install couldn't complete; carries a short reason.
    Failed(String),
}

impl Shuffle {
    fn new(dir: PathBuf, cx: &mut Context<Self>) -> Self {
        ensure_base_icons(); // real folder/file icons ready before first render
        ensure_sidebar_icons(); // Applications/Documents/… + Mac/home icons
        ensure_dynamic_sidebar_icons(); // cloud providers + mounted volumes
        // Sync + repaint whenever the theme changes (e.g. from Settings).
        cx.observe_global::<ThemeGlobal>(|_, cx| {
            set_active_theme(cx.global::<ThemeGlobal>().0);
            cx.notify();
        })
        .detach();
        // Sync + repaint whenever feature prefs change.
        cx.observe_global::<PrefsGlobal>(|_, cx| {
            set_active_prefs(cx.global::<PrefsGlobal>().0);
            cx.notify();
        })
        .detach();
        // Sync + repaint whenever the saved SFTP servers change (from Settings).
        cx.observe_global::<SftpServersGlobal>(|_, cx| {
            set_active_sftp_servers(cx.global::<SftpServersGlobal>().0.clone());
            cx.notify();
        })
        .detach();
        // Sync + repaint whenever the menu style changes (e.g. from Settings).
        cx.observe_global::<MenuStyleGlobal>(|_, cx| {
            set_active_menu(cx.global::<MenuStyleGlobal>().0);
            cx.notify();
        })
        .detach();
        // Sync the keymap when it changes (e.g. from the Settings window).
        cx.observe_global::<KeymapGlobal>(|_, cx| {
            set_active_keymap(cx.global::<KeymapGlobal>().0.clone());
            cx.notify();
        })
        .detach();
        // React to the Settings window's update checks: an explicit check that
        // finds a new version shows the banner here (even a previously
        // dismissed one), and "Install & Relaunch" starts the self-update.
        cx.observe_global::<UpdateCheckGlobal>(|this, cx| {
            match cx.global::<UpdateCheckGlobal>().0.clone() {
                UpdateCheck::Available(tag) => {
                    this.update = Some(UpdateStatus::Available(tag));
                    cx.notify();
                }
                UpdateCheck::Install(tag) => {
                    if !matches!(this.update, Some(UpdateStatus::Downloading(_))) {
                        this.update = Some(UpdateStatus::Available(tag));
                        this.start_self_update(cx);
                    }
                }
                _ => {}
            }
        })
        .detach();
        // Watch the visible folders for outside changes (a finishing download,
        // files created/deleted by other apps): poll each pane's directory
        // mtime once a second — off-thread, so a slow network mount can't
        // stall a frame — and reload the pane when it changes. The reload
        // re-applies the tab's sort and any active filter, so new files land
        // in the right spot immediately.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1000))
                    .await;
                let dirs = match this.update(cx, |this, _| {
                    this.panes
                        .iter()
                        .map(|p| p.active_tab().current_dir.clone())
                        .collect::<Vec<_>>()
                }) {
                    Ok(d) => d,
                    Err(_) => break,
                };
                let sampled = dirs.clone();
                let stats = cx
                    .background_spawn(async move {
                        sampled
                            .iter()
                            .map(|d| fs::metadata(d).ok().and_then(|m| m.modified().ok()))
                            .collect::<Vec<Option<SystemTime>>>()
                    })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    for (pane, (dir, cur)) in dirs.into_iter().zip(stats).enumerate() {
                        // Skip if the pane went away or navigated meanwhile.
                        if pane >= this.panes.len() || this.tab(pane).current_dir != dir {
                            continue;
                        }
                        // Remote tabs aren't on the local filesystem — the mtime
                        // stat is meaningless (and would reload constantly).
                        if this.tab(pane).remote.is_some() {
                            continue;
                        }
                        match (this.tab(pane).dir_mtime, cur) {
                            (Some(prev), Some(now)) if prev != now => {
                                this.reload_pane(pane, cx);
                            }
                            (None, Some(now)) => this.tab_mut(pane).dir_mtime = Some(now),
                            _ => {}
                        }
                    }
                    // Same tick: live-refresh the waterfall tree's folders.
                    this.refresh_waterfall(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        // Hidden debug harness (SHUFFLE_MQ_SIM=1): drive a marquee + edge drag
        // programmatically and log what the auto-scroll loop does. Synthetic OS
        // mouse events don't reach gpui, so this is how the mechanics are
        // verified end-to-end in-process.
        if std::env::var_os("SHUFFLE_MQ_SIM").is_some() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |this, cx| {
                    let (top, bottom) = {
                        let st = this.tab(0).scroll_handle.0.borrow();
                        let b = st.base_handle.bounds();
                        let top = f64::from(b.origin.y) as f32;
                        (top, top + f64::from(b.size.height) as f32)
                    };
                    eprintln!("[mq-sim] viewport top={top:.0} bottom={bottom:.0}");
                    let mid = (top + bottom) / 2.0;
                    this.begin_marquee(0, 400.0, mid, cx);
                    // Drag 60px past the bottom edge and hold.
                    this.update_marquee(400.0, bottom + 60.0, cx);
                    eprintln!("[mq-sim] begin at y={mid:.0}, cur held at y={:.0}", bottom + 60.0);
                });
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |this, cx| {
                    eprintln!(
                        "[mq-sim] after 3s down: scrolled={:.0} selected={}",
                        this.current_scrolled(0),
                        this.tab(0).selection.len()
                    );
                    // Now hold 40px above the top edge — should scroll back up.
                    let top = {
                        let st = this.tab(0).scroll_handle.0.borrow();
                        f64::from(st.base_handle.bounds().origin.y) as f32
                    };
                    this.update_marquee(400.0, top - 40.0, cx);
                });
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = this.update(cx, |this, cx| {
                    eprintln!(
                        "[mq-sim] after 3s up: scrolled={:.0} selected={}",
                        this.current_scrolled(0),
                        this.tab(0).selection.len()
                    );
                    this.end_marquee(cx);
                    eprintln!("[mq-sim] done");
                });
            })
            .detach();
        }
        // Rebuild icons when the icon pack changes.
        cx.observe_global::<IconPackGlobal>(|this, cx| {
            set_active_icon_pack(cx.global::<IconPackGlobal>().0.clone());
            clear_icon_cache();
            ensure_base_icons();
            ensure_sidebar_icons();
            ensure_dynamic_sidebar_icons();
            cx.notify();
            this.prewarm_icons(cx);
        })
        .detach();
        Self {
            panes: vec![Pane::new(dir)],
            active_pane: 0,
            split_ratio: 0.5,
            divider_drag: None,
            split_epoch: 0,
            collapse_anim: None,
            fade_ticker: false,
            recents: read_path_list("recents.txt"),
            bookmarks: read_path_list("bookmarks.txt"),
            groups: load_groups(),
            sidebar_menu: None,
            group_dialog: None,
            collapsed_sections: read_string_list("collapsed_sections.txt").into_iter().collect(),
            widths: ColumnWidths::default(),
            resize: None,
            scroll_drag: None,
            focus: cx.focus_handle(),
            palette_open: false,
            query: String::new(),
            query_cursor: 0,
            query_anchor: None,
            palette_hist: read_string_list("palette_history.txt"),
            palette_hist_pos: None,
            palette_items: Vec::new(),
            selected: 0,
            palette_actions: None,
            search_gen: 0,
            palette_scroll: ScrollHandle::new(),
            index: None,
            context_menu: None,
            rename: None,
            sort_menu: None,
            confirm_delete: None,
            next_load_gen: 0,
            marquee: None,
            drag_candidate: None,
            server_dialog: None,
            ssh_ask: false,
            server_history: read_string_list("servers.txt"),
            term_input: String::new(),
            term_output: Vec::new(),
            term_focused: false,
            term_scroll: ScrollHandle::new(),
            update: None,
            preview_page: 0,
            hovered: None,
            drop_hover: None,
            remote_error: None,
            waterfall_expanded: HashSet::new(),
            waterfall_children: HashMap::new(),
            waterfall_pending: HashSet::new(),
            waterfall_mtime: HashMap::new(),
            cloud_busy: HashSet::new(),
        }
    }

    // ----- pane / tab accessors -----

    fn pane(&self, ix: usize) -> &Pane {
        &self.panes[ix]
    }

    fn pane_mut(&mut self, ix: usize) -> &mut Pane {
        &mut self.panes[ix]
    }

    fn tab(&self, pane: usize) -> &Tab {
        self.panes[pane].active_tab()
    }

    fn tab_mut(&mut self, pane: usize) -> &mut Tab {
        self.panes[pane].active_tab_mut()
    }

    fn active_tab(&self) -> &Tab {
        self.tab(self.active_pane)
    }

    // ----- right-click context menu -----

    fn open_context_menu(&mut self, pane: usize, x: f32, y: f32, target: Option<(PathBuf, bool)>, cx: &mut Context<Self>) {
        self.active_pane = pane.min(self.panes.len() - 1);
        self.rename = None;
        self.context_menu = Some(ContextMenu {
            x,
            y,
            pane: self.active_pane,
            target,
            view: MenuView::Root,
        });
        cx.notify();
    }

    /// Switch the open context menu to a different level (keeps it open).
    fn set_menu_view(&mut self, view: MenuView, cx: &mut Context<Self>) {
        if let Some(menu) = self.context_menu.as_mut() {
            menu.view = view;
            cx.notify();
        }
    }

    fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Re-read a pane's current directory contents (after a create/trash).
    fn refresh_pane(&mut self, pane: usize, cx: &mut Context<Self>) {
        clear_column_cache();
        self.reload_pane(pane, cx);
    }

    /// Load (or reload) a pane's directory: a near-instant cheap pass for first
    /// paint, then a background pass that fills in sizes/dates without blocking.
    fn reload_pane(&mut self, pane: usize, cx: &mut Context<Self>) {
        // Remote (SFTP) tabs load over the network on a background thread.
        if self.tab(pane).remote.is_some() {
            self.reload_remote_pane(pane, cx);
            return;
        }
        let dir = self.tab(pane).current_dir.clone();
        ensure_dynamic_sidebar_icons(); // pick up newly-mounted volumes/cloud
        // Stamp the mtime *before* reading so a change racing the read bumps
        // it again and the watcher catches it on the next tick.
        self.tab_mut(pane).dir_mtime = fs::metadata(&dir).ok().and_then(|m| m.modified().ok());
        self.tab_mut(pane).entries = read_entries_fast(&dir);
        self.next_load_gen += 1;
        let gen = self.next_load_gen;
        self.tab_mut(pane).load_gen = gen;
        self.sort_tab(pane);
        if self.tab(pane).find_query.is_some() {
            self.recompute_find(pane);
        }
        cx.notify();
        self.prewarm_icons(cx);

        // Fill full metadata in the background, then swap it in if still current.
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let full = cx.background_spawn(async move { read_entries(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                if pane < this.panes.len()
                    && this.tab(pane).load_gen == gen
                    && this.tab(pane).current_dir == dir
                {
                    this.tab_mut(pane).entries = full;
                    this.sort_tab(pane);
                    if this.tab(pane).find_query.is_some() {
                        this.recompute_find(pane);
                    }
                    cx.notify();
                    this.prewarm_icons(cx);
                }
            });
        })
        .detach();
    }

    /// Load a remote SFTP directory listing on a background thread, then swap
    /// it in if the tab is still on that path.
    fn reload_remote_pane(&mut self, pane: usize, cx: &mut Context<Self>) {
        let Some(server) = self.tab(pane).remote.clone() else {
            return;
        };
        let dir = self.tab(pane).current_dir.clone();
        let path = dir.to_string_lossy().into_owned();
        self.next_load_gen += 1;
        let gen = self.next_load_gen;
        self.tab_mut(pane).load_gen = gen;
        // Show a placeholder (empty) immediately; the listing arrives async.
        self.tab_mut(pane).entries.clear();
        cx.notify();
        let use_system = prefs().ssh_use_system;
        cx.spawn(async move |this, cx| {
            let s = server.clone();
            let p = path.clone();
            let result = cx
                .background_spawn(async move { sftp_list(&s, &p, use_system) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if pane >= this.panes.len()
                    || this.tab(pane).load_gen != gen
                    || this.tab(pane).current_dir != dir
                {
                    return;
                }
                match result {
                    Ok(entries) => {
                        this.tab_mut(pane).entries = entries;
                        this.remote_error = None;
                        this.sort_tab(pane);
                        if this.tab(pane).find_query.is_some() {
                            this.recompute_find(pane);
                        }
                        this.prewarm_icons(cx);
                    }
                    Err(e) => {
                        this.tab_mut(pane).entries.clear();
                        this.remote_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a remote operation off-thread, then reload the pane (or show the
    /// error). The closure captures the server + parameters it needs.
    fn run_remote<F>(&mut self, pane: usize, op: F, cx: &mut Context<Self>)
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.remote_error = None;
                        if pane < this.panes.len() {
                            this.reload_pane(pane, cx);
                        }
                    }
                    Err(e) => {
                        this.remote_error = Some(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// A unique child name in a remote dir, computed from the current listing.
    fn unique_remote_name(&self, pane: usize, base: &str) -> String {
        let names: Vec<String> = self.tab(pane).entries.iter().map(|e| e.name.clone()).collect();
        if !names.iter().any(|n| n == base) {
            return base.to_string();
        }
        for i in 2..1000 {
            let cand = format!("{base} {i}");
            if !names.iter().any(|n| n == &cand) {
                return cand;
            }
        }
        base.to_string()
    }

    /// Make a new folder (or empty file) on the remote server; the new item is
    /// selected when the listing refreshes (press Enter to rename it).
    fn new_remote_item(&mut self, pane: usize, is_dir: bool, cx: &mut Context<Self>) {
        let Some(server) = self.tab(pane).remote.clone() else {
            return;
        };
        let base = if is_dir { "untitled folder" } else { "untitled file" };
        let name = self.unique_remote_name(pane, base);
        let path = self.tab(pane).current_dir.join(&name);
        let remote_path = path.to_string_lossy().into_owned();
        let use_system = prefs().ssh_use_system;
        cx.spawn(async move |this, cx| {
            let s = server.clone();
            let rp = remote_path.clone();
            let r = cx
                .background_spawn(async move {
                    if is_dir {
                        sftp_mkdir(&s, &rp, use_system)
                    } else {
                        sftp_touch(&s, &rp, use_system)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if pane >= this.panes.len() {
                    return;
                }
                match r {
                    Ok(()) => {
                        this.remote_error = None;
                        this.tab_mut(pane).selection = std::iter::once(path.clone()).collect();
                        this.tab_mut(pane).anchor = Some(path);
                        this.reload_pane(pane, cx);
                    }
                    Err(e) => {
                        this.remote_error = Some(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Upload local file(s) into a remote directory on `pane`.
    fn upload_to_remote(&mut self, pane: usize, remote_dir: PathBuf, locals: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(server) = self.tab(pane).remote.clone() else {
            return;
        };
        let dir = remote_dir.to_string_lossy().into_owned();
        let use_system = prefs().ssh_use_system;
        self.run_remote(
            pane,
            move || {
                for local in &locals {
                    sftp_upload(&server, local, &dir, use_system)?;
                }
                Ok(())
            },
            cx,
        );
    }

    /// Re-apply this tab's sort criterion to its entries.
    fn sort_tab(&mut self, pane: usize) {
        let (key, asc) = (self.tab(pane).sort_key, self.tab(pane).sort_asc);
        sort_entries(&mut self.tab_mut(pane).entries, key, asc);
    }

    /// Set the sort criterion (clicking the same column toggles direction).
    fn set_sort(&mut self, pane: usize, key: SortKey, cx: &mut Context<Self>) {
        {
            let tab = self.tab_mut(pane);
            if tab.sort_key == key && key != SortKey::None {
                tab.sort_asc = !tab.sort_asc;
            } else {
                tab.sort_key = key;
                tab.sort_asc = true;
            }
        }
        self.sort_tab(pane);
        if self.tab(pane).find_query.is_some() {
            self.recompute_find(pane);
        }
        cx.notify();
    }

    /// Switch how a pane displays its items.
    fn set_view(&mut self, pane: usize, mode: ViewMode, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.tab_mut(pane).view = mode;
        // Gallery needs a focused item + its preview to show something at once.
        if mode == ViewMode::Gallery {
            let sel = self.tab(pane).anchor.clone().or_else(|| {
                let dir = self.tab(pane).current_dir.clone();
                self.tab(pane)
                    .entries
                    .iter()
                    .find(|e| !e.is_dir)
                    .map(|e| dir.join(&e.name))
            });
            if let Some(p) = sel {
                self.tab_mut(pane).anchor = Some(p.clone());
                self.ensure_preview(p, true, cx);
            }
        }
        cx.notify();
    }

    fn new_folder(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab(pane).remote.is_some() {
            self.new_remote_item(pane, true, cx);
            return;
        }
        let path = unique_child(&self.tab(pane).current_dir, "untitled folder");
        if fs::create_dir(&path).is_ok() {
            self.refresh_pane(pane, cx);
            self.reveal_and_rename(pane, path, window, cx);
        }
    }

    fn new_file(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab(pane).remote.is_some() {
            self.new_remote_item(pane, false, cx);
            return;
        }
        let path = unique_child(&self.tab(pane).current_dir, "untitled file");
        if fs::File::create(&path).is_ok() {
            self.refresh_pane(pane, cx);
            self.reveal_and_rename(pane, path, window, cx);
        }
    }

    /// Scroll an item into view in `pane`, select it, and make it the focused
    /// entry (inspector target / rename target).
    fn reveal_in_pane(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let paths = self.display_paths(pane);
        if let Some(ix) = paths.iter().position(|p| p == &path) {
            let (view, off) = {
                let tab = self.tab(pane);
                let off = usize::from(
                    tab.find_query.is_none()
                        && prefs().show_parent
                        && tab.current_dir.parent().is_some(),
                );
                (tab.view, off)
            };
            let item = match view {
                ViewMode::Icons => ix / self.icon_cols(pane),
                _ => ix + off,
            };
            self.tab(pane).scroll_handle.scroll_to_item(item, ScrollStrategy::Center);
            self.mark_scrolled(pane, cx);
        }
        self.tab_mut(pane).selection = std::iter::once(path.clone()).collect();
        self.focus_entry(pane, path, cx);
    }

    /// Scroll a freshly-created item into view, select it, and open the name
    /// editor so the user can type the real name immediately. Enter with
    /// nothing typed, Escape, or clicking elsewhere keeps the default name.
    fn reveal_and_rename(
        &mut self,
        pane: usize,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reveal_in_pane(pane, path.clone(), cx);
        self.begin_rename(pane, path, window, cx);
    }

    /// Quick Look the current selection (spacebar), like Finder. Uses macOS
    /// `qlmanage -p`, which pops the system preview panel. Local files only —
    /// remote (SFTP) items aren't on disk for QuickLook to read.
    fn quick_look(&self, pane: usize) {
        if self.tab(pane).remote.is_some() {
            return;
        }
        let mut paths: Vec<PathBuf> = self.tab(pane).selection.iter().cloned().collect();
        if paths.is_empty() {
            if let Some(a) = self.tab(pane).anchor.clone() {
                paths.push(a);
            }
        }
        if paths.is_empty() {
            return;
        }
        paths.sort();
        let _ = Command::new("qlmanage")
            .arg("-p")
            .args(&paths)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    fn open_path(&mut self, pane: usize, path: PathBuf, is_dir: bool, cx: &mut Context<Self>) {
        // App bundles are directories, but double-clicking one should *launch*
        // it like Finder — browse inside via right-click → Show Package Contents.
        let is_app = is_dir && path.extension().is_some_and(|e| e == "app");
        if is_dir && !is_app {
            self.navigate_in(pane, path, cx);
        } else if !is_app && self.tab(pane).remote.is_some() {
            // Remote file → download to ~/Downloads, then open it.
            self.download_remote(pane, path, None, true, cx);
        } else {
            let _ = Command::new("open").arg(&path).spawn();
        }
    }

    /// Download a remote file to a local directory (default: ~/Downloads) on a
    /// background thread; optionally `open` it when finished.
    fn download_remote(
        &mut self,
        pane: usize,
        remote_path: PathBuf,
        dest_dir: Option<PathBuf>,
        open_after: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.tab(pane).remote.clone() else {
            return;
        };
        let name = remote_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        let dest_dir = dest_dir.unwrap_or_else(|| home_dir().join("Downloads"));
        let local = unique_child(&dest_dir, &name);
        let remote = remote_path.to_string_lossy().into_owned();
        let use_system = prefs().ssh_use_system;
        cx.spawn(async move |this, cx| {
            let s = server.clone();
            let r = remote.clone();
            let l = local.clone();
            let result = cx
                .background_spawn(async move {
                    let _ = fs::create_dir_all(l.parent().unwrap_or(&l));
                    // sftp `get "remote" "local"`.
                    let script = format!(
                        "get \"{}\" \"{}\"",
                        r.replace('"', ""),
                        l.to_string_lossy().replace('"', "")
                    );
                    sftp_batch(&s, &script, use_system).map(|_| l)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(local) => {
                        this.remote_error = None;
                        if open_after {
                            let _ = Command::new("open").arg(&local).spawn();
                        }
                    }
                    Err(e) => this.remote_error = Some(format!("Download failed: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn move_to_trash(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        if trash_path(&path) {
            self.refresh_pane(pane, cx);
        }
    }

    /// Ask to move the current selection (or the focused item) to Trash.
    fn request_delete(&mut self, pane: usize, cx: &mut Context<Self>) {
        let mut paths: Vec<PathBuf> = self.tab(pane).selection.iter().cloned().collect();
        if paths.is_empty() {
            if let Some(a) = self.tab(pane).anchor.clone() {
                paths.push(a);
            }
        }
        if paths.is_empty() {
            return;
        }
        paths.sort();
        self.confirm_delete = Some((pane, paths));
        cx.notify();
    }

    /// Carry out a confirmed delete: Trash locally, or permanently remove on a
    /// remote server (no Trash exists there).
    fn perform_delete(&mut self, cx: &mut Context<Self>) {
        if let Some((pane, paths)) = self.confirm_delete.take() {
            {
                let tab = self.tab_mut(pane);
                tab.selection.clear();
                tab.anchor = None;
            }
            if let Some(server) = self.tab(pane).remote.clone() {
                let use_system = prefs().ssh_use_system;
                let remote_paths: Vec<String> =
                    paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                self.run_remote(
                    pane,
                    move || {
                        for rp in &remote_paths {
                            sftp_delete(&server, rp, use_system)?;
                        }
                        Ok(())
                    },
                    cx,
                );
            } else {
                for p in &paths {
                    trash_path(p);
                }
                self.refresh_pane(pane, cx);
            }
        }
        cx.notify();
    }

    /// Re-read every pane's directory (after a move that may affect two panes).
    fn refresh_all_panes(&mut self, cx: &mut Context<Self>) {
        clear_column_cache();
        for p in 0..self.panes.len() {
            self.reload_pane(p, cx);
        }
    }

    /// Move `src` into `dest_dir` (a drag-and-drop between folders/panes). No-op
    /// if it's already there; refuses to overwrite an existing item.
    fn move_into(&mut self, dest_dir: PathBuf, src: PathBuf, cx: &mut Context<Self>) {
        if !dest_dir.is_dir() {
            return;
        }
        let Some(name) = src.file_name() else { return };
        // Already in this folder, or dropping a folder onto itself → ignore.
        if src.parent() == Some(dest_dir.as_path()) || dest_dir == src {
            return;
        }
        if dest_dir.starts_with(&src) {
            return; // can't move a folder into its own descendant
        }
        let dest = dest_dir.join(name);
        if dest.exists() {
            return; // don't clobber existing files
        }
        // `mv` handles cross-volume moves (copy + delete) too.
        let _ = Command::new("mv").arg(&src).arg(&dest).status();
        self.refresh_all_panes(cx);
    }

    /// Handle files dropped onto `dest_dir` in `pane`: upload to the server when
    /// the pane is remote, otherwise move locally. Dropped `ExternalPaths` are
    /// always local (from Finder or another pane), so remote = upload.
    fn drop_files(&mut self, pane: usize, dest_dir: PathBuf, srcs: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.tab(pane).remote.is_some() {
            let locals: Vec<PathBuf> = srcs.into_iter().filter(|p| p.exists()).collect();
            if !locals.is_empty() {
                self.upload_to_remote(pane, dest_dir, locals, cx);
            }
        } else {
            for s in srcs {
                self.move_into(dest_dir.clone(), s, cx);
            }
        }
    }

    // ----- inline rename -----

    /// Begin renaming `path`: the row's name becomes an editable field.
    fn begin_rename(&mut self, pane: usize, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let text = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Start with the whole name selected, like Finder.
        let end = text.chars().count();
        self.rename = Some(Rename {
            pane,
            path,
            text,
            cursor: end,
            anchor: if end > 0 { Some(0) } else { None },
        });
        window.focus(&self.focus);
        cx.notify();
    }

    /// Why the current rename text can't be committed (`None` = it can).
    /// Shown live under the pane and blocks Enter while present.
    fn rename_error(&self) -> Option<&'static str> {
        let r = self.rename.as_ref()?;
        let name = r.text.trim();
        if name.is_empty() {
            return None; // Enter on an empty name just cancels, like Finder
        }
        if name.contains('/') {
            return Some("Names can’t contain “/”");
        }
        if name.contains(':') {
            return Some("Names can’t contain “:”");
        }
        if name == "." || name == ".." {
            return Some("That name is reserved");
        }
        if name.len() > 255 {
            return Some("That name is too long");
        }
        let current = r.path.file_name().map(|n| n.to_string_lossy().into_owned());
        // Renaming to itself (or a case-only variant on this case-insensitive
        // filesystem) is fine; anything else that exists is taken.
        let same = current
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(name));
        if !same && r.path.parent().is_some_and(|p| p.join(name).exists()) {
            return Some("Another item here already has this name");
        }
        None
    }

    /// Commit the in-progress rename (Enter), renaming the file on disk.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        // An invalid name keeps the field open with the error showing — Enter
        // shouldn't silently discard the edit. Escape still cancels.
        if self.rename.is_some() && self.rename_error().is_some() {
            cx.notify();
            return;
        }
        if let Some(r) = self.rename.take() {
            let new = r.text.trim();
            if !new.is_empty() {
                if let Some(parent) = r.path.parent() {
                    let dest = parent.join(new);
                    if dest == r.path {
                        cx.notify();
                        return;
                    }
                    // Remote rename runs over SFTP on a background thread.
                    if let Some(server) = self.tab(r.pane).remote.clone() {
                        let use_system = prefs().ssh_use_system;
                        let from = r.path.to_string_lossy().into_owned();
                        let to = dest.to_string_lossy().into_owned();
                        let pane = r.pane;
                        self.run_remote(
                            pane,
                            move || sftp_rename(&server, &from, &to, use_system),
                            cx,
                        );
                        cx.notify();
                        return;
                    }
                    // Case-only renames are legal even though the destination
                    // "exists" on APFS's case-insensitive default.
                    let case_only = r
                        .path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(new));
                    if (case_only || !dest.exists()) && fs::rename(&r.path, &dest).is_ok() {
                        let tab = self.tab_mut(r.pane);
                        if tab.selection.remove(&r.path) {
                            tab.selection.insert(dest.clone());
                        }
                        if tab.anchor.as_deref() == Some(r.path.as_path()) {
                            tab.anchor = Some(dest);
                        }
                        self.refresh_pane(r.pane, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    /// The rename field's selected range as (lo, hi) char indices, if any.
    fn rename_sel(&self) -> Option<(usize, usize)> {
        let r = self.rename.as_ref()?;
        let a = r.anchor?;
        if a == r.cursor {
            return None;
        }
        Some((a.min(r.cursor), a.max(r.cursor)))
    }

    /// Delete the rename selection; returns whether there was one.
    fn rename_delete_sel(&mut self) -> bool {
        if let Some((lo, hi)) = self.rename_sel() {
            if let Some(r) = self.rename.as_mut() {
                let (bl, bh) = (char_byte(&r.text, lo), char_byte(&r.text, hi));
                r.text.replace_range(bl..bh, "");
                r.cursor = lo;
                r.anchor = None;
            }
            true
        } else {
            if let Some(r) = self.rename.as_mut() {
                r.anchor = None;
            }
            false
        }
    }

    /// Insert `s` at the rename cursor, replacing any selection first.
    fn rename_insert(&mut self, s: &str) {
        self.rename_delete_sel();
        if let Some(r) = self.rename.as_mut() {
            let b = char_byte(&r.text, r.cursor);
            r.text.insert_str(b, s);
            r.cursor += s.chars().count();
        }
    }

    /// Move (or extend) the rename cursor. `word` jumps by word, `select`
    /// extends the selection (Option+Shift selects a word at a time).
    fn rename_move_h(&mut self, left: bool, word: bool, select: bool) {
        let Some(r) = self.rename.as_ref() else {
            return;
        };
        let s = r.text.clone();
        let end = s.chars().count();
        let cursor = r.cursor.min(end);
        let target = match (left, word) {
            (true, true) => prev_word_boundary(&s, cursor),
            (true, false) => cursor.saturating_sub(1),
            (false, true) => next_word_boundary(&s, cursor),
            (false, false) => (cursor + 1).min(end),
        };
        let sel = self.rename_sel();
        let Some(r) = self.rename.as_mut() else {
            return;
        };
        if select {
            if r.anchor.is_none() {
                r.anchor = Some(cursor);
            }
            r.cursor = target;
            if r.anchor == Some(target) {
                r.anchor = None;
            }
        } else if let Some((lo, hi)) = sel {
            r.cursor = if word {
                target
            } else if left {
                lo
            } else {
                hi
            };
            r.anchor = None;
        } else {
            r.cursor = target;
            r.anchor = None;
        }
    }

    /// Keystrokes while a rename field is active. Full text editing: arrows
    /// move, Option jumps words, Cmd jumps to the ends, Shift extends the
    /// selection, Cmd+A/C/X/V work on the selection.
    fn handle_rename_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let alt = ks.modifiers.alt;
        let shift = ks.modifiers.shift;
        match ks.key.as_str() {
            "escape" => {
                self.rename = None;
                cx.notify();
            }
            "enter" => self.commit_rename(cx),
            "left" | "right" => {
                let left = ks.key == "left";
                if cmd {
                    if let Some(r) = self.rename.as_mut() {
                        let end = r.text.chars().count();
                        let cursor = r.cursor;
                        if shift && r.anchor.is_none() {
                            r.anchor = Some(cursor);
                        }
                        r.cursor = if left { 0 } else { end };
                        if !shift {
                            r.anchor = None;
                        }
                    }
                } else {
                    self.rename_move_h(left, alt, shift);
                }
                cx.notify();
            }
            "a" if cmd => {
                if let Some(r) = self.rename.as_mut() {
                    let end = r.text.chars().count();
                    if end == 0 {
                        r.anchor = None;
                    } else {
                        r.anchor = Some(0);
                        r.cursor = end;
                    }
                }
                cx.notify();
            }
            "c" if cmd => {
                let text = match self.rename_sel() {
                    Some((lo, hi)) => self.rename.as_ref().map(|r| {
                        r.text[char_byte(&r.text, lo)..char_byte(&r.text, hi)].to_string()
                    }),
                    None => self.rename.as_ref().map(|r| r.text.clone()),
                };
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            "x" if cmd => {
                if let Some((lo, hi)) = self.rename_sel() {
                    if let Some(r) = self.rename.as_ref() {
                        let cut =
                            r.text[char_byte(&r.text, lo)..char_byte(&r.text, hi)].to_string();
                        cx.write_to_clipboard(ClipboardItem::new_string(cut));
                    }
                    self.rename_delete_sel();
                    cx.notify();
                }
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.rename_insert(t.trim());
                    cx.notify();
                }
            }
            "backspace" => {
                if !self.rename_delete_sel() {
                    if let Some(r) = self.rename.as_mut() {
                        if r.cursor > 0 {
                            let start = char_byte(&r.text, r.cursor - 1);
                            let stop = char_byte(&r.text, r.cursor);
                            r.text.replace_range(start..stop, "");
                            r.cursor -= 1;
                        }
                    }
                }
                cx.notify();
            }
            "delete" => {
                if !self.rename_delete_sel() {
                    if let Some(r) = self.rename.as_mut() {
                        let end = r.text.chars().count();
                        if r.cursor < end {
                            let start = char_byte(&r.text, r.cursor);
                            let stop = char_byte(&r.text, r.cursor + 1);
                            r.text.replace_range(start..stop, "");
                        }
                    }
                }
                cx.notify();
            }
            _ => {
                if cmd {
                    return;
                }
                if let Some(ch) = ks.key_char.clone() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        self.rename_insert(&ch);
                        cx.notify();
                    }
                }
            }
        }
    }

    /// Duplicate a file/folder as "name copy" (Finder-style), recursively.
    fn duplicate_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else { return };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
        let base = match &ext {
            Some(e) => format!("{stem} copy.{e}"),
            None => format!("{stem} copy"),
        };
        let dest = unique_child(parent, &base);
        let _ = Command::new("cp").arg("-R").arg(&path).arg(&dest).status();
        self.refresh_pane(pane, cx);
    }

    /// Make a Finder alias of `path` in the same folder.
    fn make_alias(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else { return };
        let script = format!(
            "tell application \"Finder\" to make alias file to (POSIX file \"{}\") at (POSIX file \"{}\")",
            path.to_string_lossy(),
            parent.to_string_lossy()
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
        self.refresh_pane(pane, cx);
    }

    /// Compress a file/folder into a `.zip` beside it (Finder uses `ditto`).
    /// Zip a file/folder beside itself, off-thread so a big folder can't
    /// freeze the UI while `ditto` runs.
    fn compress_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else { return };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = unique_child(parent, &format!("{stem}.zip"));
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                let _ = Command::new("ditto")
                    .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                    .arg(&path)
                    .arg(&dest)
                    .status();
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if pane < this.panes.len() {
                    this.refresh_pane(pane, cx);
                }
            });
        })
        .detach();
    }

    /// "Extract Here" (context menu on archives): unpack into a folder named
    /// after the archive, beside it. Runs off-thread; the listing refreshes
    /// when it finishes.
    fn extract_archive(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else { return };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // "proj.tar.gz" extracts into "proj", not "proj.tar".
        let stem = match archive_suffix(&path) {
            Some(sfx) => name[..name.len() - sfx.len()].to_string(),
            None => return,
        };
        let dest = unique_child(parent, &stem);
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                if fs::create_dir_all(&dest).is_err() {
                    return;
                }
                if let Some(mut c) = archive_extract_command(&path) {
                    let _ = c.arg(&dest).stdout(Stdio::null()).stderr(Stdio::null()).status();
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if pane < this.panes.len() {
                    this.refresh_pane(pane, cx);
                }
            });
        })
        .detach();
    }

    /// Run a user script action on `targets`, off the UI thread. The paths are
    /// passed as arguments and as `$SHUFFLE_PATHS`; `$SHUFFLE_DIR` is the pane's
    /// folder (also the working directory). Refreshes the pane when it finishes
    /// (the script may have created/renamed/deleted files).
    fn run_script_action(
        &mut self,
        pane: usize,
        script: PathBuf,
        targets: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let dir = self.tab(pane).current_dir.clone();
        let paths_env = targets
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            cx.background_spawn(async move {
                let _ = Command::new(&script)
                    .args(&targets)
                    .current_dir(&d)
                    .env("SHUFFLE_DIR", &d)
                    .env("SHUFFLE_PATHS", &paths_env)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if pane < this.panes.len() {
                    this.refresh_pane(pane, cx);
                }
            });
        })
        .detach();
    }

    /// Open `path` with a specific application.
    fn open_with(&mut self, app: &Path, path: &Path, cx: &mut Context<Self>) {
        let _ = Command::new("open").arg("-a").arg(app).arg(path).spawn();
        self.close_context_menu(cx);
    }

    /// Set (or clear, index 0) the Finder color label / tag on `path`.
    fn set_tag(&mut self, pane: usize, path: PathBuf, label: u8, cx: &mut Context<Self>) {
        let script = format!(
            "tell application \"Finder\" to set label index of (POSIX file \"{}\" as alias) to {label}",
            path.to_string_lossy()
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
        self.close_context_menu(cx);
        self.refresh_pane(pane, cx);
    }

    /// Rotate an image in place by `degrees` (Quick Action).
    fn rotate_image(&mut self, pane: usize, path: PathBuf, degrees: i32, cx: &mut Context<Self>) {
        let _ = Command::new("sips").arg("-r").arg(degrees.to_string()).arg(&path).status();
        self.close_context_menu(cx);
        self.refresh_after_edit(pane, path, cx);
    }

    /// Convert an image to another format beside the original (Quick Action).
    /// `fmt` is the sips format name; `ext` the new file extension.
    fn convert_image(&mut self, pane: usize, path: PathBuf, fmt: &str, ext: &str, cx: &mut Context<Self>) {
        if let Some(parent) = path.parent() {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let dest = unique_child(parent, &format!("{stem}.{ext}"));
            let _ = Command::new("sips")
                .args(["-s", "format", fmt])
                .arg(&path)
                .arg("--out")
                .arg(&dest)
                .status();
        }
        self.close_context_menu(cx);
        self.refresh_pane(pane, cx);
    }

    /// Remove an image's background via the native Vision helper, writing a new
    /// transparent PNG beside it. Runs in the background (Vision can take a sec).
    fn remove_background(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.close_context_menu(cx);
        let Some(tool) = removebg_path() else { return };
        let Some(parent) = path.parent() else { return };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = unique_child(parent, &format!("{stem} (no background).png"));
        cx.spawn(async move |this, cx| {
            let ok = cx
                .background_spawn(async move {
                    Command::new(&tool)
                        .arg(&path)
                        .arg(&dest)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                })
                .await;
            if ok {
                let _ = this.update(cx, |this, cx| this.refresh_pane(pane, cx));
            }
        })
        .detach();
    }

    /// Compute a folder's total size in the background (once) and cache it, then
    /// repaint so List view can show it. No-op if cached or already computing.
    fn ensure_folder_size(&self, dir: PathBuf, cx: &mut Context<Self>) {
        if folder_size_lookup(&dir).is_some() {
            return;
        }
        let pending = FOLDER_SIZE_PENDING.get_or_init(|| Mutex::new(HashSet::new()));
        if !pending.lock().unwrap().insert(dir.clone()) {
            return; // already in flight
        }
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let size = cx.background_spawn(async move { dir_total_size(&d) }).await;
            let _ = this.update(cx, |_, cx| {
                FOLDER_SIZE
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .unwrap()
                    .insert(dir.clone(), size);
                FOLDER_SIZE_PENDING
                    .get_or_init(|| Mutex::new(HashSet::new()))
                    .lock()
                    .unwrap()
                    .remove(&dir);
                cx.notify();
            });
        })
        .detach();
    }

    /// The badge state for an item: syncing if a download/evict is in flight,
    /// online-only if it's a cloud placeholder, else local (no badge). Only
    /// files under a cloud root are stat'd (cheap, cached) — everything else is
    /// free.
    fn cloud_state(&self, path: &Path, is_dir: bool) -> CloudSync {
        if self.cloud_busy.contains(path) {
            return CloudSync::Syncing;
        }
        if !is_dir && cloud_kind(path).is_some() && cached_dataless(path) {
            return CloudSync::OnlineOnly;
        }
        CloudSync::Local
    }

    /// Download (materialize) online-only cloud files to disk. `path` may be a
    /// file or a folder (downloads its online-only descendants). Runs the
    /// `cloudctl` helper per file in the background; badges show ↻ meanwhile.
    fn cloud_download(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.close_context_menu(cx);
        let Some(tool) = cloudctl_path() else { return };
        // Gather the online-only files to fetch (the file itself, or a folder's
        // dataless descendants — capped so a giant tree can't spawn thousands).
        let targets = collect_cloud_files(&path, /* want_dataless */ true, 2000);
        if targets.is_empty() {
            return;
        }
        for t in &targets {
            self.cloud_busy.insert(t.clone());
        }
        cx.notify();
        let dir = self.tab(pane).current_dir.clone();
        cx.spawn(async move |this, cx| {
            let done = targets.clone();
            cx.background_spawn(async move {
                for f in &targets {
                    let _ = Command::new(&tool).arg("download").arg(f).status();
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                for f in &done {
                    this.cloud_busy.remove(f);
                }
                // Re-read the folder so freshly-materialized files lose the badge.
                if this.tab(pane).current_dir == dir {
                    this.refresh_pane(pane, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Free up space: evict downloaded cloud files back to online-only. iCloud
    /// only (third-party eviction is driven by the provider's own app). `path`
    /// may be a file or a folder (evicts its materialized descendants).
    fn cloud_evict(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.close_context_menu(cx);
        let Some(tool) = cloudctl_path() else { return };
        let targets = collect_cloud_files(&path, /* want_dataless */ false, 5000);
        if targets.is_empty() {
            return;
        }
        for t in &targets {
            self.cloud_busy.insert(t.clone());
        }
        cx.notify();
        let dir = self.tab(pane).current_dir.clone();
        cx.spawn(async move |this, cx| {
            let all = targets.clone();
            let err = cx
                .background_spawn(async move {
                    let mut last_err = None;
                    for f in &targets {
                        let out = Command::new(&tool).arg("evict").arg(f).output();
                        if let Ok(o) = out {
                            if !o.status.success() {
                                last_err = Some(String::from_utf8_lossy(&o.stderr).trim().to_string());
                            }
                        }
                    }
                    last_err
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for f in &all {
                    this.cloud_busy.remove(f);
                }
                if let Some(e) = err.filter(|e| !e.is_empty()) {
                    this.remote_error = Some(e);
                }
                if this.tab(pane).current_dir == dir {
                    this.refresh_pane(pane, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Set the file as the desktop picture on every display (Service).
    fn set_desktop_picture(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let script = format!(
            "tell application \"System Events\" to set picture of every desktop to \"{}\"",
            path.to_string_lossy()
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
        self.close_context_menu(cx);
    }

    /// After an in-place edit, drop stale caches and refresh the listing.
    fn refresh_after_edit(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        PREVIEW_CACHE.with(|c| {
            c.borrow_mut().remove(&path);
        });
        INFO_CACHE.with(|c| {
            c.borrow_mut().remove(&path);
        });
        self.refresh_pane(pane, cx);
        if self.tab(pane).anchor.as_deref() == Some(path.as_path()) {
            let gallery = self.tab(pane).view == ViewMode::Gallery;
            self.ensure_preview(path.clone(), gallery, cx);
            self.ensure_info(path, cx);
        }
    }

    /// Root level of the context menu.
    fn menu_root(
        &self,
        pane: usize,
        target: Option<(PathBuf, bool)>,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        // Remote (SFTP) tabs get a reduced menu — the local-only actions
        // (Quick Actions, Compress, Tags, aliases, Reveal in Finder…) don't
        // apply to a remote path.
        if self.tab(pane).remote.is_some() {
            return self.menu_remote(pane, target, cx);
        }
        let mut items: Vec<AnyElement> = Vec::new();
        if let Some((path, is_dir)) = target {
            let p = path.clone();
            items.push(
                ctx_item("Open", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.open_path(pane, p.clone(), is_dir, cx);
                }))
                .into_any_element(),
            );
            items.push(
                ctx_parent("Open With", cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_menu_view(MenuView::OpenWith, cx);
                }))
                .into_any_element(),
            );
            // Cloud storage: download-on-demand / free up space. Offered for
            // files/folders in a cloud store, when the helper is present.
            if let (Some(kind), true) = (cloud_kind(&path), cloudctl_path().is_some()) {
                let has_online = collect_cloud_files(&path, true, 1).len() == 1;
                let has_local = collect_cloud_files(&path, false, 1).len() == 1;
                if has_online {
                    let label = if is_dir { "Download Contents" } else { "Download Now" };
                    let p = path.clone();
                    items.push(
                        ctx_item(label, cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.cloud_download(pane, p.clone(), cx);
                        }))
                        .into_any_element(),
                    );
                }
                // Eviction is iCloud-only here (third-party is provider-driven).
                if has_local && kind == CloudKind::ICloud {
                    let label = if is_dir { "Free Up Space in Folder" } else { "Free Up Space" };
                    let p = path.clone();
                    items.push(
                        ctx_item(label, cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.cloud_evict(pane, p.clone(), cx);
                        }))
                        .into_any_element(),
                    );
                }
            }
            // Type-specific actions, Finder-style: archives extract in place,
            // app bundles reveal their contents.
            if archive_suffix(&path).is_some() {
                let p = path.clone();
                items.push(
                    ctx_item("Extract Here", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        this.extract_archive(pane, p.clone(), cx);
                    }))
                    .into_any_element(),
                );
            }
            if is_dir && path.extension().is_some_and(|e| e == "app") {
                let p = path.clone();
                items.push(
                    ctx_item(
                        "Show Package Contents",
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.close_context_menu(cx);
                            this.navigate_in(pane, p.clone(), cx);
                        }),
                    )
                    .into_any_element(),
                );
            }
            // User script actions that apply to this item (Scripts folder).
            if prefs().script_actions {
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let acts: Vec<ScriptAction> = discover_script_actions()
                    .into_iter()
                    .filter(|a| script_action_applies(a, &fname, is_dir))
                    .collect();
                if !acts.is_empty() {
                    items.push(ctx_separator().into_any_element());
                    for (i, a) in acts.into_iter().take(12).enumerate() {
                        let script = a.path.clone();
                        let tgt = path.clone();
                        items.push(
                            ctx_item_owned(
                                ("script-action", i),
                                a.name,
                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.close_context_menu(cx);
                                    this.run_script_action(pane, script.clone(), vec![tgt.clone()], cx);
                                }),
                            )
                            .into_any_element(),
                        );
                    }
                }
            }
            items.push(ctx_separator().into_any_element());
            let p = path.clone();
            items.push(
                ctx_item("Rename", cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.close_context_menu(cx);
                    this.begin_rename(pane, p.clone(), window, cx);
                }))
                .into_any_element(),
            );
            let p = path.clone();
            items.push(
                ctx_item("Reveal in Finder", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    let _ = Command::new("open").arg("-R").arg(&p).spawn();
                }))
                .into_any_element(),
            );
            let p = path.clone();
            items.push(
                ctx_item("Copy Path", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
                    this.close_context_menu(cx);
                }))
                .into_any_element(),
            );
            let p = path.clone();
            let already = self.bookmarks.iter().any(|b| b == &p);
            items.push(
                ctx_item(
                    if already { "Remove Bookmark" } else { "Add to Bookmarks" },
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        if already {
                            this.remove_bookmark(&p, cx);
                        } else {
                            this.bookmark_path(p.clone(), cx);
                        }
                    }),
                )
                .into_any_element(),
            );
            // "Add to Group ▸" submenu (only when groups are enabled and exist).
            if prefs().groups_enabled && !self.groups.is_empty() {
                items.push(
                    ctx_parent("Add to Group", cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.set_menu_view(MenuView::AddToGroup, cx);
                    }))
                    .into_any_element(),
                );
            }
            let p = path.clone();
            items.push(
                ctx_item("Duplicate", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.duplicate_entry(pane, p.clone(), cx);
                }))
                .into_any_element(),
            );
            let p = path.clone();
            items.push(
                ctx_item("Make Alias", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.make_alias(pane, p.clone(), cx);
                }))
                .into_any_element(),
            );
            let p = path.clone();
            items.push(
                ctx_item("Compress", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.compress_entry(pane, p.clone(), cx);
                }))
                .into_any_element(),
            );
            // Move to Trash — kept high so it's always visible.
            items.push(ctx_separator().into_any_element());
            let p = path.clone();
            items.push(
                ctx_item("Move to Trash", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.move_to_trash(pane, p.clone(), cx);
                }))
                .into_any_element(),
            );
            items.push(ctx_separator().into_any_element());
            items.push(
                ctx_parent("Tags", cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_menu_view(MenuView::Tags, cx);
                }))
                .into_any_element(),
            );
            items.push(
                ctx_parent("Quick Actions", cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_menu_view(MenuView::QuickActions, cx);
                }))
                .into_any_element(),
            );
            items.push(
                ctx_parent("Services", cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_menu_view(MenuView::Services, cx);
                }))
                .into_any_element(),
            );
            items.push(ctx_separator().into_any_element());
        }
        items.push(
            ctx_item("New Folder", cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.close_context_menu(cx);
                this.new_folder(pane, window, cx);
            }))
            .into_any_element(),
        );
        items.push(
            ctx_item("New File", cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.close_context_menu(cx);
                this.new_file(pane, window, cx);
            }))
            .into_any_element(),
        );
        items
    }

    /// The right-click menu for a remote (SFTP) tab: only actions that work
    /// over the network.
    fn menu_remote(
        &self,
        pane: usize,
        target: Option<(PathBuf, bool)>,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut items: Vec<AnyElement> = Vec::new();
        if let Some((path, is_dir)) = target {
            let p = path.clone();
            items.push(
                ctx_item(
                    if is_dir { "Open" } else { "Open (Download)" },
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        this.open_path(pane, p.clone(), is_dir, cx);
                    }),
                )
                .into_any_element(),
            );
            if !is_dir {
                let p = path.clone();
                items.push(
                    ctx_item("Download to Downloads", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        this.download_remote(pane, p.clone(), None, false, cx);
                    }))
                    .into_any_element(),
                );
            }
            items.push(ctx_separator().into_any_element());
            let p = path.clone();
            items.push(
                ctx_item("Rename", cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.close_context_menu(cx);
                    this.begin_rename(pane, p.clone(), window, cx);
                }))
                .into_any_element(),
            );
            let p = path.clone();
            items.push(
                ctx_item("Copy Path", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
                    this.close_context_menu(cx);
                }))
                .into_any_element(),
            );
            items.push(ctx_separator().into_any_element());
            let p = path.clone();
            items.push(
                ctx_item("Delete from Server", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.confirm_delete = Some((pane, vec![p.clone()]));
                    cx.notify();
                }))
                .into_any_element(),
            );
            items.push(ctx_separator().into_any_element());
        }
        items.push(
            ctx_item("New Folder", cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.close_context_menu(cx);
                this.new_folder(pane, window, cx);
            }))
            .into_any_element(),
        );
        items.push(
            ctx_item("New File", cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.close_context_menu(cx);
                this.new_file(pane, window, cx);
            }))
            .into_any_element(),
        );
        items
    }

    /// "Open With" submenu — apps that can open the target (via LaunchServices).
    fn menu_open_with(&self, _pane: usize, path: PathBuf, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut items: Vec<AnyElement> = vec![
            ctx_item("‹ Back", cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_menu_view(MenuView::Root, cx);
            }))
            .into_any_element(),
            ctx_separator().into_any_element(),
        ];
        let apps = apps_for_file(&path);
        if apps.is_empty() {
            items.push(ctx_disabled("No applications").into_any_element());
        }
        for (i, (name, app)) in apps.into_iter().enumerate() {
            let p = path.clone();
            items.push(
                ctx_app(i, name, cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_with(&app, &p, cx);
                }))
                .into_any_element(),
            );
        }
        items
    }

    /// "Add to Group" submenu — one row per group.
    fn menu_add_to_group(&self, path: PathBuf, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut items: Vec<AnyElement> = vec![
            ctx_item("‹ Back", cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_menu_view(MenuView::Root, cx);
            }))
            .into_any_element(),
            ctx_separator().into_any_element(),
        ];
        if self.groups.is_empty() {
            items.push(ctx_disabled("No groups").into_any_element());
        }
        for (i, g) in self.groups.iter().enumerate() {
            let p = path.clone();
            let has = g.paths.contains(&p);
            let label = if has {
                format!("✓ {}", g.name)
            } else {
                g.name.clone()
            };
            items.push(
                ctx_app(i, label, cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.add_to_group(i, p.clone(), cx);
                }))
                .into_any_element(),
            );
        }
        items
    }

    /// "Tags" submenu — Finder color labels.
    fn menu_tags(&self, pane: usize, path: PathBuf, cx: &Context<Self>) -> Vec<AnyElement> {
        // (name, color dot, Finder label index)
        const TAGS: &[(&str, u32, u8)] = &[
            ("None", 0x6b6b73, 0),
            ("Red", 0xff5f57, 6),
            ("Orange", 0xff9f0a, 7),
            ("Yellow", 0xffd60a, 5),
            ("Green", 0x34c759, 2),
            ("Blue", 0x0a84ff, 4),
            ("Purple", 0xbf5af0, 3),
            ("Gray", 0x8e8e93, 1),
        ];
        let mut items: Vec<AnyElement> = vec![
            ctx_item("‹ Back", cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_menu_view(MenuView::Root, cx);
            }))
            .into_any_element(),
            ctx_separator().into_any_element(),
        ];
        for (i, (name, color, label)) in TAGS.iter().enumerate() {
            let (name, color, label) = (*name, *color, *label);
            let p = path.clone();
            items.push(
                ctx_tag(i, name, color, cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_tag(pane, p.clone(), label, cx);
                }))
                .into_any_element(),
            );
        }
        items
    }

    /// "Quick Actions" submenu — image/PDF operations via built-in tools.
    fn menu_quick_actions(&self, pane: usize, path: PathBuf, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut items: Vec<AnyElement> = vec![
            ctx_item("‹ Back", cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_menu_view(MenuView::Root, cx);
            }))
            .into_any_element(),
            ctx_separator().into_any_element(),
        ];

        let img = is_image(&path);
        let pdf = is_pdf(&path);

        if img {
            let p = path.clone();
            items.push(ctx_item("Rotate Left", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.rotate_image(pane, p.clone(), -90, cx);
            })).into_any_element());
            let p = path.clone();
            items.push(ctx_item("Rotate Right", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.rotate_image(pane, p.clone(), 90, cx);
            })).into_any_element());
        }

        if img || pdf {
            let p = path.clone();
            items.push(ctx_item("Markup", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.close_context_menu(cx);
                let _ = Command::new("open").arg("-a").arg("Preview").arg(&p).spawn();
            })).into_any_element());
        }

        if img {
            let p = path.clone();
            items.push(ctx_item("Create PDF", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.convert_image(pane, p.clone(), "pdf", "pdf", cx);
            })).into_any_element());
            // Convert Image to … (sips formats).
            for (i, (label, fmt, ext)) in [
                ("Convert to JPEG", "jpeg", "jpg"),
                ("Convert to PNG", "png", "png"),
                ("Convert to HEIC", "heic", "heic"),
            ].iter().enumerate()
            {
                let (fmt, ext) = (fmt.to_string(), ext.to_string());
                let p = path.clone();
                items.push(
                    ctx_app(100 + i, label.to_string(), cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.convert_image(pane, p.clone(), &fmt, &ext, cx);
                    }))
                    .into_any_element(),
                );
            }
            // Remove Background (native Vision helper, if compiled in).
            if removebg_path().is_some() {
                let p = path.clone();
                items.push(ctx_item("Remove Background", cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.remove_background(pane, p.clone(), cx);
                })).into_any_element());
            }
        }

        if !img && !pdf {
            items.push(ctx_disabled("No quick actions").into_any_element());
        }
        items
    }

    /// "Services" submenu — a useful, implementable subset of Finder's services.
    fn menu_services(&self, _pane: usize, path: PathBuf, is_dir: bool, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut items: Vec<AnyElement> = vec![
            ctx_item("‹ Back", cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_menu_view(MenuView::Root, cx);
            }))
            .into_any_element(),
            ctx_separator().into_any_element(),
        ];

        let mut any = false;
        if is_image(&path) {
            any = true;
            let p = path.clone();
            items.push(ctx_item("Set Desktop Picture", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_desktop_picture(p.clone(), cx);
            })).into_any_element());
        }

        // "Open in <terminal>" — opens the folder (or the file's folder).
        let dir = if is_dir { path.clone() } else {
            path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone())
        };
        for (i, (name, app)) in installed_terminals().into_iter().enumerate() {
            any = true;
            let d = dir.clone();
            items.push(
                ctx_app(200 + i, format!("Open in {name}"), cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    let _ = Command::new("open").arg("-a").arg(&app).arg(&d).spawn();
                }))
                .into_any_element(),
            );
        }

        if !any {
            items.push(ctx_disabled("No services").into_any_element());
        }
        items
    }

    fn render_context_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let menu = self.context_menu.as_ref().expect("called only when open");
        let pane = menu.pane;
        let items: Vec<AnyElement> = match (menu.view, menu.target.clone()) {
            (MenuView::OpenWith, Some((path, _))) => self.menu_open_with(pane, path, cx),
            (MenuView::AddToGroup, Some((path, _))) => self.menu_add_to_group(path, cx),
            (MenuView::Tags, Some((path, _))) => self.menu_tags(pane, path, cx),
            (MenuView::QuickActions, Some((path, _))) => self.menu_quick_actions(pane, path, cx),
            (MenuView::Services, Some((path, is_dir))) => self.menu_services(pane, path, is_dir, cx),
            (_, target) => self.menu_root(pane, target, cx),
        };

        // Full-window backdrop: any click/right-click outside closes the menu.
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_context_menu(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| this.close_context_menu(cx)),
            )
            .child(
                // Anchored so the menu repositions to stay fully visible near a
                // window edge instead of being clipped.
                anchored()
                    .position(point(px(menu.x), px(menu.y)))
                    .snap_to_window()
                    .child(
                        div()
                            .min_w(px(200.0))
                            .py_1()
                            .bg(menu_style().bg_rgba())
                            .text_color(rgb(menu_style().text))
                            .text_size(px(menu_style().font_px))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(theme().border_strong))
                            .shadow_lg()
                            // Clicks inside the menu shouldn't close it via the backdrop.
                            .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                            .children(items),
                    ),
            )
    }

    /// The "Sort By" dropdown (the Finder-style arrange list).
    fn render_sort_menu(&self, pane: usize, x: f32, y: f32, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let cur = self.tab(pane).sort_key;
        let asc = self.tab(pane).sort_asc;
        const OPTS: &[SortKey] = &[
            SortKey::None,
            SortKey::Name,
            SortKey::Kind,
            SortKey::Modified,
            SortKey::Created,
            SortKey::Size,
        ];
        let mut items: Vec<AnyElement> = Vec::new();
        for (i, k) in OPTS.iter().enumerate() {
            let k = *k;
            let active = k == cur;
            let marker = if active {
                if k == SortKey::None {
                    "•".to_string()
                } else if asc {
                    "▲".to_string()
                } else {
                    "▼".to_string()
                }
            } else {
                " ".to_string()
            };
            items.push(
                div()
                    .id(("sortopt", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_1()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(t.text))
                    .hover(|s| s.bg(rgb(t.selected)))
                    .child(div().flex_none().w(px(12.0)).text_color(rgb(t.accent)).child(marker))
                    .child(k.label().to_string())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.sort_menu = None;
                        this.set_sort(pane, k, cx);
                    }))
                    .into_any_element(),
            );
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.sort_menu = None;
                cx.notify();
            }))
            .child(
                anchored()
                    .position(point(px(x), px(y)))
                    .snap_to_window()
                    .child(
                        div()
                            .min_w(px(180.0))
                            .py_1()
                            .bg(rgb(t.surface))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(t.border_strong))
                            .shadow_lg()
                            .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                            .children(items),
                    ),
            )
    }

    /// The "Move to Trash" confirmation modal.
    fn render_confirm_delete(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let (_, paths) = self.confirm_delete.as_ref().expect("only when open");
        let msg = if paths.len() == 1 {
            format!("Move “{}” to the Trash?", path_label(&paths[0]))
        } else {
            format!("Move {} items to the Trash?", paths.len())
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000066))
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.confirm_delete = None;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(t.surface))
                    .border_1()
                    .border_color(rgb(t.border_strong))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                    .child(div().text_color(rgb(t.text)).child(msg))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(t.text_muted))
                            .child("They can be recovered from the Trash."),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("del-cancel")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(t.text))
                                    .bg(rgb(t.hover))
                                    .hover(|s| s.bg(rgb(t.selected)))
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.confirm_delete = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("del-confirm")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(0xffffff))
                                    .bg(rgb(0xd9544f))
                                    .hover(|s| s.bg(rgb(0xc6433e)))
                                    .child("Move to Trash")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.perform_delete(cx);
                                    })),
                            ),
                    ),
            )
    }

    // ----- Connect to Server -----

    fn open_server_dialog(&mut self, cx: &mut Context<Self>) {
        // First run: ask how to authenticate over SSH (a permission-style
        // prompt), then open the connect dialog. Afterwards, go straight in.
        if !prefs().ssh_configured {
            self.ssh_ask = true;
        } else {
            let mut form = ServerForm::default();
            form.mode = if prefs().ssh_use_system {
                ServerMode::Quick
            } else {
                ServerMode::Credentials
            };
            self.server_dialog = Some(form);
        }
        cx.notify();
    }

    /// Answer the first-run SSH prompt, remember the default, and open the
    /// connect dialog on the matching tab.
    fn choose_ssh_mode(&mut self, use_system: bool, cx: &mut Context<Self>) {
        let mut np = prefs();
        np.ssh_use_system = use_system;
        np.ssh_configured = true;
        apply_prefs(np, cx);
        self.ssh_ask = false;
        let mut form = ServerForm::default();
        form.mode = if use_system {
            ServerMode::Quick
        } else {
            ServerMode::Credentials
        };
        self.server_dialog = Some(form);
        cx.notify();
    }

    /// Submit whichever connect tab is active.
    fn submit_server_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.server_dialog.clone() else {
            return;
        };
        match form.mode {
            ServerMode::Quick => self.connect_to_server(&form.addr, cx),
            ServerMode::Credentials => self.submit_credentials(cx),
        }
    }

    /// Save an SFTP server from the Credentials tab (password → Keychain) and
    /// connect to it.
    fn submit_credentials(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.server_dialog.clone() else {
            return;
        };
        let host = form.host.trim().to_string();
        if host.is_empty() {
            return;
        }
        let name = if form.name.trim().is_empty() {
            host.clone()
        } else {
            form.name.trim().to_string()
        };
        let server = SftpServer {
            name,
            host,
            user: form.user.trim().to_string(),
            port: form.port.trim().parse().unwrap_or(0),
            key: String::new(),
            use_password: true,
            auto_reopen: form.auto_reopen,
        };
        // Only overwrite the Keychain when a password was entered (editing an
        // existing server without retyping keeps the stored one).
        if !form.password.is_empty() {
            keychain_set_password(&server, &form.password);
        }
        let mut list = sftp_servers();
        // When editing, drop the original entry (its target may have changed).
        if let Some(old) = &form.editing {
            if *old != server.display() {
                if let Some(prev) = list.iter().find(|s| &s.display() == old).cloned() {
                    keychain_delete_password(&prev);
                }
            }
            list.retain(|s| &s.display() != old);
        }
        if let Some(existing) = list.iter_mut().find(|s| s.display() == server.display()) {
            *existing = server.clone();
        } else {
            list.push(server.clone());
        }
        apply_sftp_servers(list, cx);
        self.server_dialog = None;
        self.connect_sftp(server, cx);
    }

    /// Open the connect dialog pre-filled to edit an existing server.
    fn edit_server(&mut self, server: &SftpServer, cx: &mut Context<Self>) {
        self.sidebar_menu = None;
        self.server_dialog = Some(ServerForm::editing_server(server));
        cx.notify();
    }

    /// Hand a server URL off to macOS (`open smb://…`), which shows the native
    /// auth prompt and mounts the share under /Volumes. Records it in history.
    /// `sftp://[user@]host[:port]` instead saves an SFTP server and connects to
    /// it in-app (browsing over SSH, no mount).
    fn connect_to_server(&mut self, raw: &str, cx: &mut Context<Self>) {
        let mut url = raw.trim().to_string();
        if url.is_empty() {
            return;
        }
        // sftp:// / ssh:// → an in-app SFTP server (saved + connected).
        if let Some(rest) = url
            .strip_prefix("sftp://")
            .or_else(|| url.strip_prefix("ssh://"))
        {
            if let Some(mut server) = parse_sftp_url(rest) {
                // Carry the dialog's "reconnect on launch" toggle onto the server.
                server.auto_reopen =
                    self.server_dialog.as_ref().map(|f| f.auto_reopen).unwrap_or(false);
                let mut list = sftp_servers();
                if let Some(existing) = list.iter_mut().find(|s| s.display() == server.display()) {
                    existing.auto_reopen = server.auto_reopen;
                } else {
                    list.push(server.clone());
                }
                apply_sftp_servers(list, cx);
                self.server_dialog = None;
                self.connect_sftp(server, cx);
                return;
            }
        }
        // Default to SMB when no scheme is given.
        if !url.contains("://") {
            url = format!("smb://{url}");
        }
        let _ = Command::new("open").arg(&url).spawn();

        // Most-recent-first, de-duplicated, capped.
        self.server_history.retain(|u| u != &url);
        self.server_history.insert(0, url);
        self.server_history.truncate(10);
        write_string_list("servers.txt", &self.server_history);

        self.server_dialog = None;
        cx.notify();
    }

    fn handle_server_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        match ks.key.as_str() {
            "escape" => {
                self.server_dialog = None;
                cx.notify();
            }
            "enter" => self.submit_server_dialog(cx),
            "tab" => {
                if let Some(f) = self.server_dialog.as_mut() {
                    if f.mode == ServerMode::Credentials {
                        f.next_field();
                    }
                }
                cx.notify();
            }
            "backspace" => {
                if let Some(f) = self.server_dialog.as_mut() {
                    f.active_field().pop();
                }
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(f) = self.server_dialog.as_mut() {
                        let s = t.trim().to_string();
                        f.active_field().push_str(&s);
                    }
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return;
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        if let Some(f) = self.server_dialog.as_mut() {
                            f.active_field().push_str(ch);
                        }
                        cx.notify();
                    }
                }
            }
        }
    }

    fn render_server_dialog(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let form = self.server_dialog.clone().unwrap_or_default();
        let mode = form.mode;

        // Mode tabs (Quick Connect | Credentials).
        let mode_tab = |label: &'static str, m: ServerMode| {
            let active = mode == m;
            div()
                .id(label)
                .flex_1()
                .flex()
                .justify_center()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { rgb(t.text) } else { rgb(t.text_muted) })
                .bg(if active { rgb(t.surface) } else { rgba(0x00000000) })
                .hover(|s| s.bg(rgb(t.hover)))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if let Some(f) = this.server_dialog.as_mut() {
                        f.mode = m;
                    }
                    cx.notify();
                }))
        };
        let tabs = div()
            .flex()
            .gap_1()
            .p_1()
            .rounded_md()
            .bg(rgb(t.bg))
            .child(mode_tab("Quick Connect", ServerMode::Quick))
            .child(mode_tab("Credentials", ServerMode::Credentials));

        // A labeled input row for the Credentials tab.
        let field_row = |label: &'static str, cf: CredField, value: &str, secret: bool| {
            let focused = mode == ServerMode::Credentials && form.field == cf;
            let empty = value.is_empty();
            let shown = if empty {
                label.to_string()
            } else if secret {
                "\u{2022}".repeat(value.chars().count())
            } else {
                value.to_string()
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(rgb(t.text_muted)).child(label))
                .child(
                    div()
                        .id(label)
                        .flex()
                        .items_center()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(t.bg))
                        .border_1()
                        .border_color(if focused { rgb(t.accent) } else { rgb(t.border) })
                        .text_color(rgb(if empty { t.text_dim } else { t.text }))
                        .cursor_pointer()
                        .child(shown)
                        .when(focused, |d| {
                            d.child(div().w(px(1.5)).h(px(15.0)).ml(px(1.0)).bg(rgb(t.text)))
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(f) = this.server_dialog.as_mut() {
                                f.field = cf;
                            }
                            cx.notify();
                        })),
                )
        };

        let body: AnyElement = match mode {
            ServerMode::Quick => {
                let empty = form.addr.is_empty();
                let shown = if empty {
                    "smb://host/share  or  sftp://user@host".to_string()
                } else {
                    form.addr.clone()
                };
                let mut recent = div().flex().flex_col().gap_1();
                if !self.server_history.is_empty() {
                    recent = recent
                        .child(div().text_xs().text_color(rgb(t.text_dim)).child("Recent"));
                    for u in self.server_history.iter().take(5) {
                        let target = u.clone();
                        recent = recent.child(
                            div()
                                .id(SharedString::from(format!("srv-{u}")))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .text_color(rgb(t.text_muted))
                                .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
                                .child(u.clone())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.connect_to_server(&target, cx);
                                })),
                        );
                    }
                }
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_xs().text_color(rgb(t.text_muted)).child(
                        "Address — smb://, afp://, ftp://, or sftp://user@host to browse over SSH.",
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(t.bg))
                            .border_1()
                            .border_color(rgb(t.accent))
                            .text_color(rgb(if empty { t.text_dim } else { t.text }))
                            .child(shown)
                            .child(div().w(px(1.5)).h(px(15.0)).ml(px(1.0)).bg(rgb(t.text))),
                    )
                    .child(recent)
                    .into_any_element()
            }
            ServerMode::Credentials => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(field_row("Name (optional)", CredField::Name, &form.name, false))
                .child(field_row("Host", CredField::Host, &form.host, false))
                .child(field_row("Username", CredField::User, &form.user, false))
                .child(field_row("Port (optional)", CredField::Port, &form.port, false))
                .child(field_row("Password", CredField::Password, &form.password, true))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(t.text_dim))
                        .child("Password is stored in your macOS Keychain — Tab moves between fields."),
                )
                .into_any_element(),
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000066))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.server_dialog = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(t.surface))
                    .border_1()
                    .border_color(rgb(t.border_strong))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(t.text))
                            .child("Connect to Server"),
                    )
                    .child(tabs)
                    .child(body)
                    .child(
                        // "Reconnect on launch" toggle for this server.
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .id("srv-autoreopen")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(f) = this.server_dialog.as_mut() {
                                    f.auto_reopen = !f.auto_reopen;
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .h(px(16.0))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(rgb(t.border_strong))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .when(form.auto_reopen, |s| {
                                        s.bg(rgb(t.accent)).text_color(rgb(0xffffff))
                                    })
                                    .child(if form.auto_reopen { "✓" } else { "" }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(t.text_muted))
                                    .child("Reconnect on launch"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("srv-cancel")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(t.text))
                                    .bg(rgb(t.hover))
                                    .hover(|s| s.bg(rgb(t.selected)))
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.server_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("srv-connect")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(0xffffff))
                                    .bg(rgb(t.accent))
                                    .hover(|s| s.bg(Theme::alpha(t.accent, 0xdd)))
                                    .child("Connect")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.submit_server_dialog(cx);
                                    })),
                            ),
                    ),
            )
    }

    /// First-run SSH prompt: choose how Shuffle authenticates over SSH.
    fn render_ssh_prompt(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let choice =
            |id: &'static str, title: &'static str, desc: &'static str, use_system: bool, primary: bool| {
                div()
                    .id(id)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(t.bg))
                    .border_1()
                    .border_color(if primary { rgb(t.accent) } else { rgb(t.border) })
                    .hover(|s| s.border_color(rgb(t.accent)))
                    .child(div().text_color(rgb(t.text)).child(title))
                    .child(div().text_xs().text_color(rgb(t.text_muted)).child(desc))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.choose_ssh_mode(use_system, cx);
                    }))
            };
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000066))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.ssh_ask = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(t.surface))
                    .border_1()
                    .border_color(rgb(t.border_strong))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(t.text))
                            .child("Connect over SSH"),
                    )
                    .child(div().text_xs().text_color(rgb(t.text_muted)).child(
                        "How should Shuffle authenticate to SFTP servers? You can change this \
                         anytime in Settings \u{2192} Connections.",
                    ))
                    .child(choice(
                        "ssh-use-system",
                        "Use my ~/.ssh configuration",
                        "Connect with your existing SSH keys, config aliases, and known_hosts. \
                         Nothing is stored in the app. Recommended.",
                        true,
                        true,
                    ))
                    .child(choice(
                        "ssh-credentials",
                        "Enter credentials per server",
                        "Type a username and password for each server; passwords are saved in \
                         your macOS Keychain.",
                        false,
                        false,
                    )),
            )
    }

    /// The "New Group" naming dialog.
    fn render_group_dialog(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let name = self.group_dialog.clone().unwrap_or_default();
        let placeholder = name.is_empty();
        let shown = if placeholder { "Group name".to_string() } else { name.clone() };

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000066))
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.group_dialog = None;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(t.surface))
                    .border_1()
                    .border_color(rgb(t.border_strong))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                    .child(div().text_color(rgb(t.text)).child("New Group"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(t.bg))
                            .border_1()
                            .border_color(rgb(t.accent))
                            .text_color(rgb(if placeholder { t.text_dim } else { t.text }))
                            .child(shown),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("grp-cancel")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(t.text))
                                    .bg(rgb(t.hover))
                                    .hover(|s| s.bg(rgb(t.selected)))
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.group_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("grp-create")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(rgb(0xffffff))
                                    .bg(rgb(t.accent))
                                    .hover(|s| s.bg(Theme::alpha(t.accent, 0xdd)))
                                    .child("Create")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        if let Some(name) = this.group_dialog.clone() {
                                            this.create_group(&name, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }

    /// The sidebar right-click context menu (New Group / Remove / Delete Group).
    fn render_sidebar_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let (x, y, target) = self.sidebar_menu.clone().expect("only when open");
        let groups_on = prefs().groups_enabled;
        let mut items: Vec<AnyElement> = Vec::new();
        match target {
            SidebarTarget::Empty => {
                if groups_on {
                    items.push(
                        ctx_item("New Group", cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.close_sidebar_menu(cx);
                            this.open_group_dialog(cx);
                        }))
                        .into_any_element(),
                    );
                }
            }
            SidebarTarget::Bookmark(p) => {
                items.push(
                    ctx_item("Remove Bookmark", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.remove_bookmark(&p, cx);
                    }))
                    .into_any_element(),
                );
            }
            SidebarTarget::GroupHeader(idx) => {
                if groups_on {
                    items.push(
                        ctx_item("New Group", cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.close_sidebar_menu(cx);
                            this.open_group_dialog(cx);
                        }))
                        .into_any_element(),
                    );
                    items.push(ctx_separator().into_any_element());
                }
                items.push(
                    ctx_item("Delete Group", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.delete_group(idx, cx);
                    }))
                    .into_any_element(),
                );
            }
            SidebarTarget::GroupMember(idx, p) => {
                items.push(
                    ctx_item("Remove from Group", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.remove_from_group(idx, &p, cx);
                    }))
                    .into_any_element(),
                );
            }
            SidebarTarget::Sftp(server) => {
                let s = server.clone();
                items.push(
                    ctx_item("Connect", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.connect_sftp(s.clone(), cx);
                    }))
                    .into_any_element(),
                );
                let s = server.clone();
                items.push(
                    ctx_item("Edit\u{2026}", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.edit_server(&s, cx);
                    }))
                    .into_any_element(),
                );
                let s = server.clone();
                items.push(
                    ctx_item("Remove", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        if s.use_password {
                            keychain_delete_password(&s);
                        }
                        let list: Vec<SftpServer> = sftp_servers()
                            .into_iter()
                            .filter(|x| x.display() != s.display())
                            .collect();
                        apply_sftp_servers(list, cx);
                    }))
                    .into_any_element(),
                );
            }
            SidebarTarget::Tab(pane, tab) => {
                items.push(
                    ctx_item("New Tab", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.new_tab_in(pane, cx);
                    }))
                    .into_any_element(),
                );
                items.push(
                    ctx_item("Duplicate Tab", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.duplicate_tab(pane, tab, cx);
                    }))
                    .into_any_element(),
                );
                items.push(ctx_separator().into_any_element());
                items.push(
                    ctx_item("Close Tab", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.close_tab(pane, tab, cx);
                    }))
                    .into_any_element(),
                );
                // Only offer "Close Other Tabs" when there's more than one.
                if self.pane(pane).tabs.len() > 1 {
                    items.push(
                        ctx_item("Close Other Tabs", cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.close_sidebar_menu(cx);
                            this.close_other_tabs(pane, tab, cx);
                        }))
                        .into_any_element(),
                    );
                }
            }
            SidebarTarget::NavBar(pane) => {
                items.push(
                    ctx_item("New Tab", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        this.new_tab_in(pane, cx);
                    }))
                    .into_any_element(),
                );
                let dir = self.tab(pane).current_dir.clone();
                let is_remote = self.tab(pane).remote.is_some();
                let dc = dir.clone();
                items.push(
                    ctx_item("Copy Path", cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_sidebar_menu(cx);
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            dc.to_string_lossy().into_owned(),
                        ));
                    }))
                    .into_any_element(),
                );
                // Reveal-in-Finder / terminal only make sense for local folders.
                if !is_remote {
                    let dr = dir.clone();
                    items.push(
                        ctx_item("Reveal in Finder", cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.close_sidebar_menu(cx);
                            let _ = Command::new("open").arg(&dr).spawn();
                        }))
                        .into_any_element(),
                    );
                }
            }
        }
        if items.is_empty() {
            items.push(ctx_disabled("No actions").into_any_element());
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close_sidebar_menu(cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| this.close_sidebar_menu(cx)))
            .child(
                anchored().position(point(px(x), px(y))).snap_to_window().child(
                    div()
                        .min_w(px(180.0))
                        .py_1()
                        .bg(menu_style().bg_rgba())
                        .text_color(rgb(menu_style().text))
                        .text_size(px(menu_style().font_px))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme().border_strong))
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                        .children(items),
                ),
            )
    }

    /// Build the ~/ fuzzy index on a background thread, then store it.
    fn build_index(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let index = cx
                .background_spawn(async move { FileIndex::build(home_dir()) })
                .await;
            this.update(cx, |this, cx| {
                this.index = Some(Arc::new(index));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Quietly ask GitHub for the latest published release and, if it's newer
    /// than this build (and the user hasn't dismissed that exact version),
    /// surface the update banner. Best-effort: any failure (offline, timeout,
    /// unparseable) just leaves the banner hidden. Never blocks the UI.
    fn check_for_update(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let Some(tag) = cx.background_spawn(async move { fetch_latest_tag() }).await else {
                return;
            };
            // Only surface real upgrades over the running build.
            if parse_version(&tag) <= parse_version(env!("CARGO_PKG_VERSION")) {
                return;
            }
            // Respect a prior "dismiss" of this exact version so we don't nag.
            if config_string("update_skip.txt").as_deref() == Some(tag.as_str()) {
                return;
            }
            this.update(cx, |this, cx| {
                this.update = Some(UpdateStatus::Available(tag));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hide the update banner and remember the dismissed version so it doesn't
    /// come back on the next launch (until a still-newer release appears).
    fn dismiss_update(&mut self, cx: &mut Context<Self>) {
        if let Some(UpdateStatus::Available(ver)) = self.update.take() {
            write_config_string("update_skip.txt", &ver);
        }
        cx.notify();
    }

    /// Download the newest release, verify it's genuinely ours (notarized +
    /// signed by our Team ID), then swap the running bundle and relaunch. Kicked
    /// off by the banner's "Update" button. All network/disk work is off-thread;
    /// the actual replace happens in a tiny helper that waits for us to quit.
    fn start_self_update(&mut self, cx: &mut Context<Self>) {
        let ver = match &self.update {
            Some(UpdateStatus::Available(v)) => v.clone(),
            _ => return,
        };
        let bundle = current_app_bundle();
        self.update = Some(UpdateStatus::Downloading(ver));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_spawn(async move {
                    let bundle = bundle.ok_or_else(|| "couldn't locate the app bundle".to_string())?;
                    let ready = download_and_verify_update()?;
                    Ok::<_, String>((bundle, ready))
                })
                .await;
            match prepared {
                Ok((bundle, ready)) => {
                    if let Err(e) = launch_swap_and_relaunch(&bundle, &ready) {
                        ready.detach(); // unmount the dmg we couldn't hand off
                        this.update(cx, |this, cx| {
                            this.update = Some(UpdateStatus::Failed(e));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    // Helper is armed and waiting for us to exit; quit so it can
                    // replace the bundle and relaunch the new version.
                    let _ = cx.update(|cx| cx.quit());
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.update = Some(UpdateStatus::Failed(e));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// The slim update bar shown under the titlebar (available / downloading /
    /// failed). Returns `None` when there's nothing to show.
    fn render_update_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let status = self.update.clone()?;
        let t = theme();
        let bar = div()
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_1p5()
            .bg(rgb(t.accent))
            .text_color(rgb(0xffffff))
            .text_xs();

        let bar = match status {
            UpdateStatus::Available(ver) => bar
                .child(div().child("↑").text_sm())
                .child(div().flex_1().child(format!(
                    "Shuffle {ver} is available (you have v{}).",
                    env!("CARGO_PKG_VERSION")
                )))
                .child(
                    div()
                        .id("update-install")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgba(0xffffff33))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0xffffff55)))
                        .child("Update")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.start_self_update(cx)),
                        ),
                )
                .child(
                    div()
                        .id("update-dismiss")
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0xffffff33)))
                        .child("✕")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.dismiss_update(cx)),
                        ),
                ),
            UpdateStatus::Downloading(ver) => bar.child(div().child("↓").text_sm()).child(
                div()
                    .flex_1()
                    .child(format!("Downloading Shuffle {ver}… the app will relaunch itself.")),
            ),
            UpdateStatus::Failed(reason) => bar
                .child(div().child("⚠").text_sm())
                .child(div().flex_1().child(format!("Update failed: {reason}.")))
                .child(
                    div()
                        .id("update-manual")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgba(0xffffff33))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0xffffff55)))
                        .child("Download manually")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                let _ = Command::new("open").arg(DMG_URL).spawn();
                                this.update = None;
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .id("update-dismiss-failed")
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0xffffff33)))
                        .child("✕")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.update = None;
                                cx.notify();
                            }),
                        ),
                ),
        };
        Some(bar.into_any_element())
    }

    /// Current scrolled distance from the top, in pixels.
    fn current_scrolled(&self, pane: usize) -> f32 {
        let state = self.tab(pane).scroll_handle.0.borrow();
        (-(f64::from(state.base_handle.offset().y) as f32)).max(0.0)
    }

    fn begin_scroll_drag(&mut self, pane: usize, y: f32) {
        self.scroll_drag = Some(ScrollDrag {
            pane,
            start_y: y,
            start_scrolled: self.current_scrolled(pane),
        });
    }

    fn update_scroll_drag(&mut self, y: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.scroll_drag else {
            return;
        };
        if drag.pane >= self.panes.len() {
            return;
        }
        let state = self.tab(drag.pane).scroll_handle.0.borrow();
        let base = &state.base_handle;
        let viewport = f64::from(base.bounds().size.height) as f32;
        let max = f64::from(base.max_offset().height) as f32;
        if viewport <= 1.0 || max <= 1.0 {
            return;
        }
        let content = viewport + max;
        let thumb_h = (viewport * viewport / content).clamp(28.0, viewport);
        let travel = (viewport - thumb_h).max(1.0);
        // Thumb moves `delta` px; scale that to content-scroll distance.
        let delta = y - drag.start_y;
        let new_scrolled = (drag.start_scrolled + delta * (max / travel)).clamp(0.0, max);
        let x = base.offset().x;
        base.set_offset(point(x, px(-new_scrolled)));
        drop(state);
        cx.notify();
    }

    fn end_scroll_drag(&mut self, cx: &mut Context<Self>) {
        // Releasing the thumb counts as the last scroll: starts the fade timer.
        if let Some(drag) = self.scroll_drag.take() {
            if drag.pane < self.panes.len() {
                self.mark_scrolled(drag.pane, cx);
            }
        }
    }

    // ----- command palette (Cmd+P) -----

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        if self.palette_open {
            self.query.clear();
            self.query_cursor = 0;
            self.query_anchor = None;
            self.palette_hist_pos = None;
            self.selected = 0;
            self.refresh_palette(cx);
            window.focus(&self.focus);
        }
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_actions = None;
        cx.notify();
    }

    /// Default items shown when the query is empty: the available commands.
    fn default_commands(&self) -> Vec<PaletteItem> {
        vec![PaletteItem {
            title: "Copy current directory".to_string(),
            subtitle: self.active_tab().current_dir.to_string_lossy().into_owned(),
            action: Action::CopyDir,
            is_dir: true,
        }]
    }

    /// Recompute the palette contents for the current query. Path-like queries
    /// resolve synchronously; name queries kick off a debounced async search.
    fn refresh_palette(&mut self, cx: &mut Context<Self>) {
        self.search_gen = self.search_gen.wrapping_add(1);
        let gen = self.search_gen;
        self.selected = 0;
        self.palette_actions = None;
        self.palette_scroll.set_offset(point(px(0.0), px(0.0)));
        let q = self.query.trim().to_string();

        if q.is_empty() {
            self.palette_items = self.default_commands();
            cx.notify();
            return;
        }

        // Path mode: browse a directory live. Split the query into a base dir
        // and a partial name; list the base's entries, ranked (typo-tolerant)
        // by how well they match the partial.
        if q.starts_with('/') || q.starts_with('~') {
            let (base, partial) = split_path_query(&q);
            if !base.is_dir() {
                self.palette_items = vec![PaletteItem {
                    title: "Path not found".to_string(),
                    subtitle: base.to_string_lossy().into_owned(),
                    action: Action::None,
                    is_dir: false,
                }];
                cx.notify();
                return;
            }

            let mut scored: Vec<(i32, String, bool)> = list_dir_names(&base)
                .into_iter()
                .map(|(name, is_dir)| {
                    let score = if partial.is_empty() {
                        0
                    } else {
                        match_score(&partial, &name)
                    };
                    (score, name, is_dir)
                })
                .collect();

            if partial.is_empty() {
                // Directories first, then alphabetical.
                scored.sort_by(|a, b| {
                    b.2.cmp(&a.2)
                        .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                });
            } else {
                // Best match first; ties → dirs first, then alphabetical.
                scored.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| b.2.cmp(&a.2))
                        .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                });
            }

            self.palette_items = scored
                .into_iter()
                .take(50)
                .map(|(_, name, is_dir)| {
                    let path = base.join(&name);
                    let subtitle = path.to_string_lossy().into_owned();
                    PaletteItem {
                        title: name,
                        subtitle,
                        action: Action::Open(path, is_dir),
                        is_dir,
                    }
                })
                .collect();
            cx.notify();
            return;
        }

        // Operator-driven search: content:/kind:/ext:/size:/date: run a
        // dedicated global search (Spotlight for content, the index otherwise)
        // instead of the plain fuzzy name lookup below.
        let fq = FilterQuery::parse(&q);
        if fq.content.is_some() || fq.has_local_filters() {
            self.palette_items = Vec::new();
            self.selected = 0;
            cx.notify();
            let index = self.index.clone();
            let home = home_dir();
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                if !this.update(cx, |this, _| this.search_gen == gen).unwrap_or(false) {
                    return;
                }
                let results = cx
                    .background_spawn(async move {
                        palette_operator_search(&fq, index.as_deref(), &home)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this.search_gen != gen {
                        return;
                    }
                    this.palette_items = results;
                    this.selected = 0;
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }

        // Search from the very first character (the in-memory index is fast
        // enough). Empty queries were already handled above.

        // Built-in commands (e.g. Settings) show instantly, ahead of the
        // async file results, so typing "settings" surfaces it immediately.
        self.palette_items = command_matches(&q);
        self.selected = 0;
        cx.notify();
        let index = self.index.clone();
        cx.spawn(async move |this, cx| {
            // Debounce: bail if a newer keystroke superseded us.
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let current = this.update(cx, |this, _| this.search_gen == gen).unwrap_or(false);
            if !current {
                return;
            }
            // In-memory index (fast, true fuzzy) once built; Spotlight until then.
            let qs = q.clone();
            let hits = match index {
                Some(idx) => cx.background_spawn(async move { idx.search(&qs, 40) }).await,
                None => cx.background_spawn(async move { search_filesystem(&qs) }).await,
            };
            this.update(cx, |this, cx| {
                if this.search_gen != gen {
                    return;
                }
                let mut items = command_matches(&q);
                items.extend(hits.into_iter().map(|(name, path, is_dir)| {
                    let subtitle = path.to_string_lossy().into_owned();
                    PaletteItem {
                        title: name,
                        subtitle,
                        action: Action::Open(path, is_dir),
                        is_dir,
                    }
                }));
                this.palette_items = items;
                this.selected = 0;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn move_selection(&mut self, delta: i64, cx: &mut Context<Self>) {
        let n = self.palette_items.len();
        if n == 0 {
            return;
        }
        let next = (self.selected as i64 + delta).clamp(0, n as i64 - 1);
        self.selected = next as usize;
        // Keep the highlighted row in view as you arrow through a long list.
        self.palette_scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Read-only scroll indicator for the palette results list.
    fn palette_scrollbar_thumb(&self) -> Option<AnyElement> {
        static_scrollbar_thumb(&self.palette_scroll)
    }

    fn activate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.palette_items.get(self.selected) else {
            return;
        };
        let action = item.action.clone();
        self.record_palette_history();
        match action {
            Action::Open(path, is_dir) => {
                self.close_palette(cx);
                if is_dir {
                    self.navigate_to(path, cx);
                } else {
                    // Enter opens the file itself (Spotlight-style); ⌘K offers
                    // "Show in enclosing folder" for the old navigate behavior.
                    let _ = Command::new("open").arg(&path).spawn();
                }
            }
            Action::CopyDir => {
                let text = self.active_tab().current_dir.to_string_lossy().into_owned();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.close_palette(cx);
            }
            Action::OpenSettings => {
                self.close_palette(cx);
                open_settings_window(cx);
            }
            Action::None => {}
        }
    }

    /// The ⌘K actions available for the palette's selected item, with labels.
    fn palette_action_list(&self) -> Vec<(PaletteAction, &'static str)> {
        match self.palette_items.get(self.selected).map(|i| &i.action) {
            Some(Action::Open(_, true)) => vec![
                (PaletteAction::Open, "Open folder"),
                (PaletteAction::OpenNewTab, "Open in new tab"),
                (PaletteAction::RevealFinder, "Reveal in Finder"),
                (PaletteAction::CopyPath, "Copy path"),
            ],
            Some(Action::Open(_, false)) => vec![
                (PaletteAction::Open, "Open file"),
                (PaletteAction::RevealShuffle, "Show in enclosing folder"),
                (PaletteAction::OpenNewTab, "Open folder in new tab"),
                (PaletteAction::RevealFinder, "Reveal in Finder"),
                (PaletteAction::CopyPath, "Copy path"),
            ],
            _ => Vec::new(),
        }
    }

    /// Run one of the ⌘K actions on the palette's selected item.
    fn run_palette_action(&mut self, act: PaletteAction, cx: &mut Context<Self>) {
        let Some(item) = self.palette_items.get(self.selected) else {
            return;
        };
        let Action::Open(path, is_dir) = item.action.clone() else {
            return;
        };
        self.record_palette_history();
        let parent = || path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone());
        match act {
            PaletteAction::Open => {
                self.close_palette(cx);
                if is_dir {
                    self.navigate_to(path, cx);
                } else {
                    let _ = Command::new("open").arg(&path).spawn();
                }
            }
            PaletteAction::RevealShuffle => {
                self.close_palette(cx);
                self.navigate_to(parent(), cx);
                let pane = self.active_pane;
                self.reveal_in_pane(pane, path, cx);
            }
            PaletteAction::OpenNewTab => {
                self.close_palette(cx);
                let pane = self.active_pane;
                self.new_tab_in(pane, cx);
                let dir = if is_dir { path.clone() } else { parent() };
                self.navigate_in(pane, dir, cx);
                if !is_dir {
                    self.reveal_in_pane(pane, path, cx);
                }
            }
            PaletteAction::RevealFinder => {
                let _ = Command::new("open").arg("-R").arg(&path).spawn();
                self.close_palette(cx);
            }
            PaletteAction::CopyPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    path.to_string_lossy().into_owned(),
                ));
                self.close_palette(cx);
            }
        }
    }

    /// Run a bound key action against the active pane.
    fn run_key_action(&mut self, action: KeyAction, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let anchor = self.tab(pane).anchor.clone();
        match action {
            KeyAction::CommandPalette => self.toggle_palette(window, cx),
            KeyAction::NewTab => self.new_tab_in(pane, cx),
            KeyAction::CloseTab => {
                let a = self.panes[pane].active;
                self.close_tab(pane, a, cx);
            }
            KeyAction::Find => self.open_find(pane, cx),
            KeyAction::SelectAll => {
                let all: HashSet<PathBuf> = self.display_paths(pane).into_iter().collect();
                self.tab_mut(pane).selection = all;
                cx.notify();
            }
            KeyAction::NewFile => self.new_file(pane, window, cx),
            KeyAction::NewFolder => self.new_folder(pane, window, cx),
            KeyAction::Rename => {
                if let Some(p) = anchor {
                    self.begin_rename(pane, p, window, cx);
                }
            }
            KeyAction::CopyPath => {
                if let Some(p) = anchor {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().into_owned()));
                }
            }
            KeyAction::Duplicate => {
                if let Some(p) = anchor {
                    self.duplicate_entry(pane, p, cx);
                }
            }
            KeyAction::MakeAlias => {
                if let Some(p) = anchor {
                    self.make_alias(pane, p, cx);
                }
            }
            KeyAction::Compress => {
                if let Some(p) = anchor {
                    self.compress_entry(pane, p, cx);
                }
            }
            KeyAction::MoveToTrash => self.request_delete(pane, cx),
            KeyAction::RevealInFinder => {
                if let Some(p) = anchor {
                    let _ = Command::new("open").arg("-R").arg(&p).spawn();
                }
            }
            KeyAction::Open => {
                if let Some(p) = anchor {
                    let is_dir = p.is_dir();
                    self.open_path(pane, p, is_dir, cx);
                }
            }
            // Palette editing actions run inside the palette handler, not here.
            KeyAction::PaletteCursorStart
            | KeyAction::PaletteCursorEnd
            | KeyAction::PaletteSelectAll
            | KeyAction::PaletteDeleteToStart
            | KeyAction::PaletteHistoryPrev
            | KeyAction::PaletteHistoryNext => {}
        }
    }

    /// Top-level key handling: Cmd+P toggles; while open, drive the palette.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let key = ks.key.as_str();

        // The first-run SSH prompt: Escape dismisses; clicks pick a mode.
        if self.ssh_ask {
            if key == "escape" {
                self.ssh_ask = false;
                cx.notify();
            }
            return;
        }
        // The Connect-to-Server dialog captures all typing while open.
        if self.server_dialog.is_some() {
            self.handle_server_key(ev, cx);
            return;
        }

        // The New Group dialog captures all typing while open.
        if self.group_dialog.is_some() {
            self.handle_group_key(ev, cx);
            return;
        }

        // The delete-confirmation dialog captures Enter (confirm) / Esc (cancel).
        if self.confirm_delete.is_some() {
            match key {
                "enter" => self.perform_delete(cx),
                "escape" => {
                    self.confirm_delete = None;
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        // While an inline rename is active, keys feed the rename field.
        if self.rename.is_some() {
            self.handle_rename_key(ev, cx);
            return;
        }

        // While editing the path bar, keys feed the text field.
        if self.active_tab().editing_path.is_some() {
            self.handle_path_edit_key(ev, cx);
            return;
        }

        // While the in-directory find bar is open, keys feed the filter.
        if self.active_tab().find_query.is_some() {
            self.handle_find_key(ev, cx);
            return;
        }

        // While the terminal input is focused, keys feed it (except Cmd+P, which
        // still opens the palette).
        if self.term_focused && prefs().terminal && !(cmd && key == "p") {
            self.handle_term_key(ev, cx);
            return;
        }

        // Dispatch a configured keybinding. When the palette is open only its
        // own toggle acts, so typed characters still reach the query.
        let kc = canon_keystroke(ks);
        if let Some(action) = keymap().action_for(&kc) {
            // Palette editing actions are handled inside the palette block only.
            if !action.is_palette() && (!self.palette_open || action == KeyAction::CommandPalette) {
                self.run_key_action(action, window, cx);
                return;
            }
        }
        if key == "escape" && self.context_menu.is_some() {
            self.close_context_menu(cx);
            return;
        }
        if !self.palette_open {
            // Arrow keys move the selection within the active pane.
            if !cmd {
                let pane = self.active_pane;
                let delta = match key {
                    "up" => Some((0, -1)),
                    "down" => Some((0, 1)),
                    "left" => Some((-1, 0)),
                    "right" => Some((1, 0)),
                    _ => None,
                };
                if let Some((dx, dy)) = delta {
                    self.arrow_move(pane, dx, dy, cx);
                    return;
                }
                // Spacebar → Quick Look the selection, like Finder.
                if key == "space" {
                    self.quick_look(pane);
                    return;
                }
            }
            // Enter renames the item under the mouse first (point-and-rename),
            // else the focused one (Finder-style; not rebindable). The hover
            // target must still live in that pane's current folder — a stale
            // hover from before a navigation must never rename off-screen.
            if !cmd && key == "enter" {
                let hovered = self.hovered.clone().filter(|(hp, p)| {
                    *hp < self.panes.len()
                        && p.parent() == Some(self.tab(*hp).current_dir.as_path())
                        && p.exists()
                });
                if let Some((hpane, p)) = hovered {
                    self.begin_rename(hpane, p, window, cx);
                } else {
                    let pane = self.active_pane;
                    if let Some(sel) = self.tab(pane).anchor.clone() {
                        self.begin_rename(pane, sel, window, cx);
                    }
                }
            }
            // Backspace / Delete asks to move the selection to Trash.
            if !cmd && (key == "backspace" || key == "delete") {
                self.request_delete(self.active_pane, cx);
            }
            return;
        }

        // Rebindable palette editing actions (resolved against the keymap so
        // they can be changed in Settings › Keybinds). History actions apply
        // only when the palette-history setting is on.
        let km = keymap();
        let end = self.query.chars().count();
        if km.get(KeyAction::PaletteCursorStart) == Some(kc.as_str()) {
            self.query_cursor = 0;
            self.query_anchor = None;
            cx.notify();
            return;
        }
        if km.get(KeyAction::PaletteCursorEnd) == Some(kc.as_str()) {
            self.query_cursor = end;
            self.query_anchor = None;
            cx.notify();
            return;
        }
        if km.get(KeyAction::PaletteSelectAll) == Some(kc.as_str()) {
            if self.query.is_empty() {
                self.query_anchor = None;
            } else {
                self.query_anchor = Some(0);
                self.query_cursor = end;
            }
            cx.notify();
            return;
        }
        if km.get(KeyAction::PaletteDeleteToStart) == Some(kc.as_str()) {
            self.palette_kill_before();
            self.refresh_palette(cx);
            return;
        }
        if prefs().palette_history {
            if km.get(KeyAction::PaletteHistoryPrev) == Some(kc.as_str()) {
                self.palette_history_prev(cx);
                return;
            }
            if km.get(KeyAction::PaletteHistoryNext) == Some(kc.as_str()) {
                self.palette_history_next(cx);
                return;
            }
        }

        // ⌘K toggles the actions panel for the selected file/folder.
        if cmd && key == "k" {
            if self.palette_actions.is_some() {
                self.palette_actions = None;
            } else if !self.palette_action_list().is_empty() {
                self.palette_actions = Some(0);
            }
            cx.notify();
            return;
        }
        // While the actions panel is open, arrows/Enter drive it; anything
        // else closes it and falls through to normal palette handling.
        if let Some(sel) = self.palette_actions {
            let acts = self.palette_action_list();
            let handled = match key {
                "escape" => {
                    self.palette_actions = None;
                    true
                }
                "up" => {
                    self.palette_actions = Some(sel.saturating_sub(1));
                    true
                }
                "down" => {
                    self.palette_actions =
                        Some((sel + 1).min(acts.len().saturating_sub(1)));
                    true
                }
                "enter" => {
                    self.palette_actions = None;
                    if let Some(&(act, _)) = acts.get(sel) {
                        self.run_palette_action(act, cx);
                    }
                    true
                }
                _ => {
                    self.palette_actions = None;
                    false
                }
            };
            cx.notify();
            if handled {
                return;
            }
        }

        match key {
            "escape" => self.close_palette(cx),
            "enter" => self.activate_selection(cx),
            "down" => self.move_selection(1, cx),
            "up" => self.move_selection(-1, cx),
            // Left/Right move the text cursor. Option jumps by word; Shift
            // extends the selection (Option+Shift selects a word at a time).
            "left" => self.palette_move_h(true, ks.modifiers.alt, ks.modifiers.shift, cx),
            "right" => self.palette_move_h(false, ks.modifiers.alt, ks.modifiers.shift, cx),
            "backspace" => {
                self.palette_backspace();
                self.refresh_palette(cx);
            }
            "v" if cmd => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.palette_insert(text.trim());
                    self.refresh_palette(cx);
                }
            }
            _ => {
                if cmd {
                    return; // ignore other Cmd-combos
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(|c| c.is_control()) {
                        self.palette_insert(ch);
                        self.refresh_palette(cx);
                    }
                }
            }
        }
    }

    // ----- palette text editing (cursor-aware) -----

    /// Byte offset of char index `i` in the query (or its end).
    fn query_byte(&self, i: usize) -> usize {
        self.query
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.query.len())
    }

    /// The current selection as a sorted `(start, end)` char range, if any.
    fn query_sel(&self) -> Option<(usize, usize)> {
        match self.query_anchor {
            Some(a) if a != self.query_cursor => {
                Some((a.min(self.query_cursor), a.max(self.query_cursor)))
            }
            _ => None,
        }
    }

    /// Delete the selected range (if any), leaving the cursor at its start.
    /// Returns true when a selection was removed. Always clears the anchor.
    fn query_delete_sel(&mut self) -> bool {
        if let Some((lo, hi)) = self.query_sel() {
            let (bl, bh) = (self.query_byte(lo), self.query_byte(hi));
            self.query.replace_range(bl..bh, "");
            self.query_cursor = lo;
            self.query_anchor = None;
            true
        } else {
            self.query_anchor = None;
            false
        }
    }

    /// New cursor char index one step (or one word) left/right of the cursor.
    fn query_h_target(&self, left: bool, word: bool) -> usize {
        let end = self.query.chars().count();
        match (left, word) {
            (true, true) => prev_word_boundary(&self.query, self.query_cursor),
            (true, false) => self.query_cursor.saturating_sub(1),
            (false, true) => next_word_boundary(&self.query, self.query_cursor),
            (false, false) => (self.query_cursor + 1).min(end),
        }
    }

    /// Move (or extend) the palette text cursor. `word` jumps by word,
    /// `select` extends the selection instead of collapsing it.
    fn palette_move_h(&mut self, left: bool, word: bool, select: bool, cx: &mut Context<Self>) {
        if select {
            if self.query_anchor.is_none() {
                self.query_anchor = Some(self.query_cursor);
            }
            self.query_cursor = self.query_h_target(left, word);
            if self.query_anchor == Some(self.query_cursor) {
                self.query_anchor = None;
            }
        } else if let Some((lo, hi)) = self.query_sel() {
            // Plain arrow collapses a selection: char-move to the edge, word-move
            // continues past it.
            self.query_cursor = if word {
                self.query_h_target(left, word)
            } else if left {
                lo
            } else {
                hi
            };
            self.query_anchor = None;
        } else {
            self.query_cursor = self.query_h_target(left, word);
            self.query_anchor = None;
        }
        cx.notify();
    }

    /// Insert `s` at the cursor (replacing the selection first, if any).
    fn palette_insert(&mut self, s: &str) {
        self.query_delete_sel();
        let b = self.query_byte(self.query_cursor);
        self.query.insert_str(b, s);
        self.query_cursor += s.chars().count();
        self.palette_hist_pos = None;
    }

    /// Delete the char before the cursor (or the whole selection).
    fn palette_backspace(&mut self) {
        if self.query_delete_sel() {
            return;
        }
        if self.query_cursor == 0 {
            return;
        }
        let start = self.query_byte(self.query_cursor - 1);
        let end = self.query_byte(self.query_cursor);
        self.query.replace_range(start..end, "");
        self.query_cursor -= 1;
    }

    /// Ctrl+U: delete everything before the cursor, keeping what's after.
    fn palette_kill_before(&mut self) {
        let b = self.query_byte(self.query_cursor);
        self.query.replace_range(0..b, "");
        self.query_cursor = 0;
        self.query_anchor = None;
    }

    /// Up in history mode: step to the previous (older) query.
    fn palette_history_prev(&mut self, cx: &mut Context<Self>) {
        if self.palette_hist.is_empty() {
            return;
        }
        let pos = match self.palette_hist_pos {
            None => self.palette_hist.len() - 1,
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.palette_hist_pos = Some(pos);
        self.query = self.palette_hist[pos].clone();
        self.query_cursor = self.query.chars().count();
        self.query_anchor = None;
        self.refresh_palette(cx);
    }

    /// Down in history mode: step to the next (newer) query, past the newest
    /// returns to an empty live query.
    fn palette_history_next(&mut self, cx: &mut Context<Self>) {
        match self.palette_hist_pos {
            None => {}
            Some(p) if p + 1 >= self.palette_hist.len() => {
                self.palette_hist_pos = None;
                self.query.clear();
                self.query_cursor = 0;
                self.query_anchor = None;
                self.refresh_palette(cx);
            }
            Some(p) => {
                let np = p + 1;
                self.palette_hist_pos = Some(np);
                self.query = self.palette_hist[np].clone();
                self.query_cursor = self.query.chars().count();
                self.query_anchor = None;
                self.refresh_palette(cx);
            }
        }
    }

    /// Record a submitted query into the palette history (when enabled).
    fn record_palette_history(&mut self) {
        if !prefs().palette_history {
            return;
        }
        let q = self.query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.palette_hist.retain(|h| h != &q);
        self.palette_hist.push(q);
        let overflow = self.palette_hist.len().saturating_sub(50);
        if overflow > 0 {
            self.palette_hist.drain(0..overflow);
        }
        write_string_list("palette_history.txt", &self.palette_hist);
    }

    fn render_palette(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        // Fit the list to its content, capped — so there's no empty gray space
        // below a short result set, and it scrolls once it's long.
        let visible = self.palette_items.len().min(PALETTE_MAX_ROWS);
        let list_h = visible as f32 * PALETTE_ROW_H;

        let rows: Vec<AnyElement> = self
            .palette_items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let selected = i == self.selected;
                // Commands get a glyph (gear for Settings); files/dirs get real
                // Finder icons.
                let icon: AnyElement = if matches!(item.action, Action::OpenSettings) {
                    div().text_color(rgb(t.text_muted)).child("⚙").into_any_element()
                } else {
                    let dir_like = item.is_dir || matches!(item.action, Action::CopyDir);
                    icon_element(Path::new(&item.subtitle), dir_like)
                };
                let base = div()
                    .id(("pal", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(PALETTE_ROW_H))
                    .cursor_pointer();
                let base = if selected {
                    base.bg(rgb(t.selected))
                } else {
                    base.hover(|s| s.bg(rgb(t.hover)))
                };
                base.child(div().flex_none().w(px(18.0)).flex().justify_center().child(icon))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .min_w_0()
                            .flex_1()
                            .child(div().flex_none().text_color(rgb(t.text)).child(item.title.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(t.text_muted))
                                    .child(item.subtitle.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.selected = i;
                        this.activate_selection(cx);
                    }))
                    .into_any_element()
            })
            .collect();

        // Input line: query with a caret, or a dim placeholder. A selection
        // (whole via Cmd+A, or a word range via Option+Shift+Arrow) is drawn as
        // a highlighted span between the unselected head and tail.
        let input = if self.query.is_empty() {
            div()
                .text_color(rgb(t.text_dim))
                .child("Type a path, or a file/folder name…")
        } else if let Some((lo, hi)) = self.query_sel() {
            let (bl, bh) = (self.query_byte(lo), self.query_byte(hi));
            div()
                .flex()
                .text_color(rgb(t.text))
                .child(div().child(self.query[..bl].to_string()))
                .child(
                    div()
                        .bg(Theme::alpha(t.accent, 0x66))
                        .rounded_sm()
                        .child(self.query[bl..bh].to_string()),
                )
                .child(div().child(self.query[bh..].to_string()))
        } else {
            // Insert a caret bar at the cursor position.
            let cursor = self.query_cursor.min(self.query.chars().count());
            let b = self.query_byte(cursor);
            let (before, after) = self.query.split_at(b);
            div()
                .text_color(rgb(t.text))
                .child(format!("{before}\u{2502}{after}"))
        };

        // What Enter would do to the current selection (footer hint).
        let enter_label = match self.palette_items.get(self.selected).map(|i| &i.action) {
            Some(Action::Open(_, true)) => "Open folder",
            Some(Action::Open(_, false)) => "Open file",
            Some(_) => "Run",
            None => "Open",
        };
        let has_actions = !self.palette_action_list().is_empty();

        let panel = div()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            // Opacity is user-configurable (Settings → Appearance); default is
            // nearly opaque so the palette doesn't read as a confusing overlay.
            .bg(Theme::alpha(t.surface, palette_alpha()))
            .rounded_lg()
            .border_1()
            .border_color(rgb(t.border_strong))
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(t.border_strong))
                    .child(div().flex_none().text_color(rgb(t.accent)).child("›"))
                    .child(input),
            )
            // Scrollable, height-capped results with a scroll indicator.
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("palette-results")
                            .h(px(list_h))
                            .overflow_y_scroll()
                            .track_scroll(&self.palette_scroll)
                            .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| {
                                cx.notify()
                            }))
                            .flex()
                            .flex_col()
                            .children(rows),
                    )
                    .children(self.palette_scrollbar_thumb()),
            )
            // Footer: what you can do with the selection.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(t.border_strong))
                    .text_xs()
                    .text_color(rgb(t.text_dim))
                    .child(palette_hint("↩", enter_label))
                    .when(has_actions, |f| f.child(palette_hint("⌘K", "Actions")))
                    .child(palette_hint("↑↓", "Navigate"))
                    .child(palette_hint("esc", "Close")),
            );

        // Wrapper: positions the palette and hosts the ⌘K dropdown OUTSIDE the
        // panel's overflow_hidden, so the menu is never clipped.
        let mut wrap = div().relative().mt(px(90.0)).w(px(680.0)).child(panel);

        // The ⌘K actions panel: drops down from the palette's bottom-right
        // corner (there's always window space below — the palette hugs the top).
        if let Some(sel) = self.palette_actions {
            let acts = self.palette_action_list();
            if !acts.is_empty() {
                let sel = sel.min(acts.len() - 1);
                wrap = wrap.child(
                    div()
                        .absolute()
                        .top(relative(1.0))
                        .right(px(10.0))
                        .mt(px(6.0))
                        .w(px(240.0))
                        .flex()
                        .flex_col()
                        .py_1()
                        .bg(Theme::alpha(t.surface, 0xf7))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(t.border_strong))
                        .shadow_lg()
                        .children(acts.into_iter().enumerate().map(|(i, (act, label))| {
                            let selected = i == sel;
                            div()
                                .id(("pal-act", i))
                                .px_3()
                                .py_1()
                                .cursor_pointer()
                                .text_color(rgb(t.text))
                                .when(selected, |s| s.bg(rgb(t.selected)))
                                .when(!selected, |s| s.hover(|s| s.bg(rgb(t.hover))))
                                .child(label)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.palette_actions = None;
                                    this.run_palette_action(act, cx);
                                }))
                                .into_any_element()
                        })),
                );
            }
        }

        // Backdrop covering the window, with the panel near the top.
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .justify_center()
            // Align to top so the panel hugs its content height instead of
            // stretching to fill the whole window.
            .items_start()
            .bg(rgba(0x00000033))
            // Block scroll/click from reaching the file list behind the palette.
            .occlude()
            .child(wrap)
    }

    /// A floating, draggable scrollbar thumb sized/positioned from the list's
    /// scroll state. Returns `None` when the content fits (or isn't measured yet).
    fn scrollbar_thumb(&self, pane: usize, cx: &Context<Self>) -> Option<AnyElement> {
        let state = self.tab(pane).scroll_handle.0.borrow();
        let base = &state.base_handle;
        let viewport = f64::from(base.bounds().size.height) as f32;
        let max = f64::from(base.max_offset().height) as f32;
        if viewport <= 1.0 || max <= 1.0 {
            return None;
        }
        let scrolled = (-(f64::from(base.offset().y) as f32)).clamp(0.0, max);
        let content = viewport + max;
        let min_thumb = 28.0_f32;
        let thumb_h = (viewport * viewport / content).clamp(min_thumb, viewport);
        let thumb_top = (viewport - thumb_h) * (scrolled / max);

        // macOS-style overlay bar: solid while scrolling or dragging, fades out
        // shortly after the last scroll, and disappears entirely once faded.
        let dragging = self.scroll_drag.is_some_and(|d| d.pane == pane);
        let tab = self.tab(pane);
        let idle = tab.last_scroll.elapsed().as_secs_f32();
        if !dragging && idle > SCROLLBAR_LINGER + SCROLLBAR_FADE {
            return None;
        }
        // Derive the thumb from the theme's text color so it stays visible on
        // light themes (a hardcoded white thumb vanished on light backgrounds).
        let color = if dragging {
            Theme::alpha(theme().text, 0x66)
        } else {
            Theme::alpha(theme().text, 0x33)
        };

        let thumb = div()
            .id(("scrollbar-thumb", pane))
            .absolute()
            .top(px(thumb_top))
            .right(px(2.0))
            .w(px(8.0))
            .h(px(thumb_h))
            .rounded_full()
            .bg(color)
            .cursor(CursorStyle::PointingHand)
            .hover(|s| s.bg(Theme::alpha(theme().text, 0x55)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.begin_scroll_drag(pane, f64::from(ev.position.y) as f32);
                    cx.notify();
                }),
            );

        if dragging || idle < SCROLLBAR_LINGER {
            Some(thumb.into_any_element())
        } else {
            // Idle: ease the thumb's opacity out. The epoch keys the animation
            // so each scroll burst restarts the fade from opaque.
            Some(
                thumb
                    .with_animation(
                        SharedString::from(format!("sb-fade-{pane}-{}", tab.scroll_epoch)),
                        Animation::new(Duration::from_millis((SCROLLBAR_FADE * 1000.0) as u64))
                            .with_easing(ease_out_quint()),
                        move |el, t| el.opacity(1.0 - t),
                    )
                    .into_any_element(),
            )
        }
    }

    fn begin_resize(&mut self, col: Column, x: f32) {
        self.resize = Some(Resize {
            col,
            start_x: x,
            start_w: self.widths.get(col),
        });
    }

    fn update_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some(resize) = self.resize {
            self.widths.set(resize.col, resize.start_w + (x - resize.start_x));
            cx.notify();
        }
    }

    fn end_resize(&mut self) {
        self.resize = None;
    }

    /// Load `dir` as a pane's current directory: re-read contents, update the
    /// breadcrumb's deepest-tail, record it as most-recent, and persist. Does
    /// NOT touch back/forward history (callers manage that).
    fn load_dir_in(&mut self, pane: usize, dir: PathBuf, cx: &mut Context<Self>) {
        self.rename = None;
        clear_column_cache();
        {
            let tab = self.tab_mut(pane);
            // Keep the grayed-out forward tail in the breadcrumb when moving to an
            // ancestor of where we were; otherwise the tail resets to here.
            let keep = tab.deepest.as_ref().is_some_and(|d| d.starts_with(&dir));
            if !keep {
                tab.deepest = Some(dir.clone());
            }
            tab.current_dir = dir.clone();
            tab.editing_path = None;
            tab.find_query = None;
            tab.find_results.clear();
            tab.content_hits = None;
            tab.content_for = None;
            tab.selection.clear();
            tab.anchor = None;
            tab.col_chain.clear();
            tab.col_active = 0;
        }
        // Recents / last-dir are for local browsing only — a remote path would
        // be treated as a (missing) local folder on next launch.
        if self.tab(pane).remote.is_none() {
            save_last_dir(&dir);
            self.recents.retain(|p| p != &dir);
            self.recents.insert(0, dir);
            self.recents.truncate(RECENTS_CAP);
            write_path_list("recents.txt", &self.recents);
        }

        // Fast first paint + background metadata fill.
        self.reload_pane(pane, cx);
    }

    /// Navigate a pane into `dir` if it is a directory. New navigation truncates
    /// any forward history, then appends `dir` as the new tip.
    fn navigate_in(&mut self, pane: usize, dir: PathBuf, cx: &mut Context<Self>) {
        if dir == self.tab(pane).current_dir {
            return;
        }
        // Local dirs are validated with a stat; remote dirs come from a listing
        // (we can't stat them locally), so trust the caller.
        if self.tab(pane).remote.is_none() && !dir.is_dir() {
            return;
        }
        self.active_pane = pane;
        let tab = self.tab_mut(pane);
        tab.history.truncate(tab.hist_pos + 1);
        tab.history.push(dir.clone());
        tab.hist_pos = tab.history.len() - 1;
        self.load_dir_in(pane, dir, cx);
    }

    /// Navigate the active pane (used by the palette).
    fn navigate_to(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.navigate_in(self.active_pane, dir, cx);
    }

    /// Go to the previous directory in a pane's history (the back arrow).
    fn go_back(&mut self, pane: usize, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let tab = self.tab_mut(pane);
        if tab.hist_pos == 0 {
            return;
        }
        tab.hist_pos -= 1;
        let dir = tab.history[tab.hist_pos].clone();
        self.load_dir_in(pane, dir, cx);
    }

    /// Go to the next directory in a pane's history (the forward arrow).
    fn go_forward(&mut self, pane: usize, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let tab = self.tab_mut(pane);
        if tab.hist_pos + 1 >= tab.history.len() {
            return;
        }
        tab.hist_pos += 1;
        let dir = tab.history[tab.hist_pos].clone();
        self.load_dir_in(pane, dir, cx);
    }

    /// Enter path-edit mode for a pane: the bar becomes an editable text field.
    fn begin_path_edit(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let text = self.tab(pane).current_dir.display().to_string();
        let end = text.chars().count();
        let tab = self.tab_mut(pane);
        tab.editing_path = Some(text);
        tab.path_cursor = end;
        tab.path_anchor = None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// The path editor's selection as a sorted `(start, end)` char range.
    fn path_sel(&self, pane: usize) -> Option<(usize, usize)> {
        let tab = self.tab(pane);
        match tab.path_anchor {
            Some(a) if a != tab.path_cursor => {
                Some((a.min(tab.path_cursor), a.max(tab.path_cursor)))
            }
            _ => None,
        }
    }

    /// Delete the path editor's selected range (if any), placing the cursor at
    /// its start. Returns true when something was removed. Clears the anchor.
    fn path_delete_sel(&mut self, pane: usize) -> bool {
        if let Some((lo, hi)) = self.path_sel(pane) {
            let tab = self.tab_mut(pane);
            if let Some(s) = tab.editing_path.as_mut() {
                let (bl, bh) = (char_byte(s, lo), char_byte(s, hi));
                s.replace_range(bl..bh, "");
            }
            tab.path_cursor = lo;
            tab.path_anchor = None;
            true
        } else {
            self.tab_mut(pane).path_anchor = None;
            false
        }
    }

    /// Insert `s` at the path cursor, replacing any selection first.
    fn path_insert(&mut self, pane: usize, s: &str) {
        self.path_delete_sel(pane);
        let tab = self.tab_mut(pane);
        if let Some(buf) = tab.editing_path.as_mut() {
            let b = char_byte(buf, tab.path_cursor);
            buf.insert_str(b, s);
            tab.path_cursor += s.chars().count();
        }
    }

    /// Move (or extend) the path text cursor. `word` jumps by word, `select`
    /// extends the selection (Option+Shift selects a word at a time).
    fn path_move_h(&mut self, pane: usize, left: bool, word: bool, select: bool) {
        let tab = self.tab(pane);
        let Some(s) = tab.editing_path.clone() else {
            return;
        };
        let end = s.chars().count();
        let cursor = tab.path_cursor.min(end);
        let target = match (left, word) {
            (true, true) => prev_word_boundary(&s, cursor),
            (true, false) => cursor.saturating_sub(1),
            (false, true) => next_word_boundary(&s, cursor),
            (false, false) => (cursor + 1).min(end),
        };
        let sel = self.path_sel(pane);
        let tab = self.tab_mut(pane);
        if select {
            if tab.path_anchor.is_none() {
                tab.path_anchor = Some(cursor);
            }
            tab.path_cursor = target;
            if tab.path_anchor == Some(target) {
                tab.path_anchor = None;
            }
        } else if let Some((lo, hi)) = sel {
            tab.path_cursor = if word {
                target
            } else if left {
                lo
            } else {
                hi
            };
            tab.path_anchor = None;
        } else {
            tab.path_cursor = target;
            tab.path_anchor = None;
        }
    }

    /// Keystrokes while the path bar is being edited (acts on the active pane).
    fn handle_path_edit_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let alt = ks.modifiers.alt;
        let shift = ks.modifiers.shift;
        let pane = self.active_pane;
        match ks.key.as_str() {
            "escape" => {
                self.tab_mut(pane).editing_path = None;
                cx.notify();
            }
            "enter" => {
                if let Some(text) = self.tab_mut(pane).editing_path.take() {
                    let path = expand_path(text.trim());
                    if path.is_dir() {
                        self.navigate_in(pane, path, cx);
                    }
                }
                cx.notify();
            }
            // Cmd+Left/Right jump to start/end (extending with Shift); otherwise
            // Left/Right move by char, Option by word, Shift extends selection.
            "left" => {
                if cmd {
                    let tab = self.tab_mut(pane);
                    if shift && tab.path_anchor.is_none() {
                        tab.path_anchor = Some(tab.path_cursor);
                    }
                    tab.path_cursor = 0;
                    if !shift {
                        tab.path_anchor = None;
                    }
                } else {
                    self.path_move_h(pane, true, alt, shift);
                }
                cx.notify();
            }
            "right" => {
                if cmd {
                    let end = self
                        .tab(pane)
                        .editing_path
                        .as_ref()
                        .map_or(0, |s| s.chars().count());
                    let tab = self.tab_mut(pane);
                    if shift && tab.path_anchor.is_none() {
                        tab.path_anchor = Some(tab.path_cursor);
                    }
                    tab.path_cursor = end;
                    if !shift {
                        tab.path_anchor = None;
                    }
                } else {
                    self.path_move_h(pane, false, alt, shift);
                }
                cx.notify();
            }
            "a" if cmd => {
                let end = self
                    .tab(pane)
                    .editing_path
                    .as_ref()
                    .map_or(0, |s| s.chars().count());
                let tab = self.tab_mut(pane);
                if end == 0 {
                    tab.path_anchor = None;
                } else {
                    tab.path_anchor = Some(0);
                    tab.path_cursor = end;
                }
                cx.notify();
            }
            "backspace" => {
                if !self.path_delete_sel(pane) {
                    let tab = self.tab_mut(pane);
                    if tab.path_cursor > 0 {
                        if let Some(s) = tab.editing_path.as_mut() {
                            let start = char_byte(s, tab.path_cursor - 1);
                            let stop = char_byte(s, tab.path_cursor);
                            s.replace_range(start..stop, "");
                        }
                        tab.path_cursor -= 1;
                    }
                }
                cx.notify();
            }
            "c" if cmd => {
                // Copy the selection if there is one, else the whole path.
                let text = match self.path_sel(pane) {
                    Some((lo, hi)) => self.tab(pane).editing_path.as_ref().map(|s| {
                        s[char_byte(s, lo)..char_byte(s, hi)].to_string()
                    }),
                    None => self.tab(pane).editing_path.clone(),
                };
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.path_insert(pane, t.trim());
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return; // leave other Cmd-combos alone
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        self.path_insert(pane, ch);
                        cx.notify();
                    }
                }
            }
        }
    }

    // ----- in-directory find (the "/" filter) -----

    /// Open the find bar for a pane, filtering only its current directory.
    fn open_find(&mut self, pane: usize, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let tab = self.tab_mut(pane);
        tab.find_query = Some(String::new());
        tab.find_cursor = 0;
        tab.find_anchor = None;
        self.recompute_find(pane);
        cx.notify();
    }

    /// The find query's selected range (char indices), if any.
    fn find_sel(&self, pane: usize) -> Option<(usize, usize)> {
        let tab = self.tab(pane);
        match tab.find_anchor {
            Some(a) if a != tab.find_cursor => {
                Some((a.min(tab.find_cursor), a.max(tab.find_cursor)))
            }
            _ => None,
        }
    }

    /// Delete the find query's selected range (if any); cursor goes to its start.
    fn find_delete_sel(&mut self, pane: usize) -> bool {
        if let Some((lo, hi)) = self.find_sel(pane) {
            let tab = self.tab_mut(pane);
            if let Some(s) = tab.find_query.as_mut() {
                let (bl, bh) = (char_byte(s, lo), char_byte(s, hi));
                s.replace_range(bl..bh, "");
            }
            tab.find_cursor = lo;
            tab.find_anchor = None;
            true
        } else {
            self.tab_mut(pane).find_anchor = None;
            false
        }
    }

    /// Insert `s` at the find cursor, replacing any selection first.
    fn find_insert(&mut self, pane: usize, s: &str) {
        self.find_delete_sel(pane);
        let tab = self.tab_mut(pane);
        if let Some(buf) = tab.find_query.as_mut() {
            let b = char_byte(buf, tab.find_cursor);
            buf.insert_str(b, s);
            tab.find_cursor += s.chars().count();
        }
    }

    /// Move (or extend) the find text cursor. `word` jumps by word, `select`
    /// extends the selection (Option+Shift selects a word at a time).
    fn find_move_h(&mut self, pane: usize, left: bool, word: bool, select: bool) {
        let tab = self.tab(pane);
        let Some(s) = tab.find_query.clone() else {
            return;
        };
        let end = s.chars().count();
        let cursor = tab.find_cursor.min(end);
        let target = match (left, word) {
            (true, true) => prev_word_boundary(&s, cursor),
            (true, false) => cursor.saturating_sub(1),
            (false, true) => next_word_boundary(&s, cursor),
            (false, false) => (cursor + 1).min(end),
        };
        let sel = self.find_sel(pane);
        let tab = self.tab_mut(pane);
        if select {
            if tab.find_anchor.is_none() {
                tab.find_anchor = Some(cursor);
            }
            tab.find_cursor = target;
            if tab.find_anchor == Some(target) {
                tab.find_anchor = None;
            }
        } else if let Some((lo, hi)) = sel {
            tab.find_cursor = if word {
                target
            } else if left {
                lo
            } else {
                hi
            };
            tab.find_anchor = None;
        } else {
            tab.find_cursor = target;
            tab.find_anchor = None;
        }
    }

    /// Recompute a pane's `find_results`. Empty query shows every entry;
    /// otherwise filters + ranks by similarity (typo-tolerant).
    fn recompute_find(&mut self, pane: usize) {
        let tab = self.tab_mut(pane);
        let Some(q) = tab.find_query.as_deref() else {
            return;
        };
        let q = q.trim();
        if q.is_empty() {
            tab.find_results = (0..tab.entries.len()).collect();
            return;
        }
        let fq = FilterQuery::parse(q);
        let has_text = !fq.text.is_empty();
        // Content search resolves asynchronously; until its hits for THIS term
        // land, a content: query matches nothing (the spinner-free "searching"
        // state). update_content_search() recomputes when mdfind returns.
        let content_ready = match &fq.content {
            None => None,
            Some(term) => {
                if tab.content_for.as_deref() == Some(term.as_str()) {
                    tab.content_hits.as_ref()
                } else {
                    tab.find_results.clear();
                    return;
                }
            }
        };
        let dir = tab.current_dir.clone();
        let plain = fq.text.clone();

        // Sort order: when a column sort is active OR there's no free text to
        // rank by, keep the list's existing order; otherwise rank by match.
        let keep_order = tab.sort_key != SortKey::None || !has_text;

        let mut scored: Vec<(i32, usize)> = Vec::new();
        for (i, e) in tab.entries.iter().enumerate() {
            if !fq.matches_entry(&e.name, e.is_dir, e.size, e.modified) {
                continue;
            }
            if let Some(hits) = content_ready {
                if !hits.contains(&dir.join(&e.name)) {
                    continue;
                }
            }
            let score = if has_text {
                match find_score(&plain, &e.name) {
                    Some(s) => s,
                    None => continue,
                }
            } else {
                0
            };
            scored.push((score, i));
        }

        if !keep_order {
            // Best score first; ties → dirs first, then alphabetical.
            let entries = &tab.entries;
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| entries[b.1].is_dir.cmp(&entries[a.1].is_dir))
                    .then_with(|| {
                        entries[a.1].name.to_lowercase().cmp(&entries[b.1].name.to_lowercase())
                    })
            });
        }
        tab.find_results = scored.into_iter().map(|(_, i)| i).collect();
    }

    /// Kick off (or clear) the Spotlight content search for a pane's filter.
    /// Called whenever the filter text changes. Runs `mdfind` scoped to the
    /// folder off-thread, then recomputes the filtered list.
    fn update_content_search(&mut self, pane: usize, cx: &mut Context<Self>) {
        let term = self
            .tab(pane)
            .find_query
            .as_deref()
            .map(|q| FilterQuery::parse(q).content)
            .unwrap_or(None);

        let Some(term) = term else {
            // No content: operator — drop any stale hits.
            let tab = self.tab_mut(pane);
            if tab.content_for.is_some() {
                tab.content_hits = None;
                tab.content_for = None;
            }
            return;
        };
        // Already resolved for this exact term → nothing to do.
        if self.tab(pane).content_for.as_deref() == Some(term.as_str()) {
            return;
        }
        let dir = self.tab(pane).current_dir.clone();
        let gen = {
            let tab = self.tab_mut(pane);
            tab.content_gen += 1;
            tab.content_gen
        };
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let t = term.clone();
            let hits = cx
                .background_spawn(async move { mdfind_content(&d, &t) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if pane >= this.panes.len() || this.tab(pane).content_gen != gen {
                    return;
                }
                let tab = this.tab_mut(pane);
                tab.content_hits = Some(hits);
                tab.content_for = Some(term);
                this.recompute_find(pane);
                cx.notify();
            });
        })
        .detach();
    }

    /// Keystrokes while the find bar is open (acts on the active pane).
    fn handle_find_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let alt = ks.modifiers.alt;
        let shift = ks.modifiers.shift;
        let pane = self.active_pane;
        match ks.key.as_str() {
            "escape" => {
                let tab = self.tab_mut(pane);
                tab.find_query = None;
                tab.find_results.clear();
                tab.content_hits = None;
                tab.content_for = None;
                cx.notify();
            }
            "enter" => {
                // Open the top match: navigate into dirs, open files.
                let tab = self.tab(pane);
                if let Some(&i) = tab.find_results.first() {
                    let entry = &tab.entries[i];
                    let target = tab.current_dir.join(&entry.name);
                    let is_dir = entry.is_dir;
                    let t = self.tab_mut(pane);
                    t.find_query = None;
                    t.find_results.clear();
                    self.open_path(pane, target, is_dir, cx);
                } else {
                    cx.notify();
                }
            }
            // Cmd+Left/Right jump to start/end (Shift extends); otherwise Left/
            // Right move by char, Option by word, Shift extends the selection.
            "left" => {
                if cmd {
                    let tab = self.tab_mut(pane);
                    if shift && tab.find_anchor.is_none() {
                        tab.find_anchor = Some(tab.find_cursor);
                    }
                    tab.find_cursor = 0;
                    if !shift {
                        tab.find_anchor = None;
                    }
                } else {
                    self.find_move_h(pane, true, alt, shift);
                }
                cx.notify();
            }
            "right" => {
                if cmd {
                    let end = self
                        .tab(pane)
                        .find_query
                        .as_ref()
                        .map_or(0, |s| s.chars().count());
                    let tab = self.tab_mut(pane);
                    if shift && tab.find_anchor.is_none() {
                        tab.find_anchor = Some(tab.find_cursor);
                    }
                    tab.find_cursor = end;
                    if !shift {
                        tab.find_anchor = None;
                    }
                } else {
                    self.find_move_h(pane, false, alt, shift);
                }
                cx.notify();
            }
            "a" if cmd => {
                let end = self
                    .tab(pane)
                    .find_query
                    .as_ref()
                    .map_or(0, |s| s.chars().count());
                let tab = self.tab_mut(pane);
                if end == 0 {
                    tab.find_anchor = None;
                } else {
                    tab.find_anchor = Some(0);
                    tab.find_cursor = end;
                }
                cx.notify();
            }
            "backspace" => {
                if !self.find_delete_sel(pane) {
                    let tab = self.tab_mut(pane);
                    if tab.find_cursor > 0 {
                        if let Some(s) = tab.find_query.as_mut() {
                            let start = char_byte(s, tab.find_cursor - 1);
                            let stop = char_byte(s, tab.find_cursor);
                            s.replace_range(start..stop, "");
                        }
                        tab.find_cursor -= 1;
                    }
                }
                self.recompute_find(pane);
                self.update_content_search(pane, cx);
                cx.notify();
            }
            "c" if cmd => {
                let text = match self.find_sel(pane) {
                    Some((lo, hi)) => self.tab(pane).find_query.as_ref().map(|s| {
                        s[char_byte(s, lo)..char_byte(s, hi)].to_string()
                    }),
                    None => self.tab(pane).find_query.clone(),
                };
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.find_insert(pane, t.trim());
                    self.recompute_find(pane);
                    self.update_content_search(pane, cx);
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return;
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        self.find_insert(pane, ch);
                        self.recompute_find(pane);
                        self.update_content_search(pane, cx);
                        cx.notify();
                    }
                }
            }
        }
    }

    // ----- tabs & split panes -----

    /// Record that the split just collapsed so the surviving pane animates out
    /// of its old footprint. `removed_left` = the removed pane was the left one
    /// (the survivor grows leftward from the right edge). Call after removing
    /// the pane but *before* resetting `split_ratio`.
    fn begin_collapse_anim(&mut self, removed_left: bool) {
        let survivor_w = if removed_left {
            1.0 - self.split_ratio
        } else {
            self.split_ratio
        };
        self.collapse_anim = Some((survivor_w, removed_left));
        self.split_epoch += 1;
    }

    /// Record scroll activity on a pane's list (keeps its overlay scrollbar
    /// visible) and make sure a low-rate ticker is alive to repaint once the
    /// fade-out is due — without it the thumb would linger until the next
    /// unrelated repaint.
    fn mark_scrolled(&mut self, pane: usize, cx: &mut Context<Self>) {
        {
            let tab = self.tab_mut(pane);
            tab.last_scroll = Instant::now();
            tab.scroll_epoch += 1;
        }
        if self.fade_ticker {
            return;
        }
        self.fade_ticker = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let all_faded = this.update(cx, |this, cx| {
                    cx.notify();
                    let done = this.panes.iter().flat_map(|p| p.tabs.iter()).all(|t| {
                        t.last_scroll.elapsed().as_secs_f32() > SCROLLBAR_LINGER + SCROLLBAR_FADE
                    });
                    if done {
                        this.fade_ticker = false;
                    }
                    done
                });
                match all_faded {
                    Ok(false) => {}
                    _ => break,
                }
            }
        })
        .detach();
    }

    /// Open a new tab in `pane`, starting in that pane's current directory.
    fn new_tab_in(&mut self, pane: usize, cx: &mut Context<Self>) {
        let dir = self.tab(pane).current_dir.clone();
        let p = self.pane_mut(pane);
        p.tabs.push(Tab::new(dir));
        p.active = p.tabs.len() - 1;
        self.active_pane = pane;
        // Fill the new tab's metadata in the background.
        self.reload_pane(pane, cx);
    }

    /// Connect to a saved SFTP server: open a new tab, resolve the remote home
    /// directory in the background, and browse it. Errors surface in the banner.
    fn connect_sftp(&mut self, server: SftpServer, cx: &mut Context<Self>) {
        // Open a placeholder remote tab in the active pane immediately.
        let pane = self.active_pane;
        let mut tab = Tab::new(home_dir()); // temporary local dir; replaced below
        tab.remote = Some(server.clone());
        tab.current_dir = PathBuf::from("/");
        tab.history = vec![PathBuf::from("/")];
        tab.hist_pos = 0;
        tab.entries.clear();
        let p = self.pane_mut(pane);
        p.tabs.push(tab);
        p.active = p.tabs.len() - 1;
        let tab_ix = p.active;
        self.active_pane = pane;
        self.remote_error = None;
        cx.notify();

        let use_system = prefs().ssh_use_system;
        cx.spawn(async move |this, cx| {
            let s = server.clone();
            let home = cx
                .background_spawn(async move { sftp_home(&s, use_system) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if pane >= this.panes.len() || tab_ix >= this.panes[pane].tabs.len() {
                    return;
                }
                match home {
                    Ok(home) => {
                        let dir = PathBuf::from(&home);
                        {
                            let tab = &mut this.panes[pane].tabs[tab_ix];
                            tab.current_dir = dir.clone();
                            tab.deepest = Some(dir.clone());
                            tab.history = vec![dir];
                            tab.hist_pos = 0;
                        }
                        this.reload_pane(pane, cx);
                    }
                    Err(e) => {
                        // Drop the failed placeholder tab and reopen the editor
                        // pre-filled so the user can fix the settings and retry.
                        if pane < this.panes.len() && tab_ix < this.panes[pane].tabs.len() {
                            this.close_tab(pane, tab_ix, cx);
                        }
                        this.remote_error = Some(format!("{}: {e}", server.name));
                        this.edit_server(&server, cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Select a tab in a pane.
    fn select_tab(&mut self, pane: usize, tab: usize, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let p = self.pane_mut(pane);
        if tab < p.tabs.len() {
            p.active = tab;
        }
        cx.notify();
        self.prewarm_icons(cx);
        // If this tab only got the fast pass, finish loading its metadata.
        if self.tab(pane).entries.iter().any(|e| !e.loaded) {
            self.reload_pane(pane, cx);
        }
    }

    /// Close a tab. Closing a pane's last tab removes the pane (collapsing the
    /// split); closing the last tab of the last pane is a no-op.
    /// Open a copy of `tab` (same folder) right after it in the same pane.
    fn duplicate_tab(&mut self, pane: usize, tab: usize, cx: &mut Context<Self>) {
        if pane >= self.panes.len() || tab >= self.panes[pane].tabs.len() {
            return;
        }
        let dir = self.panes[pane].tabs[tab].current_dir.clone();
        let remote = self.panes[pane].tabs[tab].remote.clone();
        let mut new = Tab::new(dir);
        new.remote = remote;
        let p = self.pane_mut(pane);
        p.tabs.insert(tab + 1, new);
        p.active = tab + 1;
        self.active_pane = pane;
        self.reload_pane(pane, cx);
    }

    /// Close every tab in `pane` except `keep`.
    fn close_other_tabs(&mut self, pane: usize, keep: usize, cx: &mut Context<Self>) {
        if pane >= self.panes.len() || keep >= self.panes[pane].tabs.len() {
            return;
        }
        let kept = self.panes[pane].tabs.remove(keep);
        let p = self.pane_mut(pane);
        p.tabs.clear();
        p.tabs.push(kept);
        p.active = 0;
        self.active_pane = pane;
        cx.notify();
    }

    fn close_tab(&mut self, pane: usize, tab: usize, cx: &mut Context<Self>) {
        if pane >= self.panes.len() || tab >= self.panes[pane].tabs.len() {
            return;
        }
        if self.panes[pane].tabs.len() > 1 {
            let p = self.pane_mut(pane);
            p.tabs.remove(tab);
            if p.active >= p.tabs.len() {
                p.active = p.tabs.len() - 1;
            } else if tab < p.active {
                p.active -= 1;
            }
        } else if self.panes.len() > 1 {
            self.panes.remove(pane);
            self.begin_collapse_anim(pane == 0);
            self.split_ratio = 0.5;
        } else {
            return; // last tab of the only pane — keep it
        }
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        }
        cx.notify();
    }

    /// Move a dragged tab to a destination pane at `to_index`. `to_pane ==
    /// panes.len()` means "create a new pane on the right" (the drag-to-split).
    fn move_tab(&mut self, from: TabDrag, to_pane: usize, to_index: usize, cx: &mut Context<Self>) {
        TAB_DRAG_LIVE.store(false, Ordering::Relaxed);
        if from.pane >= self.panes.len() || from.tab >= self.panes[from.pane].tabs.len() {
            return;
        }
        // Two panes max: a split-zone drop while the canvas is already split
        // moves the tab to the end of the right pane instead of creating an
        // invisible third pane.
        let (to_pane, to_index) = if to_pane >= self.panes.len() && self.panes.len() >= 2 {
            let right = self.panes.len() - 1;
            (right, self.panes[right].tabs.len())
        } else {
            (to_pane, to_index)
        };
        let splitting = to_pane >= self.panes.len();
        // Don't bother splitting when the source pane has a single tab and we'd
        // just move it to a brand-new pane (no visible change).
        if splitting && self.panes[from.pane].tabs.len() == 1 && self.panes.len() == 1 {
            return;
        }
        let tab = self.panes[from.pane].tabs.remove(from.tab);
        // Fix up the source pane's active index after removal.
        {
            let p = &mut self.panes[from.pane];
            if !p.tabs.is_empty() {
                if p.active >= p.tabs.len() {
                    p.active = p.tabs.len() - 1;
                } else if from.tab < p.active {
                    p.active -= 1;
                }
            }
        }

        if splitting {
            // New pane on the right with just this tab.
            self.panes.push(Pane { tabs: vec![tab], active: 0 });
            self.split_ratio = 0.5;
            self.split_epoch += 1;
            self.collapse_anim = None;
            // Drop the source pane if it's now empty.
            if self.panes[from.pane].tabs.is_empty() {
                self.panes.remove(from.pane);
            }
            self.active_pane = self.panes.len() - 1;
        } else {
            // Account for removal shifting indices when within the same pane.
            let mut dst = to_pane;
            let mut idx = to_index;
            if to_pane == from.pane && from.tab < to_index {
                idx = idx.saturating_sub(1);
            }
            // Insert, then prune an emptied source pane (which may shift dst).
            let src_now_empty = self.panes[from.pane].tabs.is_empty();
            let at = idx.min(self.panes[dst].tabs.len());
            self.panes[dst].tabs.insert(at, tab);
            let new_len = self.panes[dst].tabs.len();
            self.panes[dst].active = at.min(new_len - 1);
            if src_now_empty && from.pane != dst {
                self.panes.remove(from.pane);
                if from.pane < dst {
                    dst -= 1;
                }
                self.begin_collapse_anim(from.pane == 0);
                self.split_ratio = 0.5;
            }
            self.active_pane = dst;
        }
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        }
        cx.notify();
        self.prewarm_icons(cx);
    }

    // ----- selection -----

    /// The paths shown in `pane`, in display order (respecting the find filter).
    fn display_paths(&self, pane: usize) -> Vec<PathBuf> {
        let tab = self.tab(pane);
        let dir = &tab.current_dir;
        if tab.find_query.is_some() {
            tab.find_results
                .iter()
                .map(|&i| dir.join(&tab.entries[i].name))
                .collect()
        } else {
            tab.entries.iter().map(|e| dir.join(&e.name)).collect()
        }
    }

    /// Make `path` the inspector focus and load its preview/info.
    fn focus_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.tab_mut(pane).anchor = Some(path.clone());
        self.preview_page = 0;
        // Remote files aren't on the local disk, so QuickLook/PDF rendering
        // can't read them directly. Fetch a copy to a temp cache and preview
        // that (images & PDFs only, size-capped). Info (local stat) is skipped.
        if self.tab(pane).remote.is_some() {
            let size = {
                let tab = self.tab(pane);
                let dir = tab.current_dir.clone();
                tab.entries
                    .iter()
                    .find(|e| dir.join(&e.name) == path)
                    .map(|e| e.size)
                    .unwrap_or(0)
            };
            self.ensure_remote_preview(path, size, cx);
            cx.notify();
            return;
        }
        let gallery = self.tab(pane).view == ViewMode::Gallery;
        self.ensure_preview(path.clone(), gallery, cx);
        if prefs().preview && prefs().preview_pages {
            self.ensure_pdf_page(path.clone(), 0, cx);
        }
        self.ensure_info(path, cx);
    }

    /// Single-click: select just this item.
    fn select_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.rename = None;
        {
            let tab = self.tab_mut(pane);
            tab.selection.clear();
            tab.selection.insert(path.clone());
        }
        cx.notify();
        self.focus_entry(pane, path, cx);
    }

    /// Cmd-click: toggle this item's membership in the selection.
    fn toggle_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.rename = None;
        let now_selected = {
            let tab = self.tab_mut(pane);
            if tab.selection.contains(&path) {
                tab.selection.remove(&path);
                false
            } else {
                tab.selection.insert(path.clone());
                true
            }
        };
        cx.notify();
        if now_selected {
            self.focus_entry(pane, path, cx);
        }
    }

    /// Shift-click: select the contiguous range from the anchor to this item.
    fn range_select(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.rename = None;
        let paths = self.display_paths(pane);
        let to = paths.iter().position(|p| p == &path);
        let from = self
            .tab(pane)
            .anchor
            .as_ref()
            .and_then(|a| paths.iter().position(|p| p == a));
        let sel: HashSet<PathBuf> = match (from, to) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                paths[lo..=hi].iter().cloned().collect()
            }
            _ => std::iter::once(path.clone()).collect(),
        };
        self.tab_mut(pane).selection = sel;
        cx.notify();
        // Keep the existing anchor; just refresh the inspector to the clicked one.
        self.focus_entry(pane, path, cx);
    }

    /// Shift-click range select within one Column-view column: select every item
    /// between the anchor and `target` in that column's listing. Falls back to a
    /// single selection when the anchor isn't in this column.
    fn range_select_column(
        &mut self,
        pane: usize,
        col_index: usize,
        target: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // The directory this column lists: column 0 is the current dir, deeper
        // columns come from the drill-in chain.
        let dir = if col_index == 0 {
            self.tab(pane).current_dir.clone()
        } else {
            match self.tab(pane).col_chain.get(col_index - 1) {
                Some(d) => d.clone(),
                None => return,
            }
        };
        let paths: Vec<PathBuf> = column_entries(&dir)
            .iter()
            .map(|e| dir.join(&e.name))
            .collect();
        let to = paths.iter().position(|p| p == &target);
        let from = self
            .tab(pane)
            .anchor
            .as_ref()
            .and_then(|a| paths.iter().position(|p| p == a));
        let sel: HashSet<PathBuf> = match (from, to) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                paths[lo..=hi].iter().cloned().collect()
            }
            _ => std::iter::once(target.clone()).collect(),
        };
        // Don't extend the drill-in past this column while range-selecting.
        self.tab_mut(pane).col_chain.truncate(col_index);
        self.tab_mut(pane).selection = sel;
        cx.notify();
    }

    /// Whether a marquee drag is in flight with real displacement — used to
    /// swallow the row click that lands on the same mouse-up (row `on_click`
    /// bubbles before the root's `end_marquee`), so finishing a box-select over
    /// a row doesn't replace the fresh multi-selection with that one row.
    /// Stationary press-releases (displacement ≈ 0) still click through.
    fn marquee_click_suppressed(&self) -> bool {
        self.marquee.is_some_and(|(pane, s, c)| {
            // The anchor is content-space; convert the cursor before comparing
            // (comparing across spaces would suppress every click once the
            // list is scrolled). Scroll during the marquee counts as movement —
            // exactly then must the click not collapse the selection.
            let (top, scrolled) = self.list_geometry(pane);
            let cur_c = c.1 - top + scrolled;
            (c.0 - s.0).abs().max((cur_c - s.1).abs()) > 6.0
        })
    }

    /// Dispatch a left-click on an item, honoring Cmd / Shift modifiers.
    fn click_entry(
        &mut self,
        pane: usize,
        path: PathBuf,
        is_dir: bool,
        ev: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        if self.marquee_click_suppressed() {
            return;
        }
        self.active_pane = pane;
        self.term_focused = false;
        let mods = ev.modifiers();
        if mods.platform {
            self.toggle_entry(pane, path, cx);
        } else if mods.shift {
            self.range_select(pane, path, cx);
        } else if is_dir {
            self.navigate_in(pane, path, cx);
        } else if ev.click_count() >= 2 {
            self.open_path(pane, path, false, cx);
        } else {
            self.select_entry(pane, path, cx);
        }
    }

    /// A pane list's `(viewport_top, scrolled)` in window coords / content px —
    /// the numbers needed to convert between window y and content y.
    fn list_geometry(&self, pane: usize) -> (f32, f32) {
        let st = self.tab(pane).scroll_handle.0.borrow();
        let top = f64::from(st.base_handle.bounds().origin.y) as f32;
        let scrolled = (-(f64::from(st.base_handle.offset().y) as f32)).max(0.0);
        (top, scrolled)
    }

    /// Start a marquee (rubber-band) selection from empty list space.
    ///
    /// The anchor is stored in CONTENT coordinates (`y - list_top + scrolled`),
    /// not window coordinates: the list can scroll underneath an in-flight
    /// marquee (wheel/trackpad, or edge auto-scroll), and a window-space anchor
    /// would slide with the scroll — moving the box off the rows it had already
    /// swept and re-selecting the wrong ones.
    fn begin_marquee(&mut self, pane: usize, x: f32, y: f32, cx: &mut Context<Self>) {
        if mq_log() {
            eprintln!("[mq] begin pane={pane} at ({x:.0},{y:.0})");
        }
        self.active_pane = pane;
        self.term_focused = false;
        self.rename = None;
        {
            let tab = self.tab_mut(pane);
            tab.selection.clear();
            tab.anchor = None;
        }
        let (top, scrolled) = self.list_geometry(pane);
        self.marquee = Some((pane, (x, y - top + scrolled), (x, y)));
        // Auto-scroll while the drag sits past the top/bottom edge of the list,
        // so a marquee can keep selecting through folders taller than the view.
        // A timer loop (not mouse-move driven) so it scrolls even while the
        // cursor holds still past the edge; it exits when the marquee ends.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.marquee.is_none() {
                            return false;
                        }
                        this.marquee_autoscroll_tick(cx);
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// One 16ms step for an active marquee. Recomputes the selection against
    /// the CURRENT scroll position (the wheel/trackpad can scroll the list
    /// mid-marquee with no mouse-move event — the content-space anchor stays
    /// glued, and the rows under the box change), then, if the pointer sits in
    /// an edge zone, auto-scrolls — slow near the edge, faster further past it.
    fn marquee_autoscroll_tick(&mut self, cx: &mut Context<Self>) {
        let Some((pane, _, cur)) = self.marquee else {
            return;
        };
        if pane >= self.panes.len() {
            return;
        }
        // Track external scrolls: re-derive selection + box from the live
        // scroll offset every tick (cheap — only the covered rows).
        self.update_marquee(cur.0, cur.1, cx);
        let Some((pane, start, cur)) = self.marquee else {
            return;
        };
        // Distance past the viewport edge decides direction and speed.
        let (top, bottom, scrolled, max) = {
            let st = self.tab(pane).scroll_handle.0.borrow();
            let b = st.base_handle.bounds();
            let top = f64::from(b.origin.y) as f32;
            let bottom = top + f64::from(b.size.height) as f32;
            let scrolled = (-(f64::from(st.base_handle.offset().y) as f32)).max(0.0);
            let max = f64::from(st.base_handle.max_offset().height) as f32;
            (top, bottom, scrolled, max)
        };
        if max <= 1.0 {
            return; // everything fits; nothing to scroll
        }
        // Scroll when the cursor is within an edge ZONE inside the viewport, not
        // only past the edge — when the list reaches the window bottom the
        // cursor may never report coordinates beyond it. Zone-scrolling only
        // engages once the box has actually been dragged a little, so a plain
        // press inside a zone doesn't scroll; after that the zone side alone
        // picks the direction (an anchor-side gate here would block reversing:
        // after a long down-scroll the glued anchor clamps to the top edge and
        // no reachable in-zone position passes it).
        const ZONE: f32 = 28.0;
        // `start` is content-space; convert the cursor for the displacement gate.
        let cur_content_y = cur.1 - top + scrolled;
        let moved =
            (cur.0 - start.0).abs().max((cur_content_y - start.1).abs()) > 8.0;
        let overshoot = if moved && cur.1 < top + ZONE {
            (cur.1 - (top + ZONE)).min(-0.1) // negative → scroll up
        } else if moved && cur.1 > bottom - ZONE {
            (cur.1 - (bottom - ZONE)).max(0.1) // positive → scroll down
        } else {
            if mq_log() {
                eprintln!(
                    "[mq] tick idle: cur.y={:.0} top={top:.0} bottom={bottom:.0} moved={moved} zone={ZONE}",
                    cur.1
                );
            }
            return; // outside both zones, or the box hasn't been dragged yet
        };
        if mq_log() {
            eprintln!(
                "[mq] tick scroll: cur.y={:.0} overshoot={overshoot:.0} scrolled={scrolled:.0}/{max:.0}",
                cur.1
            );
        }
        // ~2px/tick at the zone boundary up to ~28px/tick (≈1700px/s) far past
        // the edge — "slowly or faster depending on where the mouse is".
        let speed = (overshoot.abs() * 0.22).clamp(2.0, 28.0);
        let target = (scrolled + speed.copysign(overshoot)).clamp(0.0, max);
        let applied = target - scrolled;
        if applied.abs() < 0.5 {
            return; // already at that end of the list
        }
        {
            let st = self.tab(pane).scroll_handle.0.borrow();
            let x = st.base_handle.offset().x;
            st.base_handle.set_offset(point(x, px(-target)));
        }
        let _ = applied; // the content-space anchor is scroll-invariant
        self.mark_scrolled(pane, cx); // show the scrollbar while auto-scrolling
        // Recompute the covered rows against the new scroll position.
        self.update_marquee(cur.0, cur.1, cx);
    }

    /// Update the marquee end point and recompute which rows it covers.
    fn update_marquee(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some((pane, start, _)) = self.marquee else {
            return;
        };
        if mq_log() {
            eprintln!("[mq] move ({x:.0},{y:.0})");
        }
        self.marquee = Some((pane, start, (x, y)));

        // Map the vertical span to row indices using the list's geometry.
        let (list_top, scrolled) = self.list_geometry(pane);
        // The anchor (`start.1`) is content-space; only the cursor converts.
        let cur_c = y - list_top + scrolled;
        let c0 = start.1.min(cur_c).max(0.0);
        let c1 = start.1.max(cur_c).max(0.0);
        let i0 = (c0 / ROW_H).floor() as i64;
        let i1 = (c1 / ROW_H).floor() as i64;
        // Row 0 is the ".." entry when present; offset to display indices.
        let has_parent =
            self.tab(pane).find_query.is_none() && prefs().show_parent && self.tab(pane).current_dir.parent().is_some();
        let off = i64::from(has_parent);
        // Only touch the covered rows — cloning every path in the directory per
        // mouse-move made marquee drags crawl in large folders.
        let mut sel = HashSet::new();
        {
            let tab = self.tab(pane);
            let find = tab.find_query.is_some();
            let len = if find {
                tab.find_results.len()
            } else {
                tab.entries.len()
            };
            for i in i0..=i1 {
                let di = i - off;
                if di < 0 {
                    continue;
                }
                let di = di as usize;
                if di >= len {
                    break;
                }
                let ix = if find { tab.find_results[di] } else { di };
                sel.insert(tab.current_dir.join(&tab.entries[ix].name));
            }
        }
        self.tab_mut(pane).selection = sel;
        cx.notify();
    }

    fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if let Some((pane, _, _)) = self.marquee.take() {
            // Give the marquee'd selection a focused item so Enter (rename),
            // the inspector, and shift-extension have a target.
            if self.tab(pane).anchor.is_none() {
                let last = self
                    .display_paths(pane)
                    .into_iter()
                    .rev()
                    .find(|p| self.tab(pane).selection.contains(p));
                if let Some(p) = last {
                    self.focus_entry(pane, p, cx);
                }
            }
            cx.notify();
        }
    }

    /// Recompute [`Self::drop_hover`] from the drag position. Runs on every
    /// (synthesized) mouse move while a drag is in flight; notifies only when
    /// the target actually changes.
    fn update_drop_hover(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        if !cx.has_active_drag() {
            TAB_DRAG_LIVE.store(false, Ordering::Relaxed);
        }
        let next = if cx.has_active_drag() && !TAB_DRAG_LIVE.load(Ordering::Relaxed) {
            self.compute_drop_hover(x, y)
        } else {
            None
        };
        if self.drop_hover != next {
            self.drop_hover = next;
            cx.notify();
        }
    }

    /// Which drop target is under (x, y): a folder row's name cell → that
    /// folder, anywhere else in a pane → the pane's directory. Mirrors the
    /// actual drop hitboxes (name cell = 12px row padding + name column).
    fn compute_drop_hover(&self, x: f32, y: f32) -> Option<(usize, Option<usize>)> {
        for pane in 0..self.panes.len() {
            let tab = self.tab(pane);
            let (origin, size, scrolled) = {
                let st = tab.scroll_handle.0.borrow();
                let b = st.base_handle.bounds();
                let scr = (-(f64::from(st.base_handle.offset().y) as f32)).max(0.0);
                (b.origin, b.size, scr)
            };
            let (y0, h) = (f64::from(origin.y) as f32, f64::from(size.height) as f32);
            // The pane's horizontal extent must come from the VIEWPORT (the
            // hscroll wrapper) — the list's own bounds grow to the full column
            // width, which can silently extend underneath the neighbor pane
            // when the columns are wider than a split pane.
            let vp = tab.h_scroll.bounds();
            let (mut x0, mut w) = (
                f64::from(vp.origin.x) as f32,
                f64::from(vp.size.width) as f32,
            );
            if w <= 1.0 {
                // Views that don't track the hscroll wrapper (Icons, …) use
                // the list bounds, which don't overflow there.
                x0 = f64::from(origin.x) as f32;
                w = f64::from(size.width) as f32;
            }
            if w <= 1.0 || x < x0 || x >= x0 + w {
                continue;
            }
            // Fine-grained folder targeting only in the List view.
            if tab.view != ViewMode::List || y < y0 || y >= y0 + h {
                return Some((pane, None));
            }
            let row = ((y - y0 + scrolled) / ROW_H).floor();
            if row < 0.0 {
                return Some((pane, None));
            }
            let row = row as usize;
            let find_active = tab.find_query.is_some();
            let has_parent =
                !find_active && prefs().show_parent && tab.current_dir.parent().is_some();
            let count = if find_active {
                tab.find_results.len()
            } else {
                tab.entries.len() + usize::from(has_parent)
            };
            if row >= count {
                return Some((pane, None));
            }
            // Inside the name cell horizontally? (Row padding is px_3 = 12,
            // shifted by any horizontal column scroll.)
            let hx = f64::from(tab.h_scroll.offset().x) as f32;
            let name_x0 = x0 + hx + 12.0;
            if x < name_x0 || x >= name_x0 + self.widths.name {
                return Some((pane, None));
            }
            // ".." row targets the parent; entries target real folders only.
            let is_folder = if has_parent && row == 0 {
                true
            } else {
                let ix = if find_active {
                    tab.find_results[row]
                } else {
                    row - usize::from(has_parent)
                };
                tab.entries.get(ix).is_some_and(|e| e.is_dir)
            };
            return Some((pane, is_folder.then_some(row)));
        }
        None
    }

    /// If a left-press on a row has moved past the drag threshold, hand the
    /// current selection to a native macOS drag session so the files can be
    /// dropped into Finder, Claude, Mail, or any other app (and back into
    /// Shuffle's own folders/panes, which arrive as an external-file drop).
    fn maybe_start_os_drag(&mut self, x: f32, y: f32, window: &Window, cx: &mut Context<Self>) {
        let Some((pane, path, (sx, sy))) = self.drag_candidate.clone() else {
            return;
        };
        if (x - sx).abs() < 6.0 && (y - sy).abs() < 6.0 {
            return; // still within the click slop; not a drag yet
        }
        self.drag_candidate = None;
        self.marquee = None;

        // Drag the whole selection if the pressed item is part of it; otherwise
        // just the pressed item.
        let sel = &self.tab(pane).selection;
        let mut paths: Vec<PathBuf> = if sel.contains(&path) {
            sel.iter().cloned().collect()
        } else {
            vec![path.clone()]
        };
        paths.sort();

        if let Some(view) = ns_view_ptr(window) {
            start_os_file_drag(view, &paths);
        }
        cx.notify();
    }

    /// The visible marquee rectangle for `pane` (in the listing-local frame).
    fn marquee_rect(&self, pane: usize) -> Option<AnyElement> {
        let (mp, start, cur) = self.marquee?;
        if mp != pane {
            return None;
        }
        let (ox, oy) = {
            let st = self.tab(pane).scroll_handle.0.borrow();
            let o = st.base_handle.bounds().origin;
            (f64::from(o.x) as f32, f64::from(o.y) as f32)
        };
        // The anchor is content-space: convert to window y with the LIVE scroll
        // offset, so the box's fixed edge glides with the content when the list
        // scrolls under an in-flight marquee.
        let (_, scrolled) = self.list_geometry(pane);
        let start_y = start.1 + oy - scrolled;
        let x = start.0.min(cur.0) - ox;
        let y = start_y.min(cur.1) - oy;
        let w = (start.0 - cur.0).abs();
        let mut h = (start_y - cur.1).abs();
        // Clamp to the container: when the anchored edge has scrolled off the
        // top, shrink the height by the clipped amount (clamping only `top`
        // would push the box's far edge past the cursor).
        if y < 0.0 {
            h = (h + y).max(0.0);
        }
        let t = theme();
        Some(
            div()
                .absolute()
                .left(px(x.max(0.0)))
                .top(px(y.max(0.0)))
                .w(px(w))
                .h(px(h))
                .bg(Theme::alpha(t.accent, 0x22))
                .border_1()
                .border_color(rgb(t.accent))
                .into_any_element(),
        )
    }

    // ----- column (Miller) view -----

    /// Handle a click in column `col_index` of the Column view.
    fn column_click(
        &mut self,
        pane: usize,
        col_index: usize,
        target: PathBuf,
        is_dir: bool,
        ev: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
        self.active_pane = pane;
        self.term_focused = false;
        let mods = ev.modifiers();
        // Cmd-click: toggle this item in a multi-selection without navigating or
        // collapsing the columns to its right (Finder-style non-contiguous pick).
        if mods.platform {
            let tab = self.tab_mut(pane);
            if !tab.selection.remove(&target) {
                tab.selection.insert(target.clone());
            }
            tab.anchor = tab.selection.iter().next().cloned();
            cx.notify();
            return;
        }
        // Shift-click: select the inclusive range from the anchor to here within
        // this column (everything in between), like Finder.
        if mods.shift {
            self.range_select_column(pane, col_index, target, cx);
            return;
        }
        if is_dir {
            if ev.click_count() >= 2 {
                // Double-click drills in as the new root.
                self.navigate_in(pane, target, cx);
                return;
            }
            let tab = self.tab_mut(pane);
            tab.col_chain.truncate(col_index);
            tab.col_chain.push(target.clone());
            tab.selection.clear();
            tab.selection.insert(target.clone());
            // Anchor this item so a following Shift-click can range from it.
            tab.anchor = Some(target);
            cx.notify();
        } else {
            {
                let tab = self.tab_mut(pane);
                tab.col_chain.truncate(col_index);
                tab.selection.clear();
                tab.selection.insert(target.clone());
            }
            if ev.click_count() >= 2 {
                self.open_path(pane, target, false, cx);
            } else {
                cx.notify();
                self.focus_entry(pane, target, cx);
            }
        }
    }

    // ----- keyboard arrow navigation -----

    /// Columns across the grid in Icons view (from the last measured width).
    fn icon_cols(&self, pane: usize) -> usize {
        let width = self.pane_list_width(pane).max(240.0);
        ((width / 108.0).floor() as usize).max(1)
    }

    /// Move the selection with an arrow key. `dx`/`dy` are -1/0/1.
    fn arrow_move(&mut self, pane: usize, dx: i32, dy: i32, cx: &mut Context<Self>) {
        self.active_pane = pane;
        match self.tab(pane).view {
            ViewMode::Columns => self.arrow_columns(pane, dx, dy, cx),
            ViewMode::Icons => {
                let cols = self.icon_cols(pane) as i32;
                self.arrow_grid(pane, dx + dy * cols, cx);
            }
            // List & Gallery are 1-D: only up/down move.
            _ => {
                if dy != 0 {
                    self.arrow_grid(pane, dy, cx);
                }
            }
        }
    }

    /// Move the anchor by `delta` positions in display order (List/Icons/Gallery).
    fn arrow_grid(&mut self, pane: usize, delta: i32, cx: &mut Context<Self>) {
        let paths = self.display_paths(pane);
        if paths.is_empty() {
            return;
        }
        let cur = self
            .tab(pane)
            .anchor
            .as_ref()
            .and_then(|a| paths.iter().position(|p| p == a));
        let ni = match cur {
            Some(i) => (i as i32 + delta).clamp(0, paths.len() as i32 - 1),
            None => 0,
        };
        let target = paths[ni as usize].clone();
        {
            let tab = self.tab_mut(pane);
            tab.selection.clear();
            tab.selection.insert(target.clone());
        }
        // Keep the row/cell visible (List uses a ".." offset; Icons rows of N).
        let view = self.tab(pane).view;
        let offset = usize::from(
            self.tab(pane).find_query.is_none()
                && prefs().show_parent
                && self.tab(pane).current_dir.parent().is_some(),
        );
        let item = match view {
            ViewMode::Icons => ni as usize / self.icon_cols(pane),
            _ => ni as usize + offset,
        };
        self.tab(pane).scroll_handle.scroll_to_item(item, ScrollStrategy::Center);
        // Keyboard scrolling shows the overlay scrollbar too.
        self.mark_scrolled(pane, cx);
        self.focus_entry(pane, target, cx);
    }

    /// Arrow navigation within the Column (Miller) view.
    fn arrow_columns(&mut self, pane: usize, dx: i32, dy: i32, cx: &mut Context<Self>) {
        let base = self.tab(pane).current_dir.clone();
        let mut dirs: Vec<PathBuf> = vec![base];
        dirs.extend(self.tab(pane).col_chain.iter().cloned());
        let mut k = self.tab(pane).col_active.min(dirs.len() - 1);

        if dx < 0 {
            // Move focus to the parent column.
            if k > 0 {
                self.tab_mut(pane).col_active = k - 1;
                cx.notify();
            }
            return;
        }
        if dx > 0 {
            // Move into the selected folder's column, if any.
            if k + 1 < dirs.len() {
                self.tab_mut(pane).col_active = k + 1;
                k += 1;
                // Select the first entry of the new column.
                self.column_set(pane, k, &dirs, 0, cx);
            }
            return;
        }

        // Up / down within column k.
        let dir = dirs[k].clone();
        let entries = column_entries(&dir);
        if entries.is_empty() {
            return;
        }
        let sel_k = if k < self.tab(pane).col_chain.len() {
            Some(self.tab(pane).col_chain[k].clone())
        } else {
            self.tab(pane).anchor.clone()
        };
        let cur = sel_k.and_then(|p| entries.iter().position(|e| dir.join(&e.name) == p));
        let ni = match cur {
            Some(i) => (i as i32 + dy).clamp(0, entries.len() as i32 - 1) as usize,
            None => 0,
        };
        self.column_set(pane, k, &dirs, ni, cx);
    }

    /// Select entry `idx` in column `k` of the Column view (keyboard-driven).
    fn column_set(&mut self, pane: usize, k: usize, dirs: &[PathBuf], idx: usize, cx: &mut Context<Self>) {
        let dir = dirs[k].clone();
        let entries = column_entries(&dir);
        let Some(e) = entries.get(idx) else { return };
        let target = dir.join(&e.name);
        let is_dir = e.is_dir;
        {
            let tab = self.tab_mut(pane);
            tab.col_chain.truncate(k);
            if is_dir {
                tab.col_chain.push(target.clone());
            }
            tab.col_active = k;
            tab.selection.clear();
            tab.selection.insert(target.clone());
        }
        cx.notify();
        self.focus_entry(pane, target, cx);
    }

    /// Gather file info for `path` in the background (once), then repaint.
    fn ensure_info(&self, path: PathBuf, cx: &mut Context<Self>) {
        // Only gather info when the Information panel is actually shown.
        if !prefs().info || lookup_info(&path).is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let info = cx.background_spawn(async move { gather_info(&p) }).await;
            let _ = this.update(cx, |_, cx| {
                INFO_CACHE.with(|c| {
                    c.borrow_mut().insert(path, info);
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Build a preview for `path` in the background (once), then repaint.
    /// `force` generates it even when the Preview pref is off (Gallery view).
    fn ensure_preview(&self, path: PathBuf, force: bool, cx: &mut Context<Self>) {
        if (!force && !prefs().preview) || lookup_preview(&path).is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let img = cx.background_spawn(async move { build_preview(&p) }).await;
            let _ = this.update(cx, |_, cx| {
                PREVIEW_CACHE.with(|c| {
                    c.borrow_mut().insert(path, img);
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Render one page of a PDF for the inspector pager in the background
    /// (once), recording the document's page count along the way.
    fn ensure_pdf_page(&self, path: PathBuf, page: usize, cx: &mut Context<Self>) {
        if !is_pdf(&path) || lookup_pdf_page(&path, page).is_some() {
            return;
        }
        // Remote PDFs live in a downloaded temp copy; render pages from there
        // but key the cache by the remote path (what the inspector looks up).
        let src = if self.active_tab().remote.is_some() {
            remote_preview_temp(&path)
        } else {
            path.clone()
        };
        cx.spawn(async move |this, cx| {
            let p = src.clone();
            let out = cx.background_spawn(async move { render_pdf_page(&p, page) }).await;
            let _ = this.update(cx, |this, cx| {
                match out {
                    Some((img, count)) => {
                        insert_pdf_page(path.clone(), page, Some(img));
                        PDF_COUNT_CACHE.with(|c| {
                            c.borrow_mut().insert(path.clone(), count);
                        });
                        // Have the second page ready before the first ‹ › click.
                        if page == 0 && count > 1 {
                            this.ensure_pdf_page(path, 1, cx);
                        }
                    }
                    None => insert_pdf_page(path, page, None),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Preview a remote (SFTP) file: download it once to a temp cache, build the
    /// preview from that local copy, and store the result under the *remote*
    /// path so the inspector's cache lookups find it. Limited to images and
    /// PDFs under a size cap so we never pull a huge binary just to preview it.
    fn ensure_remote_preview(&self, remote_path: PathBuf, size: u64, cx: &mut Context<Self>) {
        if !prefs().preview || lookup_preview(&remote_path).is_some() {
            return;
        }
        let Some(server) = self.active_tab().remote.clone() else {
            return;
        };
        // Only fetch types worth previewing.
        if !is_image(&remote_path) && !is_pdf(&remote_path) {
            return;
        }
        // Cap the download; mark oversized files unavailable so we don't retry.
        const MAX_PREVIEW_BYTES: u64 = 40 * 1024 * 1024;
        if size > MAX_PREVIEW_BYTES {
            PREVIEW_CACHE.with(|c| c.borrow_mut().insert(remote_path, None));
            cx.notify();
            return;
        }
        let use_system = prefs().ssh_use_system;
        let want_pdf = is_pdf(&remote_path);
        cx.spawn(async move |this, cx| {
            let s = server.clone();
            let rp = remote_path.clone();
            let built = cx
                .background_spawn(async move {
                    let local = remote_preview_temp(&rp);
                    if let Some(parent) = local.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    // Fetch a fresh copy (the remote file may have changed).
                    let r = rp.to_string_lossy().replace('"', "");
                    let l = local.to_string_lossy().replace('"', "");
                    let script = format!("get \"{r}\" \"{l}\"");
                    match sftp_batch(&s, &script, use_system) {
                        Ok(_) => {
                            let img = build_preview(&local);
                            let pdf = if want_pdf { render_pdf_page(&local, 0) } else { None };
                            Some((img, pdf))
                        }
                        Err(_) => None,
                    }
                })
                .await;
            let _ = this.update(cx, |_this, cx| {
                match built {
                    Some((img, pdf)) => {
                        PREVIEW_CACHE.with(|c| c.borrow_mut().insert(remote_path.clone(), img));
                        if let Some((pimg, count)) = pdf {
                            insert_pdf_page(remote_path.clone(), 0, Some(pimg));
                            PDF_COUNT_CACHE
                                .with(|c| c.borrow_mut().insert(remote_path.clone(), count));
                        }
                    }
                    // Download/build failed → cache "unavailable" (shows the icon).
                    None => {
                        PREVIEW_CACHE.with(|c| c.borrow_mut().insert(remote_path.clone(), None));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The floating "why this name won't work" pill shown over `pane` while an
    /// in-progress rename there has an invalid name.
    fn rename_error_pill(&self, pane: usize) -> Option<AnyElement> {
        let r = self.rename.as_ref()?;
        if r.pane != pane {
            return None;
        }
        let msg = self.rename_error()?;
        let t = theme();
        Some(
            div()
                .absolute()
                .bottom(px(16.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(Theme::alpha(t.surface, 0xf2))
                        .border_1()
                        .border_color(rgb(RENAME_ERR_COLOR))
                        .shadow_lg()
                        .text_color(rgb(RENAME_ERR_COLOR))
                        .child("⚠")
                        .child(div().text_color(rgb(t.text)).child(msg)),
                )
                .into_any_element(),
        )
    }

    /// The right-hand inspector: preview and/or information for the selected
    /// file. `None` when neither feature is on or nothing is selected.
    fn render_inspector(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let p = prefs();
        // In Gallery view the big preview already shows, so don't duplicate it
        // in the side inspector even when the Preview toggle is on.
        let show_preview = p.preview && self.active_tab().view != ViewMode::Gallery;
        if !show_preview && !p.info {
            return None;
        }
        let sel = self.active_tab().anchor.clone()?;
        let t = theme();

        let mut col = div()
            .id("inspector")
            .flex_none()
            .w(px(320.0))
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(t.sidebar))
            .border_l_1()
            .border_color(rgb(t.border))
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(t.text))
                    .truncate()
                    .child(path_label(&sel)),
            );

        if show_preview {
            // Multi-page PDFs: show the natively rendered current page when the
            // pager is on, falling back to the QuickLook thumbnail (page 1)
            // while it builds.
            let paging = p.preview_pages && is_pdf(&sel);
            let page_count = if paging { lookup_pdf_count(&sel) } else { None };
            let page = self
                .preview_page
                .min(page_count.unwrap_or(1).saturating_sub(1));
            let page_img = if paging {
                lookup_pdf_page(&sel, page).flatten()
            } else {
                None
            };
            let handle = page_img.or_else(|| lookup_preview(&sel).flatten());
            let body: AnyElement = match handle {
                Some(handle) => img(ImageSource::Render(handle))
                    .max_w(px(288.0))
                    .max_h(px(360.0))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                // Not ready yet or unavailable → show the file's icon.
                None => icon_element_sized(&sel, false, 96.0),
            };
            let preview_box = div()
                .relative()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .min_h(px(120.0))
                // Clip so a tall preview can't paint over the title above or the
                // "Information" section below.
                .max_h(px(380.0))
                .overflow_hidden()
                .p_2()
                .rounded_md()
                // White "page" so document/text previews (black text, often
                // transparent background) stay readable on dark themes.
                .bg(rgb(0xffffff))
                .child(body);

            // Finder-style ‹ 2 / 14 › pill over the bottom of the preview.
            if let Some(count) = page_count.filter(|&c| c > 1) {
                let prev_sel = sel.clone();
                let next_sel = sel.clone();
                let pager_btn = |id: &'static str, label: &'static str, enabled: bool| {
                    div()
                        .id(id)
                        .px_1p5()
                        .rounded_full()
                        .cursor_pointer()
                        .text_color(if enabled {
                            rgba(0xffffffff)
                        } else {
                            rgba(0xffffff44)
                        })
                        .when(enabled, |s| s.hover(|s| s.bg(rgba(0xffffff33))))
                        .child(label)
                };
                col = col.child(
                    preview_box.child(
                        div()
                            .absolute()
                            .bottom_2()
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_1()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgba(0x000000aa))
                                    .text_xs()
                                    .child(pager_btn("pdf-prev", "‹", page > 0).on_click(
                                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            if this.preview_page > 0 {
                                                this.preview_page -= 1;
                                                let pg = this.preview_page;
                                                this.ensure_pdf_page(prev_sel.clone(), pg, cx);
                                                if pg > 0 {
                                                    this.ensure_pdf_page(prev_sel.clone(), pg - 1, cx);
                                                }
                                                cx.notify();
                                            }
                                        }),
                                    ))
                                    .child(
                                        div()
                                            .text_color(rgba(0xffffffdd))
                                            .child(format!("{} / {}", page + 1, count)),
                                    )
                                    .child(pager_btn("pdf-next", "›", page + 1 < count).on_click(
                                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            let count =
                                                lookup_pdf_count(&next_sel).unwrap_or(1);
                                            if this.preview_page + 1 < count {
                                                this.preview_page += 1;
                                                let pg = this.preview_page;
                                                this.ensure_pdf_page(next_sel.clone(), pg, cx);
                                                if pg + 1 < count {
                                                    this.ensure_pdf_page(next_sel.clone(), pg + 1, cx);
                                                }
                                                cx.notify();
                                            }
                                        }),
                                    )),
                            ),
                    ),
                );
            } else {
                col = col.child(preview_box);
            }
        }

        if p.info {
            col = col.child(settings_title("Information"));
            if let Some(info) = lookup_info(&sel) {
                let mut rows = div().flex().flex_col().gap_1();
                rows = rows.child(info_row("Kind", &info.kind));
                rows = rows.child(info_row("Size", &info.size));
                rows = rows.child(info_row("Created", &info.created));
                rows = rows.child(info_row("Modified", &info.modified));
                rows = rows.child(info_row("Last opened", &info.accessed));
                if let Some(d) = &info.dimensions {
                    rows = rows.child(info_row("Dimensions", d));
                }
                if let Some(c) = &info.color {
                    rows = rows.child(info_row("Color", c));
                }
                if let Some(s) = &info.signed {
                    rows = rows.child(info_row("Signature", s));
                }
                col = col.child(rows);
            } else {
                col = col.child(
                    div()
                        .text_xs()
                        .text_color(rgb(t.text_dim))
                        .child("Loading…"),
                );
            }
        }

        Some(col.into_any_element())
    }

    // ----- terminal mode (the bottom command bar) -----

    /// Append a line to the terminal scrollback, capped to a sane length.
    fn term_push(&mut self, line: impl Into<String>) {
        self.term_output.push(line.into());
        let n = self.term_output.len();
        if n > 400 {
            self.term_output.drain(0..n - 400);
        }
    }

    /// Run the current terminal input against the active pane's directory.
    fn run_term_command(&mut self, cx: &mut Context<Self>) {
        let pane = self.active_pane;
        let cwd = self.tab(pane).current_dir.clone();
        let cmd = self.term_input.trim().to_string();
        self.term_input.clear();
        if cmd.is_empty() {
            return;
        }
        self.term_push(format!("{} ❯ {}", path_label(&cwd), cmd));

        if cmd == "clear" {
            self.term_output.clear();
            cx.notify();
            return;
        }

        // `cd` navigates the explorer instead of spawning a shell.
        if cmd == "cd" || cmd.starts_with("cd ") {
            let arg = cmd[2..].trim();
            let target = resolve_dir(&cwd, arg);
            if target.is_dir() {
                self.navigate_in(pane, target, cx);
            } else {
                self.term_push(format!("cd: no such directory: {arg}"));
                cx.notify();
            }
            return;
        }

        // Everything else runs in a shell, rooted at the current directory.
        let output = Command::new("sh")
            .arg("-lc")
            .arg(&cmd)
            .current_dir(&cwd)
            .output();
        match output {
            Ok(out) => {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    self.term_push(line.to_string());
                }
                for line in String::from_utf8_lossy(&out.stderr).lines() {
                    self.term_push(line.to_string());
                }
            }
            Err(e) => self.term_push(format!("error: {e}")),
        }
        // The command may have changed the directory contents.
        self.refresh_pane(pane, cx);
        self.term_scroll.set_offset(point(px(0.0), px(-1e6)));
        cx.notify();
    }

    /// Tab-completion for the terminal input: completes the last token against
    /// directory entries (and built-in commands for the first token).
    fn term_autocomplete(&mut self, cx: &mut Context<Self>) {
        let cwd = self.tab(self.active_pane).current_dir.clone();
        let input = self.term_input.clone();
        let (prefix, last) = match input.rfind(' ') {
            Some(i) => (input[..=i].to_string(), input[i + 1..].to_string()),
            None => (String::new(), input.clone()),
        };
        let is_command = prefix.is_empty();

        // Split the last token into a base directory and a partial name.
        let (base, partial) = match last.rfind('/') {
            Some(i) => (resolve_dir(&cwd, &last[..=i]), last[i + 1..].to_string()),
            None => (cwd.clone(), last.clone()),
        };

        let mut cands: Vec<(String, bool)> = list_dir_names(&base)
            .into_iter()
            .filter(|(n, _)| n.to_lowercase().starts_with(&partial.to_lowercase()))
            .collect();
        if is_command && last.rfind('/').is_none() {
            for c in ["cd", "ls", "clear", "mkdir", "rm", "cp", "mv", "open", "cat", "grep", "git"] {
                if c.starts_with(&partial) {
                    cands.push((c.to_string(), false));
                }
            }
        }
        if cands.is_empty() {
            return;
        }

        // Complete to the longest common prefix of the candidates.
        let common = longest_common_prefix(cands.iter().map(|(n, _)| n.as_str()));
        let base_str = match last.rfind('/') {
            Some(i) => last[..=i].to_string(),
            None => String::new(),
        };
        if cands.len() == 1 {
            let (name, is_dir) = &cands[0];
            let suffix = if *is_dir { "/" } else { "" };
            self.term_input = format!("{prefix}{base_str}{name}{suffix}");
        } else {
            if common.len() > partial.len() {
                self.term_input = format!("{prefix}{base_str}{common}");
            }
            // Show the options.
            let names: Vec<String> = cands.iter().map(|(n, _)| n.clone()).take(40).collect();
            self.term_push(names.join("    "));
        }
        cx.notify();
    }

    /// Keystrokes while the terminal input is focused.
    fn handle_term_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        match ks.key.as_str() {
            "escape" => {
                self.term_focused = false;
                cx.notify();
            }
            "enter" => self.run_term_command(cx),
            "tab" => self.term_autocomplete(cx),
            "backspace" => {
                self.term_input.pop();
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.term_input.push_str(t.trim());
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return;
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        self.term_input.push_str(ch);
                        cx.notify();
                    }
                }
            }
        }
    }

    /// The bottom terminal-mode bar: scrollback + a prompt input line.
    fn render_terminal_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let cwd = path_label(&self.active_tab().current_dir);
        let focused = self.term_focused;

        let mut bar = div()
            .id("terminal-bar")
            .flex_none()
            .flex()
            .flex_col()
            .w_full()
            .bg(rgb(t.sidebar))
            .border_t_1()
            .border_color(rgb(if focused { t.accent } else { t.border }))
            .text_color(rgb(t.text))
            .font_family("monospace")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.term_focused = true;
                    window.focus(&this.focus);
                    cx.notify();
                }),
            );

        // Scrollback — only when the history toggle is on and there's output.
        if prefs().term_history && !self.term_output.is_empty() {
            let lines: Vec<AnyElement> = self
                .term_output
                .iter()
                .map(|l| {
                    div()
                        .text_xs()
                        .text_color(rgb(t.text_muted))
                        .child(l.clone())
                        .into_any_element()
                })
                .collect();
            bar = bar.child(
                div()
                    .id("terminal-out")
                    .max_h(px(140.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.term_scroll)
                    .px_3()
                    .py_1()
                    .flex()
                    .flex_col()
                    .children(lines),
            );
        }

        // Prompt line.
        bar.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(rgb(t.border))
                .child(div().flex_none().text_color(rgb(t.accent)).child(format!("{cwd} ❯")))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(if self.term_input.is_empty() && !focused {
                            "Type a command… (cd to navigate, Tab to autocomplete)".to_string()
                        } else if focused {
                            format!("{}\u{2502}", self.term_input)
                        } else {
                            self.term_input.clone()
                        }),
                ),
        )
    }

    /// The floating filter box, anchored bottom-right while find is active.
    fn render_find_box(&self, pane: usize, query: &str) -> impl IntoElement {
        let t = theme();
        let tab = self.tab(pane);
        let count = tab.find_results.len();
        // Operator awareness: colour the label when filters are active, and show
        // a "searching…" state while a content: query resolves via Spotlight.
        let fq = FilterQuery::parse(query);
        let filtered = fq.has_local_filters() || fq.content.is_some();
        let content_pending = fq
            .content
            .as_deref()
            .is_some_and(|term| tab.content_for.as_deref() != Some(term));
        // The editable text: placeholder, a highlighted selection span, or the
        // text split around a static caret at the cursor.
        let field = if query.is_empty() {
            div()
                .min_w(px(80.0))
                .text_color(rgb(t.text_dim))
                .child("filter…  kind: ext: size: date: content:")
        } else if let Some((lo, hi)) = self.find_sel(pane) {
            let (bl, bh) = (char_byte(query, lo), char_byte(query, hi));
            div()
                .flex()
                .items_center()
                .min_w(px(80.0))
                .child(div().flex_none().child(query[..bl].to_string()))
                .child(
                    div()
                        .flex_none()
                        .bg(Theme::alpha(t.accent, 0x66))
                        .rounded_sm()
                        .child(query[bl..bh].to_string()),
                )
                .child(div().flex_none().child(query[bh..].to_string()))
        } else {
            let cursor = tab.find_cursor.min(query.chars().count());
            let b = char_byte(query, cursor);
            div()
                .flex()
                .items_center()
                .min_w(px(80.0))
                .child(div().flex_none().child(query[..b].to_string()))
                .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(rgb(t.text)))
                .child(div().flex_none().child(query[b..].to_string()))
        };
        div()
            .absolute()
            .bottom(px(16.0))
            .right(px(16.0))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(Theme::alpha(t.surface, 0xf2))
            .border_1()
            .border_color(rgb(t.accent))
            .shadow_lg()
            .text_color(rgb(t.text))
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(if filtered { t.accent } else { t.text_muted }))
                    .child("Filter"),
            )
            .child(field)
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(t.text_muted))
                    .text_xs()
                    .child(if content_pending {
                        "searching…".to_string()
                    } else {
                        format!("{count}")
                    }),
            )
    }

    /// Bottom-right filter affordance for a pane. When a find is active it shows
    /// the live editable box; otherwise it shows a compact, clickable "Filter"
    /// pill so mouse users can start filtering without pressing "/".
    fn render_filter_box(&self, pane: usize, cx: &Context<Self>) -> AnyElement {
        let t = theme();
        match self.tab(pane).find_query.clone() {
            Some(q) => self.render_find_box(pane, &q).into_any_element(),
            // The always-on pill can be hidden in Settings; "/" still opens it.
            None if !prefs().show_filter_button => gpui::Empty.into_any_element(),
            None => div()
                .id(("filter-pill", pane))
                .absolute()
                .bottom(px(16.0))
                .right(px(16.0))
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .py_1()
                .rounded_lg()
                .cursor_pointer()
                .bg(Theme::alpha(t.surface, 0xe6))
                .border_1()
                .border_color(rgb(t.border_strong))
                .shadow_lg()
                .text_color(rgb(t.text_muted))
                .hover(|s| s.text_color(rgb(t.text)).border_color(rgb(t.accent)))
                .child(div().flex_none().text_xs().child("🔍"))
                .child(div().flex_none().child("Filter"))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.active_pane = pane;
                    this.open_find(pane, cx);
                    window.focus(&this.focus);
                }))
                .into_any_element(),
        }
    }

    /// Build macOS file-type icons for the current directory off the render
    /// thread, one file-type at a time, yielding between each so scrolling stays
    /// smooth. Until an icon is ready the row shows the emoji placeholder.
    fn prewarm_icons(&self, cx: &mut Context<Self>) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut jobs: Vec<(String, PathBuf)> = Vec::new();
        ICON_CACHE.with(|cache| {
            let cache = cache.borrow();
            let mut has_dir = false;
            // Warm icons for the active tab of every visible pane.
            for pane in &self.panes {
                let tab = pane.active_tab();
                for entry in &tab.entries {
                    if entry.is_dir {
                        has_dir = true;
                        continue;
                    }
                    let path = tab.current_dir.join(&entry.name);
                    if let Some(key) = icon_key(&path) {
                        // Skip types we've already built or already queued.
                        if cache.contains_key(&key) || !seen.insert(key.clone()) {
                            continue;
                        }
                        jobs.push((key, path));
                    }
                }
            }
            // The shared generic folder icon, built once for all directories.
            if has_dir && !cache.contains_key(FOLDER_KEY) {
                jobs.push((FOLDER_KEY.to_string(), folder_dir_path()));
            }
        });
        if jobs.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            for (key, path) in jobs {
                // An active icon pack overrides the macOS icon. Pack images are
                // read + decoded entirely off the main thread.
                let built = if let Some(pack_file) = pack_icon_path(&key) {
                    cx.background_spawn(async move { decode_image_file(&pack_file) })
                        .await
                } else {
                    // AppKit's icon fetch must stay on the main thread (calling it
                    // off-main can deadlock), but it's the cheap part. The heavy
                    // decode/resize runs on a background thread.
                    let tiff = icon_tiff(&path);
                    match tiff {
                        Some(t) => cx.background_spawn(async move { decode_icon(&t) }).await,
                        None => None,
                    }
                };
                ICON_CACHE.with(|cache| {
                    cache.borrow_mut().insert(key, built);
                });
                // Repaint so the freshly-built icon appears; stop if the view
                // is gone. (The decode already happened off-thread above.)
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Pin the current directory to bookmarks (no-op if already pinned).
    fn add_bookmark(&mut self, cx: &mut Context<Self>) {
        let dir = self.active_tab().current_dir.clone();
        self.bookmark_path(dir, cx);
    }

    /// Pin an arbitrary path (file or folder) to the Bookmarks section.
    fn bookmark_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.bookmarks.iter().any(|b| b == &path) {
            self.bookmarks.push(path);
            write_path_list("bookmarks.txt", &self.bookmarks);
            cx.notify();
        }
    }

    /// Whether `path` is the target of the currently-open right-click menu
    /// (so its row can keep the hovered look while the menu is up).
    fn is_ctx_target(&self, path: &Path) -> bool {
        self.context_menu
            .as_ref()
            .and_then(|m| m.target.as_ref())
            .is_some_and(|(p, _)| p.as_path() == path)
    }

    /// Remove a bookmark (from the right-click "Remove Bookmark" action).
    fn remove_bookmark(&mut self, path: &Path, cx: &mut Context<Self>) {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b != path);
        if self.bookmarks.len() != before {
            write_path_list("bookmarks.txt", &self.bookmarks);
            cx.notify();
        }
    }

    // ----- sidebar groups -----

    /// Open the "New Group" naming dialog.
    fn open_group_dialog(&mut self, cx: &mut Context<Self>) {
        self.sidebar_menu = None;
        self.group_dialog = Some(String::new());
        cx.notify();
    }

    /// Create a group with the given name (ignored if blank or a duplicate).
    fn create_group(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if !name.is_empty() && !self.groups.iter().any(|g| g.name == name) {
            self.groups.push(Group { name, paths: Vec::new() });
            save_groups(&self.groups);
        }
        self.group_dialog = None;
        cx.notify();
    }

    /// Delete a group entirely.
    fn delete_group(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.groups.len() {
            self.groups.remove(idx);
            save_groups(&self.groups);
            cx.notify();
        }
    }

    /// Add a path (file or folder) to a group.
    fn add_to_group(&mut self, idx: usize, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(g) = self.groups.get_mut(idx) {
            if !g.paths.contains(&path) {
                g.paths.push(path);
                save_groups(&self.groups);
                cx.notify();
            }
        }
    }

    /// Remove a path from a group.
    fn remove_from_group(&mut self, idx: usize, path: &Path, cx: &mut Context<Self>) {
        if let Some(g) = self.groups.get_mut(idx) {
            let before = g.paths.len();
            g.paths.retain(|p| p != path);
            if g.paths.len() != before {
                save_groups(&self.groups);
                cx.notify();
            }
        }
    }

    fn open_sidebar_menu(&mut self, x: f32, y: f32, target: SidebarTarget, cx: &mut Context<Self>) {
        self.sidebar_menu = Some((x, y, target));
        cx.notify();
    }

    fn close_sidebar_menu(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_menu.take().is_some() {
            cx.notify();
        }
    }

    fn handle_group_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        match ks.key.as_str() {
            "escape" => {
                self.group_dialog = None;
                cx.notify();
            }
            "enter" => {
                if let Some(name) = self.group_dialog.clone() {
                    self.create_group(&name, cx);
                }
            }
            "backspace" => {
                if let Some(s) = self.group_dialog.as_mut() {
                    s.pop();
                }
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(s) = self.group_dialog.as_mut() {
                        s.push_str(t.trim());
                    }
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return;
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        if let Some(s) = self.group_dialog.as_mut() {
                            s.push_str(ch);
                        }
                        cx.notify();
                    }
                }
            }
        }
    }

    /// Toggle whether a sidebar section (by title) is collapsed, and persist it.
    fn toggle_section(&mut self, title: String, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(&title) {
            self.collapsed_sections.insert(title);
        }
        let list: Vec<String> = self.collapsed_sections.iter().cloned().collect();
        write_string_list("collapsed_sections.txt", &list);
        cx.notify();
    }

    /// A collapsible section header: a ▾/▸ arrow + title that toggles the
    /// section, plus an optional trailing element (e.g. the Bookmarks "+").
    fn section_header_el(
        &self,
        title: &'static str,
        trailing: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = theme();
        let is_col = self.collapsed_sections.contains(title);
        let arrow = if is_col { "▸" } else { "▾" };
        let title_owned = title.to_string();
        let mut row = div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .pt_4()
            .pb_1()
            .child(
                div()
                    .id(SharedString::from(format!("sec-{title}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(t.text_dim))
                    .hover(|s| s.text_color(rgb(t.text)))
                    .child(div().w(px(10.0)).child(arrow.to_string()))
                    .child(title.to_string())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_section(title_owned.clone(), cx);
                    })),
            );
        if let Some(tr) = trailing {
            row = row.child(tr);
        }
        row.into_any_element()
    }

    /// Push a collapsible section header (expanded sidebar) or a divider
    /// (icon-only rail). Returns whether the section's items should be rendered.
    /// Ensure a folder's subfolders are loaded into the waterfall cache. Reads
    /// the directory off the main thread; when it lands, the tree re-renders.
    fn ensure_waterfall_children(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.waterfall_children.contains_key(&dir) || self.waterfall_pending.contains(&dir) {
            return;
        }
        self.waterfall_pending.insert(dir.clone());
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let (subs, mtime) = cx.background_spawn(async move { read_subdirs(&d) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(m) = mtime {
                    this.waterfall_mtime.insert(dir.clone(), m);
                }
                this.waterfall_children.insert(dir.clone(), subs);
                this.waterfall_pending.remove(&dir);
                cx.notify();
            });
        })
        .detach();
    }

    /// Make sure every folder the tree will render has its children loading —
    /// the root, each expanded folder's children, and one level beyond the
    /// visible frontier so we know which rows get a disclosure triangle.
    /// Called each render, before the tree is built (which only has `&self`).
    fn ensure_waterfall_loaded(&mut self, cx: &mut Context<Self>) {
        if !prefs().waterfall || prefs().sidebar_collapsed {
            return;
        }
        // The waterfall is local-only; remote tabs have no cheap subdir listing.
        if self.active_tab().remote.is_some() {
            return;
        }
        let root = self.active_tab().current_dir.clone();
        self.waterfall_prefetch(root, cx);
    }

    /// Load `dir`'s children, and — so every visible row knows whether to show a
    /// triangle — each child's children too. Recurse only into expanded folders.
    fn waterfall_prefetch(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.ensure_waterfall_children(dir.clone(), cx);
        let Some(children) = self.waterfall_children.get(&dir).cloned() else {
            return; // still loading; we'll prefetch its children next frame
        };
        for child in children {
            if self.waterfall_expanded.contains(&child) {
                self.waterfall_prefetch(child, cx);
            } else {
                // One level ahead: enough to know if it has subfolders.
                self.ensure_waterfall_children(child, cx);
            }
        }
    }

    /// Expand or collapse a folder in the waterfall tree.
    fn toggle_waterfall(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.waterfall_expanded.contains(&dir) {
            self.waterfall_expanded.remove(&dir);
        } else {
            self.waterfall_expanded.insert(dir.clone());
            self.ensure_waterfall_children(dir, cx);
        }
        cx.notify();
    }

    /// Watcher hook: re-stat every cached waterfall folder and invalidate any
    /// whose mtime changed, so the tree live-refreshes when files appear or
    /// disappear. Runs off the main thread; returns true if anything changed.
    fn refresh_waterfall(&mut self, cx: &mut Context<Self>) {
        if !prefs().waterfall || self.waterfall_mtime.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = self.waterfall_mtime.keys().cloned().collect();
        cx.spawn(async move |this, cx| {
            let sampled = paths.clone();
            let stats = cx
                .background_spawn(async move {
                    sampled
                        .iter()
                        .map(|d| fs::metadata(d).ok().and_then(|m| m.modified().ok()))
                        .collect::<Vec<Option<SystemTime>>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let mut dirty = false;
                for (dir, cur) in paths.into_iter().zip(stats) {
                    match (this.waterfall_mtime.get(&dir).copied(), cur) {
                        // Folder changed on disk: drop the cache so the next
                        // render reloads it via `ensure_waterfall_children`.
                        (Some(prev), Some(now)) if prev != now => {
                            this.waterfall_children.remove(&dir);
                            this.waterfall_mtime.remove(&dir);
                            dirty = true;
                        }
                        // Folder vanished: forget it entirely.
                        (Some(_), None) => {
                            this.waterfall_children.remove(&dir);
                            this.waterfall_mtime.remove(&dir);
                            this.waterfall_expanded.remove(&dir);
                            dirty = true;
                        }
                        _ => {}
                    }
                }
                if dirty {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Append the waterfall folder tree (rooted at the active dir) to `items`.
    fn render_waterfall(
        &self,
        items: &mut Vec<AnyElement>,
        key: &mut usize,
        current: &Path,
        cx: &Context<Self>,
    ) {
        let root = self.active_tab().current_dir.clone();
        items.push(self.section_header_el("WATERFALL", None, cx));
        if self.collapsed_sections.contains("WATERFALL") {
            return;
        }
        let mut rows: Vec<AnyElement> = Vec::new();
        self.waterfall_rows(&root, 0, key, current, &mut rows, cx);
        if rows.is_empty() {
            items.push(empty_hint("No subfolders").into_any_element());
        } else {
            items.append(&mut rows);
        }
    }

    /// Depth-first: render each subfolder of `dir`, recursing into expanded ones
    /// so children appear indented directly beneath their parent.
    fn waterfall_rows(
        &self,
        dir: &Path,
        depth: usize,
        key: &mut usize,
        current: &Path,
        out: &mut Vec<AnyElement>,
        cx: &Context<Self>,
    ) {
        let Some(children) = self.waterfall_children.get(dir) else {
            return;
        };
        let t = theme();
        for child in children {
            *key += 1;
            let expanded = self.waterfall_expanded.contains(child);
            let active = child.as_path() == current;
            let label = path_label(child);
            let indent = 8.0 + depth as f32 * 12.0;
            // Only folders with subfolders get a triangle. `None` means the
            // child's listing hasn't loaded yet (prefetch fills it in shortly),
            // so we show a spacer to avoid a triangle that flips to nothing.
            let has_subdirs = self
                .waterfall_children
                .get(child)
                .map(|v| !v.is_empty())
                .unwrap_or(false);

            let toggle_target = child.clone();
            let nav_target = child.clone();
            let mut row = div()
                .id(("wf", *key))
                .flex()
                .items_center()
                .gap_1()
                .mx_2()
                .py_1()
                .pr_2()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if active { t.text } else { t.text_muted }));
            row = if active {
                row.bg(rgb(t.surface))
            } else {
                row.hover(|s| s.bg(rgb(t.hover))).active(|s| s.bg(rgb(t.selected)))
            };
            // Disclosure triangle (or a blank spacer of the same width so names
            // stay aligned whether or not a folder can expand).
            let tri = if has_subdirs {
                let arrow = if expanded { "▾" } else { "▸" };
                div()
                    .id(("wf-tri", *key))
                    .w(px(12.0))
                    .flex()
                    .justify_center()
                    .text_color(rgb(t.text_dim))
                    .hover(|s| s.text_color(rgb(t.text)))
                    .child(arrow)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_waterfall(toggle_target.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element()
            } else {
                div().w(px(12.0)).into_any_element()
            };
            let row = row
                .pl(px(indent))
                .child(tri)
                .child(icon_element(child, true))
                .child(div().min_w_0().overflow_hidden().child(label))
                // Clicking the row (name/icon) opens the folder in the pane.
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.navigate_to(nav_target.clone(), cx);
                }));
            out.push(row.into_any_element());

            if expanded && has_subdirs {
                self.waterfall_rows(child, depth + 1, key, current, out, cx);
            }
        }
    }

    fn begin_section(
        &self,
        items: &mut Vec<AnyElement>,
        title: &'static str,
        sidebar_collapsed: bool,
        cx: &Context<Self>,
    ) -> bool {
        if sidebar_collapsed {
            push_divider(items);
            return true;
        }
        items.push(self.section_header_el(title, None, cx));
        !self.collapsed_sections.contains(title)
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = self.active_tab().current_dir.clone();
        let current = current.as_path();
        let collapsed = prefs().sidebar_collapsed;
        let mut items: Vec<AnyElement> = Vec::new();
        let mut key = 0usize;

        // --- Collapse / expand toggle (always present) ---
        let chevron = if collapsed { "»" } else { "«" };
        items.push(
            div()
                .id("sidebar-toggle")
                .flex()
                .items_center()
                .when(collapsed, |d| d.justify_center())
                .when(!collapsed, |d| d.justify_end())
                .px_2()
                .pt_2()
                .pb_1()
                .cursor_pointer()
                .text_color(rgb(theme().text_dim))
                .hover(|s| s.text_color(rgb(theme().text)))
                .child(chevron)
                .tooltip(tip(if collapsed { "Expand sidebar" } else { "Collapse sidebar" }))
                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.sidebar_collapsed = !np.sidebar_collapsed;
                    apply_prefs(np, cx);
                    cx.notify();
                }))
                .into_any_element(),
        );

        // --- Favorites (Applications, Documents, …) ---
        if self.begin_section(&mut items, "FAVORITES", collapsed, cx) {
            for (label, slug) in SIDEBAR_FAVORITES {
                let path = fav_path(slug);
                if !cached_is_dir(&path) {
                    continue;
                }
                push_nav(
                    &mut items,
                    cx,
                    &mut key,
                    label.to_string(),
                    fav_key(slug),
                    path,
                    current,
                    collapsed,
                );
            }
        }

        // --- Bookmarks (with a "+" to pin the current folder) ---
        let show_bookmarks = if collapsed {
            push_divider(&mut items);
            items.push(
                div()
                    .id("add-bookmark")
                    .flex()
                    .items_center()
                    .justify_center()
                    .mx_1()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(theme().text_dim))
                    .hover(|s| s.bg(rgb(theme().hover)).text_color(rgb(theme().text)))
                    .child("+")
                    .tooltip(tip("Pin current folder"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.add_bookmark(cx);
                    }))
                    .into_any_element(),
            );
            true
        } else {
            let plus = div()
                .id("add-bookmark")
                .cursor_pointer()
                .px_1()
                .text_color(rgb(theme().text_dim))
                .hover(|s| s.text_color(rgb(theme().text)))
                .child("+")
                .tooltip(tip("Pin current folder"))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.add_bookmark(cx);
                }))
                .into_any_element();
            items.push(self.section_header_el("BOOKMARKS", Some(plus), cx));
            !self.collapsed_sections.contains("BOOKMARKS")
        };
        if show_bookmarks {
            if self.bookmarks.is_empty() {
                if !collapsed {
                    items.push(empty_hint("Click + to pin a folder").into_any_element());
                }
            } else {
                for p in &self.bookmarks {
                    push_bookmark_nav(&mut items, cx, &mut key, p.clone(), current, collapsed);
                }
            }
        }

        // --- Groups (user-defined; only when the feature is enabled) ---
        if prefs().groups_enabled {
            for (gidx, g) in self.groups.iter().enumerate() {
                if collapsed {
                    push_divider(&mut items);
                    for p in &g.paths {
                        push_group_member(&mut items, cx, &mut key, gidx, p.clone(), current, true);
                    }
                    continue;
                }
                let ckey = format!("group:{}", g.name);
                let is_col = self.collapsed_sections.contains(&ckey);
                let arrow = if is_col { "▸" } else { "▾" };
                let toggle_key = ckey.clone();
                items.push(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .pt_4()
                        .pb_1()
                        .child(
                            div()
                                .id(SharedString::from(format!("grp-{gidx}")))
                                .flex()
                                .items_center()
                                .gap_1()
                                .cursor_pointer()
                                .text_xs()
                                .text_color(rgb(theme().text_dim))
                                .hover(|s| s.text_color(rgb(theme().text)))
                                .child(div().w(px(10.0)).child(arrow.to_string()))
                                .child(g.name.to_uppercase())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.toggle_section(toggle_key.clone(), cx);
                                }))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                        let (x, y) = (
                                            f64::from(ev.position.x) as f32,
                                            f64::from(ev.position.y) as f32,
                                        );
                                        this.open_sidebar_menu(
                                            x,
                                            y,
                                            SidebarTarget::GroupHeader(gidx),
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    }),
                                ),
                        )
                        .into_any_element(),
                );
                if !is_col {
                    if g.paths.is_empty() {
                        items.push(empty_hint("Right-click a file to add it").into_any_element());
                    } else {
                        for p in &g.paths {
                            push_group_member(&mut items, cx, &mut key, gidx, p.clone(), current, false);
                        }
                    }
                }
            }
        }

        // --- Recents (count is user-configurable; 0 hides the section) ---
        let recent_limit = prefs().recent_limit;
        if recent_limit > 0 && self.begin_section(&mut items, "RECENTS", collapsed, cx) {
            if self.recents.is_empty() {
                if !collapsed {
                    items.push(empty_hint("No recent folders").into_any_element());
                }
            } else {
                for p in self.recents.iter().take(recent_limit) {
                    push_nav(
                        &mut items,
                        cx,
                        &mut key,
                        path_label(p),
                        FOLDER_KEY.to_string(),
                        p.clone(),
                        current,
                        collapsed,
                    );
                }
            }
        }

        // --- Cloud (Dropbox, Google Drive, iCloud, …) ---
        let (cloud, volumes) = sidebar_locations();
        if !cloud.is_empty() && self.begin_section(&mut items, "CLOUD", collapsed, cx) {
            for (label, path) in cloud {
                let icon_key = path.to_string_lossy().into_owned();
                push_nav(&mut items, cx, &mut key, label, icon_key, path, current, collapsed);
            }
        }

        // --- Servers (the Mac, mounted volumes/shares, Connect to Server) ---
        if self.begin_section(&mut items, "SERVERS", collapsed, cx) {
            // Servers holds real servers/shares: mounted (browseable) volumes,
            // saved SFTP servers, and Connect to Server. The boot disk and home
            // live in Favorites, so they aren't duplicated here.
            for (label, path) in volumes {
                let icon_key = path.to_string_lossy().into_owned();
                push_nav(&mut items, cx, &mut key, label, icon_key, path, current, collapsed);
            }
            // Saved SFTP servers — click to connect and browse remotely.
            for server in sftp_servers() {
                key += 1;
                let s = server.clone();
                let label = server.name.clone();
                let tip_text = format!("{}\nsftp://{}", server.name, server.display());
                let row = div()
                    .id(("sftp", key))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(theme().text_muted))
                    .hover(|s| s.bg(rgb(theme().hover)).text_color(rgb(theme().text)));
                let row = if collapsed {
                    row.mx_1().py_1().justify_center().child("🖧")
                } else {
                    row.mx_2()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .child(div().w(px(16.0)).flex().justify_center().child("🖧"))
                        .child(div().min_w_0().truncate().child(label))
                };
                let s2 = server.clone();
                items.push(
                    row.tooltip(tip(tip_text))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.connect_sftp(s.clone(), cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                let (x, y) = (
                                    f64::from(ev.position.x) as f32,
                                    f64::from(ev.position.y) as f32,
                                );
                                this.open_sidebar_menu(x, y, SidebarTarget::Sftp(s2.clone()), cx);
                                cx.stop_propagation();
                            }),
                        )
                        .into_any_element(),
                );
            }
            // "Connect to Server…" action row.
            let base = div()
                .id("connect-server")
                .flex()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(theme().text_muted))
                .hover(|s| s.bg(rgb(theme().hover)).text_color(rgb(theme().text)));
            let base = if collapsed {
                base.mx_1().py_1().justify_center().child("🌐")
            } else {
                base.mx_2()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(div().w(px(16.0)).flex().justify_center().child("🌐"))
                    .child("Connect to Server…")
            };
            items.push(
                base.tooltip(tip("Connect to Server…"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.open_server_dialog(cx);
                    }))
                    .into_any_element(),
            );
        }

        // --- Waterfall: an inline, expandable folder tree of the active dir ---
        if prefs().waterfall && !collapsed && self.active_tab().remote.is_none() {
            self.render_waterfall(&mut items, &mut key, current, cx);
        }

        let groups_on = prefs().groups_enabled;
        div()
            .id("sidebar")
            .flex_none()
            .w(px(if collapsed { SIDEBAR_COLLAPSED_W } else { SIDEBAR_W }))
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb_3()
            .bg(rgb(theme().sidebar))
            .border_r_1()
            .border_color(rgb(theme().border))
            // Right-click empty sidebar space → "New Group" (when enabled).
            .when(groups_on, |d| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        let (x, y) = (
                            f64::from(ev.position.x) as f32,
                            f64::from(ev.position.y) as f32,
                        );
                        this.open_sidebar_menu(x, y, SidebarTarget::Empty, cx);
                    }),
                )
            })
            .children(items)
    }

    /// The top bar for a pane: back/forward arrows then either the clickable
    /// breadcrumb or, in edit mode, an editable text field.
    fn render_path_bar(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let tab = self.tab(pane);
        let can_back = tab.hist_pos > 0;
        let can_fwd = tab.hist_pos + 1 < tab.history.len();

        let content: AnyElement = if tab.editing_path.is_some() {
            self.render_path_editor(pane).into_any_element()
        } else {
            self.render_breadcrumb(pane, cx).into_any_element()
        };

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .min_w_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(rgb(theme().border))
            // Right-click the nav bar → New Tab / Copy Path / Reveal in Finder.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    let (x, y) = (
                        f64::from(ev.position.x) as f32,
                        f64::from(ev.position.y) as f32,
                    );
                    this.open_sidebar_menu(x, y, SidebarTarget::NavBar(pane), cx);
                    cx.stop_propagation();
                }),
            )
            .child(nav_arrow(
                ("nav-back", pane),
                "‹",
                can_back,
                cx.listener(move |this, _, _, cx| this.go_back(pane, cx)),
            ))
            .child(nav_arrow(
                ("nav-fwd", pane),
                "›",
                can_fwd,
                cx.listener(move |this, _, _, cx| this.go_forward(pane, cx)),
            ))
            .child(content)
            .child(self.render_view_toolbar(pane, cx))
    }

    /// View-mode switcher (list / icons / gallery) + the Sort-By button.
    fn render_view_toolbar(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let view = self.tab(pane).view;
        let btn = |id: &'static str, glyph: &'static str, name: &'static str, mode: ViewMode, cx: &Context<Self>| {
            let on = view == mode;
            div()
                .id((id, pane)) // ids must be unique per pane, or only one pane works
                .flex_none()
                .w(px(24.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .text_color(if on { rgb(t.text) } else { rgb(t.text_dim) })
                .when(on, |s| s.bg(rgb(t.surface)))
                .hover(|s| s.bg(rgb(t.hover)))
                .child(glyph)
                .tooltip(tip(name))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_view(pane, mode, cx);
                }))
        };
        // Magnifier that opens the command palette (Cmd+P) — a mouse-reachable
        // entry point sitting alongside the view-mode icons.
        let search_btn = div()
            .id(("palette-search", pane))
            .flex_none()
            .w(px(24.0))
            .h(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(rgb(t.text_dim))
            .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
            .child("🔍")
            .tooltip(tip("Search (⌘P)"))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.active_pane = pane;
                if !this.palette_open {
                    this.toggle_palette(window, cx);
                }
            }));
        // New Folder — creates a folder in this pane and drops into rename.
        // Remote tabs can't create folders locally, so hide it there.
        let is_remote = self.tab(pane).remote.is_some();
        let new_folder_btn = div()
            .id(("tb-new-folder", pane))
            .flex_none()
            .w(px(24.0))
            .h(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(rgb(t.text_dim))
            .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
            .child("🗀")
            .tooltip(tip("New Folder (⇧⌘N)"))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.active_pane = pane;
                this.new_folder(pane, window, cx);
            }));
        // Delete — Trash the selection; greyed out when nothing is selected.
        let has_sel =
            !self.tab(pane).selection.is_empty() || self.tab(pane).anchor.is_some();
        let delete_btn = div()
            .id(("tb-delete", pane))
            .flex_none()
            .w(px(24.0))
            .h(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .when(has_sel, |d| d.text_color(rgb(t.text_dim)))
            .when(!has_sel, |d| d.text_color(Theme::alpha(t.text_dim, 0x66)))
            .child("🗑")
            .tooltip(tip(if has_sel { "Move to Trash (⌫)" } else { "Move to Trash (select an item first)" }))
            .when(has_sel, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.active_pane = pane;
                        this.request_delete(pane, cx);
                    }))
            });
        // Sort has no effect in Column view (columns are always folders-first,
        // by name), so grey it out and make it inert there rather than misleading.
        let sort_enabled = view != ViewMode::Columns;
        let sort_btn = div()
            .id(("sort-by", pane))
            .flex_none()
            .px_2()
            .h(px(22.0))
            .flex()
            .items_center()
            .rounded_md()
            .when(sort_enabled, |d| d.text_color(rgb(t.text_dim)))
            .when(!sort_enabled, |d| d.text_color(Theme::alpha(t.text_dim, 0x66)))
            .child("⇅")
            .tooltip(tip(if sort_enabled { "Sort By" } else { "Sort By (not available in Column view)" }))
            .when(sort_enabled, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let (x, y) = (
                                f64::from(ev.position.x) as f32,
                                f64::from(ev.position.y) as f32,
                            );
                            this.sort_menu = Some((pane, x, y));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
            });
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .pl_2()
            // File actions (hidden on remote tabs, which can't create locally).
            .when(!is_remote, |d| d.child(new_folder_btn).child(delete_btn))
            .child(search_btn)
            .child(btn("view-list", "☰", "List view", ViewMode::List, cx))
            .child(btn("view-icons", "▦", "Icon view", ViewMode::Icons, cx))
            .child(btn("view-columns", "▥", "Column view", ViewMode::Columns, cx))
            .child(btn("view-gallery", "▭", "Gallery view", ViewMode::Gallery, cx))
            .child(sort_btn)
    }

    /// Clickable breadcrumb for a pane. Segments up to and including the current
    /// directory are bright; any deeper "forward tail" is grayed but still
    /// clickable. Empty space to the right enters edit mode.
    fn render_breadcrumb(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        use std::path::Component;

        let tab = self.tab(pane);
        let current_dir = tab.current_dir.clone();
        // Show the deepest tail if the current dir is an ancestor of it.
        let display = match &tab.deepest {
            Some(d) if d.starts_with(&current_dir) => d.clone(),
            _ => current_dir.clone(),
        };

        // A remote tab's root is the server, not the local disk.
        let root_label = match &tab.remote {
            Some(s) => s.name.clone(),
            None => "Macintosh HD".to_string(),
        };
        let mut segs: Vec<AnyElement> = Vec::new();
        let mut acc = PathBuf::new();
        let mut idx = 0usize;
        for comp in display.components() {
            let (label, full) = match comp {
                Component::RootDir => {
                    acc.push("/");
                    (root_label.clone(), acc.clone())
                }
                Component::Normal(s) => {
                    acc.push(s);
                    (s.to_string_lossy().into_owned(), acc.clone())
                }
                _ => continue,
            };
            if idx > 0 {
                segs.push(breadcrumb_sep());
            }
            let active = current_dir.starts_with(&full);
            segs.push(breadcrumb_seg(pane * 4096 + idx, pane, label, full, active, cx));
            idx += 1;
        }

        div()
            .id(("breadcrumb", pane))
            .flex()
            .items_center()
            .flex_1()
            .min_w_0()
            // Clip so long paths can't paint over the view/sort toolbar (or
            // bleed into the neighbouring pane) when a pane is narrow.
            .overflow_hidden()
            .h(px(22.0))
            .children(segs)
            // Filler captures clicks on the empty part of the bar → edit mode.
            .child(
                div()
                    .id(("path-edit-zone", pane))
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.begin_path_edit(pane, window, cx)
                    })),
            )
    }

    /// The editable address-bar field shown in edit mode.
    fn render_path_editor(&self, pane: usize) -> impl IntoElement {
        let t = theme();
        let tab = self.tab(pane);
        let text = tab.editing_path.clone().unwrap_or_default();

        // The editable text: either a highlighted selection span, or the text
        // split around a static caret at the cursor.
        let field = if let Some((lo, hi)) = self.path_sel(pane) {
            let (bl, bh) = (char_byte(&text, lo), char_byte(&text, hi));
            div()
                .flex()
                .items_center()
                .min_w_0()
                .child(div().flex_none().child(text[..bl].to_string()))
                .child(
                    div()
                        .flex_none()
                        .bg(Theme::alpha(t.accent, 0x66))
                        .rounded_sm()
                        .child(text[bl..bh].to_string()),
                )
                .child(div().flex_none().child(text[bh..].to_string()))
        } else {
            let cursor = tab.path_cursor.min(text.chars().count());
            let b = char_byte(&text, cursor);
            div()
                .flex()
                .items_center()
                .min_w_0()
                .child(div().flex_none().child(text[..b].to_string()))
                // Blinking would need a timer; a static caret reads clearly.
                .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(rgb(t.text)))
                .child(div().flex_none().child(text[b..].to_string()))
        };

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .px_2()
            .h(px(22.0))
            .rounded_md()
            .bg(rgb(t.surface))
            .border_1()
            .border_color(rgb(t.accent))
            .text_color(rgb(t.text))
            .child(field)
    }

    /// The canvas: one full-width pane, or two panes split by a draggable
    /// divider, plus a right-edge drop zone (shown while dragging a tab).
    fn render_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut row = div().flex_1().flex().min_w_0().h_full().relative();

        if self.panes.len() == 1 {
            // After a split collapses, the survivor eases from its old width to
            // full; anchored right when the *left* pane was the one closed, so
            // it grows toward where its sibling was.
            if let Some((start_w, anchor_right)) = self.collapse_anim {
                if anchor_right {
                    row = row.justify_end();
                }
                row = row.child(
                    div()
                        .flex_none()
                        .min_w_0()
                        .h_full()
                        .child(self.render_pane(0, cx))
                        .with_animation(
                            ("split-close", self.split_epoch),
                            Animation::new(Duration::from_millis(SPLIT_ANIM_MS))
                                .with_easing(ease_out_quint()),
                            move |el, t| el.w(relative(start_w + (1.0 - start_w) * t)),
                        ),
                );
            } else {
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.render_pane(0, cx)),
                );
            }
        } else {
            // On split creation the left pane eases from full width down to the
            // ratio, so the right pane slides in from the edge. Once finished
            // the animation pins at t=1, i.e. exactly `split_ratio`, which the
            // divider drag then updates live.
            let ratio = self.split_ratio;
            row = row
                .child(
                    div()
                        .flex_none()
                        .min_w_0()
                        .h_full()
                        .child(self.render_pane(0, cx))
                        .with_animation(
                            ("split-open", self.split_epoch),
                            Animation::new(Duration::from_millis(SPLIT_ANIM_MS))
                                .with_easing(ease_out_quint()),
                            move |el, t| el.w(relative(1.0 + (ratio - 1.0) * t)),
                        ),
                )
                .child(self.render_divider(cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(self.render_pane(1, cx)),
                );
        }

        // Right-edge split target — only present while a *tab* is being
        // dragged, so it never blocks normal interaction and its border never
        // shows up as a stray line during file drags.
        if cx.has_active_drag() && TAB_DRAG_LIVE.load(Ordering::Relaxed) {
            let new_pane = self.panes.len();
            row = row.child(
                div()
                    .id("split-zone")
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(relative(0.32))
                    .border_l_2()
                    .border_color(rgb(theme().border))
                    .drag_over::<TabDrag>(|s, _, _, _| {
                        s.bg(Theme::alpha(theme().accent, 0x33))
                            .border_color(rgb(theme().accent))
                    })
                    .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                        this.move_tab(*drag, new_pane, 0, cx);
                    })),
            );
        }
        row
    }

    /// The draggable divider between two panes.
    fn render_divider(&self, cx: &Context<Self>) -> impl IntoElement {
        // Accent + slightly thicker while grabbed, so the drag feels "held".
        let dragging = self.divider_drag.is_some();
        div()
            .id("pane-divider")
            .flex_none()
            .w(px(6.0))
            .h_full()
            .flex()
            .justify_center()
            .cursor(CursorStyle::ResizeLeftRight)
            .child(
                div()
                    .w(px(if dragging { 2.0 } else { 1.0 }))
                    .h_full()
                    .bg(if dragging {
                        rgb(theme().accent)
                    } else {
                        rgb(theme().border_strong)
                    }),
            )
            .when(dragging, |s| s.bg(Theme::alpha(theme().accent, 0x22)))
            .hover(|s| s.bg(rgb(theme().hover)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    // Remember where the grab started and the ratio at that
                    // moment, so the drag continues from the current size rather
                    // than snapping the divider to the cursor.
                    this.divider_drag = Some((f64::from(ev.position.x) as f32, this.split_ratio));
                    cx.notify();
                }),
            )
    }

    /// Width of a pane's list viewport (≈ pane width), from its scroll state.
    fn pane_list_width(&self, pane: usize) -> f32 {
        let st = self.tab(pane).scroll_handle.0.borrow();
        f64::from(st.base_handle.bounds().size.width) as f32
    }

    /// Update the split ratio while dragging the divider. `x` is window-relative.
    /// Delta-based: new ratio = ratio-at-grab + (cursor moved) / content width.
    fn update_divider(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some((start_x, start_ratio)) = self.divider_drag else {
            return;
        };
        if self.panes.len() < 2 {
            return;
        }
        let content_w = (self.pane_list_width(0) + self.pane_list_width(1)).max(1.0);
        let delta = (x - start_x) / content_w;
        self.split_ratio = (start_ratio + delta).clamp(0.2, 0.8);
        cx.notify();
    }

    /// The tab strip atop a pane: draggable tab chips + a "+" button.
    fn render_tab_strip(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let p = self.pane(pane);
        let mut chips: Vec<AnyElement> = Vec::new();
        for (i, tab) in p.tabs.iter().enumerate() {
            let active = i == p.active && pane == self.active_pane;
            // A remote tab at its root shows the server name, not "Macintosh HD".
            let label = match &tab.remote {
                Some(s) if tab.current_dir == Path::new("/") => s.name.clone(),
                _ => path_label(&tab.current_dir),
            };
            let drag_label = label.clone();
            let drag = TabDrag { pane, tab: i };
            chips.push(
                div()
                    .id(("tab", pane * 4096 + i))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(TAB_H - 6.0))
                    .max_w(px(180.0))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if active { rgb(t.text) } else { rgb(t.text_muted) })
                    .bg(if active { rgb(t.surface) } else { rgba(0x00000000) })
                    .hover(|s| s.bg(rgb(t.hover)))
                    .active(|s| s.bg(rgb(t.selected)))
                    // Drag this tab (live floating preview).
                    .on_drag(drag, move |_, _, _, cx| {
                        TAB_DRAG_LIVE.store(true, Ordering::Relaxed);
                        cx.new(|_| TabDragPreview {
                            label: drag_label.clone(),
                        })
                    })
                    // Drop another tab here → insert at this position.
                    .drag_over::<TabDrag>(|s, _, _, _| s.bg(Theme::alpha(theme().accent, 0x33)))
                    .on_drop(cx.listener(move |this, from: &TabDrag, _, cx| {
                        this.move_tab(*from, pane, i, cx);
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_tab(pane, i, cx);
                    }))
                    // Right-click → tab context menu (New/Duplicate/Close/…).
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let (x, y) = (
                                f64::from(ev.position.x) as f32,
                                f64::from(ev.position.y) as f32,
                            );
                            this.open_sidebar_menu(x, y, SidebarTarget::Tab(pane, i), cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .id(("tab-close", pane * 4096 + i))
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .text_color(rgb(t.text_dim))
                            .hover(|s| s.text_color(rgb(t.text)).bg(rgb(t.selected)))
                            .child("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.close_tab(pane, i, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .h(px(TAB_H))
            .bg(rgb(t.sidebar))
            .border_b_1()
            .border_color(rgb(t.border))
            // Dropping on empty strip space appends to this pane.
            .drag_over::<TabDrag>(|s, _, _, _| s.bg(rgb(theme().hover)))
            .on_drop(cx.listener(move |this, from: &TabDrag, _, cx| {
                let len = this.pane(pane).tabs.len();
                this.move_tab(*from, pane, len, cx);
            }))
            .children(chips)
            .child(
                div()
                    .id(("tab-add", pane))
                    .flex_none()
                    .w(px(22.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(t.text_muted))
                    .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
                    .active(|s| s.bg(rgb(t.selected)))
                    .child("+")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.new_tab_in(pane, cx);
                    })),
            )
    }

    /// Render one pane: tab strip → path bar → column header → virtualized
    /// listing + scrollbar, plus the right-edge split drop zone.
    fn render_pane(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let view = self.tab(pane).view;
        // Highlight the active pane's border (only meaningful when split).
        let split = self.panes.len() > 1;
        let active = pane == self.active_pane;
        let body: AnyElement = match view {
            ViewMode::List => self.render_list_body(pane, cx),
            ViewMode::Icons => self.render_icons_body(pane, cx),
            ViewMode::Columns => self.render_columns_body(pane, cx),
            ViewMode::Gallery => self.render_gallery_body(pane, cx),
        };

        div()
            .flex()
            .flex_col()
            .min_w_0()
            .h_full()
            .when(split && active, |s| {
                s.border_color(rgb(theme().accent))
            })
            // Clicking anywhere in the pane focuses it (and leaves terminal input).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, _| {
                    this.active_pane = pane;
                    this.term_focused = false;
                }),
            )
            .child(self.render_tab_strip(pane, cx))
            // Path bar: back/forward arrows + breadcrumb + view/sort toolbar.
            .child(self.render_path_bar(pane, cx))
            // Body plus the always-present bottom-right filter affordance,
            // overlaid in a shared relative container so it covers every view.
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .children(self.rename_error_pill(pane))
                    .child(self.render_filter_box(pane, cx)),
            )
    }

    /// The List view body: clickable column header + virtualized rows.
    fn render_list_body(&self, pane: usize, cx: &Context<Self>) -> AnyElement {
        let tab = self.tab(pane);
        // In find mode the list shows the filtered results (no ".." row);
        // otherwise the full directory with a leading ".." when there's a parent.
        let find_active = tab.find_query.is_some();
        let has_parent = !find_active && prefs().show_parent && tab.current_dir.parent().is_some();
        let item_count = if find_active {
            tab.find_results.len()
        } else {
            tab.entries.len() + usize::from(has_parent)
        };
        let scroll = tab.scroll_handle.clone();
        let h_scroll = tab.h_scroll.clone();
        let pane_dir = tab.current_dir.clone();
        let total_w =
            self.widths.name + self.widths.kind + self.widths.date + self.widths.size + 24.0;
        // Only engage horizontal scrolling when the columns genuinely overflow
        // the pane — otherwise the small x component of a trackpad flick
        // jiggles the listing sideways while scrolling vertically.
        let h_vw = f64::from(h_scroll.bounds().size.width) as f32;
        let h_overflows = h_vw <= 1.0 || total_w > h_vw + 1.0;
        if !h_overflows && h_scroll.offset().x < px(0.0) {
            let y = h_scroll.offset().y;
            h_scroll.set_offset(point(px(0.0), y));
        }

                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // Dropping file(s) on empty pane space moves them here. The
                    // tint comes from our tracked drop target rather than
                    // gpui's per-element drag hover, so it can't flicker
                    // between the source and destination panes.
                    .when(
                        cx.has_active_drag() && self.drop_hover == Some((pane, None)),
                        |s| s.bg(Theme::alpha(theme().accent, 0x22)),
                    )
                    .on_drop(cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                        this.drop_files(pane, pane_dir.clone(), drag.paths().to_vec(), cx);
                    }))
                    .child(
                        // Horizontal scroller holding the column header + rows, so
                        // they scroll sideways together and never overflow the pane.
                        div()
                            .id(("hscroll", pane))
                            .size_full()
                            .when(h_overflows, |d| d.overflow_x_scroll())
                            .track_scroll(&h_scroll)
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .h_full()
                                    .w_full()
                                    .min_w(px(total_w))
                                    .child(self.column_header(pane, cx))
                                    .child(
                                        div()
                                            .relative()
                                            .flex_1()
                                            .min_h_0()
                                            // Right-click empty space → New menu.
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                                    let (x, y) = (
                                                        f64::from(ev.position.x) as f32,
                                                        f64::from(ev.position.y) as f32,
                                                    );
                                                    this.open_context_menu(pane, x, y, None, cx);
                                                }),
                                            )
                                            // Press on blank space, or on a row's
                                            // Kind/Date/Size area (only the name
                                            // cell stops propagation), → start a
                                            // marquee. Skip modified presses so
                                            // Cmd/Shift-click keep their toggle /
                                            // range-extend meaning (a marquee
                                            // start would clear the selection).
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                                    if ev.modifiers.platform || ev.modifiers.shift {
                                                        return;
                                                    }
                                                    this.begin_marquee(
                                                        pane,
                                                        f64::from(ev.position.x) as f32,
                                                        f64::from(ev.position.y) as f32,
                                                        cx,
                                                    );
                                                }),
                                            )
                                            .children(self.marquee_rect(pane))
                                            .child(uniform_list(
                    ("file-list", pane),
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let widths = this.widths;
                        let tab = this.tab(pane);
                        let find_active = tab.find_query.is_some();
                        let has_parent = !find_active && prefs().show_parent && tab.current_dir.parent().is_some();
                        let base_dir = tab.current_dir.clone();
                        let mut items: Vec<AnyElement> = Vec::with_capacity(range.len());

                        for ix in range {
                            let row_key = pane * 100_000 + ix;
                            if has_parent && ix == 0 {
                                let parent =
                                    base_dir.parent().unwrap_or(Path::new("/")).to_path_buf();
                                let icon = icon_element(&parent, true);
                                items.push(
                                    file_row(
                                        "..",
                                        true,
                                        0,
                                        None,        // no size for the ".." row
                                        None,
                                        true,        // ".." has no metadata to load
                                        row_key,
                                        false,
                                        false,       // ".." is never the menu target
                                        widths,
                                        icon,
                                        true,        // accepts drops (move into parent)
                                        this.drop_hover == Some((pane, Some(ix))),
                                        None,        // never renamed
                                        cx.listener({
                                            let parent = parent.clone();
                                            move |this, _: &ClickEvent, _, cx| {
                                                // Finishing a marquee over ".." must
                                                // not navigate away.
                                                if this.marquee_click_suppressed() {
                                                    return;
                                                }
                                                this.navigate_in(pane, parent.clone(), cx);
                                            }
                                        }),
                                        // ".." → background (New Folder/File) menu.
                                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                            let (x, y) = (
                                                f64::from(ev.position.x) as f32,
                                                f64::from(ev.position.y) as f32,
                                            );
                                            this.open_context_menu(pane, x, y, None, cx);
                                            cx.stop_propagation();
                                        }),
                                        cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                                            this.drop_files(pane, parent.clone(), drag.paths().to_vec(), cx);
                                        }),
                                        // ".." isn't draggable; just swallow the press.
                                        |_, _, cx: &mut App| cx.stop_propagation(),
                                        // ".." can't be renamed; no hover tracking.
                                        |_: &bool, _, _| {},
                                    )
                                    .into_any_element(),
                                );
                                continue;
                            }

                            let tab = this.tab(pane);
                            let entry_ix = if find_active {
                                tab.find_results[ix]
                            } else if has_parent {
                                ix - 1
                            } else {
                                ix
                            };
                            let entry = &tab.entries[entry_ix];
                            let name = entry.name.clone();
                            let is_dir = entry.is_dir;
                            let entry_size = entry.size;
                            let modified = entry.modified;
                            let entry_loaded = entry.loaded;
                            let target = base_dir.join(&name);
                            let cloud = this.cloud_state(&target, is_dir);
                            // Folder sizes: look up the cached total, kicking off
                            // a background sum on first sight (List view only).
                            let folder_size = if is_dir && tab.remote.is_none() {
                                let fs = folder_size_lookup(&target);
                                if fs.is_none() {
                                    this.ensure_folder_size(target.clone(), cx);
                                }
                                fs
                            } else {
                                None
                            };
                            let ctx_target = target.clone();
                            let drop_target = target.clone();
                            let hover_target = target.clone();
                            let is_selected = tab.selection.contains(&target);
                            let ctx_active = this.is_ctx_target(&target);
                            let rename_text = this
                                .rename
                                .as_ref()
                                .filter(|r| r.path == target)
                                .map(|r| (r.text.clone(), r.cursor, r.anchor, this.rename_error().is_some()));
                            // Don't drag the row while it's being renamed.
                            let drag_target = if rename_text.is_some() {
                                None
                            } else {
                                Some(target.clone())
                            };
                            let icon = cloud_badged_icon(icon_element(&target, is_dir), cloud, 11.0);

                            items.push(
                                file_row(
                                    &name,
                                    is_dir,
                                    entry_size,
                                    folder_size,
                                    modified,
                                    entry_loaded,
                                    row_key,
                                    is_selected,
                                    ctx_active,
                                    widths,
                                    icon,
                                    is_dir,            // folders accept drops
                                    this.drop_hover == Some((pane, Some(ix))),
                                    rename_text,       // editable name when renaming
                                    cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                        // Cmd/Shift extend the selection; otherwise
                                        // folders open and files select / double-open.
                                        this.click_entry(pane, target.clone(), is_dir, ev, cx);
                                    }),
                                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                        let (x, y) = (
                                            f64::from(ev.position.x) as f32,
                                            f64::from(ev.position.y) as f32,
                                        );
                                        this.open_context_menu(
                                            pane,
                                            x,
                                            y,
                                            Some((ctx_target.clone(), is_dir)),
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    }),
                                    cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                                        // Drop onto a folder → move (or upload) the file(s) into it.
                                        this.drop_files(pane, drop_target.clone(), drag.paths().to_vec(), cx);
                                    }),
                                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                        if let Some(dp) = &drag_target {
                                            let (x, y) = (
                                                f64::from(ev.position.x) as f32,
                                                f64::from(ev.position.y) as f32,
                                            );
                                            this.drag_candidate = Some((pane, dp.clone(), (x, y)));
                                        }
                                        cx.stop_propagation();
                                    }),
                                    cx.listener(move |this, on: &bool, _, _| {
                                        if *on {
                                            this.hovered = Some((pane, hover_target.clone()));
                                        } else if this
                                            .hovered
                                            .as_ref()
                                            .is_some_and(|(hp, p)| *hp == pane && *p == hover_target)
                                        {
                                            this.hovered = None;
                                        }
                                    }),
                                )
                                .into_any_element(),
                            );
                        }
                        items
                    }),
                )
                .size_full()
                .track_scroll(scroll)
                .on_scroll_wheel(cx.listener(move |this, _: &ScrollWheelEvent, _, cx| {
                    this.active_pane = pane;
                    this.mark_scrolled(pane, cx);
                    cx.notify()
                })),
                                            ) // close listing div .child(uniform_list)
                                    ) // close content flex_col .child(listing div)
                            ) // close hscroll .child(content)
                    ) // close body .child(hscroll)
                    // Vertical scrollbar, pinned to the pane's right edge.
                    .children(self.scrollbar_thumb(pane, cx))
                    // Horizontal scrollbar, shown when columns overflow.
                    .children(self.h_scrollbar_thumb(pane, total_w))
                    // (The filter box is rendered once at the pane level.)
                    .into_any_element()
    }

    /// The Icons view body: a virtualized grid of large icons.
    fn render_icons_body(&self, pane: usize, cx: &Context<Self>) -> AnyElement {
        let tab = self.tab(pane);
        let scroll = tab.scroll_handle.clone();
        let pane_dir = tab.current_dir.clone();
        let width = self.pane_list_width(pane).max(240.0);
        let cell_w = 108.0_f32;
        let cols = ((width / cell_w).floor() as usize).max(1);
        let n = if tab.find_query.is_some() {
            tab.find_results.len()
        } else {
            tab.entries.len()
        };
        let rows = n.div_ceil(cols);

        div()
            .relative()
            .flex_1()
            .min_h_0()
            // Tracked drop target (see render_list_body) — no flicker.
            .when(
                cx.has_active_drag() && self.drop_hover == Some((pane, None)),
                |s| s.bg(Theme::alpha(theme().accent, 0x22)),
            )
            .on_drop(cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                this.drop_files(pane, pane_dir.clone(), drag.paths().to_vec(), cx);
            }))
            .child(
                uniform_list(
                    ("icons", pane),
                    rows,
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        let tab = this.tab(pane);
                        let find_active = tab.find_query.is_some();
                        let base_dir = tab.current_dir.clone();
                        let n = if find_active {
                            tab.find_results.len()
                        } else {
                            tab.entries.len()
                        };
                        let mut out: Vec<AnyElement> = Vec::with_capacity(range.len());
                        for row in range {
                            let mut cells: Vec<AnyElement> = Vec::with_capacity(cols);
                            for c in 0..cols {
                                let i = row * cols + c;
                                if i >= n {
                                    break;
                                }
                                let tab = this.tab(pane);
                                let entry_ix = if find_active { tab.find_results[i] } else { i };
                                let entry = &tab.entries[entry_ix];
                                let name = entry.name.clone();
                                let is_dir = entry.is_dir;
                                let target = base_dir.join(&name);
                                let selected = tab.selection.contains(&target);
                                let ctx_active = this.is_ctx_target(&target);
                                cells.push(icon_cell(pane, name, target, is_dir, selected, ctx_active, cell_w, cx));
                            }
                            out.push(div().flex().w_full().px_2().children(cells).into_any_element());
                        }
                        out
                    }),
                )
                .size_full()
                .track_scroll(scroll)
                .on_scroll_wheel(cx.listener(move |this, _: &ScrollWheelEvent, _, cx| {
                    this.active_pane = pane;
                    this.mark_scrolled(pane, cx);
                    cx.notify()
                })),
            )
            .children(self.scrollbar_thumb(pane, cx))
            // (The filter box is rendered once at the pane level.)
            .into_any_element()
    }

    /// The Column (Miller) view body: cascading folder columns.
    fn render_columns_body(&self, pane: usize, cx: &Context<Self>) -> AnyElement {
        let t = theme();
        let tab = self.tab(pane);
        let mut dirs: Vec<PathBuf> = vec![tab.current_dir.clone()];
        dirs.extend(tab.col_chain.iter().cloned());
        let anchor = tab.anchor.clone();

        let mut cols: Vec<AnyElement> = Vec::new();
        for (i, dir) in dirs.iter().enumerate() {
            let entries = column_entries(dir);
            let next_dir = dirs.get(i + 1).cloned();
            let mut rows: Vec<AnyElement> = Vec::new();
            for e in &entries {
                let target = dir.join(&e.name);
                let selected = next_dir.as_deref() == Some(target.as_path())
                    || anchor.as_deref() == Some(target.as_path())
                    || tab.selection.contains(&target);
                let ctx_active = self.is_ctx_target(&target);
                rows.push(column_row(pane, i, &e.name, target, e.is_dir, selected, ctx_active, cx));
            }
            let vscroll = col_scroll(pane, i);
            let thumb = static_scrollbar_thumb(&vscroll);
            cols.push(
                div()
                    .id(("col", pane * 100 + i))
                    .relative()
                    .flex_none()
                    .w(px(230.0))
                    .h_full()
                    .border_r_1()
                    .border_color(rgb(t.border))
                    .child(
                        div()
                            .id(("col-scroll", pane * 100 + i))
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&vscroll)
                            .flex()
                            .flex_col()
                            .py_1()
                            .children(rows),
                    )
                    .children(thumb)
                    .into_any_element(),
            );
        }

        div()
            .id(("columns", pane))
            .flex_1()
            .min_h_0()
            .overflow_x_scroll()
            .track_scroll(&tab.h_scroll)
            .flex()
            .flex_row()
            .children(cols)
            .into_any_element()
    }

    /// The Gallery view body: a large preview on top + a filmstrip below.
    fn render_gallery_body(&self, pane: usize, cx: &Context<Self>) -> AnyElement {
        let t = theme();
        let tab = self.tab(pane);
        let pane_dir = tab.current_dir.clone();
        let sel = tab.anchor.clone();

        // Top preview area (white page so documents stay readable).
        let preview: AnyElement = match &sel {
            Some(p) => match lookup_preview(p) {
                Some(Some(handle)) => img(ImageSource::Render(handle))
                    .max_w(px(560.0))
                    .max_h(px(560.0))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                // Not ready yet or unavailable → show the file's large icon.
                _ => icon_element_sized(p, false, 128.0),
            },
            None => div()
                .text_color(rgb(t.text_dim))
                .child("Select an item")
                .into_any_element(),
        };

        // Filmstrip (capped for very large directories).
        let mut strip: Vec<AnyElement> = Vec::new();
        for entry in tab.entries.iter().take(400) {
            let name = entry.name.clone();
            let is_dir = entry.is_dir;
            let target = pane_dir.join(&name);
            let cloud = self.cloud_state(&target, is_dir);
            let selected = tab.selection.contains(&target);
            let nav_t = target.clone();
            strip.push(
                div()
                    .id(SharedString::from(format!("film:{}", target.to_string_lossy())))
                    .flex_none()
                    .w(px(80.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(rgb(t.selected)))
                    .hover(|s| s.bg(rgb(t.hover)))
                    .child(cloud_badged_icon(icon_element_sized(&target, is_dir, 44.0), cloud, 16.0))
                    .child(div().w_full().truncate().text_xs().text_color(rgb(t.text_muted)).child(name))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        let mods = ev.modifiers();
                        if mods.platform {
                            this.toggle_entry(pane, nav_t.clone(), cx);
                        } else if mods.shift {
                            this.range_select(pane, nav_t.clone(), cx);
                        } else if ev.click_count() >= 2 {
                            this.open_path(pane, nav_t.clone(), is_dir, cx);
                        } else {
                            this.select_entry(pane, nav_t.clone(), cx);
                        }
                    }))
                    .into_any_element(),
            );
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .max_w(px(600.0))
                            .max_h(px(600.0))
                            .p_2()
                            .rounded_md()
                            .bg(rgb(0xffffff))
                            .child(preview),
                    ),
            )
            .child(
                div()
                    .id(("filmstrip", pane))
                    .flex_none()
                    .h(px(108.0))
                    .overflow_x_scroll()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_t_1()
                    .border_color(rgb(t.border))
                    .bg(rgb(t.sidebar))
                    .children(strip),
            )
            .into_any_element()
    }

    /// Read-only horizontal scroll indicator for a pane's columns. Returns
    /// `None` when the columns fit (no horizontal overflow).
    fn h_scrollbar_thumb(&self, pane: usize, _total_w: f32) -> Option<AnyElement> {
        let base = &self.tab(pane).h_scroll;
        let viewport = f64::from(base.bounds().size.width) as f32;
        let max = f64::from(base.max_offset().width) as f32;
        if viewport <= 1.0 || max <= 1.0 {
            return None;
        }
        let scrolled = (-(f64::from(base.offset().x) as f32)).clamp(0.0, max);
        let content = viewport + max;
        let thumb_w = (viewport * viewport / content).clamp(28.0, viewport);
        let thumb_left = (viewport - thumb_w) * (scrolled / max);
        Some(
            div()
                .absolute()
                .bottom(px(2.0))
                .left(px(thumb_left))
                .h(px(8.0))
                .w(px(thumb_w))
                .rounded_full()
                .bg(Theme::alpha(theme().text, 0x33))
                .into_any_element(),
        )
    }

    /// The non-scrolling header row with labels and drag-to-resize handles.
    fn column_header(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let w = self.widths;
        let key = self.tab(pane).sort_key;
        let asc = self.tab(pane).sort_asc;
        div()
            .flex()
            .items_center()
            .flex_none()
            .px_3()
            .py_1()
            .text_xs()
            .text_color(rgb(theme().text_dim))
            .border_b_1()
            .border_color(rgb(theme().border))
            .child(header_cell(pane, "Name", w.name, Column::Name, SortKey::Name, ICON_W + 8.0, false, key, asc, cx))
            .child(header_cell(pane, "Kind", w.kind, Column::Kind, SortKey::Kind, 0.0, false, key, asc, cx))
            .child(header_cell(pane, "Date Modified", w.date, Column::Date, SortKey::Modified, 0.0, false, key, asc, cx))
            .child(header_cell(pane, "Size", w.size, Column::Size, SortKey::Size, 0.0, true, key, asc, cx))
            // Slack space after the last column.
            .child(div().flex_1())
    }
}

/// A header cell: a clickable sort label plus a drag handle on its right edge
/// that resizes the column. `left_pad` aligns the Name label past the row icon;
/// `align_right` right-justifies (for Size).
#[allow(clippy::too_many_arguments)]
fn header_cell(
    pane: usize,
    label: &str,
    width: f32,
    col: Column,
    sort: SortKey,
    left_pad: f32,
    align_right: bool,
    cur_key: SortKey,
    cur_asc: bool,
    cx: &Context<Shuffle>,
) -> impl IntoElement {
    let active = cur_key == sort;
    let arrow = if active {
        if cur_asc { " ▲" } else { " ▼" }
    } else {
        ""
    };
    let mut label_box = div()
        .id(("hdr", pane * 10 + col.key()))
        .flex_1()
        .min_w_0()
        .truncate()
        .cursor_pointer()
        .when(active, |s| s.text_color(rgb(theme().text)))
        .hover(|s| s.text_color(rgb(theme().text)))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.set_sort(pane, sort, cx);
        }));
    if left_pad > 0.0 {
        label_box = label_box.pl(px(left_pad));
    }
    if align_right {
        label_box = label_box.flex().justify_end().pr_2();
    }

    div()
        .flex_none()
        .w(px(width))
        .h_full()
        .flex()
        .items_center()
        .child(label_box.child(format!("{label}{arrow}")))
        // Drag handle: a wide grab zone centered on a visible 1px divider line.
        .child(
            div()
                .id(("resize", col.key()))
                .flex_none()
                .w(px(11.0))
                .h_full()
                .flex()
                .justify_center()
                .cursor(CursorStyle::ResizeLeftRight)
                .child(div().w(px(1.0)).h_full().bg(rgb(theme().border_strong)))
                .hover(|s| s.bg(rgb(theme().selected)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.begin_resize(col, f64::from(ev.position.x) as f32);
                        cx.notify();
                    }),
                ),
        )
}

impl Render for Shuffle {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        // Kick off any pending waterfall folder reads before rendering the tree.
        self.ensure_waterfall_loaded(cx);
        // Main row: sidebar | content (canvas) | optional inspector.
        let mut main_row = div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_sidebar(cx))
            .child(self.render_content(cx));
        if let Some(inspector) = self.render_inspector(cx) {
            main_row = main_row.child(inspector);
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(t.bg))
            .text_color(rgb(t.text))
            .text_sm()
            // Focusable so it receives key events (Cmd+P, palette typing).
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            // Native menu-bar actions (File / View / Go menus).
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                let p = this.active_pane;
                this.new_tab_in(p, cx);
            }))
            .on_action(cx.listener(|this, _: &NewFolder, window, cx| {
                let p = this.active_pane;
                this.new_folder(p, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                let p = this.active_pane;
                let t = this.pane(p).active;
                this.close_tab(p, t, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveToTrash, _, cx| {
                let p = this.active_pane;
                this.request_delete(p, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewList, _, cx| {
                let p = this.active_pane;
                this.set_view(p, ViewMode::List, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewIcons, _, cx| {
                let p = this.active_pane;
                this.set_view(p, ViewMode::Icons, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewColumns, _, cx| {
                let p = this.active_pane;
                this.set_view(p, ViewMode::Columns, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewGallery, _, cx| {
                let p = this.active_pane;
                this.set_view(p, ViewMode::Gallery, cx);
            }))
            .on_action(cx.listener(|_, _: &ToggleSidebar, _, cx| {
                let mut np = prefs();
                np.sidebar_collapsed = !np.sidebar_collapsed;
                apply_prefs(np, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &GoBack, _, cx| {
                let p = this.active_pane;
                this.go_back(p, cx);
            }))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| {
                let p = this.active_pane;
                this.go_forward(p, cx);
            }))
            .on_action(cx.listener(|this, _: &GoHome, _, cx| {
                this.navigate_to(home_dir(), cx);
            }))
            .on_action(cx.listener(|this, _: &GoApplications, _, cx| {
                this.navigate_to(PathBuf::from("/Applications"), cx);
            }))
            .on_action(cx.listener(|this, _: &GoComputer, _, cx| {
                this.navigate_to(PathBuf::from("/"), cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                if !this.palette_open {
                    this.toggle_palette(window, cx);
                }
            }))
            // Track column drags anywhere in the window so the cursor can leave
            // the thin handle without dropping the resize.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                let x = f64::from(ev.position.x) as f32;
                let y = f64::from(ev.position.y) as f32;
                // A press-then-move on a file row becomes a native OS drag (so
                // files can be dropped into Finder or any other app).
                this.maybe_start_os_drag(x, y, window, cx);
                this.update_resize(x, cx);
                this.update_scroll_drag(y, cx);
                this.update_divider(x, cx);
                this.update_marquee(x, y, cx);
                this.update_drop_hover(x, y, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag_candidate = None;
                    this.end_resize();
                    this.end_scroll_drag(cx);
                    this.divider_drag = None;
                    this.end_marquee(cx);
                    // A native file drag ends as a synthesized mouse-up.
                    if this.drop_hover.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            // A slim titlebar strip in the app's own background color. With the
            // OS titlebar transparent, this is what shows behind the traffic
            // lights, and it keeps the content clear of them.
            .child(div().flex_none().w_full().h(px(TITLEBAR_H)).bg(rgb(t.bg)));

        // The update bar (available / downloading / failed), just under the titlebar.
        if let Some(banner) = self.render_update_banner(cx) {
            root = root.child(banner);
        }
        // SFTP error bar (connection / transfer failure).
        if let Some(err) = self.remote_error.clone() {
            root = root.child(
                div()
                    .flex_none()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_1p5()
                    .bg(rgb(0xef4444))
                    .text_color(rgb(0xffffff))
                    .child(div().flex_none().child("⚠"))
                    .child(div().flex_1().min_w_0().truncate().child(err))
                    .child(
                        div()
                            .id("sftp-err-dismiss")
                            .flex_none()
                            .px_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgba(0xffffff33)))
                            .child("✕")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.remote_error = None;
                                cx.notify();
                            })),
                    ),
            );
        }

        root = root.child(main_row);

        // Terminal-mode command bar at the bottom.
        if prefs().terminal {
            root = root.child(self.render_terminal_bar(cx));
        }

        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        if self.context_menu.is_some() {
            root = root.child(self.render_context_menu(cx));
        }
        if let Some((p, x, y)) = self.sort_menu {
            root = root.child(self.render_sort_menu(p, x, y, cx));
        }
        if self.confirm_delete.is_some() {
            root = root.child(self.render_confirm_delete(cx));
        }
        if self.ssh_ask {
            root = root.child(self.render_ssh_prompt(cx));
        }
        if self.server_dialog.is_some() {
            root = root.child(self.render_server_dialog(cx));
        }
        if self.sidebar_menu.is_some() {
            root = root.child(self.render_sidebar_menu(cx));
        }
        if self.group_dialog.is_some() {
            root = root.child(self.render_group_dialog(cx));
        }
        if prefs().show_fps {
            root = root.child(fps_overlay());
        }
        root
    }
}

/// Build a sidebar nav item that navigates to `target`, and push it onto `items`.
#[allow(clippy::too_many_arguments)]
fn push_nav(
    items: &mut Vec<AnyElement>,
    cx: &Context<Shuffle>,
    key: &mut usize,
    label: String,
    icon_key: String,
    target: PathBuf,
    current: &Path,
    collapsed: bool,
) {
    *key += 1;
    let active = target.as_path() == current;
    let nav_target = target.clone();
    // Tooltip: the friendly path (`~/Documents/Projects`). When expanded the
    // label is already visible, so lead with it for clarity.
    let path_str = display_path(&target);
    let tooltip = if collapsed || path_str == label {
        format!("{label}\n{path_str}")
    } else {
        path_str
    };
    let item = nav_item(
        label,
        tooltip,
        sidebar_icon(&icon_key, 16.0),
        *key,
        active,
        collapsed,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.navigate_to(nav_target.clone(), cx);
        }),
        |_, _, _| {},
    );
    items.push(item.into_any_element());
}

/// Like [`push_nav`] but for a bookmark, which may be a file or a folder:
/// folders navigate, files open in their default app, and the icon reflects the
/// file type.
fn push_bookmark_nav(
    items: &mut Vec<AnyElement>,
    cx: &Context<Shuffle>,
    key: &mut usize,
    target: PathBuf,
    current: &Path,
    collapsed: bool,
) {
    *key += 1;
    let is_dir = cached_is_dir(&target);
    let active = is_dir && target.as_path() == current;
    let label = path_label(&target);
    let path_str = display_path(&target);
    let tooltip = if collapsed || path_str == label {
        format!("{label}\n{path_str}")
    } else {
        path_str
    };
    let nav_target = target.clone();
    let right_target = target.clone();
    let item = nav_item(
        label,
        tooltip,
        icon_element(&target, is_dir),
        *key,
        active,
        collapsed,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            if nav_target.is_dir() {
                this.navigate_to(nav_target.clone(), cx);
            } else {
                let _ = Command::new("open").arg(&nav_target).spawn();
            }
        }),
        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
            let (x, y) = (f64::from(ev.position.x) as f32, f64::from(ev.position.y) as f32);
            this.open_sidebar_menu(x, y, SidebarTarget::Bookmark(right_target.clone()), cx);
            cx.stop_propagation();
        }),
    );
    items.push(item.into_any_element());
}

/// A group member row: click opens/navigates, right-click offers "Remove from
/// Group". `gidx` is the owning group's index.
#[allow(clippy::too_many_arguments)]
fn push_group_member(
    items: &mut Vec<AnyElement>,
    cx: &Context<Shuffle>,
    key: &mut usize,
    gidx: usize,
    target: PathBuf,
    current: &Path,
    collapsed: bool,
) {
    *key += 1;
    let is_dir = cached_is_dir(&target);
    let active = is_dir && target.as_path() == current;
    let label = path_label(&target);
    let path_str = display_path(&target);
    let tooltip = if collapsed || path_str == label {
        format!("{label}\n{path_str}")
    } else {
        path_str
    };
    let nav_target = target.clone();
    let right_target = target.clone();
    let item = nav_item(
        label,
        tooltip,
        icon_element(&target, is_dir),
        *key,
        active,
        collapsed,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            if nav_target.is_dir() {
                this.navigate_to(nav_target.clone(), cx);
            } else {
                let _ = Command::new("open").arg(&nav_target).spawn();
            }
        }),
        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
            let (x, y) = (f64::from(ev.position.x) as f32, f64::from(ev.position.y) as f32);
            this.open_sidebar_menu(
                x,
                y,
                SidebarTarget::GroupMember(gidx, right_target.clone()),
                cx,
            );
            cx.stop_propagation();
        }),
    );
    items.push(item.into_any_element());
}

/// A 1px separator used between sections in the collapsed rail.
fn push_divider(items: &mut Vec<AnyElement>) {
    items.push(
        div()
            .mx_2()
            .my_1()
            .h(px(1.0))
            .bg(rgb(theme().border))
            .into_any_element(),
    );
}

/// A path shown with the home directory abbreviated to `~`.
fn display_path(p: &Path) -> String {
    let home = home_dir();
    if let Ok(rest) = p.strip_prefix(&home) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

/// A back/forward navigation arrow. Dimmed and inert when `enabled` is false.
fn nav_arrow(
    id: impl Into<ElementId>,
    glyph: &'static str,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let t = theme();
    let base = div()
        .id(id)
        .flex_none()
        .w(px(22.0))
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(if enabled { rgb(t.text) } else { rgb(t.text_dim) })
        .child(glyph);
    if enabled {
        base.cursor_pointer()
            .hover(|s| s.bg(rgb(t.hover)))
            .on_click(on_click)
            .into_any_element()
    } else {
        base.into_any_element()
    }
}

/// One clickable breadcrumb segment that navigates `pane` to `full` when clicked.
fn breadcrumb_seg(
    key: usize,
    pane: usize,
    label: String,
    full: PathBuf,
    active: bool,
    cx: &Context<Shuffle>,
) -> AnyElement {
    let t = theme();
    div()
        .id(("crumb", key))
        .flex_none()
        .px_1()
        .h(px(20.0))
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(if active { rgb(t.text) } else { rgb(t.text_dim) })
        .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
        .child(label)
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.navigate_in(pane, full.clone(), cx);
            cx.stop_propagation();
        }))
        .into_any_element()
}

/// The "/" divider between breadcrumb segments.
fn breadcrumb_sep() -> AnyElement {
    div()
        .flex_none()
        .px(px(2.0))
        .text_color(rgb(theme().text_dim))
        .child("/")
        .into_any_element()
}

/// Expand a leading `~` to the home directory.
fn expand_path(s: &str) -> PathBuf {
    if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// Resolve a `cd` argument against `cwd` (handles `~`, absolute, relative, `..`).
fn resolve_dir(cwd: &Path, arg: &str) -> PathBuf {
    let arg = arg.trim();
    if arg.is_empty() || arg == "~" {
        return if arg == "~" { home_dir() } else { cwd.to_path_buf() };
    }
    if let Some(rest) = arg.strip_prefix("~/") {
        return normalize_path(&home_dir().join(rest));
    }
    let p = PathBuf::from(arg);
    let joined = if p.is_absolute() { p } else { cwd.join(p) };
    normalize_path(&joined)
}

/// Lexically normalize a path, collapsing `.` and `..` without touching disk.
fn normalize_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Longest common string prefix across the given strings.
fn longest_common_prefix<'a>(it: impl Iterator<Item = &'a str>) -> String {
    let mut prefix: Option<String> = None;
    for s in it {
        match prefix {
            None => prefix = Some(s.to_string()),
            Some(ref mut p) => {
                let common: String = p
                    .chars()
                    .zip(s.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect();
                *p = common;
            }
        }
    }
    prefix.unwrap_or_default()
}

/// Open the Settings window (shared by the menu action and the palette).
fn open_settings_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(760.0), px(560.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Settings".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(Settings::new);
            window.focus(&view.read(cx).focus);
            view
        },
    )
    .ok();
    cx.activate(true);
}

/// One clickable row in the right-click context menu.
fn ctx_item(
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(label)
        .flex()
        .items_center()
        .mx_1()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(menu_style().text))
        .hover(|s| s.bg(rgb(theme().selected)))
        .child(label)
        .on_click(on_click)
}

/// Like [`ctx_item`] but with a runtime label (e.g. a user script's name).
/// `id` is a stable element id; `label` is the shown text.
fn ctx_item_owned(
    id: impl Into<ElementId>,
    label: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .mx_1()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(menu_style().text))
        .hover(|s| s.bg(rgb(theme().selected)))
        .child(label)
        .on_click(on_click)
}

fn ctx_separator() -> impl IntoElement {
    div().my_1().mx_2().h(px(1.0)).bg(rgb(theme().border_strong))
}

/// A context-menu row that opens a submenu (shows a trailing "›").
fn ctx_parent(
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    div()
        .id(label)
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .mx_1()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(menu_style().text))
        .hover(|s| s.bg(rgb(t.selected)))
        .child(label)
        .child(div().flex_none().text_color(rgb(t.text_dim)).child("›"))
        .on_click(on_click)
}

/// An app row in the "Open With" submenu (dynamic label, unique id).
fn ctx_app(
    idx: usize,
    name: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(("ow", idx))
        .flex()
        .items_center()
        .mx_1()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(menu_style().text))
        .hover(|s| s.bg(rgb(theme().selected)))
        .child(name)
        .on_click(on_click)
}

/// A color row in the "Tags" submenu.
fn ctx_tag(
    idx: usize,
    name: &'static str,
    color: u32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    div()
        .id(("tag", idx))
        .flex()
        .items_center()
        .gap_2()
        .mx_1()
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(t.text))
        .hover(|s| s.bg(rgb(t.selected)))
        .child(div().flex_none().w(px(10.0)).h(px(10.0)).rounded_full().bg(rgb(color)))
        .child(name)
        .on_click(on_click)
}

/// A non-interactive, dimmed context-menu row.
fn ctx_disabled(label: &'static str) -> impl IntoElement {
    div()
        .mx_1()
        .px_3()
        .py_1()
        .text_color(rgb(theme().text_dim))
        .child(label)
}

/// Whether `path` looks like a raster image we can act on.
fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "tiff" | "tif" | "bmp" | "webp")
    )
}

/// Whether `path` is a PDF.
fn is_pdf(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() == Some("pdf")
}

/// Archive suffixes "Extract Here" can unpack with built-in tools, longest
/// first so `foo.tar.gz` strips as a tarball rather than a ".gz".
const ARCHIVE_SUFFIXES: &[&str] = &[
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tgz", ".tbz", ".txz", ".tar", ".zip",
];

/// The matched archive suffix of `path`, if it's an extractable archive.
fn archive_suffix(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    ARCHIVE_SUFFIXES.iter().find(|s| name.ends_with(*s)).copied()
}

/// The extraction command for `path`, ready for the destination directory to
/// be appended as its final argument (`ditto -xk src DEST` / `tar -xf src -C DEST`).
fn archive_extract_command(path: &Path) -> Option<Command> {
    let suffix = archive_suffix(path)?;
    let mut c;
    if suffix == ".zip" {
        c = Command::new("ditto");
        c.arg("-xk").arg(path);
    } else {
        c = Command::new("tar");
        c.arg("-xf").arg(path).arg("-C");
    }
    Some(c)
}

// ----- user shell-script actions (the "Scripts" extension point) -----

/// A user-provided shell-script action discovered in the Scripts folder. The
/// user drops an executable script there; Shuffle offers it in the right-click
/// menu for matching items and runs it with the selected paths as arguments.
struct ScriptAction {
    name: String,
    path: PathBuf,
    /// Type matchers (OR'd): `any`, `folder`/`file`, a kind word (image/video/
    /// pdf/…), or a bare extension (`png`).
    types: Vec<String>,
}

/// The folder scripts live in: `…/Application Support/Shuffle/actions`.
fn script_actions_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("actions"))
}

/// A regular file the current user can execute (a runnable script).
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Match a `@shuffle.KEY: value` metadata line (case-insensitive key).
fn script_header_value(line: &str, key: &str) -> Option<String> {
    let prefix = format!("@shuffle.{key}:");
    line.to_lowercase()
        .starts_with(&prefix)
        .then(|| line[prefix.len()..].trim().to_string())
}

/// Read a script's leading metadata: display name (defaults to the file stem)
/// and the item types it applies to (defaults to `any`).
fn parse_script_header(path: &Path, fname: &str) -> (String, Vec<String>) {
    let mut name = fname.rsplit_once('.').map(|(s, _)| s).unwrap_or(fname).to_string();
    let mut types = vec!["any".to_string()];
    if let Ok(f) = fs::File::open(path) {
        for line in BufReader::new(f).lines().take(40).map_while(Result::ok) {
            let Some(rest) = line.trim().strip_prefix('#') else {
                continue;
            };
            let rest = rest.trim();
            if let Some(v) = script_header_value(rest, "name") {
                if !v.is_empty() {
                    name = v;
                }
            } else if let Some(v) = script_header_value(rest, "types") {
                let parsed: Vec<String> = v
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !parsed.is_empty() {
                    types = parsed;
                }
            }
        }
    }
    (name, types)
}

/// Scan the Scripts folder for executable actions. Cheap (small folder), called
/// when a context menu opens. Skips dotfiles, the README, and non-executables.
fn discover_script_actions() -> Vec<ScriptAction> {
    let Some(dir) = script_actions_dir() else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if fname.starts_with('.') || fname.eq_ignore_ascii_case("readme.md") {
            continue;
        }
        if !is_executable_file(&path) {
            continue;
        }
        let (name, types) = parse_script_header(&path, &fname);
        out.push(ScriptAction { name, path, types });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Whether a script action applies to an item of this name/kind.
fn script_action_applies(a: &ScriptAction, name: &str, is_dir: bool) -> bool {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    a.types.iter().any(|t| match t.as_str() {
        "any" | "*" | "all" => true,
        "folder" | "dir" | "directory" => is_dir,
        "file" => !is_dir,
        other => match KindClass::from_word(other) {
            Some(k) => k.matches(name, is_dir),
            None => ext.as_deref() == Some(other),
        },
    })
}

/// Create the Scripts folder (and, on first creation, a README + a couple of
/// safe example scripts) and return its path. Idempotent.
fn ensure_scripts_dir() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = script_actions_dir()?;
    let fresh = !dir.exists();
    fs::create_dir_all(&dir).ok()?;
    if fresh {
        let readme = r#"# Shuffle — Script Actions

Drop an **executable** script in this folder and Shuffle shows it in the
right-click menu for matching items. The selected paths are passed as arguments
(`"$@"`), and also as `$SHUFFLE_PATHS` (newline-separated) with `$SHUFFLE_DIR`
set to the current folder.

Add an optional metadata header (any comment style) to control how it appears:

    # @shuffle.name: Optimize Image
    # @shuffle.types: png,jpg,jpeg      # extensions, or: image video audio pdf
    #                                   # doc archive code app folder file any

- No `name` → the file's name is used.
- No `types` → shows for everything (`any`).
- The file must be executable: `chmod +x "your script"`.

Turn the feature on in Settings → Script Actions.
"#;
        let _ = fs::write(dir.join("README.md"), readme);

        let copy_path = "#!/bin/bash\n# @shuffle.name: Copy Path\n# @shuffle.types: any\nprintf '%s' \"$1\" | pbcopy\n";
        let open_term = "#!/bin/bash\n# @shuffle.name: Open in Terminal\n# @shuffle.types: folder\nopen -a Terminal \"$1\"\n";
        for (fname, body) in [("Copy Path.sh", copy_path), ("Open in Terminal.sh", open_term)] {
            let p = dir.join(fname);
            if fs::write(&p, body).is_ok() {
                if let Ok(md) = fs::metadata(&p) {
                    let mut perm = md.permissions();
                    perm.set_mode(0o755);
                    let _ = fs::set_permissions(&p, perm);
                }
            }
        }
    }
    Some(dir)
}

/// Path to the bundled `removebg` Swift helper, if it was compiled in.
fn removebg_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join("removebg");
    cand.exists().then_some(cand)
}

/// Path to the bundled `cloudctl` Swift helper (download/evict cloud files).
fn cloudctl_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join("cloudctl");
    cand.exists().then_some(cand)
}

/// Which cloud store `path` lives in, or `None` if it isn't under one. iCloud
/// Drive is `~/Library/Mobile Documents`; third-party File Provider stores live
/// under `~/Library/CloudStorage`.
fn cloud_kind(path: &Path) -> Option<CloudKind> {
    let home = home_dir();
    if path.starts_with(home.join("Library/Mobile Documents")) {
        return Some(CloudKind::ICloud);
    }
    if path.starts_with(home.join("Library/CloudStorage")) {
        return Some(CloudKind::Provider);
    }
    None
}

/// Whether `path` is an online-only cloud placeholder (kernel `SF_DATALESS`).
/// Whether marquee debug logging is on (`SHUFFLE_MARQUEE_LOG=1`).
fn mq_log() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SHUFFLE_MARQUEE_LOG").is_some())
}

fn is_dataless(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    fs::metadata(path)
        .map(|m| m.st_flags() & SF_DATALESS != 0)
        .unwrap_or(false)
}

/// `SF_DATALESS` cache for render, keyed by path with a short TTL. The flag
/// isn't reliably captured during bulk directory enumeration (it lags on a
/// background thread), so the badge reads it lazily here instead. The stat is a
/// local metadata read (cloud placeholders live on-disk; only their bytes are
/// remote), so it's fast on the main thread — no network blocking like a dead
/// mount, hence no background dance.
static DATALESS_STAT: OnceLock<Mutex<HashMap<PathBuf, (bool, Instant)>>> = OnceLock::new();

fn cached_dataless(path: &Path) -> bool {
    const TTL: Duration = Duration::from_secs(3);
    let map = DATALESS_STAT.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((v, at)) = map.lock().unwrap().get(path).copied() {
        if at.elapsed() <= TTL {
            return v;
        }
    }
    let v = is_dataless(path);
    map.lock().unwrap().insert(path.to_path_buf(), (v, Instant::now()));
    v
}

/// Files to act on for a cloud download/evict: `path` itself when it's a file,
/// or the matching descendants when it's a folder. `want_dataless` selects
/// online-only files (download) or materialized ones (evict). Capped at `cap`
/// so a huge tree can't spawn an unbounded number of helper calls.
fn collect_cloud_files(path: &Path, want_dataless: bool, cap: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if path.is_file() {
        if is_dataless(path) == want_dataless {
            out.push(path.to_path_buf());
        }
        return out;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if out.len() >= cap {
                break;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    let p = e.path();
                    if is_dataless(&p) == want_dataless {
                        out.push(p);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Installed terminal emulators, for the "Open in …" services.
fn installed_terminals() -> Vec<(&'static str, PathBuf)> {
    [
        ("Terminal", "/System/Applications/Utilities/Terminal.app"),
        ("iTerm", "/Applications/iTerm.app"),
        ("Ghostty", "/Applications/Ghostty.app"),
        ("kitty", "/Applications/kitty.app"),
        ("WezTerm", "/Applications/WezTerm.app"),
        ("Warp", "/Applications/Warp.app"),
        ("Alacritty", "/Applications/Alacritty.app"),
    ]
    .into_iter()
    .filter(|(_, p)| Path::new(p).exists())
    .map(|(n, p)| (n, PathBuf::from(p)))
    .collect()
}

/// Applications that can open `path`, via LaunchServices (Finder's "Open With").
fn apps_for_file(path: &Path) -> Vec<(String, PathBuf)> {
    let Some(s) = path.to_str() else {
        return Vec::new();
    };
    let ns = NSString::from_str(s);
    let url = NSURL::fileURLWithPath(&ns);
    let ws = NSWorkspace::sharedWorkspace();
    let arr = ws.URLsForApplicationsToOpenURL(&url);
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for i in 0..arr.count() {
        let u = arr.objectAtIndex(i);
        if let Some(p) = u.path() {
            let pb = PathBuf::from(p.to_string());
            let name = pb
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.is_empty() && seen.insert(name.clone()) {
                out.push((name, pb));
            }
        }
        if out.len() >= 16 {
            break;
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

/// A non-existing child path under `dir` based on `base` (adds " 2", " 3" …).
fn unique_child(dir: &Path, base: &str) -> PathBuf {
    let mut path = dir.join(base);
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base} {n}"));
        n += 1;
    }
    path
}

/// Move a path to the macOS Trash (recoverable). Returns whether it succeeded.
fn trash_path(path: &Path) -> bool {
    let ns_path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&ns_path);
    let fm = NSFileManager::defaultManager();
    fm.trashItemAtURL_resultingItemURL_error(&url, None).is_ok()
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn nav_item(
    label: String,
    tooltip: String,
    icon: AnyElement,
    key: usize,
    active: bool,
    collapsed: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let mut base = div()
        .id(("nav", key))
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(if active { t.text } else { t.text_muted }));
    base = if collapsed {
        base.mx_1().px_0().py_1().justify_center()
    } else {
        base.mx_2().px_2().py_1().gap_2()
    };
    let base = if active {
        base.bg(rgb(t.surface))
    } else {
        base.hover(|s| s.bg(rgb(t.hover)))
            .active(|s| s.bg(rgb(t.selected)))
    };
    let base = base.child(icon);
    let base = if collapsed {
        base
    } else {
        base.child(div().min_w_0().overflow_hidden().child(label))
    };
    base.tooltip(tip(tooltip))
        .on_click(on_click)
        .on_mouse_down(MouseButton::Right, on_right)
}

/// A "key chip + label" pair for the palette's footer hints ("↩ Open file").
fn palette_hint(key: &'static str, label: &'static str) -> impl IntoElement {
    let t = theme();
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .px_1()
                .rounded_sm()
                .bg(Theme::alpha(t.text_dim, 0x22))
                .text_color(rgb(t.text_muted))
                .child(key),
        )
        .child(label)
}

fn empty_hint(text: &str) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .text_color(rgb(theme().text_dim))
        .child(text.to_string())
}

/// One clickable listing row in the main pane: icon · name · kind · date · size.
#[allow(clippy::too_many_arguments)]
fn file_row(
    name: &str,
    is_dir: bool,
    size: u64,
    // For folders: the recursively-summed size once computed (else None → "--").
    folder_size: Option<u64>,
    modified: Option<SystemTime>,
    loaded: bool,
    key: usize,
    selected: bool,
    // True while this row is the open right-click menu's target — keep it looking
    // hovered even though the cursor is over the menu backdrop.
    ctx_active: bool,
    widths: ColumnWidths,
    icon: AnyElement,
    // Drag-and-drop: `accept_drop` true => it's a drop target (a folder or the
    // ".." row) that runs `on_drop_file` when files are dropped on it.
    accept_drop: bool,
    // True while the tracked drag target is this row's folder: highlights the
    // name cell (driven by Shuffle::drop_hover, not gpui's drag hover).
    drop_hilite: bool,
    // When Some, this row is being renamed: the name shows as an editable field
    // with (text, cursor char-index, selection-anchor char-index, invalid).
    // Invalid names render the field in red (the pane shows the reason).
    rename_text: Option<(String, usize, Option<usize>, bool)>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_drop_file: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    // Left-press handler: records a drag candidate + stops marquee propagation.
    on_press: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    // Hover tracking, so Enter can rename the row under the mouse.
    on_hover_row: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let kind = kind_label(name, is_dir);
    let name_color = if is_dir { t.accent } else { t.text };
    let meta_color = rgb(t.text_muted);
    // The name element: an editable field while renaming, else the label.
    let name_el: AnyElement = match &rename_text {
        Some((txt, cursor, anchor, invalid)) => {
            let edge = if *invalid { RENAME_ERR_COLOR } else { t.accent };
            let field = div()
                .flex_1()
                .min_w_0()
                .px_1()
                .rounded_sm()
                .bg(rgb(t.bg))
                .border_1()
                .border_color(rgb(edge))
                .text_color(rgb(if *invalid { RENAME_ERR_COLOR } else { t.text }))
                .flex()
                .items_center();
            let n = txt.chars().count();
            let cursor = (*cursor).min(n);
            let sel = anchor
                .filter(|&a| a != cursor)
                .map(|a| (a.min(cursor), a.max(cursor)));
            if let Some((lo, hi)) = sel {
                // Highlighted selection between the plain head and tail.
                let (bl, bh) = (char_byte(txt, lo), char_byte(txt, hi));
                field
                    .child(div().flex_none().child(txt[..bl].to_string()))
                    .child(
                        div()
                            .flex_none()
                            .bg(Theme::alpha(edge, 0x66))
                            .rounded_sm()
                            .child(txt[bl..bh].to_string()),
                    )
                    .child(div().flex_none().child(txt[bh..].to_string()))
                    .into_any_element()
            } else {
                // Static caret at the cursor position.
                let b = char_byte(txt, cursor);
                field
                    .child(div().flex_none().child(txt[..b].to_string()))
                    .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(rgb(t.text)))
                    .child(div().flex_none().child(txt[b..].to_string()))
                    .into_any_element()
            }
        }
        None => div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(rgb(name_color))
            .child(name.to_string())
            .into_any_element(),
    };

    div()
        .id(("row", key))
        .flex()
        .items_center()
        .px_3()
        .h(px(ROW_H))
        .cursor_pointer()
        .when(selected, |s| s.bg(rgb(t.selected)))
        .when(ctx_active && !selected, |s| s.bg(rgb(t.hover)))
        .hover(|s| s.bg(rgb(t.hover)))
        // Instant press feedback (the click action lands on mouse-up).
        .active(|s| s.bg(rgb(t.selected)))
        .on_hover(on_hover_row)
        // Name (icon + label). Long names truncate with an ellipsis. Only this
        // cell is the "drop into this folder" target — dropping further along
        // the row (Kind/Date/Size) falls through to the pane and lands in the
        // current directory instead, like Finder. It's also the only cell whose
        // press starts a FILE drag (candidate promoted to a native OS drag on
        // move); presses on Kind/Date/Size fall through to the list behind, so
        // a rubber-band marquee can start from a row's non-name area even in a
        // folder whose rows fill the whole viewport — like Finder.
        .child(
            div()
                .flex_none()
                .w(px(widths.name))
                .flex()
                .items_center()
                .gap_2()
                .pr_2()
                .on_mouse_down(MouseButton::Left, on_press)
                .when(accept_drop, |c| c.rounded_sm().on_drop(on_drop_file))
                .when(drop_hilite, |c| c.bg(rgb(theme().selected)))
                .child(
                    div()
                        .flex_none()
                        .w(px(ICON_W))
                        .flex()
                        .justify_center()
                        .child(icon),
                )
                .child(name_el),
        )
        // Kind.
        .child(
            div()
                .flex_none()
                .w(px(widths.kind))
                .pr_3()
                .truncate()
                .text_color(meta_color)
                .child(kind),
        )
        // Date modified ("--" until the background metadata pass fills it in).
        .child(
            div()
                .flex_none()
                .w(px(widths.date))
                .pr_3()
                .truncate()
                .text_color(meta_color)
                .child(if loaded { format_date(modified) } else { "--".to_string() }),
        )
        // Size (right-aligned).
        .child(
            div()
                .flex_none()
                .w(px(widths.size))
                .flex()
                .justify_end()
                .text_color(meta_color)
                .child(if !loaded {
                    "--".to_string()
                } else if is_dir {
                    // Folder size once summed; sentinel/none → "--".
                    match folder_size {
                        Some(s) if s != u64::MAX => format_size(false, s),
                        _ => "--".to_string(),
                    }
                } else {
                    format_size(is_dir, size)
                }),
        )
        // Slack space after the last column (keeps row hover full-width).
        .child(div().flex_1())
        .on_click(on_click)
        .on_mouse_down(MouseButton::Right, on_right)
}

/// One cell in the Icons view: a large icon above a (truncated) name, with the
/// same click / drag / drop / context-menu behavior as a list row.
fn icon_cell(
    pane: usize,
    name: String,
    target: PathBuf,
    is_dir: bool,
    selected: bool,
    ctx_active: bool,
    cell_w: f32,
    cx: &Context<Shuffle>,
) -> AnyElement {
    let t = theme();
    let press_t = target.clone();
    let drop_t = target.clone();
    let ctx_t = target.clone();
    let click_t = target.clone();
    let hover_t = target.clone();
    let mut cell = div()
        .id(SharedString::from(format!("cell:{}", target.to_string_lossy())))
        .flex_none()
        .w(px(cell_w))
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .p_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |s| s.bg(rgb(t.selected)))
        .when(ctx_active && !selected, |s| s.bg(rgb(t.hover)))
        .hover(|s| s.bg(rgb(t.hover)))
        // Instant press feedback (the click action lands on mouse-up).
        .active(|s| s.bg(rgb(t.selected)))
        // Track the hovered item so Enter renames the cell under the mouse.
        .on_hover(cx.listener(move |this, on: &bool, _, _| {
            if *on {
                this.hovered = Some((pane, hover_t.clone()));
            } else if this
                .hovered
                .as_ref()
                .is_some_and(|(hp, p)| *hp == pane && *p == hover_t)
            {
                this.hovered = None;
            }
        }))
        // Press records a drag candidate; moving past the threshold starts a
        // native OS drag (drop into Finder / other apps or back into Shuffle).
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                let (x, y) = (
                    f64::from(ev.position.x) as f32,
                    f64::from(ev.position.y) as f32,
                );
                this.drag_candidate = Some((pane, press_t.clone(), (x, y)));
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .h(px(56.0))
                .flex()
                .items_center()
                .child(icon_element_sized(&target, is_dir, 52.0)),
        )
        .child(
            div()
                .w_full()
                .truncate()
                .text_xs()
                .text_color(rgb(if is_dir { t.accent } else { t.text }))
                .child(name),
        )
        .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
            this.click_entry(pane, click_t.clone(), is_dir, ev, cx);
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                let (x, y) = (
                    f64::from(ev.position.x) as f32,
                    f64::from(ev.position.y) as f32,
                );
                this.open_context_menu(pane, x, y, Some((ctx_t.clone(), is_dir)), cx);
                cx.stop_propagation();
            }),
        );
    if is_dir {
        cell = cell
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme().selected)))
            .on_drop(cx.listener(move |this, d: &ExternalPaths, _, cx| {
                this.drop_files(pane, drop_t.clone(), d.paths().to_vec(), cx);
            }));
    }
    cell.into_any_element()
}

/// One row in a Column-view column: icon · name · (chevron for folders).
fn column_row(
    pane: usize,
    col_index: usize,
    name: &str,
    target: PathBuf,
    is_dir: bool,
    selected: bool,
    ctx_active: bool,
    cx: &Context<Shuffle>,
) -> AnyElement {
    let t = theme();
    let icon = icon_element(&target, is_dir);
    let click_t = target.clone();
    let ctx_t = target.clone();
    div()
        .id(SharedString::from(format!("colrow:{col_index}:{}", target.to_string_lossy())))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .h(px(ROW_H))
        .cursor_pointer()
        .when(selected, |s| s.bg(rgb(t.selected)))
        .when(ctx_active && !selected, |s| s.bg(rgb(t.hover)))
        .hover(|s| s.bg(rgb(t.hover)))
        .child(div().flex_none().w(px(ICON_W)).flex().justify_center().child(icon))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgb(if is_dir { t.accent } else { t.text }))
                .child(name.to_string()),
        )
        .when(is_dir, |r| {
            r.child(div().flex_none().text_color(rgb(t.text_dim)).child("›"))
        })
        .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
            this.column_click(pane, col_index, click_t.clone(), is_dir, ev, cx);
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                let (x, y) = (
                    f64::from(ev.position.x) as f32,
                    f64::from(ev.position.y) as f32,
                );
                this.open_context_menu(pane, x, y, Some((ctx_t.clone(), is_dir)), cx);
                cx.stop_propagation();
            }),
        )
        .into_any_element()
}

/// A human-readable kind label for a file (e.g. "Microsoft Excel", "DWG File").
fn kind_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Directory".to_string();
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    match ext.as_deref() {
        Some("xlsx" | "xls" | "xlsm" | "xlsb") => "Microsoft Excel".to_string(),
        Some("docx" | "doc") => "Microsoft Word".to_string(),
        Some("pptx" | "ppt") => "Microsoft PowerPoint".to_string(),
        Some("pdf") => "PDF Document".to_string(),
        Some("txt" | "md" | "rtf" | "log") => "Text Document".to_string(),
        Some("csv" | "tsv") => "CSV File".to_string(),
        Some("dwg") => "DWG File".to_string(),
        Some("dxf") => "DXF File".to_string(),
        // Images/video/audio/archives show their format, e.g. "PNG Image".
        Some("jpg" | "jpeg") => "JPEG Image".to_string(),
        Some(e @ ("png" | "gif" | "bmp" | "tiff" | "heic" | "webp" | "svg")) => {
            format!("{} Image", e.to_uppercase())
        }
        Some(e @ ("mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v")) => {
            format!("{} Video", e.to_uppercase())
        }
        Some(e @ ("mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg")) => {
            format!("{} Audio", e.to_uppercase())
        }
        Some(e @ ("zip" | "tar" | "gz" | "7z" | "rar" | "bz2" | "xz" | "dmg")) => {
            format!("{} Archive", e.to_uppercase())
        }
        Some(
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "cpp" | "h" | "hpp" | "go" | "java"
            | "rb" | "swift" | "zig" | "sh" | "json" | "toml" | "yaml" | "yml" | "html" | "css",
        ) => "Source Code".to_string(),
        Some("app") => "Application".to_string(),
        Some(other) => format!("{} File", other.to_uppercase()),
        None => "Document".to_string(),
    }
}

/// Build the icon element for an entry: a real macOS file-type icon when we can
/// fetch one, otherwise an emoji fallback. Directories always use the folder
/// emoji (per design — folder icon stays as-is for now).
fn icon_element(path: &Path, is_dir: bool) -> AnyElement {
    icon_element_sized(path, is_dir, 16.0)
}

/// Wrap a file icon with a small corner badge showing its cloud-sync state:
/// a cloud for online-only files, a rotating arrow while downloading. Local
/// (fully-present) files get no badge. `badge` is the badge diameter in px.
fn cloud_badged_icon(icon: AnyElement, cloud: CloudSync, badge: f32) -> AnyElement {
    if cloud == CloudSync::Local {
        return icon;
    }
    let t = theme();
    // A small corner chip. `☁`/`↻` aren't in the UI font (they render blank),
    // so use a solid colored disc with a glyph the app already draws: `▾`
    // (points down = "download this") in blue for online-only, amber while a
    // download/evict is in flight. A ring in the surface color separates it.
    let (bg, glyph) = match cloud {
        CloudSync::OnlineOnly => (0x3b82f6u32, "\u{25BE}"), // blue ▾ — online-only
        CloudSync::Syncing => (0xf59e0bu32, "\u{25BE}"),    // amber ▾ — working
        CloudSync::Local => unreachable!(),
    };
    div()
        .relative()
        .flex()
        .child(icon)
        .child(
            div()
                .absolute()
                .bottom(px(-3.0))
                .right(px(-4.0))
                .w(px(badge))
                .h(px(badge))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgb(bg))
                .border_1()
                .border_color(rgb(t.bg))
                .text_size(px(badge * 0.64))
                .text_color(rgb(0xffffff))
                .child(glyph),
        )
        .into_any_element()
}

/// Like [`icon_element`] but at an explicit pixel size (for the icon/gallery views).
fn icon_element_sized(path: &Path, is_dir: bool, size: f32) -> AnyElement {
    // Cache-only lookup — never build on the render thread. Directories use the
    // shared generic folder icon (built synchronously at startup, so no emoji
    // placeholder). Files use their type-specific icon once the background
    // pre-warm has it, otherwise the shared generic file icon.
    let handle = if is_dir {
        lookup_cached(FOLDER_KEY)
    } else {
        lookup_icon(path).or_else(|| lookup_cached(FILE_KEY))
    };
    if let Some(handle) = handle {
        return img(ImageSource::Render(handle))
            .w(px(size))
            .h(px(size))
            .into_any_element();
    }
    // Last resort only if the base icons couldn't be built.
    div()
        .child(if is_dir { "📁" } else { "📄" })
        .into_any_element()
}

thread_local! {
    // Cache macOS icons by lowercase extension so we hit AppKit once per type,
    // not once per file. `None` records "couldn't build one" to avoid retrying.
    static ICON_CACHE: RefCell<HashMap<String, Option<Arc<RenderImage>>>> =
        RefCell::new(HashMap::new());
}

fn icon_key(path: &Path) -> Option<String> {
    let key = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Read a previously-built icon from the cache by its key. Never builds.
fn lookup_cached(key: &str) -> Option<Arc<RenderImage>> {
    ICON_CACHE.with(|cache| cache.borrow().get(key).cloned().flatten())
}

/// Read a previously-built file icon from the cache (keyed by extension). Never
/// builds, so it's safe to call every frame from `render`.
fn lookup_icon(path: &Path) -> Option<Arc<RenderImage>> {
    let key = icon_key(path)?;
    lookup_cached(&key)
}

/// A guaranteed-plain folder whose icon is the generic macOS folder icon (our
/// own config dir — we create it, so it never has a custom icon).
fn folder_dir_path() -> PathBuf {
    let dir = config_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = fs::create_dir_all(&dir);
    dir
}

/// A guaranteed-plain, extensionless file whose icon is the generic macOS
/// document icon.
fn file_probe_path() -> PathBuf {
    let probe = folder_dir_path().join("icon_probe");
    if !probe.exists() {
        let _ = fs::write(&probe, b"");
    }
    probe
}

/// Build the generic folder + file icons synchronously (a few ms, once) so the
/// very first render shows real Finder icons — no emoji placeholder / swap.
fn ensure_base_icons() {
    if ICON_CACHE.with(|c| !c.borrow().contains_key(FOLDER_KEY)) {
        let icon = pack_icon_path(FOLDER_KEY)
            .and_then(|p| decode_image_file(&p))
            .or_else(|| build_macos_icon(&folder_dir_path()));
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(FOLDER_KEY.to_string(), icon);
        });
    }
    if ICON_CACHE.with(|c| !c.borrow().contains_key(FILE_KEY)) {
        let icon = pack_icon_path(FILE_KEY)
            .and_then(|p| decode_image_file(&p))
            .or_else(|| build_macos_icon(&file_probe_path()));
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(FILE_KEY.to_string(), icon);
        });
    }
}

/// The favorite/location shortcuts shown at the top of the sidebar. Each is
/// `(label, slug)`; the slug both names the cache key (`fav:<slug>`) and the
/// pack override file (`<slug>.png`). The real path is resolved by [`fav_path`].
const SIDEBAR_FAVORITES: &[(&str, &str)] = &[
    ("Applications", "applications"),
    ("Desktop", "desktop"),
    ("Documents", "documents"),
    ("Downloads", "downloads"),
    ("Pictures", "pictures"),
    ("Music", "music"),
    ("Movies", "movies"),
];

/// The path a favorite/location slug points at (used for navigation and to fetch
/// the real macOS special-folder icon when no pack overrides it).
fn fav_path(slug: &str) -> PathBuf {
    let home = home_dir();
    match slug {
        "applications" => PathBuf::from("/Applications"),
        "computer" => PathBuf::from("/"),
        "home" => home,
        "desktop" => home.join("Desktop"),
        "documents" => home.join("Documents"),
        "downloads" => home.join("Downloads"),
        "pictures" => home.join("Pictures"),
        "music" => home.join("Music"),
        "movies" => home.join("Movies"),
        other => home.join(other),
    }
}

fn fav_key(slug: &str) -> String {
    format!("fav:{slug}")
}

/// Build the special sidebar icons (Applications, Documents, the Mac, home, …)
/// synchronously. There are only a handful and each is a few ms, so this is fine
/// at startup and on icon-pack changes. A pack override (`<slug>.png`) wins;
/// otherwise we use the real macOS special-folder icon for that path.
fn ensure_sidebar_icons() {
    let mut slugs: Vec<&str> = SIDEBAR_FAVORITES.iter().map(|(_, s)| *s).collect();
    slugs.push("home");
    slugs.push("computer");
    for slug in slugs {
        let key = fav_key(slug);
        if ICON_CACHE.with(|c| c.borrow().contains_key(&key)) {
            continue;
        }
        let icon = pack_icon_path(&key)
            .and_then(|p| decode_image_file(&p))
            .or_else(|| build_macos_icon(&fav_path(slug)));
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(key, icon);
        });
    }
}

thread_local! {
    /// Render timestamps from the last second, for the FPS meter.
    static FRAME_TIMES: RefCell<std::collections::VecDeque<Instant>> =
        RefCell::new(std::collections::VecDeque::new());
}

/// Record this render and return how many happened in the last second.
fn fps_sample() -> usize {
    let now = Instant::now();
    FRAME_TIMES.with(|q| {
        let mut q = q.borrow_mut();
        q.push_back(now);
        while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            q.pop_front();
        }
        q.len()
    })
}

/// The live FPS readout (Settings → "Frame rate meter"). The repeating no-op
/// animation forces a redraw every vsync, so the number reflects the real
/// achievable frame rate rather than going stale between repaints.
fn fps_overlay() -> AnyElement {
    let fps = fps_sample();
    let color = if fps >= 55 {
        rgb(0x4ade80) // green: smooth
    } else if fps >= 30 {
        rgb(0xfacc15) // yellow: noticeable
    } else {
        rgb(0xf87171) // red: janky
    };
    div()
        .absolute()
        .top(px(TITLEBAR_H + 8.0))
        .right(px(12.0))
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgba(0x000000aa))
        .text_xs()
        .text_color(color)
        .child(format!("{fps} fps"))
        .with_animation(
            "fps-tick",
            Animation::new(Duration::from_millis(100)).repeat(),
            |el, _| el,
        )
        .into_any_element()
}

/// Snapshot of the sidebar's cloud + volume scans, refreshed off-thread.
static SIDEBAR_SCAN: OnceLock<Mutex<Option<SidebarScan>>> = OnceLock::new();
static SIDEBAR_SCANNING: AtomicBool = AtomicBool::new(false);
type SidebarScan = (Instant, Vec<(String, PathBuf)>, Vec<(String, PathBuf)>);

/// The sidebar's cloud providers + mounted volumes, from a short-lived cache.
/// `render` calls this every frame; the actual `read_dir`/`stat` walk (which
/// can block for seconds on a stale network mount) runs at most every couple of
/// seconds, on a background thread. The very first call scans synchronously so
/// the sidebar is complete on first paint.
fn sidebar_locations() -> (Vec<(String, PathBuf)>, Vec<(String, PathBuf)>) {
    const TTL: Duration = Duration::from_secs(2);
    let cell = SIDEBAR_SCAN.get_or_init(|| Mutex::new(None));
    let cached = cell.lock().unwrap().clone();
    match cached {
        Some((at, cloud, vols)) => {
            if at.elapsed() > TTL && !SIDEBAR_SCANNING.swap(true, Ordering::SeqCst) {
                std::thread::spawn(|| {
                    let fresh = (Instant::now(), cloud_locations(), mounted_volumes());
                    *SIDEBAR_SCAN.get().unwrap().lock().unwrap() = Some(fresh);
                    SIDEBAR_SCANNING.store(false, Ordering::SeqCst);
                });
            }
            (cloud, vols)
        }
        None => {
            let cloud = cloud_locations();
            let vols = mounted_volumes();
            *cell.lock().unwrap() = Some((Instant::now(), cloud.clone(), vols.clone()));
            (cloud, vols)
        }
    }
}

/// List a folder's visible subdirectories (sorted case-insensitively) plus its
/// current mtime. Used by the waterfall sidebar tree; runs off the main thread
/// (may block on I/O). The mtime lets the watcher detect outside changes.
fn read_subdirs(dir: &Path) -> (Vec<PathBuf>, Option<SystemTime>) {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = match e.file_type() {
                Ok(ft) if ft.is_dir() => true,
                Ok(ft) if ft.is_symlink() => p.is_dir(),
                _ => false,
            };
            let hidden = p
                .file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false);
            if is_dir && !hidden {
                out.push(p);
            }
        }
    }
    out.sort_by_cached_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    let mtime = fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    (out, mtime)
}

/// Recursively-summed folder sizes for List view, computed off-thread and
/// cached (a folder can hold millions of files; we never walk on the render
/// thread). `u64::MAX` is a sentinel: the folder blew past the entry cap, so we
/// show "--" rather than a wrong number.
static FOLDER_SIZE: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
static FOLDER_SIZE_PENDING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
/// Stop summing a folder past this many entries (bounds cost on huge trees like
/// `/` or a home with node_modules); such folders show "--".
const FOLDER_SIZE_CAP: usize = 400_000;

fn folder_size_lookup(path: &Path) -> Option<u64> {
    FOLDER_SIZE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(path)
        .copied()
}

/// Total bytes of all files under `dir` (jwalk, parallel, symlinks not
/// followed). Returns `u64::MAX` if the tree exceeds [`FOLDER_SIZE_CAP`].
fn dir_total_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for entry in jwalk::WalkDir::new(dir).skip_hidden(false) {
        count += 1;
        if count > FOLDER_SIZE_CAP {
            return u64::MAX;
        }
        if let Ok(e) = entry {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// `stat` results for sidebar rows (bookmarks, group members, favorites),
/// cached so `render` never touches the filesystem per frame — a single dead
/// network bookmark would otherwise freeze every repaint. Stale entries are
/// re-checked on a background thread.
static DIR_STAT: OnceLock<Mutex<HashMap<PathBuf, (bool, Instant)>>> = OnceLock::new();
static DIR_STAT_SCANNING: AtomicBool = AtomicBool::new(false);

fn cached_is_dir(path: &Path) -> bool {
    const TTL: Duration = Duration::from_secs(3);
    let map = DIR_STAT.get_or_init(|| Mutex::new(HashMap::new()));
    let hit = map.lock().unwrap().get(path).copied();
    match hit {
        Some((v, at)) => {
            if at.elapsed() > TTL && !DIR_STAT_SCANNING.swap(true, Ordering::SeqCst) {
                std::thread::spawn(|| {
                    let map = DIR_STAT.get().unwrap();
                    let stale: Vec<PathBuf> = {
                        let m = map.lock().unwrap();
                        m.iter()
                            .filter(|(_, (_, at))| at.elapsed() > TTL)
                            .map(|(p, _)| p.clone())
                            .collect()
                    };
                    for p in stale {
                        let v = p.is_dir(); // may block; we're off-thread
                        map.lock().unwrap().insert(p, (v, Instant::now()));
                    }
                    DIR_STAT_SCANNING.store(false, Ordering::SeqCst);
                });
            }
            v
        }
        None => {
            // First sighting (startup / newly added): one synchronous stat so
            // the answer is correct immediately.
            let v = path.is_dir();
            map.lock().unwrap().insert(path.to_path_buf(), (v, Instant::now()));
            v
        }
    }
}

/// Cloud-storage locations macOS syncs to disk: iCloud Drive plus every
/// provider under `~/Library/CloudStorage` (Dropbox, Google Drive, OneDrive,
/// Box, …), and a legacy `~/Dropbox` if present. Returns `(label, path)`.
fn cloud_locations() -> Vec<(String, PathBuf)> {
    let home = home_dir();
    let mut out: Vec<(String, PathBuf)> = Vec::new();

    let icloud = home.join("Library/Mobile Documents/com~apple~CloudDocs");
    if icloud.is_dir() {
        out.push(("iCloud Drive".to_string(), icloud));
    }

    let cs = home.join("Library/CloudStorage");
    if let Ok(rd) = fs::read_dir(&cs) {
        let mut providers: Vec<(String, PathBuf)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .map(|p| {
                let raw = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                (pretty_cloud_name(&raw), p)
            })
            .collect();
        providers.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out.extend(providers);
    }

    // Legacy Dropbox install (pre-CloudStorage) at ~/Dropbox.
    let legacy_dropbox = home.join("Dropbox");
    if legacy_dropbox.is_dir() && !out.iter().any(|(l, _)| l == "Dropbox") {
        out.push(("Dropbox".to_string(), legacy_dropbox));
    }

    out
}

/// Turn a raw CloudStorage folder name ("GoogleDrive-me@x.com", "OneDrive-Personal")
/// into a friendly label ("Google Drive", "OneDrive").
fn pretty_cloud_name(raw: &str) -> String {
    let base = raw.split('-').next().unwrap_or(raw).trim();
    match base {
        "GoogleDrive" => "Google Drive".to_string(),
        "" => raw.to_string(),
        other => other.to_string(),
    }
}

/// Mounted volumes under `/Volumes` (external drives + network shares),
/// excluding the boot volume. Returns `(label, path)`.
fn mounted_volumes() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    // Mount points flagged `nobrowse` (installer DMGs, simulator runtimes, …)
    // are hidden from Finder's sidebar; hide them here too.
    let hidden = nobrowse_mounts();
    if let Ok(rd) = fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            // Skip the boot volume (a /Volumes entry that resolves to "/").
            if fs::canonicalize(&p).map(|t| t == Path::new("/")).unwrap_or(false) {
                continue;
            }
            // Skip nobrowse mounts (and their resolved targets).
            let resolved = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if hidden.contains(&p) || hidden.contains(&resolved) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            out.push((name, p));
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

/// The set of currently-mounted paths flagged `nobrowse` (per `mount`), which
/// macOS hides from Finder — mounted installer disk images, OS simulator
/// runtimes, cryptex mounts, and the like.
fn nobrowse_mounts() -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    if let Ok(out) = Command::new("/sbin/mount").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                // Format: "<dev> on <mount point> (<fstype>, <flags…>)".
                let Some(on) = line.find(" on ") else { continue };
                let rest = &line[on + 4..];
                let Some(paren) = rest.rfind(" (") else { continue };
                let mount_point = &rest[..paren];
                let flags = &rest[paren + 2..];
                if flags.contains("nobrowse") {
                    set.insert(PathBuf::from(mount_point));
                }
            }
        }
    }
    set
}

/// Paths whose real macOS icon should be cached for the sidebar (cloud
/// providers + mounted volumes). These are dynamic, so their icons are keyed by
/// the path string and (re)built by [`ensure_dynamic_sidebar_icons`].
fn dynamic_sidebar_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = cloud_locations().into_iter().map(|(_, p)| p).collect();
    v.extend(mounted_volumes().into_iter().map(|(_, p)| p));
    v
}

/// Build the real macOS icons for the currently-mounted volumes and synced
/// cloud folders, keyed by path. Cheap (icon_tiff is a few ms) and main-thread,
/// so it's safe to call on navigation / startup — never from render.
fn ensure_dynamic_sidebar_icons() {
    for p in dynamic_sidebar_paths() {
        let key = p.to_string_lossy().into_owned();
        if ICON_CACHE.with(|c| c.borrow().contains_key(&key)) {
            continue;
        }
        let icon = build_macos_icon(&p);
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(key, icon);
        });
    }
}

// ----- SFTP servers (browse remote hosts over SSH) ---------------------------

/// A saved SFTP server. Auth is delegated to the system `ssh`/`sftp`, so a
/// server is mostly a host reference; `key` is only used in "configure in-app"
/// mode (otherwise the user's `~/.ssh` config/keys/agent handle auth).
#[derive(Clone, Default, PartialEq)]
struct SftpServer {
    /// Display label in the sidebar.
    name: String,
    /// Hostname or a `~/.ssh/config` alias.
    host: String,
    /// Login user (empty = from ssh config / current user).
    user: String,
    /// Port (0 = default / from ssh config).
    port: u16,
    /// Explicit private-key path (in-app auth mode only; empty otherwise).
    key: String,
    /// True when this server authenticates with a password (stored in the macOS
    /// Keychain, fetched at connect time via an askpass helper). False = use the
    /// user's ~/.ssh keys/agent (or the explicit `key`).
    use_password: bool,
    /// Reconnect and reopen this server automatically on launch.
    auto_reopen: bool,
}

impl SftpServer {
    /// The `[user@]host` target passed to ssh/sftp.
    fn target(&self) -> String {
        if self.user.trim().is_empty() {
            self.host.trim().to_string()
        } else {
            format!("{}@{}", self.user.trim(), self.host.trim())
        }
    }

    /// A short display of the connection ("user@host:port").
    fn display(&self) -> String {
        let mut s = self.target();
        if self.port != 0 {
            s.push_str(&format!(":{}", self.port));
        }
        s
    }
}

/// Which tab of the Connect-to-Server dialog is showing.
#[derive(Clone, Copy, PartialEq)]
enum ServerMode {
    /// A single address (smb://, afp://, ftp://, or sftp://[user@]host).
    Quick,
    /// SFTP with an explicit username + password (stored in the Keychain).
    Credentials,
}

/// A focused field in the Credentials tab.
#[derive(Clone, Copy, PartialEq)]
enum CredField {
    Name,
    Host,
    User,
    Port,
    Password,
}

/// State of the Connect-to-Server dialog.
#[derive(Clone)]
struct ServerForm {
    mode: ServerMode,
    /// Quick tab: the address being typed.
    addr: String,
    /// Credentials tab fields.
    name: String,
    host: String,
    user: String,
    port: String,
    password: String,
    field: CredField,
    /// "Reconnect on launch" toggle for the server being added.
    auto_reopen: bool,
    /// When editing an existing server, its original `display()` (so submit
    /// replaces it instead of adding a duplicate). `None` = adding a new one.
    editing: Option<String>,
}

impl Default for ServerForm {
    fn default() -> Self {
        ServerForm {
            mode: ServerMode::Quick,
            addr: String::new(),
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: String::new(),
            password: String::new(),
            field: CredField::Host,
            auto_reopen: false,
            editing: None,
        }
    }
}

impl ServerForm {
    /// Pre-fill the dialog from an existing server, for editing. Password
    /// servers open the Credentials tab; key/agent servers open Quick.
    fn editing_server(s: &SftpServer) -> Self {
        if s.use_password {
            ServerForm {
                mode: ServerMode::Credentials,
                name: s.name.clone(),
                host: s.host.clone(),
                user: s.user.clone(),
                port: if s.port == 0 { String::new() } else { s.port.to_string() },
                auto_reopen: s.auto_reopen,
                editing: Some(s.display()),
                field: CredField::Host,
                ..ServerForm::default()
            }
        } else {
            ServerForm {
                mode: ServerMode::Quick,
                addr: format!("sftp://{}", s.display()),
                auto_reopen: s.auto_reopen,
                editing: Some(s.display()),
                ..ServerForm::default()
            }
        }
    }
}

impl ServerForm {
    /// The credentials field currently focused, as a mutable string.
    fn field_mut(&mut self) -> &mut String {
        match self.field {
            CredField::Name => &mut self.name,
            CredField::Host => &mut self.host,
            CredField::User => &mut self.user,
            CredField::Port => &mut self.port,
            CredField::Password => &mut self.password,
        }
    }

    /// The string being edited: the Quick address, or the focused credentials
    /// field.
    fn active_field(&mut self) -> &mut String {
        match self.mode {
            ServerMode::Quick => &mut self.addr,
            ServerMode::Credentials => self.field_mut(),
        }
    }

    /// Advance focus to the next credentials field (Tab).
    fn next_field(&mut self) {
        self.field = match self.field {
            CredField::Name => CredField::Host,
            CredField::Host => CredField::User,
            CredField::User => CredField::Port,
            CredField::Port => CredField::Password,
            CredField::Password => CredField::Name,
        };
    }
}

/// The Keychain account name for a server's password.
fn keychain_account(s: &SftpServer) -> String {
    format!("sftp:{}", s.display())
}

/// Store a server's password in the login Keychain (updating any existing one).
fn keychain_set_password(s: &SftpServer, pw: &str) {
    let _ = Command::new("security")
        .args(["add-generic-password", "-U", "-s", "shuffle-sftp", "-a"])
        .arg(keychain_account(s))
        .arg("-w")
        .arg(pw)
        .output();
}

/// Remove a server's stored password.
fn keychain_delete_password(s: &SftpServer) {
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", "shuffle-sftp", "-a"])
        .arg(keychain_account(s))
        .output();
}

/// Read a server's stored password back from the Keychain (empty = none).
fn keychain_get_password(s: &SftpServer) -> Option<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-w", "-s", "shuffle-sftp", "-a"])
        .arg(keychain_account(s))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pw = String::from_utf8_lossy(&out.stdout);
    let pw = pw.trim_end_matches(['\n', '\r']);
    (!pw.is_empty()).then(|| pw.to_string())
}

/// The `expect` script that establishes a password SSH connection: it drives
/// `sftp`, types the password at the prompt (password AND keyboard-interactive
/// servers both use this prompt), and leaves a persistent ControlMaster socket
/// that later operations reuse without re-authenticating. Created once.
/// Args: <control-socket> <user@host> <port>. Env: SHUFFLE_SFTP_PW.
fn ensure_connect_expect() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = config_dir()?.join("sftp-connect.exp");
    let body = r#"#!/usr/bin/expect -f
set timeout 25
set sock [lindex $argv 0]
set target [lindex $argv 1]
set port [lindex $argv 2]
set pw $env(SHUFFLE_SFTP_PW)
spawn sftp -o ControlMaster=yes -o ControlPath=$sock -o ControlPersist=180 \
  -o ConnectTimeout=12 -o StrictHostKeyChecking=accept-new \
  -o PubkeyAuthentication=no -o NumberOfPasswordPrompts=1 -P $port $target
expect {
  -re "(?i)permission denied" { exit 11 }
  -re "(?i)could not resolve|name or service not known" { exit 12 }
  -re "(?i)assword:" { send -- "$pw\r"; exp_continue }
  "sftp>" { send -- "quit\r"; exit 0 }
  timeout { exit 13 }
  eof { exit 14 }
}
"#;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path, body).ok()?;
    if let Ok(md) = fs::metadata(&path) {
        let mut perm = md.permissions();
        perm.set_mode(0o755);
        let _ = fs::set_permissions(&path, perm);
    }
    Some(path)
}

/// Is the multiplexed master connection for a password server alive?
fn password_master_alive(s: &SftpServer) -> bool {
    Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", ssh_control_path(s).to_string_lossy()))
        .arg("-O")
        .arg("check")
        .arg(s.target())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Ensure a password server has a live authenticated master connection,
/// establishing it via `expect` (typing the Keychain password) if needed.
/// No-op for key/agent servers.
fn ensure_master(server: &SftpServer) -> Result<(), String> {
    if !server.use_password {
        return Ok(());
    }
    if password_master_alive(server) {
        return Ok(());
    }
    let pw = keychain_get_password(server)
        .ok_or_else(|| "No saved password — edit the server to set it.".to_string())?;
    let script = ensure_connect_expect().ok_or_else(|| "helper unavailable".to_string())?;
    let port = if server.port == 0 { 22 } else { server.port };
    let out = Command::new("expect")
        .arg(script)
        .arg(ssh_control_path(server))
        .arg(server.target())
        .arg(port.to_string())
        .env("SHUFFLE_SFTP_PW", pw)
        .output()
        .map_err(|e| format!("couldn't launch expect: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(11) => Err("Authentication failed — check the username and password.".into()),
        Some(12) => Err("Host not found.".into()),
        Some(13) => Err("Connection timed out.".into()),
        _ => Err("Couldn't connect to the server.".into()),
    }
}

/// Parse `[user@]host[:port]` (the part after `sftp://`) into a server. The
/// display name defaults to the host/alias.
fn parse_sftp_url(s: &str) -> Option<SftpServer> {
    // Drop any trailing path.
    let s = s.split('/').next().unwrap_or(s).trim();
    if s.is_empty() {
        return None;
    }
    let (user, hostport) = match s.split_once('@') {
        Some((u, h)) => (u.to_string(), h),
        None => (String::new(), s),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(0))
        }
        _ => (hostport.to_string(), 0u16),
    };
    if host.is_empty() {
        return None;
    }
    Some(SftpServer {
        name: host.clone(),
        host,
        user,
        port,
        key: String::new(),
        use_password: false,
        auto_reopen: false,
    })
}

#[derive(Clone)]
struct SftpServersGlobal(Vec<SftpServer>);
impl gpui::Global for SftpServersGlobal {}

thread_local! {
    static ACTIVE_SERVERS: RefCell<Vec<SftpServer>> = const { RefCell::new(Vec::new()) };
}

/// The saved SFTP servers (read this in render code).
fn sftp_servers() -> Vec<SftpServer> {
    ACTIVE_SERVERS.with(|s| s.borrow().clone())
}

fn set_active_sftp_servers(list: Vec<SftpServer>) {
    ACTIVE_SERVERS.with(|c| *c.borrow_mut() = list);
}

/// Persist + broadcast a new server list to every window.
fn apply_sftp_servers(list: Vec<SftpServer>, cx: &mut App) {
    set_active_sftp_servers(list.clone());
    save_sftp_servers(&list);
    cx.set_global(SftpServersGlobal(list));
    cx.refresh_windows();
}

/// A per-server socket path for SSH connection multiplexing (ControlMaster), so
/// repeated listings/transfers reuse one authenticated connection.
fn ssh_control_path(s: &SftpServer) -> PathBuf {
    // Unix socket paths are capped near 104 bytes, so we can't use macOS's long
    // per-user temp dir (/var/folders/…/T). A short, stable /tmp name keyed by a
    // hash of the target stays well under the limit.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.display().hash(&mut h);
    PathBuf::from(format!("/tmp/shuffle-ssh-{:016x}.sock", h.finish()))
}

/// Common `ssh`/`sftp` options for a server: multiplexing, non-interactive
/// (fail fast instead of hanging on a password prompt), trust-on-first-use host
/// keys, and an explicit key when in "configure in-app" mode.
fn ssh_options(s: &SftpServer, use_system: bool) -> Vec<String> {
    let cp = ssh_control_path(s);
    let mut o = vec![
        "-o".into(),
        format!("ControlPath={}", cp.to_string_lossy()),
        "-o".into(),
        "ConnectTimeout=12".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    if s.use_password {
        // Password auth: reuse the authenticated master that `ensure_master`
        // established via expect; never prompt (fail fast if it's gone).
        o.push("-o".into());
        o.push("ControlMaster=no".into());
        o.push("-o".into());
        o.push("BatchMode=yes".into());
    } else {
        // Key/agent auth: multiplex directly, fail fast instead of prompting.
        o.push("-o".into());
        o.push("ControlMaster=auto".into());
        o.push("-o".into());
        o.push("ControlPersist=120".into());
        o.push("-o".into());
        o.push("BatchMode=yes".into());
        if !use_system && !s.key.trim().is_empty() {
            o.push("-o".into());
            o.push("IdentitiesOnly=yes".into());
            o.push("-i".into());
            o.push(s.key.trim().to_string());
        }
    }
    o
}

/// Single-quote a path for a remote POSIX shell command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a shell command over ssh on `server` (reusing the multiplexed
/// connection). Used for operations sftp lacks (recursive delete, touch).
fn ssh_exec(server: &SftpServer, remote_cmd: &str, use_system: bool) -> Result<String, String> {
    ensure_master(server)?;
    let mut cmd = Command::new("ssh");
    for opt in ssh_options(server, use_system) {
        cmd.arg(opt);
    }
    if server.port != 0 {
        cmd.arg("-p").arg(server.port.to_string());
    }
    cmd.arg(server.target()).arg(remote_cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().map_err(|e| format!("couldn't launch ssh: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("command failed")
            .to_string())
    }
}

/// Run a batch of sftp commands against `server`, returning stdout on success.
/// `use_system` selects auth mode (delegate to ~/.ssh vs. explicit key).
fn sftp_batch(server: &SftpServer, script: &str, use_system: bool) -> Result<String, String> {
    use std::io::Write;
    // Password servers: make sure the authenticated master is up first.
    ensure_master(server)?;
    let mut cmd = Command::new("sftp");
    cmd.arg("-b").arg("-"); // read commands from stdin
    for opt in ssh_options(server, use_system) {
        cmd.arg(opt);
    }
    if server.port != 0 {
        cmd.arg("-P").arg(server.port.to_string());
    }
    cmd.arg(server.target());
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("couldn't launch sftp: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
        let _ = stdin.write_all(b"\nquit\n");
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        // Surface the most useful line (auth/host/connection failure).
        let msg = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("connection failed")
            .to_string();
        Err(msg)
    }
}

/// The server's default remote directory (the login home), via `sftp pwd`.
fn sftp_home(server: &SftpServer, use_system: bool) -> Result<String, String> {
    let out = sftp_batch(server, "pwd", use_system)?;
    // sftp prints: "Remote working directory: /home/user"
    for line in out.lines() {
        if let Some(p) = line.split_once("working directory:") {
            let path = p.1.trim();
            if !path.is_empty() {
                return Ok(path.to_string());
            }
        }
    }
    Ok("/".to_string())
}

/// List a remote directory, returning entries parsed from sftp's `ls -l`.
fn sftp_list(server: &SftpServer, path: &str, use_system: bool) -> Result<Vec<Entry>, String> {
    // Quote the path so spaces work; sftp treats the argument as a glob-free path.
    let script = format!("ls -la \"{}\"", path.replace('"', ""));
    let out = sftp_batch(server, &script, use_system)?;
    Ok(parse_sftp_ls(&out))
}

/// Create a remote directory.
fn sftp_mkdir(server: &SftpServer, path: &str, use_system: bool) -> Result<(), String> {
    sftp_batch(server, &format!("mkdir \"{}\"", path.replace('"', "")), use_system).map(|_| ())
}

/// Rename/move a remote path (same server).
fn sftp_rename(server: &SftpServer, from: &str, to: &str, use_system: bool) -> Result<(), String> {
    sftp_batch(
        server,
        &format!("rename \"{}\" \"{}\"", from.replace('"', ""), to.replace('"', "")),
        use_system,
    )
    .map(|_| ())
}

/// Upload a local file/folder into a remote directory (recursive for folders).
fn sftp_upload(
    server: &SftpServer,
    local: &Path,
    remote_dir: &str,
    use_system: bool,
) -> Result<(), String> {
    let l = local.to_string_lossy().replace('"', "");
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = format!("{}/{}", remote_dir.trim_end_matches('/'), name).replace('"', "");
    let recurse = if local.is_dir() { "-r " } else { "" };
    sftp_batch(server, &format!("put {recurse}\"{l}\" \"{dest}\""), use_system).map(|_| ())
}

/// Permanently delete a remote path (files or folders, recursively) via ssh.
fn sftp_delete(server: &SftpServer, path: &str, use_system: bool) -> Result<(), String> {
    ssh_exec(server, &format!("rm -rf {}", shell_quote(path)), use_system).map(|_| ())
}

/// Create an empty remote file via ssh `touch`.
fn sftp_touch(server: &SftpServer, path: &str, use_system: bool) -> Result<(), String> {
    ssh_exec(server, &format!("touch {}", shell_quote(path)), use_system).map(|_| ())
}

/// Parse sftp `ls -l` output into entries. Lines look like:
/// `drwxr-xr-x  2 1000 1000  4096 Jul 30 10:00 name` — 8 fixed fields + name.
/// The format is produced by the sftp client itself, so it's consistent across
/// remote operating systems.
fn parse_sftp_ls(out: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim_end();
        // Skip the echoed command ("sftp> ls …") and blank lines.
        if line.is_empty() || line.starts_with("sftp>") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 9 {
            continue;
        }
        let perms = parts[0];
        // A permissions field starts with a type char (d/-/l/…); skip anything
        // that isn't a long-listing line (e.g. stray output).
        let first = perms.chars().next().unwrap_or(' ');
        if !matches!(first, 'd' | '-' | 'l' | 'c' | 'b' | 'p' | 's') || perms.len() < 10 {
            continue;
        }
        let is_dir = first == 'd';
        let size = parts[4].parse::<u64>().unwrap_or(0);
        let mut name = parts[8..].join(" ");
        // Symlinks list as "name -> target"; keep just the name.
        if first == 'l' {
            if let Some((n, _)) = name.split_once(" -> ") {
                name = n.to_string();
            }
        }
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }
        entries.push(Entry {
            name,
            is_dir,
            size,
            modified: None,
            created: None,
            loaded: true,
        });
    }
    sort_default(&mut entries);
    entries
}

/// Persist saved servers, one per line (tab-separated fields).
fn save_sftp_servers(list: &[SftpServer]) {
    if let Some(file) = config_file("sftp_servers.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body: String = list
            .iter()
            .map(|s| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    s.name.replace('\t', " "),
                    s.host.replace('\t', " "),
                    s.user.replace('\t', " "),
                    s.port,
                    s.key.replace('\t', " "),
                    s.use_password,
                    s.auto_reopen
                )
            })
            .collect();
        let _ = fs::write(&file, body);
    }
}

/// Load saved servers.
fn load_sftp_servers() -> Vec<SftpServer> {
    let mut out = Vec::new();
    if let Some(file) = config_file("sftp_servers.txt") {
        if let Ok(s) = fs::read_to_string(&file) {
            for line in s.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 2 || f[0].trim().is_empty() {
                    continue;
                }
                out.push(SftpServer {
                    name: f[0].to_string(),
                    host: f.get(1).unwrap_or(&"").to_string(),
                    user: f.get(2).unwrap_or(&"").to_string(),
                    port: f.get(3).and_then(|p| p.parse().ok()).unwrap_or(0),
                    key: f.get(4).unwrap_or(&"").to_string(),
                    use_password: f.get(5).map(|v| v.trim() == "true").unwrap_or(false),
                    auto_reopen: f.get(6).map(|v| v.trim() == "true").unwrap_or(false),
                });
            }
        }
    }
    out
}

// ----- native OS file drag-out (drag files into Finder / other apps) ---------

define_class!(
    // A minimal NSDraggingSource: it only needs to say the drag is a "copy".
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShuffleDragSource"]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            // Copy only: dragging out to another app never moves/deletes the
            // original file.
            NSDragOperation::Copy
        }
    }
);

thread_local! {
    /// One shared dragging-source instance, reused for every drag.
    static DRAG_SOURCE: RefCell<Option<objc2::rc::Retained<DragSource>>> = RefCell::new(None);
}

fn drag_source(mtm: objc2::MainThreadMarker) -> objc2::rc::Retained<DragSource> {
    DRAG_SOURCE.with(|c| {
        c.borrow_mut()
            .get_or_insert_with(|| unsafe { objc2::msg_send![DragSource::alloc(mtm), init] })
            .clone()
    })
}

/// The GPUI content `NSView` pointer for this window (for native drag sessions).
fn ns_view_ptr(window: &Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr()),
        _ => None,
    }
}

/// Start a native macOS drag session carrying `paths` as file URLs. macOS then
/// drives the drag through the normal run loop; dropping on another app copies
/// the files there, and dropping back on Shuffle arrives as an external-file
/// drop (handled by our `ExternalPaths` drop targets).
fn start_os_file_drag(view_ptr: *mut std::ffi::c_void, paths: &[PathBuf]) {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::AllocAnyThread;
    use objc2_app_kit::{
        NSApplication, NSDraggingItem, NSDraggingSource, NSPasteboardWriting, NSView, NSWorkspace,
    };
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};

    if paths.is_empty() || view_ptr.is_null() {
        return;
    }
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    // SAFETY: the GPUI content view is a live NSView on the main thread.
    let view: &NSView = unsafe { &*(view_ptr as *const NSView) };
    let app = NSApplication::sharedApplication(mtm);
    let Some(event) = app.currentEvent() else {
        return;
    };
    let base = event.locationInWindow();
    let workspace = NSWorkspace::sharedWorkspace();

    let mut items: Vec<Retained<NSDraggingItem>> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let Some(s) = p.to_str() else { continue };
        let ns = NSString::from_str(s);
        let url = NSURL::fileURLWithPath(&ns);
        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*url);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);

        let icon = workspace.iconForFile(&ns);
        let size = NSSize { width: 48.0, height: 48.0 };
        icon.setSize(size);
        // Fan the icons out slightly so a multi-file drag reads as a stack.
        let off = i as f64 * 6.0;
        let frame = NSRect {
            origin: NSPoint { x: base.x - 24.0 + off, y: base.y - 24.0 - off },
            size,
        };
        let contents: &AnyObject = &icon;
        unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
        items.push(item);
    }
    if items.is_empty() {
        return;
    }
    let array = NSArray::from_retained_slice(&items);

    let source = drag_source(mtm);
    let source_proto: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*source);
    let _ = view.beginDraggingSessionWithItems_event_source(&array, &event, source_proto);
}

thread_local! {
    /// Persistent vertical scroll handles for Column view, keyed by (pane,
    /// column index) so each column keeps its scroll position across frames and
    /// we can read its offset to draw a scrollbar thumb.
    static COL_SCROLLS: RefCell<HashMap<(usize, usize), gpui::ScrollHandle>> =
        RefCell::new(HashMap::new());
}

/// The persistent scroll handle for one Column-view column.
fn col_scroll(pane: usize, col: usize) -> gpui::ScrollHandle {
    COL_SCROLLS.with(|m| {
        m.borrow_mut()
            .entry((pane, col))
            .or_insert_with(gpui::ScrollHandle::new)
            .clone()
    })
}

/// A minimal, non-interactive vertical scrollbar thumb for any tracked
/// `ScrollHandle`. Returns `None` when the content fits (nothing to scroll).
/// Used by the palette, Settings, and column view (the main list has its own
/// richer draggable/fading bar).
fn static_scrollbar_thumb(base: &gpui::ScrollHandle) -> Option<AnyElement> {
    let viewport = f64::from(base.bounds().size.height) as f32;
    let max = f64::from(base.max_offset().height) as f32;
    if viewport <= 1.0 || max <= 1.0 {
        return None;
    }
    let scrolled = (-(f64::from(base.offset().y) as f32)).clamp(0.0, max);
    let content = viewport + max;
    let thumb_h = (viewport * viewport / content).clamp(20.0, viewport);
    let thumb_top = (viewport - thumb_h) * (scrolled / max);
    Some(
        div()
            .absolute()
            .top(px(thumb_top))
            .right(px(2.0))
            .w(px(6.0))
            .h(px(thumb_h))
            .rounded_full()
            .bg(Theme::alpha(theme().text, 0x44))
            .into_any_element(),
    )
}

/// Build a `.tooltip(...)` callback showing `text` in a small floating label.
fn tip(text: impl Into<String>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| TooltipView { text: text.clone() }).into()
}

/// A sidebar icon element by cache key, falling back to the generic folder icon
/// so a row is never blank.
fn sidebar_icon(key: &str, size: f32) -> AnyElement {
    let handle = lookup_cached(key).or_else(|| lookup_cached(FOLDER_KEY));
    if let Some(handle) = handle {
        return img(ImageSource::Render(handle))
            .w(px(size))
            .h(px(size))
            .into_any_element();
    }
    div().child("📁").into_any_element()
}

/// Ask NSWorkspace for `path`'s icon, decode it, and convert to a GPUI image.
/// This is the expensive part (AppKit + TIFF decode + resize), so it runs only
/// off the render path, in the background pre-warm task.
/// Fetch a file's macOS icon as TIFF bytes, rendered at a small fixed size.
///
/// `iconForFile` is instant, but `NSImage::TIFFRepresentation` renders *every*
/// representation up to 1024px (tens to hundreds of ms). Instead we draw the
/// icon once into a 128px bitmap and serialize only that — a few ms. Touches
/// AppKit, so it must run on the main thread.
fn icon_tiff(path: &Path) -> Option<Vec<u8>> {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let path_str = path.to_str()?;
    let workspace = NSWorkspace::sharedWorkspace();
    let ns_path = NSString::from_str(path_str);
    let image: objc2::rc::Retained<NSImage> = workspace.iconForFile(&ns_path);

    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            128,
            128,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let dst = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(128.0, 128.0));
    let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
    image.drawInRect_fromRect_operation_fraction(dst, zero, NSCompositingOperation::Copy, 1.0);
    NSGraphicsContext::restoreGraphicsState_class();

    let data: objc2::rc::Retained<NSData> = rep.TIFFRepresentation()?;
    if data.len() == 0 {
        return None;
    }
    Some(data.to_vec())
}

/// Decode TIFF icon bytes and convert to a 128px GPUI image. Pure CPU (the
/// expensive part — the large TIFF decode), safe to run off the main thread.
fn decode_icon(tiff: &[u8]) -> Option<Arc<RenderImage>> {
    let decoded = image::load_from_memory(tiff).ok()?;
    // 128px stays crisp in the Icons/Gallery views; the GPU downscales cleanly
    // to 16px in list view. One cached icon per file type.
    let decoded = decoded.resize_exact(128, 128, image::imageops::FilterType::Lanczos3);
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut raw = rgba.into_raw();
    // RenderImage expects BGRA; the decoded buffer is RGBA, so swap R and B.
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, raw)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Build a file's icon synchronously (AppKit fetch + decode). Used only at
/// startup for the base folder/file icons; the per-type prewarm splits these
/// across threads to keep navigation smooth.
fn build_macos_icon(path: &Path) -> Option<Arc<RenderImage>> {
    let tiff = icon_tiff(path)?;
    decode_icon(&tiff)
}

thread_local! {
    /// Cache of generated file previews. `None` = generation failed/unavailable.
    static PREVIEW_CACHE: RefCell<HashMap<PathBuf, Option<Arc<RenderImage>>>> =
        RefCell::new(HashMap::new());
    /// Cache of gathered file information.
    static INFO_CACHE: RefCell<HashMap<PathBuf, FileInfo>> = RefCell::new(HashMap::new());
}

fn lookup_preview(path: &Path) -> Option<Option<Arc<RenderImage>>> {
    PREVIEW_CACHE.with(|c| c.borrow().get(path).cloned())
}

thread_local! {
    /// Rendered PDF pages for the inspector pager: (path, page) → image.
    /// `None` = that page couldn't be rendered.
    static PDF_PAGE_CACHE: RefCell<HashMap<(PathBuf, usize), Option<Arc<RenderImage>>>> =
        RefCell::new(HashMap::new());
    /// Page counts of PDFs we've rendered at least one page of.
    static PDF_COUNT_CACHE: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
}

fn lookup_pdf_page(path: &Path, page: usize) -> Option<Option<Arc<RenderImage>>> {
    PDF_PAGE_CACHE.with(|c| c.borrow().get(&(path.to_path_buf(), page)).cloned())
}

fn lookup_pdf_count(path: &Path) -> Option<usize> {
    PDF_COUNT_CACHE.with(|c| c.borrow().get(path).copied())
}

/// Insert a rendered page, evicting far-away pages so flipping through a long
/// PDF can't accumulate unbounded image memory (each page is a few MB).
fn insert_pdf_page(path: PathBuf, page: usize, img: Option<Arc<RenderImage>>) {
    PDF_PAGE_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() >= 12 {
            let lo = page.saturating_sub(3);
            let hi = page + 3;
            m.retain(|(p, pg), _| p == &path && (lo..=hi).contains(pg));
        }
        m.insert((path, page), img);
    });
}

/// Rasterize one page of a PDF via AppKit's `NSPDFImageRep`, off the render
/// thread. Returns the page image and the document's page count.
fn render_pdf_page(path: &Path, page: usize) -> Option<(Arc<RenderImage>, usize)> {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let bytes = fs::read(path).ok()?;
    let data = NSData::with_bytes(&bytes);
    let rep = NSPDFImageRep::imageRepWithData(&data)?;
    let count = rep.pageCount().max(1) as usize;
    rep.setCurrentPage(page.min(count - 1) as isize);

    let size = rep.size();
    if size.width < 1.0 || size.height < 1.0 {
        return None;
    }
    // ~800px on the long edge: crisp at the inspector's 288px (incl. retina)
    // without ballooning the page cache.
    let scale = (800.0 / size.width.max(size.height)).clamp(0.5, 4.0);
    let (w, h) = (
        (size.width * scale).round() as isize,
        (size.height * scale).round() as isize,
    );

    let bmp = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bmp)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let dst = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
    let ok = rep.drawInRect(dst);
    NSGraphicsContext::restoreGraphicsState_class();
    if !ok {
        return None;
    }

    let tiff = bmp.TIFFRepresentation()?;
    if tiff.len() == 0 {
        return None;
    }
    let decoded = image::load_from_memory(&tiff.to_vec()).ok()?;
    let rgba = decoded.to_rgba8();
    let (dw, dh) = rgba.dimensions();
    let mut raw = rgba.into_raw();
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA → BGRA
    }
    let buffer = image::RgbaImage::from_raw(dw, dh, raw)?;
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        count,
    ))
}

fn lookup_info(path: &Path) -> Option<FileInfo> {
    INFO_CACHE.with(|c| c.borrow().get(path).cloned())
}

/// Deterministic local temp path for previewing a remote file. Keeps the
/// original file name (so the extension drives QuickLook/PDF rendering) and
/// hashes the full remote path so distinct files never collide.
fn remote_preview_temp(remote: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    remote.to_string_lossy().hash(&mut h);
    let name = remote
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    std::env::temp_dir()
        .join("shuffle-remote-preview")
        .join(format!("{:016x}-{name}", h.finish()))
}

/// Generate a preview image for any file via macOS QuickLook (`qlmanage -t`),
/// then decode it into a GPUI `RenderImage`. Runs off the render thread.
fn build_preview(path: &Path) -> Option<Arc<RenderImage>> {
    let out_dir = std::env::temp_dir().join("shuffle-preview");
    let _ = fs::create_dir_all(&out_dir);
    let name = path.file_name()?.to_string_lossy().into_owned();
    let png = out_dir.join(format!("{name}.png"));
    let _ = fs::remove_file(&png); // avoid showing a stale preview

    // QuickLook renders almost anything (images, PDFs, Office docs, code, …).
    let ok = Command::new("qlmanage")
        .args(["-t", "-s", "600", "-o"])
        .arg(&out_dir)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !png.exists() {
        return None;
    }

    let decoded = image::open(&png).ok()?;
    let decoded = decoded.thumbnail(600, 600); // bound memory; keeps aspect
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut raw = rgba.into_raw();
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA → BGRA
    }
    let buffer = image::RgbaImage::from_raw(w, h, raw)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Everything we display in the Information inspector for one file.
#[derive(Clone)]
struct FileInfo {
    kind: String,
    size: String,
    created: String,
    modified: String,
    accessed: String,
    dimensions: Option<String>,
    color: Option<String>,
    signed: Option<String>,
}

/// Gather file information (cheap calls only; image header read, optional
/// codesign check). Safe to call off the render thread.
fn gather_info(path: &Path) -> FileInfo {
    let md = fs::metadata(path).ok();
    let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let size = md.as_ref().map(|m| format_size(is_dir, m.len())).unwrap_or_else(|| "--".into());
    let created = format_date(md.as_ref().and_then(|m| m.created().ok()));
    let modified = format_date(md.as_ref().and_then(|m| m.modified().ok()));
    let accessed = format_date(md.as_ref().and_then(|m| m.accessed().ok()));

    // Image dimensions + color, both from the header only (no full decode).
    let (mut dimensions, mut color) = (None, None);
    if let Ok((w, h)) = image::image_dimensions(path) {
        dimensions = Some(format!("{w} × {h}"));
        use image::ImageDecoder;
        if let Ok(reader) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
            if let Ok(decoder) = reader.into_decoder() {
                color = Some(color_label(decoder.color_type()));
            }
        }
    }

    // Code signature (apps / binaries). Cheap-ish; only run for plausible items.
    let signed = code_signature(path, is_dir);

    FileInfo {
        kind: kind_label(path.file_name().and_then(|n| n.to_str()).unwrap_or(""), is_dir),
        size,
        created,
        modified,
        accessed,
        dimensions,
        color,
        signed,
    }
}

fn color_label(c: image::ColorType) -> String {
    use image::ColorType::*;
    match c {
        L8 | L16 => "Grayscale",
        La8 | La16 => "Grayscale + Alpha",
        Rgb8 | Rgb16 | Rgb32F => "RGB",
        Rgba8 | Rgba16 | Rgba32F => "RGB + Alpha",
        _ => "Other",
    }
    .to_string()
}

/// Returns a short code-signature status for apps/executables, else `None`.
fn code_signature(path: &Path, is_dir: bool) -> Option<String> {
    let is_app = path.extension().and_then(|e| e.to_str()) == Some("app");
    if !is_app && is_dir {
        return None;
    }
    let out = Command::new("codesign")
        .args(["-dv", "--verbose=2"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return is_app.then(|| "Not signed".to_string());
    }
    let authority = text
        .lines()
        .find_map(|l| l.strip_prefix("Authority="))
        .map(|a| a.to_string());
    Some(authority.unwrap_or_else(|| "Signed".to_string()))
}

/// Format a modification time as a local date/time, or "--" if unknown.
fn format_date(modified: Option<SystemTime>) -> String {
    match modified {
        Some(time) => {
            let dt: DateTime<Local> = time.into();
            dt.format("%b %e, %Y %l:%M %p").to_string()
        }
        None => "--".to_string(),
    }
}

/// Human-readable size; directories show "--".
fn format_size(is_dir: bool, size: u64) -> String {
    if is_dir {
        return "--".to_string();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Read a directory's entries with full metadata (one `stat` per entry), sorted
/// directories-first then case-insensitive by name. This is the slow path; the
/// UI shows `read_entries_fast` first and swaps this in from the background.
fn read_entries(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // One stat, following symlinks: gives is_dir, size, and mtime.
            let md = fs::metadata(entry.path()).ok();
            let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = md.as_ref().and_then(|m| m.modified().ok());
            let created = md.as_ref().and_then(|m| m.created().ok());
            entries.push(Entry {
                name,
                is_dir,
                size,
                modified,
                created,
                loaded: true,
            });
        }
    }
    sort_default(&mut entries);
    entries
}

/// Read a directory's entries cheaply — names + folder/file from the readdir
/// `d_type`, with **no** per-file `stat` (only symlinks are resolved). This is
/// near-instant even for very large folders; size/dates fill in later.
fn read_entries_fast(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_dir() => true,
                // Resolving a symlink needs a stat, but symlinks are rare.
                Ok(t) if t.is_symlink() => entry.path().is_dir(),
                _ => false,
            };
            entries.push(Entry {
                name,
                is_dir,
                size: 0,
                modified: None,
                created: None,
                loaded: false,
            });
        }
    }
    sort_default(&mut entries);
    entries
}

/// The default ordering (folders first, then case-insensitive by name). Uses
/// `sort_by_cached_key` so each name is lowercased once, not on every compare.
fn sort_default(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
}

thread_local! {
    /// Cache of directory listings for Column view, so it isn't re-read from
    /// disk on every frame. Cleared whenever the filesystem might have changed.
    static COL_ENTRIES: RefCell<HashMap<PathBuf, Vec<Entry>>> = RefCell::new(HashMap::new());
}

/// Directory listing for a column (cached). Default sort (folders first, name).
fn column_entries(dir: &Path) -> Vec<Entry> {
    if let Some(v) = COL_ENTRIES.with(|c| c.borrow().get(dir).cloned()) {
        return v;
    }
    // Columns only show name + icon, so the cheap (no-stat) read is enough.
    let v = read_entries_fast(dir);
    COL_ENTRIES.with(|c| c.borrow_mut().insert(dir.to_path_buf(), v.clone()));
    v
}

fn clear_column_cache() {
    COL_ENTRIES.with(|c| c.borrow_mut().clear());
}

/// Sort a directory listing in place by the given criterion/direction. Uses
/// `sort_by_cached_key` for name/kind so strings are lowercased once per item
/// (not on every comparison) — important for large folders.
fn sort_entries(entries: &mut [Entry], key: SortKey, asc: bool) {
    match key {
        SortKey::None => {
            sort_default(entries);
            return;
        }
        SortKey::Name => entries.sort_by_cached_key(|e| e.name.to_lowercase()),
        SortKey::Kind => {
            entries.sort_by_cached_key(|e| {
                (kind_label(&e.name, e.is_dir).to_lowercase(), e.name.to_lowercase())
            });
        }
        SortKey::Modified => entries.sort_by_key(|e| e.modified),
        SortKey::Created => entries.sort_by_key(|e| e.created),
        SortKey::Size => entries.sort_by_key(|e| e.size),
    }
    if !asc {
        entries.reverse();
    }
}

/// Split a path-like query into (base directory, partial trailing name).
/// Handles `~`/`~/` expansion. A trailing `/` means "list this dir" (no partial).
fn split_path_query(q: &str) -> (PathBuf, String) {
    let home = home_dir().to_string_lossy().into_owned();
    let expanded = if q == "~" {
        format!("{home}/")
    } else if let Some(rest) = q.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        q.to_string()
    };

    if expanded.ends_with('/') {
        let base = expanded.trim_end_matches('/');
        let base = if base.is_empty() { "/" } else { base };
        return (PathBuf::from(base), String::new());
    }
    match expanded.rsplit_once('/') {
        Some((base, partial)) => {
            let base = if base.is_empty() { "/" } else { base };
            (PathBuf::from(base), partial.to_string())
        }
        None => (PathBuf::from(expanded), String::new()),
    }
}

/// Lightweight directory listing for the palette: (name, is_dir). Uses the
/// readdir file-type (cheap), only stat-ing symlinks to resolve dir-ness.
fn list_dir_names(dir: &Path) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if let Ok(read_dir) = fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_dir() => true,
                Ok(t) if t.is_symlink() => entry.path().is_dir(),
                _ => false,
            };
            out.push((name, is_dir));
        }
    }
    out
}

/// Character bigrams of a string.
fn bigrams(s: &str) -> Vec<(char, char)> {
    let chars: Vec<char> = s.chars().collect();
    chars.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Sørensen–Dice similarity (0..1) over character bigrams. Tolerant of typos
/// and transpositions (e.g. "dcouments" vs "documents"), unlike subsequence.
fn dice(a: &str, b: &str) -> f32 {
    let ba = bigrams(a);
    let bb = bigrams(b);
    if ba.is_empty() || bb.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }
    let mut counts: HashMap<(char, char), i32> = HashMap::new();
    for g in &bb {
        *counts.entry(*g).or_insert(0) += 1;
    }
    let mut inter = 0;
    for g in &ba {
        if let Some(c) = counts.get_mut(g) {
            if *c > 0 {
                *c -= 1;
                inter += 1;
            }
        }
    }
    2.0 * inter as f32 / (ba.len() + bb.len()) as f32
}

/// Typo-tolerant match score of a partial name against a candidate name.
/// Combines exact/prefix/substring/subsequence signals with Dice similarity.
/// Score one directory entry against the in-directory find query. Returns
/// `None` for non-matches so they're filtered out. Subsequence matches rank
/// highest; close typos (Sørensen–Dice ≥ 0.5) still match so "dcouments"
/// finds "Documents".
// ----- filter query operators (kind: / ext: / size: / date: / content:) -----

/// Broad file-type categories for `kind:` filters.
#[derive(Clone, Copy, PartialEq)]
enum KindClass {
    Folder,
    Image,
    Video,
    Audio,
    Pdf,
    Doc,
    Archive,
    Code,
    App,
}

impl KindClass {
    fn from_word(w: &str) -> Option<KindClass> {
        Some(match w {
            "folder" | "dir" | "directory" | "folders" => KindClass::Folder,
            "image" | "img" | "images" | "picture" | "photo" | "pic" => KindClass::Image,
            "video" | "videos" | "movie" | "movies" | "film" => KindClass::Video,
            "audio" | "music" | "sound" | "song" => KindClass::Audio,
            "pdf" | "pdfs" => KindClass::Pdf,
            "doc" | "docs" | "document" | "documents" | "text" => KindClass::Doc,
            "archive" | "archives" | "zip" | "compressed" => KindClass::Archive,
            "code" | "source" | "src" => KindClass::Code,
            "app" | "apps" | "application" => KindClass::App,
            _ => return None,
        })
    }

    fn matches(self, name: &str, is_dir: bool) -> bool {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        let ext = ext.as_deref();
        match self {
            KindClass::Folder => is_dir,
            KindClass::App => ext == Some("app"),
            // Every remaining category is file-only.
            _ if is_dir => false,
            KindClass::Image => matches!(
                ext,
                Some("jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "tiff" | "tif" | "bmp"
                    | "webp" | "svg" | "ico" | "raw")
            ),
            KindClass::Video => matches!(
                ext,
                Some("mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "mpg" | "mpeg" | "wmv" | "flv")
            ),
            KindClass::Audio => matches!(
                ext,
                Some("mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "aiff" | "aif" | "opus")
            ),
            KindClass::Pdf => ext == Some("pdf"),
            KindClass::Doc => matches!(
                ext,
                Some("doc" | "docx" | "txt" | "md" | "rtf" | "pages" | "key" | "numbers" | "xls"
                    | "xlsx" | "ppt" | "pptx" | "odt" | "csv" | "tsv" | "log")
            ),
            KindClass::Archive => matches!(
                ext,
                Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "xz" | "txz" | "7z" | "rar")
            ),
            KindClass::Code => matches!(
                ext,
                Some("rs" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "cpp" | "cc" | "h"
                    | "hpp" | "java" | "swift" | "zig" | "rb" | "php" | "sh" | "json" | "toml"
                    | "yaml" | "yml" | "html" | "css" | "sql" | "lua" | "kt" | "scala")
            ),
        }
    }
}

/// A parsed size constraint in bytes.
#[derive(Clone, Copy)]
enum SizeBound {
    Gt(u64),
    Lt(u64),
    Range(u64, u64),
}

impl SizeBound {
    fn matches(self, size: u64) -> bool {
        match self {
            SizeBound::Gt(n) => size >= n,
            SizeBound::Lt(n) => size <= n,
            SizeBound::Range(a, b) => size >= a && size <= b,
        }
    }
}

/// Parse a byte count with an optional unit: `500`, `10k`, `2mb`, `1.5g`.
fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("kb").or_else(|| s.strip_suffix('k')) {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1)
    } else {
        (s.as_str(), 1)
    };
    let val: f64 = num.trim().parse().ok()?;
    if val < 0.0 {
        return None;
    }
    Some((val * mult as f64) as u64)
}

/// Parse a `size:` value: `>1mb`, `<100k`, `1mb..5mb`, or a bare `2gb` (exact-ish
/// → treated as "at least").
fn parse_size(s: &str) -> Option<SizeBound> {
    let s = s.trim();
    if let Some((a, b)) = s.split_once("..") {
        return Some(SizeBound::Range(parse_bytes(a)?, parse_bytes(b)?));
    }
    if let Some(rest) = s.strip_prefix(">=").or_else(|| s.strip_prefix('>')) {
        return Some(SizeBound::Gt(parse_bytes(rest)?));
    }
    if let Some(rest) = s.strip_prefix("<=").or_else(|| s.strip_prefix('<')) {
        return Some(SizeBound::Lt(parse_bytes(rest)?));
    }
    Some(SizeBound::Gt(parse_bytes(s)?))
}

/// Local midnight `days` days ago, as a `SystemTime`.
fn day_start_ago(days: i64) -> Option<SystemTime> {
    use chrono::TimeZone;
    let day = (chrono::Local::now() - chrono::Duration::days(days)).date_naive();
    let naive = day.and_hms_opt(0, 0, 0)?;
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    let ts = local.timestamp();
    (ts >= 0).then(|| std::time::UNIX_EPOCH + Duration::from_secs(ts as u64))
}

/// Midnight of an explicit `YYYY-MM-DD`, as a `SystemTime`.
fn date_start(ymd: &str) -> Option<SystemTime> {
    use chrono::TimeZone;
    let day = chrono::NaiveDate::parse_from_str(ymd.trim(), "%Y-%m-%d").ok()?;
    let naive = day.and_hms_opt(0, 0, 0)?;
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    let ts = local.timestamp();
    (ts >= 0).then(|| std::time::UNIX_EPOCH + Duration::from_secs(ts as u64))
}

/// A parsed search query: free text plus any filter operators.
#[derive(Default, Clone)]
struct FilterQuery {
    /// Free text for name matching (operators stripped out).
    text: String,
    kinds: Vec<KindClass>,
    exts: Vec<String>,
    size: Option<SizeBound>,
    after: Option<SystemTime>,
    before: Option<SystemTime>,
    /// `content:`/`text:` term, searched via Spotlight (mdfind).
    content: Option<String>,
}

impl FilterQuery {
    /// Split a raw query into free text + `key:value` operators. Unknown keys
    /// are treated as plain text so a stray colon never eats the query.
    fn parse(q: &str) -> FilterQuery {
        let mut fq = FilterQuery::default();
        let mut text_parts: Vec<&str> = Vec::new();
        for tok in q.split_whitespace() {
            let Some((key, val)) = tok.split_once(':') else {
                text_parts.push(tok);
                continue;
            };
            if val.is_empty() {
                text_parts.push(tok);
                continue;
            }
            match key.to_lowercase().as_str() {
                "kind" | "type" => match KindClass::from_word(&val.to_lowercase()) {
                    Some(k) => fq.kinds.push(k),
                    None => text_parts.push(tok),
                },
                "ext" | "extension" => fq.exts.push(val.trim_start_matches('.').to_lowercase()),
                "size" => match parse_size(val) {
                    Some(s) => fq.size = Some(s),
                    None => text_parts.push(tok),
                },
                "date" | "modified" | "mtime" => {
                    let v = val.to_lowercase();
                    let applied = match v.as_str() {
                        "today" => {
                            fq.after = day_start_ago(0);
                            fq.after.is_some()
                        }
                        "yesterday" => {
                            fq.after = day_start_ago(1);
                            fq.before = day_start_ago(0);
                            fq.after.is_some()
                        }
                        "week" | "thisweek" | "7d" => {
                            fq.after = day_start_ago(7);
                            fq.after.is_some()
                        }
                        "month" | "thismonth" | "30d" => {
                            fq.after = day_start_ago(30);
                            fq.after.is_some()
                        }
                        "year" | "365d" => {
                            fq.after = day_start_ago(365);
                            fq.after.is_some()
                        }
                        _ => {
                            if let Some(d) = v.strip_prefix(">=").or_else(|| v.strip_prefix('>')) {
                                fq.after = date_start(d);
                                fq.after.is_some()
                            } else if let Some(d) =
                                v.strip_prefix("<=").or_else(|| v.strip_prefix('<'))
                            {
                                fq.before = date_start(d);
                                fq.before.is_some()
                            } else {
                                false
                            }
                        }
                    };
                    if !applied {
                        text_parts.push(tok);
                    }
                }
                "content" | "text" | "contains" => fq.content = Some(val.to_string()),
                _ => text_parts.push(tok),
            }
        }
        fq.text = text_parts.join(" ");
        fq
    }

    /// True when any local (non-content) operator is present.
    fn has_local_filters(&self) -> bool {
        !self.kinds.is_empty()
            || !self.exts.is_empty()
            || self.size.is_some()
            || self.after.is_some()
            || self.before.is_some()
    }

    /// Whether an entry passes the local operators (kind/ext/size/date). Content
    /// is handled separately (async, via Spotlight).
    fn matches_entry(&self, name: &str, is_dir: bool, size: u64, modified: Option<SystemTime>) -> bool {
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k.matches(name, is_dir)) {
            return false;
        }
        if !self.exts.is_empty() {
            let ext = Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());
            match ext {
                Some(e) if self.exts.iter().any(|x| x == &e) => {}
                _ => return false,
            }
        }
        if let Some(sb) = self.size {
            if is_dir || !sb.matches(size) {
                return false;
            }
        }
        if self.after.is_some() || self.before.is_some() {
            let Some(m) = modified else { return false };
            if let Some(a) = self.after {
                if m < a {
                    return false;
                }
            }
            if let Some(b) = self.before {
                if m >= b {
                    return false;
                }
            }
        }
        true
    }
}

fn find_score(q: &str, name: &str) -> Option<i32> {
    let ql = q.to_lowercase();
    let nl = name.to_lowercase();
    let penalty = nl.len() as i32 / 4;
    if nl == ql {
        return Some(100_000);
    }
    if nl.starts_with(&ql) {
        return Some(50_000 - penalty);
    }
    if let Some(fs) = fuzzy_score(&ql, &nl) {
        return Some(10_000 + fs - penalty);
    }
    let d = dice(&ql, &nl);
    if d >= 0.5 {
        return Some((d * 5_000.0) as i32 - penalty);
    }
    None
}

/// Built-in app commands whose name matches `q` (prefix or close typo), shown
/// in the palette ahead of file results. Currently just "Settings".
fn command_matches(q: &str) -> Vec<PaletteItem> {
    let ql = q.to_lowercase();
    let mut out = Vec::new();
    let aliases = ["settings", "preferences", "config"];
    let hit = aliases
        .iter()
        .any(|a| a.starts_with(&ql) || dice(&ql, a) >= 0.6);
    if hit {
        out.push(PaletteItem {
            title: "Settings".to_string(),
            subtitle: "Open Shuffle settings".to_string(),
            action: Action::OpenSettings,
            is_dir: false,
        });
    }
    out
}

fn match_score(partial: &str, name: &str) -> i32 {
    let p = partial.to_lowercase();
    let n = name.to_lowercase();
    if p.is_empty() {
        return 0;
    }
    let mut score = 0;
    if n == p {
        score += 10_000;
    }
    if n.starts_with(&p) {
        score += 4_000;
    }
    if n.contains(&p) {
        score += 1_500;
    }
    if let Some(fs) = fuzzy_score(&p, &n) {
        score += 800 + fs;
    }
    score += (dice(&p, &n) * 2_000.0) as i32;
    score -= (n.len() as i32) / 4; // mild preference for shorter names
    score
}

/// Case-insensitive fuzzy (subsequence) score of `needle` against `haystack`.
/// Higher is better; `None` if `needle` isn't a subsequence. Rewards
/// contiguous runs and word-start matches, lightly penalizes length.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let n: Vec<char> = needle.to_lowercase().chars().collect();
    let h: Vec<char> = haystack.to_lowercase().chars().collect();
    if n.is_empty() {
        return Some(0);
    }
    let mut hi = 0usize;
    let mut score = 0i32;
    let mut last_match: i32 = -2;
    for &nc in &n {
        let mut found = None;
        while hi < h.len() {
            if h[hi] == nc {
                found = Some(hi);
                break;
            }
            hi += 1;
        }
        let pos = found?;
        if pos as i32 == last_match + 1 {
            score += 6; // contiguous run
        }
        if pos == 0
            || matches!(h[pos - 1], '/' | ' ' | '_' | '-' | '.')
        {
            score += 10; // start of a word/segment
        }
        score -= (pos as i32) / 4; // earlier matches slightly better
        last_match = pos as i32;
        hi = pos + 1;
    }
    score -= (h.len() as i32) / 8; // prefer shorter names
    Some(score)
}

/// One entry in the in-memory file index.
struct IndexEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

/// A flat, in-memory index of everything under a root directory, used for fast
/// fuzzy search without spawning Spotlight.
struct FileIndex {
    entries: Vec<IndexEntry>,
}

/// Non-hidden directory names we never descend into (huge + irrelevant to file
/// search). Hidden dirs (dotfiles like .bun, .pyenv, .cargo, .git, .Trash) are
/// skipped wholesale via `skip_hidden`, which removes the bulk of the noise.
///
/// The build-output/dependency dirs below are the big win on a developer
/// machine: a single `~/Documents/Projects` can hide hundreds of thousands of
/// generated files under `target`/`build`/`Pods`/etc. Indexing those bloats the
/// walk (slow launch), the memory, and the search results. We skip them the way
/// `ripgrep`/`fd` do by default.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "Library",
    // Build outputs / generated artifacts.
    "target",       // Rust/Java
    "build",        // Gradle/CMake/Xcode/etc.
    "dist",         // JS/Python bundles
    "DerivedData",  // Xcode
    "Pods",         // CocoaPods
    "Carthage",     // Carthage
    "__pycache__",  // Python bytecode
    // Vendored dependencies.
    "vendor",
    "bower_components",
];

impl FileIndex {
    /// Walk `root` in parallel (jwalk), skipping hidden dirs and noise dirs,
    /// into a flat list. Runs off the UI thread.
    fn build(root: PathBuf) -> Self {
        // Leave a couple of cores for the UI thread so the (still fairly large)
        // first-launch walk can't starve rendering and make Cmd+P feel frozen.
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get().saturating_sub(2)).max(2))
            .unwrap_or(2);
        let walker = jwalk::WalkDir::new(&root)
            .skip_hidden(true)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .process_read_dir(|_depth, path, _state, children| {
                // The Go module cache (`~/go/pkg`, hundreds of thousands of files)
                // is the single biggest source of index noise, but `pkg` is a
                // legitimate source-dir name in Go projects — so only skip it
                // when its parent directory is literally `go`.
                let parent_is_go = path.file_name().is_some_and(|n| n == "go");
                children.retain(|res| match res {
                    Ok(e) => {
                        if e.file_type().is_dir() {
                            let name = e.file_name();
                            let name = name.to_string_lossy();
                            if SKIP_DIRS.contains(&name.as_ref()) {
                                return false;
                            }
                            if parent_is_go && name == "pkg" {
                                return false;
                            }
                            true
                        } else {
                            true
                        }
                    }
                    Err(_) => false,
                });
            });

        let mut entries = Vec::new();
        for entry in walker {
            let Ok(e) = entry else { continue };
            if e.depth() == 0 {
                continue; // the root itself
            }
            let is_dir = e.file_type().is_dir();
            let name = e.file_name().to_string_lossy().into_owned();
            entries.push(IndexEntry {
                name,
                path: e.path(),
                is_dir,
            });
        }
        FileIndex { entries }
    }

    /// Fuzzy-rank the index against `query` in parallel; return the top `limit`.
    fn search(&self, query: &str, limit: usize) -> Vec<(String, PathBuf, bool)> {
        let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
        if q.is_empty() {
            return Vec::new();
        }
        let q_str: String = q.iter().collect();
        let q_bigrams: Vec<(char, char)> = q.windows(2).map(|w| (w[0], w[1])).collect();
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .par_iter()
            .enumerate()
            .filter_map(|(i, e)| {
                rank_entry(&q, &q_str, &q_bigrams, &e.name, &e.path, e.is_dir).map(|s| (s, i))
            })
            .collect();
        scored.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| self.entries[a.1].name.len().cmp(&self.entries[b.1].name.len()))
        });
        scored.truncate(limit);
        scored
            .into_iter()
            .map(|(_, i)| {
                let e = &self.entries[i];
                (e.name.clone(), e.path.clone(), e.is_dir)
            })
            .collect()
    }
}

fn is_word_boundary(c: char) -> bool {
    matches!(c, '/' | ' ' | '_' | '-' | '.')
}

/// Allocation-free Sørensen–Dice over character bigrams: query bigrams are
/// pre-computed (lowercased); the name is lowercased on the fly. Tolerant of
/// typos/transpositions (e.g. "dcouments" vs "documents"). Fast enough to run
/// on every index entry that fails the subsequence test.
fn name_bigram_dice(q_bigrams: &[(char, char)], name: &str) -> f32 {
    let qn = q_bigrams.len();
    if qn == 0 {
        return 0.0;
    }
    let cap = qn.min(64);
    let mut used = [false; 64];
    let mut inter = 0usize;
    let mut name_bigrams = 0usize;
    let mut prev: Option<char> = None;
    for ch in name.chars() {
        let lc = ch.to_ascii_lowercase();
        if let Some(p) = prev {
            name_bigrams += 1;
            for i in 0..cap {
                if !used[i] && q_bigrams[i] == (p, lc) {
                    used[i] = true;
                    inter += 1;
                    break;
                }
            }
        }
        prev = Some(lc);
    }
    if name_bigrams == 0 {
        return 0.0;
    }
    2.0 * inter as f32 / (qn + name_bigrams) as f32
}

/// Rank one candidate: prefer a real subsequence match (`score_entry`); if it
/// isn't a subsequence, fall back to typo-tolerant bigram similarity so
/// transposed/misspelled queries still surface the right file.
fn rank_entry(
    q: &[char],
    q_str: &str,
    q_bigrams: &[(char, char)],
    name: &str,
    path: &Path,
    is_dir: bool,
) -> Option<i32> {
    if let Some(s) = score_entry(q, q_str, name, path, is_dir) {
        return Some(s);
    }
    let sim = name_bigram_dice(q_bigrams, name);
    if sim < 0.5 {
        return None;
    }
    let mut score = (sim * 1500.0) as i32;
    let name_len = name.chars().count() as i32;
    score -= (name_len - q.len() as i32).max(0) * 4; // coverage
    score -= path.components().count() as i32 * 8; // depth
    if is_dir {
        score += 40;
    }
    Some(score)
}

/// Full ranking of one candidate: subsequence gate + strong exact/prefix
/// bonuses, a coverage penalty (favor names close to the query length), and a
/// path-depth penalty (shallower = closer to home = ranked higher). Shared by
/// the in-memory index and the Spotlight fallback so ranking is consistent.
/// `q_str` is the lowercased query string; `q` its chars.
fn score_entry(q: &[char], q_str: &str, name: &str, path: &Path, is_dir: bool) -> Option<i32> {
    let mut score = index_score(q, name)?;
    let name_lc = name.to_lowercase();

    if name_lc == q_str {
        score += 100_000; // exact name match — always wins
    } else {
        let stem = name_lc.rsplit_once('.').map(|(s, _)| s).unwrap_or(&name_lc);
        if stem == q_str {
            score += 60_000; // name without extension matches
        }
    }
    if name_lc.starts_with(q_str) {
        score += 5_000;
    } else if name_lc.contains(q_str) {
        score += 1_500;
    }

    // Coverage: the closer the name's length is to the query, the better
    // ("Documents" beats "DocumentSymbolProvider.js" for "documents").
    let name_len = name.chars().count() as i32;
    score -= (name_len - q.len() as i32).max(0) * 4;
    // Depth: shallower paths (closer to home) rank higher.
    score -= path.components().count() as i32 * 8;
    if is_dir {
        score += 40;
    }
    Some(score)
}

/// Allocation-free subsequence fuzzy score (fzf-style). `query` is pre-lowercased.
/// Returns `None` if `query` isn't a subsequence of `name`. Rewards contiguous
/// runs and word-start matches; lightly penalizes length. Built for speed —
/// called on every index entry, every keystroke.
fn index_score(query: &[char], name: &str) -> Option<i32> {
    let mut qi = 0;
    let mut score = 0i32;
    let mut last: i32 = -2;
    let mut idx: i32 = 0;
    let mut prev = '/'; // treat string start as a boundary
    for ch in name.chars() {
        if qi >= query.len() {
            break;
        }
        if ch.to_ascii_lowercase() == query[qi] {
            if idx == last + 1 {
                score += 6;
            }
            if idx == 0 || is_word_boundary(prev) {
                score += 10;
            }
            score -= idx / 4;
            last = idx;
            qi += 1;
        }
        prev = ch;
        idx += 1;
    }
    if qi == query.len() {
        Some(score - idx / 8)
    } else {
        None
    }
}

/// Spotlight-backed name search: gather candidates with `mdfind`, then fuzzy
/// rank by filename. Used as a fallback while the in-memory index is building.
/// Full-text search inside `dir` via Spotlight: paths whose content matches
/// `term`. Scoped with `-onlyin` so it's fast and folder-local. Returns the
/// absolute paths (the in-folder filter keeps only direct children of these).
fn mdfind_content(dir: &Path, term: &str) -> HashSet<PathBuf> {
    let term = term.trim();
    let mut out = HashSet::new();
    if term.is_empty() {
        return out;
    }
    // Case- and diacritic-insensitive substring match on indexed text content.
    let query = format!("kMDItemTextContent == \"*{}*\"cd", term.replace('"', ""));
    let output = Command::new("mdfind")
        .arg("-onlyin")
        .arg(dir)
        .arg(query)
        .stderr(Stdio::null())
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if !line.is_empty() {
                    out.insert(PathBuf::from(line));
                }
            }
        }
    }
    out
}

/// Operator-driven global search for the command palette: content:/kind:/ext:/
/// size:/date: over the home index (or Spotlight for content), returning up to
/// 40 palette items. Runs off the UI thread.
///
/// Staged to keep metadata I/O bounded: cheap predicates (kind/ext + name)
/// narrow the candidate set first; only the surviving top slice is `stat`ed for
/// size/date, so a `kind:pdf` over a 100k-entry index never stats everything.
fn palette_operator_search(
    fq: &FilterQuery,
    index: Option<&FileIndex>,
    home: &Path,
) -> Vec<PaletteItem> {
    // 1. Raw candidates: Spotlight content hits, else the whole name index.
    let mut cands: Vec<(String, PathBuf, bool)> = Vec::new();
    if let Some(term) = &fq.content {
        for p in mdfind_content(home, term).into_iter().take(4000) {
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let is_dir = p.is_dir();
            cands.push((name, p, is_dir));
        }
    } else if let Some(idx) = index {
        cands.reserve(idx.entries.len());
        for e in &idx.entries {
            cands.push((e.name.clone(), e.path.clone(), e.is_dir));
        }
    }

    // 2. Cheap filter (kind/ext) + optional name ranking — no disk I/O.
    let has_text = !fq.text.is_empty();
    let mut scored: Vec<(i32, String, PathBuf, bool)> = Vec::new();
    for (name, path, is_dir) in cands {
        if !fq.kinds.is_empty() && !fq.kinds.iter().any(|k| k.matches(&name, is_dir)) {
            continue;
        }
        if !fq.exts.is_empty() {
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());
            match ext {
                Some(e) if fq.exts.iter().any(|x| x == &e) => {}
                _ => continue,
            }
        }
        let score = if has_text {
            match find_score(&fq.text, &name) {
                Some(s) => s,
                None => continue,
            }
        } else {
            0
        };
        scored.push((score, name, path, is_dir));
    }

    // 3. Rank, then cap before the (potentially) stat-heavy stage.
    if has_text {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    } else {
        scored.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    }
    scored.truncate(400);

    // 4. Apply size/date (needs metadata) only to the survivors.
    let need_meta = fq.size.is_some() || fq.after.is_some() || fq.before.is_some();
    let mut out = Vec::new();
    for (_, name, path, is_dir) in scored {
        if need_meta {
            let md = fs::metadata(&path).ok();
            let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = md.as_ref().and_then(|m| m.modified().ok());
            if !fq.matches_entry(&name, is_dir, size, mtime) {
                continue;
            }
        }
        out.push(PaletteItem {
            title: name,
            subtitle: display_path(&path),
            action: Action::Open(path, is_dir),
            is_dir,
        });
        if out.len() >= 40 {
            break;
        }
    }
    out
}

fn search_filesystem(query: &str) -> Vec<(String, PathBuf, bool)> {
    let mut child = match Command::new("mdfind")
        .arg("-name")
        .arg(query)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Vec::new();
    };

    // Same ranking as the in-memory index (is_dir unknown pre-stat → false; the
    // exact/prefix/coverage/depth signals still rank "Documents" correctly).
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let q_str: String = q.iter().collect();
    let mut scored: Vec<(i32, String, PathBuf)> = Vec::new();
    // Cap how much we read so a broad query can't stall us.
    for line in BufReader::new(stdout).lines().take(4000) {
        let Ok(line) = line else { continue };
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(&line);
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if let Some(score) = score_entry(&q, &q_str, &name, &path, false) {
            scored.push((score, name, path));
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(40);
    scored
        .into_iter()
        .map(|(_, name, path)| {
            let is_dir = path.is_dir();
            (name, path, is_dir)
        })
        .collect()
}

/// A short, human label for a path (last component; "/" → "Macintosh HD").
fn path_label(p: &Path) -> String {
    if p == Path::new("/") {
        return "Macintosh HD".to_string();
    }
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

// ----- persisted state -------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/Shuffle"))
}

fn config_file(name: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(name))
}

/// The directory to open on launch: the last-visited one if still valid,
/// otherwise the home directory.
fn load_last_dir() -> PathBuf {
    if let Some(path) = config_file("last_dir.txt") {
        if let Ok(saved) = fs::read_to_string(&path) {
            let dir = PathBuf::from(saved.trim());
            if dir.is_dir() {
                return dir;
            }
        }
    }
    home_dir()
}

fn save_last_dir(dir: &Path) {
    if let Some(path) = config_file("last_dir.txt") {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, dir.to_string_lossy().as_bytes());
    }
}

/// Read a newline-separated list of paths, keeping only ones that still exist.
fn read_path_list(name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(file) = config_file(name) {
        if let Ok(contents) = fs::read_to_string(&file) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let path = PathBuf::from(line);
                // Keep files too (bookmarks can be files); drop only stale paths.
                if path.exists() {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// Read a newline-separated list of arbitrary strings (e.g. server URLs).
fn read_string_list(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(file) = config_file(name) {
        if let Ok(contents) = fs::read_to_string(&file) {
            for line in contents.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    out.push(line.to_string());
                }
            }
        }
    }
    out
}

fn write_string_list(name: &str, items: &[String]) {
    if let Some(file) = config_file(name) {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&file, items.join("\n"));
    }
}

/// Read a single-line config value (trimmed), or `None` if absent/empty.
fn config_string(name: &str) -> Option<String> {
    let file = config_file(name)?;
    let s = fs::read_to_string(&file).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Write a single-line config value.
fn write_config_string(name: &str, value: &str) {
    if let Some(file) = config_file(name) {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&file, value.as_bytes());
    }
}

// --- Update check (GitHub "latest release" → dismissible banner) ---

/// The GitHub repo whose latest release drives the update banner.
const GITHUB_REPO: &str = "WizenPainter/shuffle";
/// The stable "always the newest DMG" download link (same one the website uses).
const DMG_URL: &str = "https://github.com/WizenPainter/shuffle/releases/latest/download/Shuffle.dmg";

/// Parse a version like "v0.2.3" / "0.2.3" into numeric components for
/// comparison. `Vec<u32>` compares lexicographically, so [0,2,4] > [0,2,3].
/// Non-numeric junk in a component parses as 0 rather than failing.
fn parse_version(s: &str) -> Vec<u32> {
    s.trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Pull a top-level string field out of a JSON blob without a JSON dependency:
/// find `"key"`, then the first `"..."` after the following `:`. Good enough for
/// GitHub's `tag_name`; not a general JSON parser.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let after_key = &body[body.find(&pat)? + pat.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let open = after_colon.find('"')? + 1;
    let rest = &after_colon[open..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// Ask GitHub for the newest release tag (like "v0.2.6"). Blocking network
/// call — run it off the render thread.
fn fetch_latest_tag() -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "8",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest"),
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    json_string_field(&body, "tag_name").map(|t| t.trim().to_string())
}

/// The Apple Developer Team that legitimately signs Shuffle. The self-updater
/// refuses to install a download signed by anyone else, so a tampered or
/// spoofed DMG can never replace the app.
const EXPECTED_TEAM_ID: &str = "Z69U4AQSH3";

/// A downloaded, mounted, and verified update, ready to be swapped in.
struct PreparedUpdate {
    dmg: PathBuf,
    mount: PathBuf,
    app: PathBuf,
}

impl PreparedUpdate {
    /// Unmount the update DMG (used on the error path).
    fn detach(&self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-quiet"])
            .arg(&self.mount)
            .status();
    }
}

/// The `.app` bundle the running process lives in (…/Shuffle.app), if it can be
/// located from the executable path.
fn current_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|a| a.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// Download the latest DMG, mount it, and verify the app inside is notarized and
/// signed by our Team ID. Returns the mounted, verified update or a short reason
/// on failure. Runs entirely off the UI thread.
fn download_and_verify_update() -> Result<PreparedUpdate, String> {
    let dir = std::env::temp_dir().join("shuffle-update");
    let _ = fs::create_dir_all(&dir);
    let dmg = dir.join("Shuffle-latest.dmg");

    // Download (follow redirects, fail on HTTP errors, cap the time).
    let ok = Command::new("curl")
        .args(["-sL", "--fail", "--max-time", "180", "-o"])
        .arg(&dmg)
        .arg(DMG_URL)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("couldn't download the update".to_string());
    }

    // Mount it read-only.
    let out = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noverify"])
        .arg(&dmg)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("couldn't mount the update".to_string());
    }
    let mount = parse_hdiutil_mount(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "couldn't find the mounted volume".to_string())?;

    let app = mount.join("Shuffle.app");
    let prepared = PreparedUpdate { dmg, mount, app: app.clone() };
    if !app.exists() {
        prepared.detach();
        return Err("the update didn't contain Shuffle.app".to_string());
    }
    // Only install a download that is genuinely, verifiably ours.
    if let Err(e) = verify_app(&app) {
        prepared.detach();
        return Err(e);
    }
    Ok(prepared)
}

/// Confirm `app` has a valid signature, passes Gatekeeper (i.e. is notarized),
/// and is signed by our Team ID. Any of these failing aborts the update.
fn verify_app(app: &Path) -> Result<(), String> {
    let sig_ok = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !sig_ok {
        return Err("the update's signature was invalid".to_string());
    }

    let gatekeeper_ok = Command::new("spctl")
        .args(["-a", "-t", "exec", "-vv"])
        .arg(app)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !gatekeeper_ok {
        return Err("the update wasn't notarized by Apple".to_string());
    }

    // codesign prints the signing info to stderr.
    let info = Command::new("codesign")
        .args(["-dv", "--verbose=4"])
        .arg(app)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&info.stderr);
    let team_matches = text
        .lines()
        .any(|l| l.trim() == format!("TeamIdentifier={EXPECTED_TEAM_ID}"));
    if !team_matches {
        return Err("the update was signed by an unexpected developer".to_string());
    }
    Ok(())
}

/// Parse `hdiutil attach` output for the `/Volumes/…` mount point (the volume
/// name may contain spaces, so take everything from `/Volumes/` to line end).
fn parse_hdiutil_mount(output: &str) -> Option<PathBuf> {
    output.lines().find_map(|line| {
        line.find("/Volumes/")
            .map(|i| PathBuf::from(line[i..].trim_end()))
    })
}

/// Write and launch a detached helper that waits for this process to quit, then
/// replaces the installed bundle with the verified update and relaunches it. If
/// the replace fails (e.g. no write permission), it falls back to opening the
/// DMG so the user can install by hand. We must do this from a separate process
/// because a running app can't overwrite its own executable.
fn launch_swap_and_relaunch(bundle: &Path, ready: &PreparedUpdate) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let pid = std::process::id();
    let script = std::env::temp_dir().join("shuffle-selfupdate.sh");
    // $1 DEST bundle  $2 SRC app  $3 MOUNT  $4 DMG  $5 PID  $6 fallback URL
    // The swap NEVER deletes the old bundle — it renames it into the Trash
    // (Sparkle-style). Hard-deleting an app reads as an uninstall to macOS,
    // which revokes its TCC privacy grants and makes the updated app re-ask
    // for every folder permission. A rename keeps the identity alive, so
    // grants carry over. Both renames are atomic on the same volume.
    let body = r#"#!/bin/bash
DEST="$1"; SRC="$2"; MOUNT="$3"; DMG="$4"; PID="$5"; URL="$6"
# Wait (up to ~20s) for the old app to exit before touching its bundle.
for _ in $(seq 1 200); do kill -0 "$PID" 2>/dev/null || break; sleep 0.1; done
TRASH="$HOME/.Trash"
OLD="$TRASH/Shuffle-old-$$.app"
ok=""
if ditto "$SRC" "$DEST.updnew" 2>/dev/null; then
  # Defensive: a quarantined copy would launch translocated (and re-prompt).
  xattr -dr com.apple.quarantine "$DEST.updnew" 2>/dev/null
  # Move the old app aside (Trash first, sibling rename as fallback)…
  if ! mv "$DEST" "$OLD" 2>/dev/null; then
    OLD="${DEST%.app}.updold.app"
    mv "$DEST" "$OLD" 2>/dev/null
  fi
  # …then the new one into place; roll the old back if that fails.
  if [ ! -e "$DEST" ] && mv "$DEST.updnew" "$DEST" 2>/dev/null; then
    ok=1
  else
    [ -d "$OLD" ] && [ ! -e "$DEST" ] && mv "$OLD" "$DEST" 2>/dev/null
  fi
fi
if [ -n "$ok" ]; then
  hdiutil detach "$MOUNT" -quiet 2>/dev/null
  rm -f "$DMG" 2>/dev/null
  # If the Trash move had failed, try once more now; never hard-delete it.
  case "$OLD" in
    "$TRASH"/*) : ;;
    *) mv "$OLD" "$TRASH/Shuffle-old-$$.app" 2>/dev/null ;;
  esac
  open "$DEST"
else
  rm -rf "$DEST.updnew" 2>/dev/null
  hdiutil detach "$MOUNT" -quiet 2>/dev/null
  open "$DMG" 2>/dev/null || open "$URL" 2>/dev/null
fi
"#;
    fs::write(&script, body).map_err(|e| e.to_string())?;
    let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o755));

    Command::new("/bin/bash")
        .arg(&script)
        .arg(bundle)
        .arg(&ready.app)
        .arg(&ready.mount)
        .arg(&ready.dmg)
        .arg(pid.to_string())
        .arg(DMG_URL)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Load sidebar groups from `groups.txt`. A `[name]` line starts a group; the
/// lines after it are member paths, until the next `[name]`.
fn load_groups() -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    if let Some(file) = config_file("groups.txt") {
        if let Ok(contents) = fs::read_to_string(&file) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    groups.push(Group { name: name.to_string(), paths: Vec::new() });
                } else if let Some(g) = groups.last_mut() {
                    g.paths.push(PathBuf::from(line));
                }
            }
        }
    }
    groups
}

/// Persist sidebar groups to `groups.txt`.
fn save_groups(groups: &[Group]) {
    if let Some(file) = config_file("groups.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut body = String::new();
        for g in groups {
            body.push('[');
            body.push_str(&g.name);
            body.push_str("]\n");
            for p in &g.paths {
                body.push_str(&p.to_string_lossy());
                body.push('\n');
            }
        }
        let _ = fs::write(&file, body);
    }
}

fn write_path_list(name: &str, paths: &[PathBuf]) {
    if let Some(file) = config_file(name) {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let _ = fs::write(&file, body.join("\n"));
    }
}

/// Persist the active theme (all eleven colors as hex, one per line).
fn save_theme(t: &Theme) {
    if let Some(file) = config_file("theme.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let v = [
            t.bg, t.sidebar, t.surface, t.hover, t.selected, t.border, t.border_strong, t.text,
            t.text_muted, t.text_dim, t.accent,
        ];
        let body: String = v.iter().map(|c| format!("{c:06x}\n")).collect();
        let _ = fs::write(&file, body);
    }
}

/// Load the saved theme, falling back to the default if absent/corrupt.
fn load_theme() -> Theme {
    if let Some(file) = config_file("theme.txt") {
        if let Ok(s) = fs::read_to_string(&file) {
            let v: Vec<u32> = s
                .lines()
                .filter_map(|l| u32::from_str_radix(l.trim(), 16).ok())
                .collect();
            if v.len() == 11 {
                return Theme {
                    bg: v[0],
                    sidebar: v[1],
                    surface: v[2],
                    hover: v[3],
                    selected: v[4],
                    border: v[5],
                    border_strong: v[6],
                    text: v[7],
                    text_muted: v[8],
                    text_dim: v[9],
                    accent: v[10],
                };
            }
        }
    }
    Theme::default()
}

/// Persist the menu style.
fn save_menu_style(m: &MenuStyle) {
    if let Some(file) = config_file("menu.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = format!(
            "bg={:06x}\ntext={:06x}\nopacity={}\nfont_px={}\ncustom={}\n",
            m.bg, m.text, m.opacity, m.font_px, m.custom
        );
        let _ = fs::write(&file, body);
    }
}

/// Load the saved menu style, falling back to defaults for missing fields.
fn load_menu_style() -> MenuStyle {
    let mut m = MenuStyle::default();
    if let Some(file) = config_file("menu.txt") {
        if let Ok(s) = fs::read_to_string(&file) {
            for line in s.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "bg" => {
                        if let Ok(c) = u32::from_str_radix(v, 16) {
                            m.bg = c;
                        }
                    }
                    "text" => {
                        if let Ok(c) = u32::from_str_radix(v, 16) {
                            m.text = c;
                        }
                    }
                    "opacity" => {
                        if let Ok(n) = v.parse::<u8>() {
                            m.opacity = n.min(100);
                        }
                    }
                    "font_px" => {
                        if let Ok(n) = v.parse::<f32>() {
                            m.font_px = n.clamp(9.0, 24.0);
                        }
                    }
                    "custom" => m.custom = v == "true",
                    _ => {}
                }
            }
        }
    }
    m
}

/// Persist feature prefs as `key=bool` lines.
fn save_prefs(p: &Prefs) {
    if let Some(file) = config_file("prefs.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = format!(
            "terminal={}\nterm_history={}\npreview={}\npreview_pages={}\ninfo={}\nshow_parent={}\nsidebar_collapsed={}\nrecent_limit={}\npalette_history={}\ngroups_enabled={}\nshow_filter_button={}\nshow_fps={}\nscript_actions={}\nssh_use_system={}\nssh_configured={}\nwaterfall={}\npalette_opacity={}\n",
            p.terminal, p.term_history, p.preview, p.preview_pages, p.info, p.show_parent, p.sidebar_collapsed, p.recent_limit, p.palette_history, p.groups_enabled, p.show_filter_button, p.show_fps, p.script_actions, p.ssh_use_system, p.ssh_configured, p.waterfall, p.palette_opacity
        );
        let _ = fs::write(&file, body);
    }
}

/// Persist the keymap as `action=keystroke` lines (empty = unbound).
fn save_keymap(k: &Keymap) {
    if let Some(file) = config_file("keymap.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body: String = KeyAction::ALL
            .iter()
            .map(|a| format!("{}={}\n", a.key(), k.get(*a).unwrap_or("")))
            .collect();
        let _ = fs::write(&file, body);
    }
}

/// Persist the active icon-pack folder (empty = none).
fn save_icon_pack(p: &Option<PathBuf>) {
    if let Some(file) = config_file("icon_pack.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = p.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = fs::write(&file, body);
    }
}

/// Persist the app-icon background choice.
fn save_icon_bg(bg: &IconBg) {
    if let Some(file) = config_file("app_icon.txt") {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = match bg {
            IconBg::Default => String::new(),
            IconBg::Color(c) => format!("color={c:06x}"),
            IconBg::Image(p) => format!("image={}", p.to_string_lossy()),
        };
        let _ = fs::write(&file, body);
    }
}

/// Load the saved app-icon background choice.
fn load_icon_bg() -> IconBg {
    let Some(file) = config_file("app_icon.txt") else {
        return IconBg::Default;
    };
    let Ok(s) = fs::read_to_string(&file) else {
        return IconBg::Default;
    };
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("color=") {
        if let Ok(c) = u32::from_str_radix(hex.trim(), 16) {
            return IconBg::Color(c);
        }
    } else if let Some(path) = s.strip_prefix("image=") {
        let p = PathBuf::from(path.trim());
        if p.is_file() {
            return IconBg::Image(p);
        }
    }
    IconBg::Default
}

/// Copy an uploaded background image into the config dir so it persists, and
/// return the stored path.
fn store_icon_bg_image(src: &Path) -> Option<PathBuf> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let dest = config_file(&format!("app_icon_bg.{ext}"))?;
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::copy(src, &dest).ok()?;
    Some(dest)
}

/// Load the saved icon-pack folder, if it still exists.
fn load_icon_pack() -> Option<PathBuf> {
    let file = config_file("icon_pack.txt")?;
    let s = fs::read_to_string(&file).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    p.is_dir().then_some(p)
}

/// Load the keymap, starting from defaults and applying saved overrides.
fn load_keymap() -> Keymap {
    let mut k = Keymap::defaults();
    if let Some(file) = config_file("keymap.txt") {
        if let Ok(s) = fs::read_to_string(&file) {
            for line in s.lines() {
                let Some((name, val)) = line.split_once('=') else {
                    continue;
                };
                if let Some(a) = KeyAction::ALL.iter().copied().find(|a| a.key() == name.trim()) {
                    let v = val.trim();
                    k.set(a, if v.is_empty() { None } else { Some(v.to_string()) });
                }
            }
        }
    }
    k
}

/// Load feature prefs, defaulting everything to off.
fn load_prefs() -> Prefs {
    let mut p = Prefs::default();
    if let Some(file) = config_file("prefs.txt") {
        if let Ok(s) = fs::read_to_string(&file) {
            for line in s.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let on = v.trim() == "true";
                match k.trim() {
                    "terminal" => p.terminal = on,
                    "term_history" => p.term_history = on,
                    "preview" => p.preview = on,
                    "preview_pages" => p.preview_pages = on,
                    "info" => p.info = on,
                    "show_parent" => p.show_parent = on,
                    "sidebar_collapsed" => p.sidebar_collapsed = on,
                    "recent_limit" => {
                        if let Ok(n) = v.trim().parse::<usize>() {
                            p.recent_limit = n.min(RECENTS_CAP);
                        }
                    }
                    "palette_history" => p.palette_history = on,
                    "groups_enabled" => p.groups_enabled = on,
                    "show_filter_button" => p.show_filter_button = on,
                    "show_fps" => p.show_fps = on,
                    "script_actions" => p.script_actions = on,
                    "ssh_use_system" => p.ssh_use_system = on,
                    "ssh_configured" => p.ssh_configured = on,
                    "waterfall" => p.waterfall = on,
                    "palette_opacity" => {
                        if let Ok(n) = v.trim().parse::<u8>() {
                            p.palette_opacity = n.clamp(20, 100);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    p
}

fn main() {
    // Hidden benchmark mode: `shuffle --index-bench <query>` builds the ~/ index
    // and runs a search, printing timings and the top hits, then exits.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--index-bench" {
        let t0 = std::time::Instant::now();
        let index = FileIndex::build(home_dir());
        eprintln!(
            "index: {} entries built in {} ms",
            index.entries.len(),
            t0.elapsed().as_millis()
        );
        let t1 = std::time::Instant::now();
        let hits = index.search(&args[2], 10);
        eprintln!(
            "search {:?}: {} hits in {} µs",
            args[2],
            hits.len(),
            t1.elapsed().as_micros()
        );
        for (name, path, is_dir) in hits {
            eprintln!(
                "  {} {:<28} {}",
                if is_dir { "DIR " } else { "file" },
                name,
                path.display()
            );
        }
        return;
    }
    // Hidden check: `shuffle --pdf-bench <file> [page]` renders one PDF page
    // through the inspector-pager pipeline and prints the result, then exits.
    if args.len() >= 3 && args[1] == "--pdf-bench" {
        let path = PathBuf::from(&args[2]);
        let page: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let t0 = std::time::Instant::now();
        match render_pdf_page(&path, page) {
            Some((_, count)) => eprintln!(
                "ok: rendered page {} of {} in {} ms",
                page + 1,
                count,
                t0.elapsed().as_millis()
            ),
            None => eprintln!("failed to render {}", path.display()),
        }
        return;
    }

    // Scaffold + discover script actions, and test matching/execution.
    // Parse a canned sftp `ls -l` block (arg 2 = file), or live-list a server.
    if args.len() >= 2 && args[1] == "--sftp-parse-test" {
        let sample = "drwxr-xr-x    5 1000     1000         4096 Jul 30 10:00 my folder\n\
                      -rw-r--r--    1 1000     1000       123456 Jul 29 09:00 report.pdf\n\
                      lrwxrwxrwx    1 1000     1000           11 Jan  1  2025 link -> target.txt\n\
                      drwxr-xr-x    2 1000     1000         4096 Jul 30 10:00 .\n\
                      drwxr-xr-x    9 1000     1000         4096 Jul 30 10:00 ..\n\
                      sftp> ls -la /home/user";
        for e in parse_sftp_ls(sample) {
            eprintln!("  {:<14} dir={} size={}", e.name, e.is_dir, e.size);
        }
        return;
    }
    if args.len() >= 3 && args[1] == "--sftp-list" {
        // --sftp-list <[user@]host[:port]> [path] [password]
        let raw = args[2].clone();
        let (user, hostport) = raw.split_once('@').map_or(("", raw.as_str()), |(u, h)| (u, h));
        let (host, port) = hostport
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
            .map(|(h, p)| (h.to_string(), p.parse().unwrap_or(0)))
            .unwrap_or((hostport.to_string(), 0));
        let password = args.get(4).cloned();
        let srv = SftpServer {
            name: host.clone(),
            host,
            user: user.to_string(),
            port,
            key: String::new(),
            use_password: password.is_some(),
            auto_reopen: false,
        };
        if let Some(pw) = &password {
            keychain_set_password(&srv, pw);
            eprintln!("(password mode: stored in Keychain, using expect+ControlMaster)");
        }
        let path = args.get(3).cloned().unwrap_or_else(|| ".".into());
        match sftp_home(&srv, true) {
            Ok(h) => eprintln!("home: {h}"),
            Err(e) => eprintln!("home error: {e}"),
        }
        match sftp_list(&srv, &path, true) {
            Ok(es) => {
                eprintln!("{} entries:", es.len());
                for e in es.iter().take(20) {
                    eprintln!("  {:<30} dir={} size={}", e.name, e.is_dir, e.size);
                }
            }
            Err(e) => eprintln!("list error: {e}"),
        }
        return;
    }

    if args.len() >= 4 && args[1] == "--sftp-preview-test" {
        // --sftp-preview-test <[user@]host[:port]> <remote_path> [password]
        // Exercises the remote-preview pipeline headlessly: fetch → QuickLook.
        let raw = args[2].clone();
        let (user, hostport) = raw.split_once('@').map_or(("", raw.as_str()), |(u, h)| (u, h));
        let (host, port) = hostport
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
            .map(|(h, p)| (h.to_string(), p.parse().unwrap_or(0)))
            .unwrap_or((hostport.to_string(), 0));
        let password = args.get(4).cloned();
        let srv = SftpServer {
            name: host.clone(),
            host,
            user: user.to_string(),
            port,
            key: String::new(),
            use_password: password.is_some(),
            auto_reopen: false,
        };
        if let Some(pw) = &password {
            keychain_set_password(&srv, pw);
        }
        let remote = PathBuf::from(&args[3]);
        let local = remote_preview_temp(&remote);
        if let Some(parent) = local.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let r = remote.to_string_lossy().replace('"', "");
        let l = local.to_string_lossy().replace('"', "");
        eprintln!("fetching {r} -> {l}");
        match sftp_batch(&srv, &format!("get \"{r}\" \"{l}\""), true) {
            Ok(_) => {
                let bytes = fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
                eprintln!("downloaded {bytes} bytes");
                match build_preview(&local) {
                    Some(img) => {
                        let sz = img.size(0);
                        eprintln!("build_preview OK: {}x{}", sz.width.0, sz.height.0);
                    }
                    None => eprintln!("build_preview FAILED"),
                }
                if is_pdf(&remote) {
                    match render_pdf_page(&local, 0) {
                        Some((img, count)) => {
                            let sz = img.size(0);
                            eprintln!("render_pdf_page OK: {}x{}, {count} page(s)", sz.width.0, sz.height.0);
                        }
                        None => eprintln!("render_pdf_page FAILED"),
                    }
                }
            }
            Err(e) => eprintln!("download error: {e}"),
        }
        return;
    }

    if args.len() >= 3 && args[1] == "--cloud-test" {
        use std::os::macos::fs::MetadataExt;
        let p = PathBuf::from(&args[2]);
        match fs::metadata(&p) {
            Ok(m) => {
                let flags = m.st_flags();
                eprintln!("st_flags = 0x{flags:08X}");
                eprintln!("SF_DATALESS set = {}", flags & SF_DATALESS != 0);
                eprintln!("is_dataless() = {}", is_dataless(&p));
                eprintln!("cloud_kind = {:?}", cloud_kind(&p).map(|k| match k {
                    CloudKind::ICloud => "iCloud",
                    CloudKind::Provider => "Provider",
                }));
            }
            Err(e) => eprintln!("stat error: {e}"),
        }
        return;
    }

    if args.len() >= 2 && args[1] == "--script-test" {
        let dir = ensure_scripts_dir();
        eprintln!("scripts dir: {:?}", dir);
        let acts = discover_script_actions();
        eprintln!("discovered {} action(s):", acts.len());
        for a in &acts {
            eprintln!(
                "  {:<20} types={:?}  applies(png={}, folder={})",
                a.name,
                a.types,
                script_action_applies(a, "photo.png", false),
                script_action_applies(a, "MyFolder", true),
            );
        }
        // Execute "Copy Path" (if present) on a sample path to confirm it runs.
        if let Some(cp) = acts.iter().find(|a| a.name == "Copy Path") {
            let out = Command::new(&cp.path)
                .arg("/tmp/shuffle-script-test.txt")
                .output();
            eprintln!("ran Copy Path -> exit {:?}", out.map(|o| o.status.code()));
        }
        return;
    }

    // Parse a filter query and print the operators + a few match checks.
    if args.len() >= 3 && args[1] == "--filter-test" {
        let fq = FilterQuery::parse(&args[2]);
        eprintln!("text={:?}", fq.text);
        eprintln!("kinds={}", fq.kinds.len());
        eprintln!("exts={:?}", fq.exts);
        eprintln!("size={}", fq.size.is_some());
        eprintln!("after={} before={}", fq.after.is_some(), fq.before.is_some());
        eprintln!("content={:?}", fq.content);
        // Match a few synthetic entries (name, is_dir, size, mtime=now).
        let now = Some(SystemTime::now());
        let cases: &[(&str, bool, u64)] = &[
            ("report.pdf", false, 2_000_000),
            ("photo.png", false, 500_000),
            ("notes.txt", false, 1_000),
            ("Projects", true, 0),
            ("archive.zip", false, 50_000_000),
        ];
        for (n, d, s) in cases {
            eprintln!("  {:>16}  match={}", n, fq.matches_entry(n, *d, *s, now));
        }
        return;
    }

    // Synthetic, disk-free validation of typo ranking.
    if args.len() >= 2 && args[1] == "--typo-test" {
        let mk = |p: &str, is_dir: bool| {
            let path = PathBuf::from(p);
            IndexEntry {
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                path,
                is_dir,
            }
        };
        let index = FileIndex {
            entries: vec![
                mk("/Users/guzma/Documents", true),
                mk("/Users/guzma/Downloads", true),
                mk("/Users/guzma/Music", true),
                mk("/Users/guzma/go/pkg/mod/x/spec-example-documents", true),
                mk("/Users/guzma/Documents/foo/DocumentSummaryInformation", false),
                mk("/Users/guzma/Documents/report.docx", false),
            ],
        };
        for q in ["documents", "dcouments", "documnets", "dwnloads", "musc"] {
            let hits = index.search(q, 3);
            eprintln!(
                "{:?} -> {:?}",
                q,
                hits.iter().map(|h| h.0.clone()).collect::<Vec<_>>()
            );
        }
        return;
    }

    // Hidden timing bench: `shuffle --dir-bench <path>` reports how long each
    // load phase takes for a real folder.
    if args.len() >= 3 && args[1] == "--dir-bench" {
        let p = PathBuf::from(&args[2]);

        let t = std::time::Instant::now();
        let fast = read_entries_fast(&p);
        eprintln!(
            "read_entries_fast: {} entries in {} ms",
            fast.len(),
            t.elapsed().as_millis()
        );

        let t = std::time::Instant::now();
        let full = read_entries(&p);
        eprintln!(
            "read_entries (stat each): {} entries in {} ms",
            full.len(),
            t.elapsed().as_millis()
        );

        // Distinct file types (what prewarm actually builds).
        let mut types: HashSet<String> = HashSet::new();
        for e in &fast {
            if let Some(k) = icon_key(&p.join(&e.name)) {
                types.insert(k);
            }
        }
        eprintln!("distinct file types: {}", types.len());

        ensure_base_icons();
        // Separate iconForFile from TIFFRepresentation to see which is slow.
        for e in fast.iter().filter(|e| !e.is_dir).take(3) {
            let path = p.join(&e.name);
            let ps = path.to_str().unwrap();
            let ws = NSWorkspace::sharedWorkspace();
            let ns = NSString::from_str(ps);
            let t1 = std::time::Instant::now();
            let img = ws.iconForFile(&ns);
            let icon_ms = t1.elapsed().as_millis();
            let t2 = std::time::Instant::now();
            let _ = img.TIFFRepresentation();
            let tiff_ms = t2.elapsed().as_millis();
            eprintln!("{}: iconForFile {icon_ms} ms, TIFFRepresentation {tiff_ms} ms", e.name);
        }

        let t = std::time::Instant::now();
        let mut total_tiff = 0u128;
        let mut seen: HashSet<String> = HashSet::new();
        for e in &fast {
            if e.is_dir {
                continue;
            }
            let path = p.join(&e.name);
            if let Some(k) = icon_key(&path) {
                if !seen.insert(k) {
                    continue;
                }
                let t1 = std::time::Instant::now();
                let _ = icon_tiff(&path);
                total_tiff += t1.elapsed().as_millis();
            }
        }
        eprintln!(
            "all distinct icon TIFF fetches: {} types in {} ms (total wall {} ms)",
            seen.len(),
            total_tiff,
            t.elapsed().as_millis()
        );
        return;
    }

    let app = Application::new();
    // Re-open the window when the Dock icon is clicked after the last window was
    // closed (otherwise the app stays running with no way to show it).
    app.on_reopen(|cx: &mut App| {
        if cx.windows().is_empty() {
            open_main_window(cx);
            cx.activate(true);
        }
    });
    app.run(|cx: &mut App| {
        // Load the saved theme into both the render-side copy and the global.
        let saved_theme = load_theme();
        set_active_theme(saved_theme);
        cx.set_global(ThemeGlobal(saved_theme));

        // Load feature prefs (terminal / preview / info), all off by default.
        let saved_prefs = load_prefs();
        set_active_prefs(saved_prefs);
        cx.set_global(PrefsGlobal(saved_prefs));

        // Load key bindings.
        let saved_keymap = load_keymap();
        set_active_keymap(saved_keymap.clone());
        cx.set_global(KeymapGlobal(saved_keymap));

        // Shared update-check state (Settings drives it, the main window
        // installs from it).
        cx.set_global(UpdateCheckGlobal::default());

        // Load the icon pack (if any).
        let saved_pack = load_icon_pack();
        set_active_icon_pack(saved_pack.clone());
        cx.set_global(IconPackGlobal(saved_pack));

        // Load the menu style; unless the user chose menu colors, derive them
        // from the active theme so menus match (incl. light themes).
        let mut saved_menu = load_menu_style();
        saved_menu.follow_theme(&saved_theme);
        set_active_menu(saved_menu);
        cx.set_global(MenuStyleGlobal(saved_menu));

        // Load saved SFTP servers.
        let saved_servers = load_sftp_servers();
        set_active_sftp_servers(saved_servers.clone());
        cx.set_global(SftpServersGlobal(saved_servers));

        // Load the app-icon background and apply it to the Dock icon.
        let saved_icon_bg = load_icon_bg();
        set_active_icon_bg(saved_icon_bg.clone());
        refresh_dock_icon(&saved_icon_bg);

        // Menu bar: app menu with Settings + Quit, plus their shortcuts.
        cx.bind_keys([
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.set_menus(vec![
            Menu {
                name: "Shuffle".into(),
                items: vec![
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Quit Shuffle", Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Tab", NewTab),
                    MenuItem::action("New Folder", NewFolder),
                    MenuItem::separator(),
                    MenuItem::action("Close Tab", CloseTab),
                    MenuItem::action("Move to Trash", MoveToTrash),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("as List", ViewList),
                    MenuItem::action("as Icons", ViewIcons),
                    MenuItem::action("as Columns", ViewColumns),
                    MenuItem::action("as Gallery", ViewGallery),
                    MenuItem::separator(),
                    MenuItem::action("Hide/Show Sidebar", ToggleSidebar),
                    MenuItem::action("Search…", FocusSearch),
                ],
            },
            Menu {
                name: "Go".into(),
                items: vec![
                    MenuItem::action("Back", GoBack),
                    MenuItem::action("Forward", GoForward),
                    MenuItem::separator(),
                    MenuItem::action("Home", GoHome),
                    MenuItem::action("Applications", GoApplications),
                    MenuItem::action("Computer", GoComputer),
                ],
            },
        ]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &OpenSettings, cx: &mut App| open_settings_window(cx));

        open_main_window(cx);
        cx.activate(true);
    });
}

/// Open the main explorer window.
fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                // No title text; transparent so our own colored bar shows.
                title: None,
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| {
                let mut finder = Shuffle::new(load_last_dir(), cx);
                finder.prewarm_icons(cx);
                finder.build_index(cx);
                // Quietly check GitHub for a newer release (shows a banner if so).
                finder.check_for_update(cx);
                // Fill the initial folder's metadata in the background.
                finder.reload_pane(0, cx);
                // Reconnect any SFTP servers marked "reconnect on launch".
                for server in sftp_servers().into_iter().filter(|s| s.auto_reopen) {
                    finder.connect_sftp(server, cx);
                }
                finder
            });
            // Focus the root so it receives keystrokes (Cmd+P) immediately.
            window.focus(&view.read(cx).focus);
            view
        },
    );
}
