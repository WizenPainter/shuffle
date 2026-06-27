//! finder2 — a fast, snappy file manager for Apple Silicon Macs.
//!
//! Milestone 2: a left sidebar of shortcuts.
//! - Recent: directories visited recently (tracked + persisted, most-recent first).
//! - Bookmarks: user-pinned directories (the "+" pins the current folder).
//! - Favorites: Applications, Pictures, Documents, Downloads.
//! - Locations: Macintosh HD (/) and the current user's home directory.
//! Clicking any item navigates the main listing. The active location is
//! highlighted. State lives in ~/Library/Application Support/finder2/.

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
    div, img, point, prelude::*, px, rgb, rgba, size, uniform_list, AnyElement, App, Application,
    Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, FocusHandle, ImageSource, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, RenderImage, ScrollHandle, ScrollWheelEvent,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use objc2_app_kit::{NSImage, NSWorkspace};
use objc2_foundation::{NSData, NSString};

const RECENTS_CAP: usize = 12;

// Default column widths for the main listing; all are user-resizable.
const ICON_W: f32 = 18.0;
const MIN_COL_W: f32 = 50.0;

// Command-palette result row height, and how many show before scrolling.
const PALETTE_ROW_H: f32 = 26.0;
const PALETTE_MAX_ROWS: usize = 7;

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

/// An in-progress scrollbar-thumb drag.
#[derive(Clone, Copy)]
struct ScrollDrag {
    start_y: f32,
    start_scrolled: f32,
}

/// Reserved cache key for the shared generic folder icon.
const FOLDER_KEY: &str = "\u{0}folder";

/// What activating a command-palette item does.
#[derive(Clone)]
enum Action {
    /// Open a path (navigate into it if a dir, else reveal its parent).
    Open(PathBuf, bool),
    /// Copy the current directory's path to the clipboard.
    CopyDir,
    /// Inert (e.g. "path not found").
    None,
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

/// The root view.
struct Finder2 {
    current_dir: PathBuf,
    entries: Vec<Entry>,
    recents: Vec<PathBuf>,
    bookmarks: Vec<PathBuf>,
    widths: ColumnWidths,
    resize: Option<Resize>,
    scroll_handle: UniformListScrollHandle,
    scroll_drag: Option<ScrollDrag>,
    // Command palette (Cmd+P).
    focus: FocusHandle,
    palette_open: bool,
    query: String,
    palette_items: Vec<PaletteItem>,
    selected: usize,
    search_gen: u64,
    palette_scroll: ScrollHandle,
}

impl Finder2 {
    fn new(dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let entries = read_entries(&dir);
        Self {
            current_dir: dir,
            entries,
            recents: read_path_list("recents.txt"),
            bookmarks: read_path_list("bookmarks.txt"),
            widths: ColumnWidths::default(),
            resize: None,
            scroll_handle: UniformListScrollHandle::new(),
            scroll_drag: None,
            focus: cx.focus_handle(),
            palette_open: false,
            query: String::new(),
            palette_items: Vec::new(),
            selected: 0,
            search_gen: 0,
            palette_scroll: ScrollHandle::new(),
        }
    }

    /// Current scrolled distance from the top, in pixels.
    fn current_scrolled(&self) -> f32 {
        let state = self.scroll_handle.0.borrow();
        (-(f64::from(state.base_handle.offset().y) as f32)).max(0.0)
    }

    fn begin_scroll_drag(&mut self, y: f32) {
        self.scroll_drag = Some(ScrollDrag {
            start_y: y,
            start_scrolled: self.current_scrolled(),
        });
    }

