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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use gpui::{
    actions, anchored, div, img, point, prelude::*, px, relative, rgb, rgba, size, uniform_list, AnyElement, App,
    Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, ElementId, ExternalPaths, FocusHandle, ImageSource,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    PathPromptOptions, Rgba,
    RenderImage, ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace, NSDraggingContext,
    NSDraggingSession, NSDraggingSource, NSDragOperation, NSGraphicsContext, NSImage, NSWorkspace,
};
use objc2_foundation::{NSData, NSFileManager, NSObject, NSString, NSURL};
use rayon::prelude::*;

const RECENTS_CAP: usize = 12;

// Menu-bar actions.
actions!(shuffle, [OpenSettings, Quit]);

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
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            terminal: false,
            term_history: false,
            preview: false,
            info: false,
            show_parent: true,
            sidebar_collapsed: false,
            recent_limit: 3,
            palette_history: false,
            groups_enabled: false,
            show_filter_button: true,
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
        info: false,
        show_parent: true,
        sidebar_collapsed: false,
        recent_limit: 3,
        palette_history: false,
        groups_enabled: false,
        show_filter_button: true,
    }) };
}

/// The active preferences. Read this anywhere in render code.
fn prefs() -> Prefs {
    ACTIVE_PREFS.with(|p| *p.borrow())
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
}

impl Default for MenuStyle {
    fn default() -> Self {
        let d = Theme::default();
        MenuStyle { bg: d.surface, text: d.text, opacity: 100, font_px: 14.0 }
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
    focus: FocusHandle,
    /// The action whose keystroke is currently being recorded, if any.
    recording: Option<KeyAction>,
    /// In-progress hex color entry: (which color, typed hex digits).
    color_edit: Option<(ColorTarget, String)>,
}

impl Settings {
    fn new(cx: &mut Context<Self>) -> Self {
        Settings {
            tab: 0,
            focus: cx.focus_handle(),
            recording: None,
            color_edit: None,
        }
    }

