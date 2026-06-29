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
    actions, div, img, point, prelude::*, px, relative, rgb, rgba, size, uniform_list, AnyElement, App,
    Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, ElementId, FocusHandle, ImageSource,
    KeyBinding, KeyDownEvent, Menu, MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent, Rgba,
    RenderImage, ScrollHandle, ScrollWheelEvent, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};
use objc2_app_kit::{NSImage, NSWorkspace};
use objc2_foundation::{NSData, NSFileManager, NSString, NSURL};
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
const BG_OPTIONS: &[u32] = &[0x1e1e22, 0x101014, 0x1e1e2e, 0x24273a, 0x303446, 0x232334, 0xeff1f5];
const TEXT_OPTIONS: &[u32] = &[0xf0f0f4, 0xcdd6f4, 0xcad3f5, 0xc6d0f5, 0xb0b0b8, 0x4c4f69];
const HOVER_OPTIONS: &[u32] = &[0x2a2a30, 0x313244, 0x363a4f, 0x414559, 0x3a3a44, 0xdce0e8];

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

/// The Settings window: a tabbed customization surface.
struct Settings {
    tab: usize,
}

impl Settings {
    fn new() -> Self {
        Settings { tab: 0 }
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme();
        let tabs = ["Customization"];

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

        div()
            .flex()
            .size_full()
            .bg(rgb(t.bg))
            .text_sm()
            .text_color(rgb(t.text))
            .child(rail)
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
                    .p_5()
                    .child(self.render_customization(cx)),
            )
    }
}

impl Settings {
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
            .child(settings_title("Background"))
            .child(self.color_row(BG_OPTIONS, t.bg, |t, c| t.bg = c, cx))
            .child(settings_title("Text"))
            .child(self.color_row(TEXT_OPTIONS, t.text, |t, c| t.text = c, cx))
            .child(settings_title("Mouseover"))
            .child(self.color_row(HOVER_OPTIONS, t.hover, |t, c| t.hover = c, cx))
    }

    /// A row of color swatches; clicking one sets a single theme field via `set`.
    fn color_row(
        &self,
        options: &'static [u32],
        current: u32,
        set: fn(&mut Theme, u32),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme();
        let mut swatches: Vec<AnyElement> = Vec::new();
        for &c in options {
            let selected = c == current;
            swatches.push(
                div()
                    .id(("swatch", c))
                    .w(px(34.0))
                    .h(px(34.0))
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
        div().flex().flex_wrap().gap_2().children(swatches)
    }
}

/// A small color dot used in preset previews.
fn swatch_dot(color: u32) -> impl IntoElement {
    div().w(px(14.0)).h(px(14.0)).rounded_full().bg(rgb(color))
}

/// A section heading inside Settings.
fn settings_title(text: &str) -> impl IntoElement {
    div()
        .text_color(rgb(theme().text_muted))
        .text_xs()
        .child(text.to_uppercase())
}

// Default column widths for the main listing; all are user-resizable.
const ICON_W: f32 = 18.0;
const MIN_COL_W: f32 = 50.0;

// Command-palette result row height, and how many show before scrolling.
const PALETTE_ROW_H: f32 = 26.0;
const PALETTE_MAX_ROWS: usize = 7;
/// Sidebar width; also the left edge of the content/canvas area.
const SIDEBAR_W: f32 = 220.0;
/// Tab strip row height.
const TAB_H: f32 = 30.0;

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
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
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
    /// When `Some`, an in-directory "find" filter is active (opened by `/`).
    /// `find_results` holds the matching `entries` indices, best match first.
    find_query: Option<String>,
    find_results: Vec<usize>,
    scroll_handle: UniformListScrollHandle,
    /// Horizontal scroll of the columns (when they're wider than the pane).
    h_scroll: ScrollHandle,
}