    fn update_scroll_drag(&mut self, y: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.scroll_drag else {
            return;
        };
        let state = self.scroll_handle.0.borrow();
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
            subtitle: self.current_dir.to_string_lossy().into_owned(),
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

        // Need at least 2 chars for a name search (avoids huge 1-char results).
        if q.chars().count() < 2 {
            self.palette_items = self.default_commands();
            cx.notify();
            return;
        }

        cx.notify();
        cx.spawn(async move |this, cx| {
            // Debounce: bail if a newer keystroke superseded us.
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let current = this.update(cx, |this, _| this.search_gen == gen).unwrap_or(false);
            if !current {
                return;
            }
            let hits = cx.background_spawn(async move { search_filesystem(&q) }).await;
            this.update(cx, |this, cx| {
                if this.search_gen != gen {
                    return;
                }
                this.palette_items = hits
                    .into_iter()
                    .map(|(name, path, is_dir)| {
                        let subtitle = path.to_string_lossy().into_owned();
                        PaletteItem {
                            title: name,
                            subtitle,
                            action: Action::Open(path, is_dir),
                            is_dir,
                        }
                    })
                    .collect();
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
                let text = self.current_dir.to_string_lossy().into_owned();
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.close_palette(cx);
            }
            Action::None => {}
        }
    }

    /// Top-level key handling: Cmd+P toggles; while open, drive the palette.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform;
        let key = ks.key.as_str();

        if cmd && key == "p" {
            self.toggle_palette(window, cx);
            return;
        }
        if !self.palette_open {
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
                let glyph = if matches!(item.action, Action::CopyDir) {
                    "⧉"
                } else if item.is_dir {
                    "📁"
                } else {
                    "📄"
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
                    base.bg(rgb(0x33334a))
                } else {
                    base.hover(|s| s.bg(rgb(0x2a2a30)))
                };
                base.child(div().flex_none().w(px(18.0)).child(glyph))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .min_w_0()
                            .flex_1()
                            .child(div().flex_none().text_color(rgb(0xf0f0f4)).child(item.title.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(0x7c7c86))
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
                .text_color(rgb(0x6b6b73))
                .child("Type a path, or a file/folder name…")
        } else {
            div()
                .text_color(rgb(0xffffff))
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
                    .bg(rgba(0x222228bf))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x3a3a44))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(rgb(0x3a3a44))
                            .child(div().flex_none().text_color(rgb(0x7aa2f7)).child("›"))
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
    fn scrollbar_thumb(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let state = self.scroll_handle.0.borrow();
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

        // Brighter while being dragged.
        let color = if self.scroll_drag.is_some() {
            rgba(0xffffff66)
        } else {
            rgba(0xffffff33)
        };

        Some(
            div()
                .id("scrollbar-thumb")
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
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.begin_scroll_drag(f64::from(ev.position.y) as f32);
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

    /// Navigate into `dir` if it is a directory: re-read its contents, record it
    /// as the most-recent location, and persist both the last dir and recents.
    fn navigate_to(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !dir.is_dir() {
            return;
        }
        self.entries = read_entries(&dir);
        self.current_dir = dir;
        save_last_dir(&self.current_dir);

        self.recents.retain(|p| p != &self.current_dir);
        self.recents.insert(0, self.current_dir.clone());
        self.recents.truncate(RECENTS_CAP);
        write_path_list("recents.txt", &self.recents);

        cx.notify();
        self.prewarm_icons(cx);
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
            for entry in &self.entries {
                if entry.is_dir {
                    has_dir = true;
                    continue;
                }
                let path = self.current_dir.join(&entry.name);
                if let Some(key) = icon_key(&path) {
                    // Skip types we've already built or already queued.
                    if cache.contains_key(&key) || !seen.insert(key.clone()) {
                        continue;
                    }
                    jobs.push((key, path));
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
        let dir = self.current_dir.clone();
        if !self.bookmarks.iter().any(|b| b == &dir) {
            self.bookmarks.push(dir);
            write_path_list("bookmarks.txt", &self.bookmarks);
            cx.notify();
        }
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = self.current_dir.as_path();
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
                .child(div().text_xs().text_color(rgb(0x6b6b73)).child("BOOKMARKS"))
                .child(
                    div()
                        .id("add-bookmark")
                        .cursor_pointer()
                        .px_1()
                        .text_color(rgb(0x6b6b73))
                        .hover(|s| s.text_color(rgb(0xffffff)))
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
            .w(px(220.0))
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb_3()
            .bg(rgb(0x17171a))
            .border_r_1()
            .border_color(rgb(0x2a2a30))
            .children(items)
    }

    fn render_main(&self, cx: &Context<Self>) -> impl IntoElement {
        let path_label = self.current_dir.display().to_string();
        // A leading ".." row exists only when there's a parent to go up to.
        let has_parent = self.current_dir.parent().is_some();
        let item_count = self.entries.len() + usize::from(has_parent);

        div()
            .flex_1()
            .flex()
            .flex_col()
            .min_w_0()
            // Path bar.
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x303036))
                    .text_color(rgb(0x9a9aa2))
                    .child(path_label),
            )
            // Column header.
            .child(self.column_header(cx))
            // Virtualized listing: only the visible rows (and their icons) are
            // ever built, so cost is constant regardless of directory size.
            // Wrapped in a relative container so the scrollbar can overlay it.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(uniform_list(
                    "file-list",
                    item_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let widths = this.widths;
                        let has_parent = this.current_dir.parent().is_some();
                        let mut items: Vec<AnyElement> = Vec::with_capacity(range.len());

                        for ix in range {
                            if has_parent && ix == 0 {
                                let parent =
                                    this.current_dir.parent().unwrap_or(Path::new("/")).to_path_buf();
                                let icon = icon_element(&parent, "..", true);
                                items.push(
                                    file_row(
                                        "..",
                                        true,
                                        0,
                                        None,
                                        ix,
                                        widths,
                                        icon,
                                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            this.navigate_to(parent.clone(), cx);
                                        }),
                                    )
                                    .into_any_element(),
                                );
                                continue;
                            }

                            let entry_ix = if has_parent { ix - 1 } else { ix };
                            let entry = &this.entries[entry_ix];
                            let name = entry.name.clone();
                            let is_dir = entry.is_dir;
                            let entry_size = entry.size;
                            let modified = entry.modified;
                            let target = this.current_dir.join(&name);
                            let icon = icon_element(&target, &name, is_dir);

                            items.push(
                                file_row(
                                    &name,
                                    is_dir,
                                    entry_size,
                                    modified,
                                    ix,
                                    widths,
                                    icon,
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.navigate_to(target.clone(), cx);
                                    }),
                                )
                                .into_any_element(),
                            );
                        }
                        items
                    }),
                )
                .size_full()
                .track_scroll(self.scroll_handle.clone())
                .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| cx.notify())),
                )
                .children(self.scrollbar_thumb(cx)),
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
            .text_color(rgb(0x6b6b73))
            .border_b_1()
            .border_color(rgb(0x303036))
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
    cx: &Context<Finder2>,
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
                .child(div().w(px(1.0)).h_full().bg(rgb(0x3a3a46)))
                .hover(|s| s.bg(rgb(0x34343e)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.begin_resize(col, f64::from(ev.position.x) as f32);
                        cx.notify();
                    }),
                ),
        )
}

