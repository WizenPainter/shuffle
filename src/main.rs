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
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use gpui::{
    div, img, prelude::*, px, rgb, size, uniform_list, AnyElement, App, Application, Bounds,
    ClickEvent, Context, CursorStyle, ImageSource, MouseButton, MouseDownEvent, MouseMoveEvent,
    RenderImage, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use objc2_app_kit::{NSImage, NSWorkspace};
use objc2_foundation::{NSData, NSString};

const RECENTS_CAP: usize = 12;

// Default column widths for the main listing; all are user-resizable.
const ICON_W: f32 = 18.0;
const MIN_COL_W: f32 = 50.0;

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
}

impl Finder2 {
    fn new(dir: PathBuf) -> Self {
        let entries = read_entries(&dir);
        Self {
            current_dir: dir,
            entries,
            recents: read_path_list("recents.txt"),
            bookmarks: read_path_list("bookmarks.txt"),
            widths: ColumnWidths::default(),
            resize: None,
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
            for entry in &self.entries {
                if entry.is_dir {
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
            .child(
                uniform_list(
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
                .flex_1(),
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
        div()
            .flex()
            .size_full()
            .bg(rgb(0x1e1e22))
            .text_sm()
            // Track column drags anywhere in the window so the cursor can leave
            // the thin handle without dropping the resize.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.update_resize(f64::from(ev.position.x) as f32, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.end_resize()),
            )
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
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
    if !is_dir {
        // Cache-only lookup — never build on the render thread. Missing icons
        // show the emoji placeholder until the background pre-warm fills them in.
        if let Some(handle) = lookup_icon(path) {
            return img(ImageSource::Render(handle))
                .w(px(16.0))
                .h(px(16.0))
                .into_any_element();
        }
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

/// Read a previously-built icon from the cache. Never builds (so it's safe to
/// call every frame from `render`).
fn lookup_icon(path: &Path) -> Option<Arc<RenderImage>> {
    let key = icon_key(path)?;
    ICON_CACHE.with(|cache| cache.borrow().get(&key).cloned().flatten())
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
            |_window, cx| {
                cx.new(|cx| {
                    let finder = Finder2::new(load_last_dir());
                    finder.prewarm_icons(cx);
                    finder
                })
            },
        )
        .unwrap();

        cx.activate(true);
    });
}