impl Tab {
    fn new(dir: PathBuf) -> Self {
        let entries = read_entries(&dir);
        Tab {
            current_dir: dir.clone(),
            entries,
            history: vec![dir.clone()],
            hist_pos: 0,
            deepest: Some(dir),
            editing_path: None,
            find_query: None,
            find_results: Vec::new(),
            scroll_handle: UniformListScrollHandle::new(),
            h_scroll: ScrollHandle::new(),
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
    /// True while the user drags the divider between two panes.
    divider_drag: bool,
    recents: Vec<PathBuf>,
    bookmarks: Vec<PathBuf>,
    widths: ColumnWidths,
    resize: Option<Resize>,
    scroll_drag: Option<ScrollDrag>,
    // Command palette (Cmd+P).
    focus: FocusHandle,
    palette_open: bool,
    query: String,
    palette_items: Vec<PaletteItem>,
    selected: usize,
    search_gen: u64,
    palette_scroll: ScrollHandle,
    /// In-memory fuzzy index of ~/ (None until the background build finishes).
    index: Option<Arc<FileIndex>>,
    context_menu: Option<ContextMenu>,
}

impl Shuffle {
    fn new(dir: PathBuf, cx: &mut Context<Self>) -> Self {
        ensure_base_icons(); // real folder/file icons ready before first render
        // Sync + repaint whenever the theme changes (e.g. from Settings).
        cx.observe_global::<ThemeGlobal>(|_, cx| {
            set_active_theme(cx.global::<ThemeGlobal>().0);
            cx.notify();
        })
        .detach();
        Self {
            panes: vec![Pane::new(dir)],
            active_pane: 0,
            split_ratio: 0.5,
            divider_drag: false,
            recents: read_path_list("recents.txt"),
            bookmarks: read_path_list("bookmarks.txt"),
            widths: ColumnWidths::default(),
            resize: None,
            scroll_drag: None,
            focus: cx.focus_handle(),
            palette_open: false,
            query: String::new(),
            palette_items: Vec::new(),
            selected: 0,
            search_gen: 0,
            palette_scroll: ScrollHandle::new(),
            index: None,
            context_menu: None,
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
        self.context_menu = Some(ContextMenu { x, y, pane: self.active_pane, target });
        cx.notify();
    }

    fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Re-read a pane's current directory contents (after a create/trash).
    fn refresh_pane(&mut self, pane: usize, cx: &mut Context<Self>) {
        let dir = self.tab(pane).current_dir.clone();
        self.tab_mut(pane).entries = read_entries(&dir);
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

    fn render_context_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let menu = self.context_menu.as_ref().expect("called only when open");
        let pane = menu.pane;
        let mut items: Vec<AnyElement> = Vec::new();

        if let Some((path, is_dir)) = menu.target.clone() {
            let is_dir2 = is_dir;
            let p_open = path.clone();
            items.push(
                ctx_item(
                    if is_dir { "Open" } else { "Open" },
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        this.open_path(pane, p_open.clone(), is_dir2, cx);
                    }),
                )
                .into_any_element(),
            );
            let p_rev = path.clone();
            items.push(
                ctx_item(
                    "Reveal in Finder",
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        let _ = Command::new("open").arg("-R").arg(&p_rev).spawn();
                    }),
                )
                .into_any_element(),
            );
            let p_copy = path.clone();
            items.push(
                ctx_item(
                    "Copy Path",
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            p_copy.to_string_lossy().into_owned(),
                        ));
                        this.close_context_menu(cx);
                    }),
                )
                .into_any_element(),
            );
            let p_trash = path.clone();
            items.push(
                ctx_item(
                    "Move to Trash",
                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.close_context_menu(cx);
                        this.move_to_trash(pane, p_trash.clone(), cx);
                    }),
                )
                .into_any_element(),
            );
            items.push(ctx_separator().into_any_element());
        }

        items.push(
            ctx_item(
                "New Folder",
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.new_folder(pane, cx);
                }),
            )
            .into_any_element(),
        );
        items.push(
            ctx_item(
                "New File",
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_context_menu(cx);
                    this.new_file(pane, cx);
                }),
            )
            .into_any_element(),
        );

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
                div()
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .min_w(px(200.0))
                    .py_1()
                    .bg(rgb(theme().surface))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme().border_strong))
                    .shadow_lg()
                    // Clicks inside the menu shouldn't close it via the backdrop.
                    .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| cx.stop_propagation())
                    .children(items),
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
        match item.action.clone() {
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

    /// Top-level key handling: Cmd+P toggles; while open, drive the palette.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let key = ks.key.as_str();

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

        if cmd && key == "p" {
            self.toggle_palette(window, cx);
            return;
        }
        // Tab management.
        if cmd && key == "t" {
            self.new_tab_in(self.active_pane, cx);
            return;
        }
        if cmd && key == "w" {
            let p = self.active_pane;
            let active = self.panes[p].active;
            self.close_tab(p, active, cx);
            return;
        }
        if key == "escape" && self.context_menu.is_some() {
            self.close_context_menu(cx);
            return;
        }
        if !self.palette_open {
            // Typing "/" in the listing opens the in-directory find filter.
            if !cmd && key == "/" {
                self.open_find(self.active_pane, cx);
            }
            return;
        }

        match key {
            "escape" => self.close_palette(cx),
            "enter" => self.activate_selection(cx),
            "down" => self.move_selection(1, cx),
            "up" => self.move_selection(-1, cx),
            "backspace" => {
                self.query.pop();
                self.refresh_palette(cx);
            }
            "v" if cmd => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.query.push_str(text.trim());
                    self.refresh_palette(cx);
                }
            }
            _ => {
                if cmd {
                    return; // ignore other Cmd-combos
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(|c| c.is_control()) {
                        self.query.push_str(ch);
                        self.refresh_palette(cx);
                    }
                }
            }
        }
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

        // Input line: query with a caret, or a dim placeholder.
        let input = if self.query.is_empty() {
            div()
                .text_color(rgb(t.text_dim))
                .child("Type a path, or a file/folder name…")
        } else {
            div()
                .text_color(rgb(t.text))
                .child(format!("{}\u{2502}", self.query))
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
        let tab = self.tab_mut(pane);
        tab.entries = read_entries(&dir);
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
        save_last_dir(&dir);

        self.recents.retain(|p| p != &dir);
        self.recents.insert(0, dir);
        self.recents.truncate(RECENTS_CAP);
        write_path_list("recents.txt", &self.recents);

        cx.notify();
        self.prewarm_icons(cx);
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
        self.tab_mut(pane).editing_path = Some(text);
        window.focus(&self.focus);
        cx.notify();
    }

    /// Keystrokes while the path bar is being edited (acts on the active pane).
    fn handle_path_edit_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
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
            "backspace" => {
                if let Some(s) = self.tab_mut(pane).editing_path.as_mut() {
                    s.pop();
                }
                cx.notify();
            }
            "c" if cmd => {
                if let Some(s) = &self.tab(pane).editing_path {
                    cx.write_to_clipboard(ClipboardItem::new_string(s.clone()));
                }
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(s) = self.tab_mut(pane).editing_path.as_mut() {
                        s.push_str(t.trim());
                    }
                    cx.notify();
                }
            }
            _ => {
                if cmd {
                    return; // leave other Cmd-combos alone
                }
                if let Some(ch) = ks.key_char.as_ref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        if let Some(s) = self.tab_mut(pane).editing_path.as_mut() {
                            s.push_str(ch);
                        }
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
        self.tab_mut(pane).find_query = Some(String::new());
        self.recompute_find(pane);
        cx.notify();
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
            "backspace" => {
                if let Some(s) = self.tab_mut(pane).find_query.as_mut() {
                    s.pop();
                }
                self.recompute_find(pane);
                cx.notify();
            }
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(s) = self.tab_mut(pane).find_query.as_mut() {
                        s.push_str(t.trim());
                    }
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
                        if let Some(s) = self.tab_mut(pane).find_query.as_mut() {
                            s.push_str(ch);
                        }
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
        cx.notify();
        self.prewarm_icons(cx);
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

    /// The floating filter box, anchored bottom-right while find is active.
    fn render_find_box(&self, pane: usize, query: &str) -> impl IntoElement {
        let t = theme();
        let count = self.tab(pane).find_results.len();
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
            .child(if query.is_empty() {
                div()
                    .min_w(px(80.0))
                    .text_color(rgb(t.text_dim))
                    .child("type to filter…")
            } else {
                div().min_w(px(80.0)).child(query.to_string())
            })
            .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(rgb(t.text)))
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(t.text_muted))
                    .text_xs()
                    .child(format!("{count}")),
            )
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
                let built = build_macos_icon(&path);
                ICON_CACHE.with(|cache| {
                    cache.borrow_mut().insert(key, built);
                });
                // Repaint so the freshly-built icon appears; stop if the view
                // is gone.
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                // Yield to the executor so frames can render between builds.
                cx.background_executor().timer(Duration::from_millis(1)).await;
            }
        })
        .detach();
    }

    /// Pin the current directory to bookmarks (no-op if already pinned).
    fn add_bookmark(&mut self, cx: &mut Context<Self>) {
        let dir = self.active_tab().current_dir.clone();
        if !self.bookmarks.iter().any(|b| b == &dir) {
            self.bookmarks.push(dir);
            write_path_list("bookmarks.txt", &self.bookmarks);
            cx.notify();
        }
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = self.active_tab().current_dir.clone();
        let current = current.as_path();
        let home = home_dir();
        let mut items: Vec<AnyElement> = Vec::new();
        let mut key = 0usize;

        // --- Recent ---
        items.push(section_header("RECENT").into_any_element());
        if self.recents.is_empty() {
            items.push(empty_hint("No recent folders").into_any_element());
        } else {
            for p in &self.recents {
                push_nav(&mut items, cx, &mut key, path_label(p), p.clone(), current);
            }
        }

        // --- Bookmarks (with a "+" to pin the current folder) ---
        items.push(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .pt_4()
                .pb_1()
                .child(div().text_xs().text_color(rgb(theme().text_dim)).child("BOOKMARKS"))
                .child(
                    div()
                        .id("add-bookmark")
                        .cursor_pointer()
                        .px_1()
                        .text_color(rgb(theme().text_dim))
                        .hover(|s| s.text_color(rgb(theme().text)))
                        .child("+")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.add_bookmark(cx);
                        })),
                )
                .into_any_element(),
        );
        if self.bookmarks.is_empty() {
            items.push(empty_hint("Click + to pin a folder").into_any_element());
        } else {
            for p in &self.bookmarks {
                push_nav(&mut items, cx, &mut key, path_label(p), p.clone(), current);
            }
        }

        // --- Favorites ---
        items.push(section_header("FAVORITES").into_any_element());
        let favorites: [(&str, PathBuf); 4] = [
            ("Applications", PathBuf::from("/Applications")),
            ("Pictures", home.join("Pictures")),
            ("Documents", home.join("Documents")),
            ("Downloads", home.join("Downloads")),
        ];
        for (label, path) in favorites {
            push_nav(&mut items, cx, &mut key, label.to_string(), path, current);
        }

        // --- Locations ---
        items.push(section_header("LOCATIONS").into_any_element());
        push_nav(
            &mut items,
            cx,
            &mut key,
            "Macintosh HD".to_string(),
            PathBuf::from("/"),
            current,
        );
        push_nav(&mut items, cx, &mut key, username(), home, current);

        div()
            .id("sidebar")
            .flex_none()
            .w(px(SIDEBAR_W))
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb_3()
            .bg(rgb(theme().sidebar))
            .border_r_1()
            .border_color(rgb(theme().border))
            .children(items)
    }

    /// The top bar for a pane: back/forward arrows then either the clickable
    /// breadcrumb or, in edit mode, an editable text field.
    fn render_path_bar(&self, pane: usize, cx: &Context<Self>) -> impl IntoElement {
        let tab = self.tab(pane);
        let can_back = tab.hist_pos > 0;
        let can_fwd = tab.hist_pos + 1 < tab.history.len();

        let content: AnyElement = if let Some(text) = &tab.editing_path {
            self.render_path_editor(text).into_any_element()
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
    fn render_path_editor(&self, text: &str) -> impl IntoElement {
        let t = theme();
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
            .child(div().min_w_0().child(text.to_string()))
            // Blinking would need a timer; a static caret reads clearly enough.
            .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(rgb(t.text)))
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
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.divider_drag = true;
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
    fn update_divider(&mut self, x: f32, cx: &mut Context<Self>) {
        if !self.divider_drag || self.panes.len() < 2 {
            return;
        }
        let content_w = (self.pane_list_width(0) + self.pane_list_width(1)).max(1.0);
        self.split_ratio = ((x - SIDEBAR_W) / content_w).clamp(0.2, 0.8);
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
        let tab = self.tab(pane);
        // In find mode the list shows the filtered results (no ".." row);
        // otherwise the full directory with a leading ".." when there's a parent.
        let find_active = tab.find_query.is_some();
        let has_parent = !find_active && tab.current_dir.parent().is_some();
        let item_count = if find_active {
            tab.find_results.len()
        } else {
            tab.entries.len() + usize::from(has_parent)
        };
        let scroll = tab.scroll_handle.clone();
        let h_scroll = tab.h_scroll.clone();
        // Total natural width of all columns (+ row horizontal padding). When the
        // pane is narrower than this, the columns scroll horizontally instead of
        // overflowing into the neighbouring pane.
        let total_w =
            self.widths.name + self.widths.kind + self.widths.date + self.widths.size + 24.0;
        // Highlight the active pane's border (only meaningful when split).
        let split = self.panes.len() > 1;
        let active = pane == self.active_pane;

        div()
            .flex()
            .flex_col()
            .min_w_0()
            .h_full()
            .when(split && active, |s| {
                s.border_color(rgb(theme().accent))
            })
            // Clicking anywhere in the pane focuses it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, _| {
                    this.active_pane = pane;
                }),
            )
            .child(self.render_tab_strip(pane, cx))
            // Path bar: back/forward arrows + clickable breadcrumb (or editor).
            .child(self.render_path_bar(pane, cx))
            // Body: clips to the pane; the vertical scrollbar + find box overlay it.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
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
                                    .child(self.column_header(cx))
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
                                            .child(uniform_list(
                    ("file-list", pane),
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let widths = this.widths;
                        let tab = this.tab(pane);
                        let find_active = tab.find_query.is_some();
                        let has_parent = !find_active && tab.current_dir.parent().is_some();
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
                                        row_key,
                                        widths,
                                        icon,
                                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            this.navigate_in(pane, parent.clone(), cx);
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
                            let target = base_dir.join(&name);
                            let ctx_target = target.clone();
                            let icon = icon_element(&target, is_dir);

                            items.push(
                                file_row(
                                    &name,
                                    is_dir,
                                    entry_size,
                                    modified,
                                    row_key,
                                    widths,
                                    icon,
                                    cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                        // Folders open on a single click; files
                                        // open (via `open`) on a double click.
                                        if is_dir {
                                            this.navigate_in(pane, target.clone(), cx);
                                        } else if ev.click_count() >= 2 {
                                            this.open_path(pane, target.clone(), false, cx);
                                        }
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
                    // Floating filter box, when this pane's find is active.
                    .children(
                        self.tab(pane)
                            .find_query
                            .as_ref()
                            .map(|q| self.render_find_box(pane, q).into_any_element()),
                    ),
            )
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
    fn column_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let w = self.widths;
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
            .child(header_cell("Name", w.name, Column::Name, ICON_W + 8.0, false, cx))
            .child(header_cell("Kind", w.kind, Column::Kind, 0.0, false, cx))
            .child(header_cell(
                "Date Modified",
                w.date,
                Column::Date,
                0.0,
                false,
                cx,
            ))
            .child(header_cell("Size", w.size, Column::Size, 0.0, true, cx))
            // Slack space after the last column.
            .child(div().flex_1())
    }
}

/// A header cell: a label plus a drag handle on its right edge that resizes the
/// column. `left_pad` aligns the Name label past the row icon; `align_right`
/// right-justifies (for Size).
fn header_cell(
    label: &str,
    width: f32,
    col: Column,
    left_pad: f32,
    align_right: bool,
    cx: &Context<Shuffle>,
) -> impl IntoElement {
    let mut label_box = div().flex_1().min_w_0().truncate();
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
        .child(label_box.child(label.to_string()))
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
        let mut root = div()
            .relative()
            .flex()
            .size_full()
            .bg(rgb(t.bg))
            .text_color(rgb(t.text))
            .text_sm()
            // Focusable so it receives key events (Cmd+P, palette typing).
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            // Track column drags anywhere in the window so the cursor can leave
            // the thin handle without dropping the resize.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let x = f64::from(ev.position.x) as f32;
                this.update_resize(x, cx);
                this.update_scroll_drag(f64::from(ev.position.y) as f32, cx);
                this.update_divider(x, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.end_resize();
                    this.end_scroll_drag();
                    this.divider_drag = false;
                }),
            )
            .child(self.render_sidebar(cx))
            .child(self.render_content(cx));

        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        if self.context_menu.is_some() {
            root = root.child(self.render_context_menu(cx));
        }
        root
    }
}