    /// Route key events: hex entry first, then keybind recording.
    fn handle_settings_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.color_edit.is_some() {
            self.handle_hex_key(ev, cx);
        } else if self.recording.is_some() {
            self.handle_keybind_key(ev, cx);
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
                                apply_menu_style(nm, cx);
                            }
                            ColorTarget::MenuText => {
                                let mut nm = menu_style();
                                nm.text = c;
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
        let tabs = ["General", "Keybinds", "Customization"];

        // Left tab rail.
        let mut tab_items: Vec<AnyElement> = Vec::new();
        for (i, name) in tabs.iter().enumerate() {
            let active = i == self.tab;
            tab_items.push(
                div()
                    .id(("tab", i))
                    .px_3()
                    .py_2()
                    .mx_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if active { rgb(t.text) } else { rgb(t.text_muted) })
                    .when(active, |s| s.bg(rgb(t.surface)))
                    .when(!active, |s| s.hover(|s| s.bg(rgb(t.hover))))
                    .child(name.to_string())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.tab = i;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        let rail = div()
            .flex_none()
            .w(px(170.0))
            .h_full()
            .pt_4()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(t.sidebar))
            .border_r_1()
            .border_color(rgb(t.border))
            .children(tab_items);

        let content = match self.tab {
            0 => self.render_general(cx).into_any_element(),
            1 => self.render_keybinds(cx).into_any_element(),
            _ => self.render_customization(cx).into_any_element(),
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
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .p_5()
                    .child(content),
            )
    }
}

impl Settings {
    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = prefs();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_5()
            .child(settings_title("Features"))
            .child(toggle_row(
                "tg-terminal",
                "Terminal mode",
                "Show a command input at the bottom to move through the explorer \
                 like a terminal (with path/command autocomplete).",
                p.terminal,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.terminal = !np.terminal;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-term-history",
                "Terminal history",
                "Show a scrollback of what you've typed and command output above \
                 the input (otherwise it's just the input line).",
                p.term_history,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.term_history = !np.term_history;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-preview",
                "Preview",
                "Click a file once to preview it (images, PDFs, documents, …) in \
                 the inspector panel.",
                p.preview,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.preview = !np.preview;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-info",
                "Information",
                "Click a file once to see its details (kind, size, dates, \
                 dimensions, color space, and more) in the inspector panel.",
                p.info,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.info = !np.info;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-show-parent",
                "Show “..” (up one level)",
                "Show the leading “..” entry in list view that navigates to the \
                 parent folder.",
                p.show_parent,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.show_parent = !np.show_parent;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-palette-history",
                "Command palette history",
                "Give Cmd+P its own query history: press Up/Down in the palette to \
                 cycle through your previous searches.",
                p.palette_history,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.palette_history = !np.palette_history;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-groups",
                "Sidebar groups",
                "Create custom groups of files/folders in the sidebar. Right-click \
                 the sidebar to make a group, then right-click any item to add it.",
                p.groups_enabled,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.groups_enabled = !np.groups_enabled;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "tg-filter-button",
                "Filter button",
                "Show the always-on \"Filter\" button in the bottom-right corner. \
                 Pressing / still opens the filter either way.",
                p.show_filter_button,
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    let mut np = prefs();
                    np.show_filter_button = !np.show_filter_button;
                    apply_prefs(np, cx);
                    cx.notify();
                }),
            ))
            .child(settings_title("Sidebar"))
            .child(stepper_row(
                "st-recents",
                "Recent folders",
                "How many recently-visited folders to show in the sidebar (0 hides \
                 the Recents section).",
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
            ))
            .child(reset_button(
                "reset-general",
                "Reset General to Default",
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    apply_prefs(Prefs::default(), cx);
                    cx.notify();
                }),
            ))
    }

    fn render_keybinds(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(settings_title("Keyboard shortcuts"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(t.text_muted))
                    .pb_2()
                    .child("Click a shortcut, then press the keys. ⌫ clears it; Esc cancels."),
            )
            .children(rows)
            .child(reset_button(
                "reset-keybinds",
                "Reset Shortcuts to Default",
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.recording = None;
                    apply_keymap(Keymap::defaults(), cx);
                    cx.notify();
                }),
            ))
    }

    fn render_customization(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                        // Mini preview: a few swatches from the preset.
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

        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(settings_title("Preset Palettes"))
            .child(div().flex().flex_wrap().gap_3().children(presets))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(settings_title("Background"))
                    .child(self.hex_field(ColorTarget::Bg, t.bg, cx)),
            )
            .child(self.color_row("bg", t.bg, |t, c| t.bg = c, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(settings_title("Text"))
                    .child(self.hex_field(ColorTarget::Text, t.text, cx)),
            )
            .child(self.color_row("text", t.text, |t, c| t.text = c, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(settings_title("Mouseover"))
                    .child(self.hex_field(ColorTarget::Hover, t.hover, cx)),
            )
            .child(self.color_row("hover", t.hover, |t, c| t.hover = c, cx))
            // ----- Menu (right-click / dropdown) appearance -----
            .child(settings_title("Menu"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(self.menu_preview())
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(t.text_muted))
                            .child("Live preview of the right-click menu."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(menu_label("Menu color"))
                    .child(self.hex_field(ColorTarget::MenuBg, menu_style().bg, cx)),
            )
            .child(self.menu_color_row("menubg", menu_style().bg, |m, c| m.bg = c, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(menu_label("Letter color"))
                    .child(self.hex_field(ColorTarget::MenuText, menu_style().text, cx)),
            )
            .child(self.menu_color_row("menutext", menu_style().text, |m, c| m.text = c, cx))
            .child(stepper_row(
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
            ))
            .child(stepper_row(
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
            ))
            // ----- App icon background -----
            .child(settings_title("App Icon"))
            .child(
                div()
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
                                    .child("Change the icon background: pick a color or upload an image (PNG or JPG; square looks best)."),
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
                                            .child("Upload Background…")
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
                    ),
            )
            .child(self.icon_color_row(cx))
            .child(settings_title("Icon Pack"))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(t.text_muted))
                    .child(match icon_pack() {
                        Some(p) => format!("Current: {}", path_label(&p)),
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
                            .child("Choose Folder…")
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
                                            let _ = this.update(cx, |_, cx| {
                                                apply_icon_pack(Some(p), cx)
                                            });
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
            .child(reset_button(
                "reset-customization",
                "Reset Customization to Default",
                cx.listener(|_, _: &ClickEvent, _, cx| {
                    // Theme, menu style, app icon, and icon pack all back to stock.
                    apply_theme(Theme::default(), cx);
                    apply_menu_style(MenuStyle::default(), cx);
                    apply_icon_bg(IconBg::Default, cx);
                    apply_icon_pack(None, cx);
                    cx.notify();
                }),
            ))
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

/// A small settings sub-label (normal case, not uppercased).
fn menu_label(text: &str) -> impl IntoElement {
    div().text_color(rgb(theme().text)).child(text.to_string())
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
#[derive(Clone)]
enum Action {
    /// Open a path (navigate into it if a dir, else reveal its parent).
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
    /// Whole-text selected (Cmd+A or on start): typing replaces everything.
    selected_all: bool,
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
    server_dialog: Option<String>,
    /// Recently-connected server URLs (most recent first).
    server_history: Vec<String>,
    // Terminal mode (the bottom command bar).
    term_input: String,
    term_output: Vec<String>,
    term_focused: bool,
    term_scroll: ScrollHandle,
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
            server_history: read_string_list("servers.txt"),
            term_input: String::new(),
            term_output: Vec::new(),
            term_focused: false,
            term_scroll: ScrollHandle::new(),
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
        let dir = self.tab(pane).current_dir.clone();
        ensure_dynamic_sidebar_icons(); // pick up newly-mounted volumes/cloud
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

    fn new_folder(&mut self, pane: usize, cx: &mut Context<Self>) {
        let path = unique_child(&self.tab(pane).current_dir, "untitled folder");
        if fs::create_dir(&path).is_ok() {
            self.refresh_pane(pane, cx);
        }
    }

    fn new_file(&mut self, pane: usize, cx: &mut Context<Self>) {
        let path = unique_child(&self.tab(pane).current_dir, "untitled file");
        if fs::File::create(&path).is_ok() {
            self.refresh_pane(pane, cx);
        }
    }

    fn open_path(&mut self, pane: usize, path: PathBuf, is_dir: bool, cx: &mut Context<Self>) {
        if is_dir {
            self.navigate_in(pane, path, cx);
        } else {
            let _ = Command::new("open").arg(&path).spawn();
        }
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

    /// Carry out a confirmed delete: move every path to Trash.
    fn perform_delete(&mut self, cx: &mut Context<Self>) {
        if let Some((pane, paths)) = self.confirm_delete.take() {
            for p in &paths {
                trash_path(p);
            }
            let tab = self.tab_mut(pane);
            tab.selection.clear();
            tab.anchor = None;
            self.refresh_pane(pane, cx);
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

    // ----- inline rename -----

    /// Begin renaming `path`: the row's name becomes an editable field.
    fn begin_rename(&mut self, pane: usize, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane = pane;
        let text = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Start with the whole name selected, like Finder.
        self.rename = Some(Rename { pane, path, text, selected_all: true });
        window.focus(&self.focus);
        cx.notify();
    }

    /// Commit the in-progress rename (Enter), renaming the file on disk.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.rename.take() {
            let new = r.text.trim();
            if !new.is_empty() && !new.contains('/') {
                if let Some(parent) = r.path.parent() {
                    let dest = parent.join(new);
                    if dest != r.path && !dest.exists() && fs::rename(&r.path, &dest).is_ok() {
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

    /// Keystrokes while a rename field is active.
    fn handle_rename_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        match ks.key.as_str() {
            "escape" => {
                self.rename = None;
                cx.notify();
            }
            "enter" => self.commit_rename(cx),
            "a" if cmd => {
                if let Some(r) = self.rename.as_mut() {
                    r.selected_all = true;
                }
                cx.notify();
            }
            "c" if cmd => {
                if let Some(r) = &self.rename {
                    cx.write_to_clipboard(ClipboardItem::new_string(r.text.clone()));
                }
            }
            "backspace" => {
                if let Some(r) = self.rename.as_mut() {
                    if r.selected_all {
                        r.text.clear();
                        r.selected_all = false;
                    } else {
                        r.text.pop();
                    }
                }
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(r) = self.rename.as_mut() {
                        if r.selected_all {
                            r.text.clear();
                            r.selected_all = false;
                        }
                        r.text.push_str(t.trim());
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
                        if let Some(r) = self.rename.as_mut() {
                            if r.selected_all {
                                r.text.clear();
                                r.selected_all = false;
                            }
                            r.text.push_str(ch);
                        }
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
    fn compress_entry(&mut self, pane: usize, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent() else { return };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = unique_child(parent, &format!("{stem}.zip"));
        let _ = Command::new("ditto")
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(&path)
            .arg(&dest)
            .status();
        self.refresh_pane(pane, cx);
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
            ctx_item("New Folder", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.close_context_menu(cx);
                this.new_folder(pane, cx);
            }))
            .into_any_element(),
        );
        items.push(
            ctx_item("New File", cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.close_context_menu(cx);
                this.new_file(pane, cx);
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
        self.server_dialog = Some(String::new());
        cx.notify();
    }

    /// Hand a server URL off to macOS (`open smb://…`), which shows the native
    /// auth prompt and mounts the share under /Volumes. Records it in history.
    fn connect_to_server(&mut self, raw: &str, cx: &mut Context<Self>) {
        let mut url = raw.trim().to_string();
        if url.is_empty() {
            return;
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
            "enter" => {
                if let Some(url) = self.server_dialog.clone() {
                    self.connect_to_server(&url, cx);
                }
            }
            "backspace" => {
                if let Some(s) = self.server_dialog.as_mut() {
                    s.pop();
                }
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(s) = self.server_dialog.as_mut() {
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
                        if let Some(s) = self.server_dialog.as_mut() {
                            s.push_str(ch);
                        }
                        cx.notify();
                    }
                }
            }
        }
    }

    fn render_server_dialog(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme();
        let url = self.server_dialog.clone().unwrap_or_default();
        let placeholder = url.is_empty();
        let shown = if placeholder { "smb://server/share".to_string() } else { url.clone() };

        let mut recent = div().flex().flex_col().gap_1();
        if !self.server_history.is_empty() {
            recent = recent.child(
                div().text_xs().text_color(rgb(t.text_dim)).child("Recent Servers"),
            );
            for u in self.server_history.iter().take(6) {
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
                this.server_dialog = None;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(420.0))
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
                    .child(div().text_color(rgb(t.text)).child("Connect to Server"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(t.text_muted))
                            .child("Enter an address, e.g. smb://host/share, afp://…, or ftp://…"),
                    )
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
                    .child(recent)
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
                                        if let Some(url) = this.server_dialog.clone() {
                                            this.connect_to_server(&url, cx);
                                        }
                                    })),
                            ),
                    ),
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

    fn end_scroll_drag(&mut self) {
        self.scroll_drag = None;
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
        let base = &self.palette_scroll;
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
                .bg(rgba(0xffffff44))
                .into_any_element(),
        )
    }

    fn activate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.palette_items.get(self.selected) else {
            return;
        };
        let action = item.action.clone();
        self.record_palette_history();
        match action {
            Action::Open(path, is_dir) => {
                let target = if is_dir {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or(path)
                };
                self.close_palette(cx);
                self.navigate_to(target, cx);
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
            KeyAction::NewFile => self.new_file(pane, cx),
            KeyAction::NewFolder => self.new_folder(pane, cx),
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
            }
            // Enter renames the focused file (Finder-style; not rebindable).
            if !cmd && key == "enter" {
                let pane = self.active_pane;
                if let Some(sel) = self.tab(pane).anchor.clone() {
                    self.begin_rename(pane, sel, window, cx);
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
            .child(
                div()
                    .mt(px(90.0))
                    .w(px(680.0))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    // ~75% opaque so the explorer shows faintly through the menu.
                    .bg(Theme::alpha(t.surface, 0xbf))
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
                    ),
            )
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

        // Brighter while this pane's thumb is being dragged.
        let dragging = self.scroll_drag.is_some_and(|d| d.pane == pane);
        let color = if dragging {
            rgba(0xffffff66)
        } else {
            rgba(0xffffff33)
        };

        Some(
            div()
                .id(("scrollbar-thumb", pane))
                .absolute()
                .top(px(thumb_top))
                .right(px(2.0))
                .w(px(8.0))
                .h(px(thumb_h))
                .rounded_full()
                .bg(color)
                .cursor(CursorStyle::PointingHand)
                .hover(|s| s.bg(rgba(0xffffff55)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.begin_scroll_drag(pane, f64::from(ev.position.y) as f32);
                        cx.notify();
                    }),
                )
                .into_any_element(),
        )
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
            tab.selection.clear();
            tab.anchor = None;
            tab.col_chain.clear();
            tab.col_active = 0;
        }
        save_last_dir(&dir);

        self.recents.retain(|p| p != &dir);
        self.recents.insert(0, dir);
        self.recents.truncate(RECENTS_CAP);
        write_path_list("recents.txt", &self.recents);

        // Fast first paint + background metadata fill.
        self.reload_pane(pane, cx);
    }

    /// Navigate a pane into `dir` if it is a directory. New navigation truncates
    /// any forward history, then appends `dir` as the new tip.
    fn navigate_in(&mut self, pane: usize, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.is_dir() || dir == self.tab(pane).current_dir {
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
        let mut scored: Vec<(i32, usize)> = tab
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| find_score(q, &e.name).map(|s| (s, i)))
            .collect();
        // Best score first; ties → dirs first, then alphabetical.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| tab.entries[b.1].is_dir.cmp(&tab.entries[a.1].is_dir))
                .then_with(|| {
                    tab.entries[a.1]
                        .name
                        .to_lowercase()
                        .cmp(&tab.entries[b.1].name.to_lowercase())
                })
        });
        tab.find_results = scored.into_iter().map(|(_, i)| i).collect();
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
                        cx.notify();
                    }
                }
            }
        }
    }

    // ----- tabs & split panes -----

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
        if from.pane >= self.panes.len() || from.tab >= self.panes[from.pane].tabs.len() {
            return;
        }
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
        let gallery = self.tab(pane).view == ViewMode::Gallery;
        self.tab_mut(pane).anchor = Some(path.clone());
        self.ensure_preview(path.clone(), gallery, cx);
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

    /// Dispatch a left-click on an item, honoring Cmd / Shift modifiers.
    fn click_entry(
        &mut self,
        pane: usize,
        path: PathBuf,
        is_dir: bool,
        ev: &ClickEvent,
        cx: &mut Context<Self>,
    ) {
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

    /// Start a marquee (rubber-band) selection from empty list space.
    fn begin_marquee(&mut self, pane: usize, x: f32, y: f32, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.term_focused = false;
        self.rename = None;
        {
            let tab = self.tab_mut(pane);
            tab.selection.clear();
            tab.anchor = None;
        }
        self.marquee = Some((pane, (x, y), (x, y)));
        cx.notify();
    }

    /// Update the marquee end point and recompute which rows it covers.
    fn update_marquee(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some((pane, start, _)) = self.marquee else {
            return;
        };
        self.marquee = Some((pane, start, (x, y)));

        // Map the vertical span to row indices using the list's geometry.
        let (list_top, scrolled) = {
            let st = self.tab(pane).scroll_handle.0.borrow();
            let top = f64::from(st.base_handle.bounds().origin.y) as f32;
            let scr = (-(f64::from(st.base_handle.offset().y) as f32)).max(0.0);
            (top, scr)
        };
        let y0 = start.1.min(y);
        let y1 = start.1.max(y);
        let c0 = (y0 - list_top + scrolled).max(0.0);
        let c1 = (y1 - list_top + scrolled).max(0.0);
        let i0 = (c0 / ROW_H).floor() as i64;
        let i1 = (c1 / ROW_H).floor() as i64;
        // Row 0 is the ".." entry when present; offset to display indices.
        let has_parent =
            self.tab(pane).find_query.is_none() && prefs().show_parent && self.tab(pane).current_dir.parent().is_some();
        let off = i64::from(has_parent);
        let paths = self.display_paths(pane);
        let mut sel = HashSet::new();
        for i in i0..=i1 {
            let di = i - off;
            if di >= 0 && (di as usize) < paths.len() {
                sel.insert(paths[di as usize].clone());
            }
        }
        self.tab_mut(pane).selection = sel;
        cx.notify();
    }

    fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if self.marquee.take().is_some() {
            cx.notify();
        }
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
        let x = start.0.min(cur.0) - ox;
        let y = start.1.min(cur.1) - oy;
        let w = (start.0 - cur.0).abs();
        let h = (start.1 - cur.1).abs();
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
            tab.selection.insert(target);
            tab.anchor = None;
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

    /// The right-hand inspector: preview and/or information for the selected
    /// file. `None` when neither feature is on or nothing is selected.
    fn render_inspector(&self, _cx: &Context<Self>) -> Option<AnyElement> {
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
                    .text_color(rgb(t.text))
                    .truncate()
                    .child(path_label(&sel)),
            );

        if show_preview {
            let body: AnyElement = match lookup_preview(&sel) {
                Some(Some(handle)) => img(ImageSource::Render(handle))
                    .max_w(px(288.0))
                    .max_h(px(360.0))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                // Not ready yet or unavailable → show the file's icon.
                _ => icon_element_sized(&sel, false, 96.0),
            };
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .min_h(px(120.0))
                    .p_2()
                    .rounded_md()
                    // White "page" so document/text previews (black text, often
                    // transparent background) stay readable on dark themes.
                    .bg(rgb(0xffffff))
                    .child(body),
            );
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
        // The editable text: placeholder, a highlighted selection span, or the
        // text split around a static caret at the cursor.
        let field = if query.is_empty() {
            div()
                .min_w(px(80.0))
                .text_color(rgb(t.text_dim))
                .child("type to filter…")
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
            .child(div().flex_none().text_color(rgb(t.text_muted)).child("Filter"))
            .child(field)
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(t.text_muted))
                    .text_xs()
                    .child(format!("{count}")),
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
        let home = home_dir();
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
                if !path.is_dir() {
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
        let cloud = cloud_locations();
        if !cloud.is_empty() && self.begin_section(&mut items, "CLOUD", collapsed, cx) {
            for (label, path) in cloud {
                let icon_key = path.to_string_lossy().into_owned();
                push_nav(&mut items, cx, &mut key, label, icon_key, path, current, collapsed);
            }
        }

        // --- Servers (the Mac, mounted volumes/shares, Connect to Server) ---
        if self.begin_section(&mut items, "SERVERS", collapsed, cx) {
            push_nav(
                &mut items,
                cx,
                &mut key,
                "Macintosh HD".to_string(),
                fav_key("computer"),
                PathBuf::from("/"),
                current,
                collapsed,
            );
            push_nav(
                &mut items,
                cx,
                &mut key,
                username(),
                fav_key("home"),
                home,
                current,
                collapsed,
            );
            for (label, path) in mounted_volumes() {
                let icon_key = path.to_string_lossy().into_owned();
                push_nav(&mut items, cx, &mut key, label, icon_key, path, current, collapsed);
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
        let btn = |id: &'static str, glyph: &'static str, mode: ViewMode, cx: &Context<Self>| {
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
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.active_pane = pane;
                if !this.palette_open {
                    this.toggle_palette(window, cx);
                }
            }));
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .pl_2()
            .child(search_btn)
            .child(btn("view-list", "☰", ViewMode::List, cx))
            .child(btn("view-icons", "▦", ViewMode::Icons, cx))
            .child(btn("view-columns", "▥", ViewMode::Columns, cx))
            .child(btn("view-gallery", "▭", ViewMode::Gallery, cx))
            .child(
                // Sort-By button — opens a dropdown at the click location.
                div()
                    .id(("sort-by", pane))
                    .flex_none()
                    .px_2()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(t.text_dim))
                    .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
                    .child("⇅")
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
                    ),
            )
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

        let mut segs: Vec<AnyElement> = Vec::new();
        let mut acc = PathBuf::new();
        let mut idx = 0usize;
        for comp in display.components() {
            let (label, full) = match comp {
                Component::RootDir => {
                    acc.push("/");
                    ("Macintosh HD".to_string(), acc.clone())
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
            row = row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_pane(0, cx)),
            );
        } else {
            row = row
                .child(
                    div()
                        .flex_none()
                        .w(relative(self.split_ratio))
                        .min_w_0()
                        .h_full()
                        .child(self.render_pane(0, cx)),
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

        // Right-edge split target — only present while a tab is being dragged,
        // so it never blocks normal interaction.
        if cx.has_active_drag() {
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
        div()
            .id("pane-divider")
            .flex_none()
            .w(px(6.0))
            .h_full()
            .flex()
            .justify_center()
            .cursor(CursorStyle::ResizeLeftRight)
            .child(div().w(px(1.0)).h_full().bg(rgb(theme().border_strong)))
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
            let label = path_label(&tab.current_dir);
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
                    // Drag this tab (live floating preview).
                    .on_drag(drag, move |_, _, _, cx| {
                        cx.new(|_| TabDragPreview {
                            label: label.clone(),
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
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(path_label(&tab.current_dir)),
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

                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // Dropping file(s) on empty pane space moves them here.
                    .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(Theme::alpha(theme().accent, 0x22)))
                    .on_drop(cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                        for p in drag.paths() {
                            this.move_into(pane_dir.clone(), p.clone(), cx);
                        }
                    }))
                    .child(
                        // Horizontal scroller holding the column header + rows, so
                        // they scroll sideways together and never overflow the pane.
                        div()
                            .id(("hscroll", pane))
                            .size_full()
                            .overflow_x_scroll()
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
                                            // Press on empty space → start a marquee
                                            // (rows stop_propagation so this only
                                            // fires on blank area).
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
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
                                        None,
                                        true,        // ".." has no metadata to load
                                        row_key,
                                        false,
                                        false,       // ".." is never the menu target
                                        widths,
                                        icon,
                                        true,        // accepts drops (move into parent)
                                        None,        // never renamed
                                        cx.listener({
                                            let parent = parent.clone();
                                            move |this, _: &ClickEvent, _, cx| {
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
                                            for p in drag.paths() {
                                                this.move_into(parent.clone(), p.clone(), cx);
                                            }
                                        }),
                                        // ".." isn't draggable; just swallow the press.
                                        |_, _, cx: &mut App| cx.stop_propagation(),
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
                            let ctx_target = target.clone();
                            let drop_target = target.clone();
                            let is_selected = tab.selection.contains(&target);
                            let ctx_active = this.is_ctx_target(&target);
                            let rename_text = this
                                .rename
                                .as_ref()
                                .filter(|r| r.path == target)
                                .map(|r| (r.text.clone(), r.selected_all));
                            // Don't drag the row while it's being renamed.
                            let drag_target = if rename_text.is_some() {
                                None
                            } else {
                                Some(target.clone())
                            };
                            let icon = icon_element(&target, is_dir);

                            items.push(
                                file_row(
                                    &name,
                                    is_dir,
                                    entry_size,
                                    modified,
                                    entry_loaded,
                                    row_key,
                                    is_selected,
                                    ctx_active,
                                    widths,
                                    icon,
                                    is_dir,            // folders accept drops
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
                                        // Drop onto a folder → move the file(s) into it.
                                        for p in drag.paths() {
                                            this.move_into(drop_target.clone(), p.clone(), cx);
                                        }
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
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(Theme::alpha(theme().accent, 0x22)))
            .on_drop(cx.listener(move |this, drag: &ExternalPaths, _, cx| {
                for p in drag.paths() {
                    this.move_into(pane_dir.clone(), p.clone(), cx);
                }
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
                    || anchor.as_deref() == Some(target.as_path());
                let ctx_active = self.is_ctx_target(&target);
                rows.push(column_row(pane, i, &e.name, target, e.is_dir, selected, ctx_active, cx));
            }
            cols.push(
                div()
                    .id(("col", pane * 100 + i))
                    .flex_none()
                    .w(px(230.0))
                    .h_full()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(rgb(t.border))
                    .flex()
                    .flex_col()
                    .py_1()
                    .children(rows)
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
                    .child(icon_element_sized(&target, is_dir, 44.0))
                    .child(div().w_full().truncate().text_xs().text_color(rgb(t.text_muted)).child(name))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() >= 2 {
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
                .bg(rgba(0xffffff33))
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
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag_candidate = None;
                    this.end_resize();
                    this.end_scroll_drag();
                    this.divider_drag = None;
                    this.end_marquee(cx);
                }),
            )
            // A slim titlebar strip in the app's own background color. With the
            // OS titlebar transparent, this is what shows behind the traffic
            // lights, and it keeps the content clear of them.
            .child(div().flex_none().w_full().h(px(TITLEBAR_H)).bg(rgb(t.bg)))
            .child(main_row);

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
        if self.server_dialog.is_some() {
            root = root.child(self.render_server_dialog(cx));
        }
        if self.sidebar_menu.is_some() {
            root = root.child(self.render_sidebar_menu(cx));
        }
        if self.group_dialog.is_some() {
            root = root.child(self.render_group_dialog(cx));
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
    let is_dir = target.is_dir();
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
    let is_dir = target.is_dir();
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

/// Path to the bundled `removebg` Swift helper, if it was compiled in.
fn removebg_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join("removebg");
    cand.exists().then_some(cand)
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
    // When Some, this row is being renamed: the name shows as an editable field.
    // The bool is whether the whole text is selected (Cmd+A / on start).
    rename_text: Option<(String, bool)>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_drop_file: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    // Left-press handler: records a drag candidate + stops marquee propagation.
    on_press: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let kind = kind_label(name, is_dir);
    let name_color = if is_dir { t.accent } else { t.text };
    let meta_color = rgb(t.text_muted);
    // The name element: an editable field while renaming, else the label.
    let name_el: AnyElement = match &rename_text {
        Some((txt, selected)) => {
            let field = div()
                .flex_1()
                .min_w_0()
                .px_1()
                .rounded_sm()
                .bg(rgb(t.bg))
                .border_1()
                .border_color(rgb(t.accent))
                .text_color(rgb(t.text));
            if *selected {
                // Whole-text selection: highlight the text block.
                field
                    .child(
                        div()
                            .bg(Theme::alpha(t.accent, 0x66))
                            .child(txt.clone()),
                    )
                    .into_any_element()
            } else {
                field.child(format!("{txt}\u{2502}")).into_any_element()
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
        // Press records a drag candidate (promoted to a native OS drag on move)
        // and stops a marquee from starting on the list behind it.
        .on_mouse_down(MouseButton::Left, on_press)
        // Folders / ".." accept dropped files (from within Shuffle or Finder).
        .when(accept_drop, |row| {
            row.drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme().selected)))
                .on_drop(on_drop_file)
        })
        // Name (icon + label). Long names truncate with an ellipsis.
        .child(
            div()
                .flex_none()
                .w(px(widths.name))
                .flex()
                .items_center()
                .gap_2()
                .pr_2()
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
                .child(if loaded { format_size(is_dir, size) } else { "--".to_string() }),
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
                for p in d.paths() {
                    this.move_into(drop_t.clone(), p.clone(), cx);
                }
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
            let name = e.file_name().to_string_lossy().into_owned();
            out.push((name, p));
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
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

fn lookup_info(path: &Path) -> Option<FileInfo> {
    INFO_CACHE.with(|c| c.borrow().get(path).cloned())
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
const SKIP_DIRS: &[&str] = &["node_modules", "Library"];

impl FileIndex {
    /// Walk `root` in parallel (jwalk), skipping hidden dirs and noise dirs,
    /// into a flat list. Runs off the UI thread.
    fn build(root: PathBuf) -> Self {
        let walker = jwalk::WalkDir::new(&root)
            .skip_hidden(true)
            .process_read_dir(|_depth, _path, _state, children| {
                children.retain(|res| match res {
                    Ok(e) => {
                        if e.file_type().is_dir() {
                            let name = e.file_name();
                            let name = name.to_string_lossy();
                            !SKIP_DIRS.contains(&name.as_ref())
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

fn username() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            home_dir()
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Home".to_string())
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
            "bg={:06x}\ntext={:06x}\nopacity={}\nfont_px={}\n",
            m.bg, m.text, m.opacity, m.font_px
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
            "terminal={}\nterm_history={}\npreview={}\ninfo={}\nshow_parent={}\nsidebar_collapsed={}\nrecent_limit={}\npalette_history={}\ngroups_enabled={}\nshow_filter_button={}\n",
            p.terminal, p.term_history, p.preview, p.info, p.show_parent, p.sidebar_collapsed, p.recent_limit, p.palette_history, p.groups_enabled, p.show_filter_button
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

    Application::new().run(|cx: &mut App| {
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

        // Load the icon pack (if any).
        let saved_pack = load_icon_pack();
        set_active_icon_pack(saved_pack.clone());
        cx.set_global(IconPackGlobal(saved_pack));

        // Load the menu style.
        let saved_menu = load_menu_style();
        set_active_menu(saved_menu);
        cx.set_global(MenuStyleGlobal(saved_menu));

        // Load the app-icon background and apply it to the Dock icon.
        let saved_icon_bg = load_icon_bg();
        set_active_icon_bg(saved_icon_bg.clone());
        refresh_dock_icon(&saved_icon_bg);

        // Menu bar: app menu with Settings + Quit, plus their shortcuts.
        cx.bind_keys([
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.set_menus(vec![Menu {
            name: "Shuffle".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit Shuffle", Quit),
            ],
        }]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_action(|_: &OpenSettings, cx: &mut App| open_settings_window(cx));

        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);

        cx.open_window(
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
                    // Fill the initial folder's metadata in the background.
                    finder.reload_pane(0, cx);
                    finder
                });
                // Focus the root so it receives keystrokes (Cmd+P) immediately.
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .unwrap();

        cx.activate(true);
    });
}