impl Render for Finder2 {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .relative()
            .flex()
            .size_full()
            .bg(rgb(0x1e1e22))
            .text_sm()
            // Focusable so it receives key events (Cmd+P, palette typing).
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            // Track column drags anywhere in the window so the cursor can leave
            // the thin handle without dropping the resize.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.update_resize(f64::from(ev.position.x) as f32, cx);
                this.update_scroll_drag(f64::from(ev.position.y) as f32, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.end_resize();
                    this.end_scroll_drag();
                }),
            )
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx));

        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        root
    }
}

/// Build a sidebar nav item that navigates to `target`, and push it onto `items`.
fn push_nav(
    items: &mut Vec<AnyElement>,
    cx: &Context<Finder2>,
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

fn nav_item(
    label: String,
    key: usize,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fg = if active { 0xffffff } else { 0xb0b0b8 };
    let base = div()
        .id(("nav", key))
        .flex()
        .items_center()
        .mx_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgb(fg));
    let base = if active {
        base.bg(rgb(0x2f2f3a))
    } else {
        base.hover(|s| s.bg(rgb(0x232329)))
    };
    base.child(label).on_click(on_click)
}

fn section_header(title: &str) -> impl IntoElement {
    div()
        .px_3()
        .pt_4()
        .pb_1()
        .text_xs()
        .text_color(rgb(0x6b6b73))
        .child(title.to_string())
}

fn empty_hint(text: &str) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .text_color(rgb(0x55555c))
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
) -> impl IntoElement {
    let kind = kind_label(name, is_dir);
    let name_color = if is_dir { 0x7aa2f7 } else { 0xe0e0e6 };
    let meta_color = rgb(0x8a8a92);

    div()
        .id(("row", key))
        .flex()
        .items_center()
        .px_3()
        .py_1()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x2a2a30)))
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

/// Emoji fallback when a real macOS icon can't be produced.
fn icon_glyph(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "📁";
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    match ext.as_deref() {
        Some("xlsx" | "xls" | "xlsm" | "xlsb") => "📊",
        Some("docx" | "doc") => "📝",
        Some("pptx" | "ppt") => "📽",
        Some("pdf") => "📕",
        Some("csv" | "tsv") => "📑",
        Some("dwg" | "dxf") => "📐",
        Some("png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "heic" | "webp" | "svg") => "🖼",
        Some("mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v") => "🎬",
        Some("mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg") => "🎵",
        Some("zip" | "tar" | "gz" | "7z" | "rar" | "bz2" | "xz" | "dmg") => "🗜",
        Some("app") => "📦",
        _ => "📄",
    }
}

/// Build the icon element for an entry: a real macOS file-type icon when we can
/// fetch one, otherwise an emoji fallback. Directories always use the folder
/// emoji (per design — folder icon stays as-is for now).
fn icon_element(path: &Path, name: &str, is_dir: bool) -> AnyElement {
    // Cache-only lookup — never build on the render thread. Missing icons show
    // the emoji placeholder until the background pre-warm fills them in.
    // Directories share one generic macOS folder icon; files key by extension.
    let handle = if is_dir {
        lookup_cached(FOLDER_KEY)
    } else {
        lookup_icon(path)
    };
    if let Some(handle) = handle {
        return img(ImageSource::Render(handle))
            .w(px(16.0))
            .h(px(16.0))
            .into_any_element();
    }
    div().child(icon_glyph(name, is_dir)).into_any_element()
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

/// Spotlight-backed name search: gather candidates with `mdfind`, then fuzzy
/// rank by filename. Runs on a background thread (blocking is fine there).
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
        if let Some(score) = fuzzy_score(query, &name) {
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
        .map(|h| PathBuf::from(h).join("Library/Application Support/finder2"))
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

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("finder2".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| {
                    let finder = Finder2::new(load_last_dir(), cx);
                    finder.prewarm_icons(cx);
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