/// Build a sidebar nav item that navigates to `target`, and push it onto `items`.
fn push_nav(
    items: &mut Vec<AnyElement>,
    cx: &Context<Shuffle>,
    key: &mut usize,
    label: String,
    target: PathBuf,
    current: &Path,
) {
    *key += 1;
    let active = target.as_path() == current;
    let nav_target = target.clone();
    let item = nav_item(
        label,
        *key,
        active,
        cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.navigate_to(nav_target.clone(), cx);
        }),
    );
    items.push(item.into_any_element());
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
        |_window, cx| cx.new(|_cx| Settings::new()),
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
        .text_color(rgb(theme().text))
        .hover(|s| s.bg(rgb(theme().selected)))
        .child(label)
        .on_click(on_click)
}

fn ctx_separator() -> impl IntoElement {
    div().my_1().mx_2().h(px(1.0)).bg(rgb(theme().border_strong))
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

fn nav_item(
    label: String,
    key: usize,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let base = div()
        .id(("nav", key))
        .flex()
        .items_center()
        .mx_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(if active { t.text } else { t.text_muted }));
    let base = if active {
        base.bg(rgb(t.surface))
    } else {
        base.hover(|s| s.bg(rgb(t.hover)))
    };
    base.child(label).on_click(on_click)
}

fn section_header(title: &str) -> impl IntoElement {
    div()
        .px_3()
        .pt_4()
        .pb_1()
        .text_xs()
        .text_color(rgb(theme().text_dim))
        .child(title.to_string())
}

fn empty_hint(text: &str) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .text_color(rgb(theme().text_dim))
        .child(text.to_string())
}

/// One clickable listing row in the main pane: icon · name · kind · date · size.
fn file_row(
    name: &str,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
    key: usize,
    widths: ColumnWidths,
    icon: AnyElement,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_right: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let t = theme();
    let kind = kind_label(name, is_dir);
    let name_color = if is_dir { t.accent } else { t.text };
    let meta_color = rgb(t.text_muted);

    div()
        .id(("row", key))
        .flex()
        .items_center()
        .px_3()
        .py_1()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(t.hover)))
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
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(name_color))
                        .child(name.to_string()),
                ),
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
        // Date modified.
        .child(
            div()
                .flex_none()
                .w(px(widths.date))
                .pr_3()
                .truncate()
                .text_color(meta_color)
                .child(format_date(modified)),
        )
        // Size (right-aligned).
        .child(
            div()
                .flex_none()
                .w(px(widths.size))
                .flex()
                .justify_end()
                .text_color(meta_color)
                .child(format_size(is_dir, size)),
        )
        // Slack space after the last column (keeps row hover full-width).
        .child(div().flex_1())
        .on_click(on_click)
        .on_mouse_down(MouseButton::Right, on_right)
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
            .w(px(16.0))
            .h(px(16.0))
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
        let icon = build_macos_icon(&folder_dir_path());
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(FOLDER_KEY.to_string(), icon);
        });
    }
    if ICON_CACHE.with(|c| !c.borrow().contains_key(FILE_KEY)) {
        let icon = build_macos_icon(&file_probe_path());
        ICON_CACHE.with(|c| {
            c.borrow_mut().insert(FILE_KEY.to_string(), icon);
        });
    }
}

/// Ask NSWorkspace for `path`'s icon, decode it, and convert to a GPUI image.
/// This is the expensive part (AppKit + TIFF decode + resize), so it runs only
/// off the render path, in the background pre-warm task.
fn build_macos_icon(path: &Path) -> Option<Arc<RenderImage>> {
    let path_str = path.to_str()?;
    let tiff: Vec<u8> = {
        let workspace = NSWorkspace::sharedWorkspace();
        let ns_path = NSString::from_str(path_str);
        let image: objc2::rc::Retained<NSImage> = workspace.iconForFile(&ns_path);
        let data: objc2::rc::Retained<NSData> = image.TIFFRepresentation()?;
        if data.len() == 0 {
            return None;
        }
        data.to_vec()
    };

    let decoded = image::load_from_memory(&tiff).ok()?;
    // The TIFF carries a large representation (up to ~1024px). We only show
    // ~16px, so downscale to 32px (Triangle is fast and looks fine at this
    // size) — this caps each cached icon at a few KB instead of multiple MB.
    let decoded = decoded.resize_exact(32, 32, image::imageops::FilterType::Triangle);
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut raw = rgba.into_raw();
    // RenderImage expects BGRA; the decoded buffer is RGBA, so swap R and B.
    for px in raw.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(w, h, raw)?;
    let frame = image::Frame::new(buffer);
    Some(Arc::new(RenderImage::new(vec![frame])))
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

/// Read a directory's entries, sorted directories-first then case-insensitive
/// by name. Unreadable directories yield an empty list.
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
            entries.push(Entry {
                name,
                is_dir,
                size,
                modified,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
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
                if path.is_dir() {
                    paths.push(path);
                }
            }
        }
    }
    paths
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

    Application::new().run(|cx: &mut App| {
        // Load the saved theme into both the render-side copy and the global.
        let saved_theme = load_theme();
        set_active_theme(saved_theme);
        cx.set_global(ThemeGlobal(saved_theme));

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
                    title: Some("Shuffle".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    let finder = Shuffle::new(load_last_dir(), cx);
                    finder.prewarm_icons(cx);
                    finder.build_index(cx);
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
