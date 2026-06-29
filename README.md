<div align="center">

<img src="logo2.png" width="120" alt="Shuffle logo" />

# Shuffle

**A fast, keyboard-driven file manager for macOS.**

An open-source alternative to Finder, Commander One, Path Finder, ForkLift, and
other macOS file explorers — built to feel instant.

</div>

---

## Why Shuffle?

macOS file managers tend to be either too limited (Finder) or heavy and slow.
Shuffle takes the opposite approach, inspired by [File Pilot](https://filepilot.tech)
on Windows: a single-binary, GPU-rendered file manager that stays snappy no
matter how big the directory or how fast you move.

- **Instant.** GPU-rendered UI (Metal) with a fully virtualized file list — only
  the visible rows are ever built, so a folder with 100,000 items scrolls as
  smoothly as one with 10.
- **Keyboard-first.** A `Cmd+P` command palette with millisecond fuzzy search
  over your home directory, typo tolerance, and live path browsing.
- **Real macOS look.** Genuine Finder file-type icons pulled straight from the
  system, accurate Kind / Date Modified / Size columns, resizable to taste.
- **Lightweight.** Ships as one small native `.app`. No Electron, no web view.

## Features

- 📂 **Familiar layout** — sidebar (Recent, Bookmarks, Applications, Pictures,
  Documents, Downloads, Macintosh HD, Home) and a sortable, resizable detail view.
- 🧭 **Smart breadcrumb path bar** — click any segment to jump there; the path
  you came from stays visible (grayed out) so you can dive back in with one click.
- ◀ ▶ **Back / forward navigation** — browser-style history with arrow buttons.
- ✏️ **Editable address bar** — click the empty part of the path bar to type,
  paste, or copy a path directly.
- ⚡ **Command palette (`Cmd+P`)** — fuzzy-find any file or folder, copy/paste a
  directory to navigate, run app commands (e.g. type `settings`), all in real time.
- 🖱️ **Right-click context menu** — Open, Reveal in Finder, Copy Path, Move to
  Trash, New Folder, New File.
- 🖼️ **Real file icons & metadata** — Kind ("Microsoft Excel", "DWG File", …),
  Date Modified, and Size, with drag-to-resize columns.
- 💾 **Remembers where you were** — reopens in your last directory; tracks recents
  and bookmarks.

## Platform support

Shuffle is built for **macOS**.

- **Apple Silicon (M-series) is the primary target** and where it's tuned and
  tested — that's where the speed shines.
- It is a standard Cocoa/Metal app and should run on **any modern Mac**
  (macOS 12 Monterey or later). Intel Macs can build from source; the only hard
  requirement is a Metal-capable GPU (every supported Mac has one).

> Currently distributed as a build-from-source app. Universal/Intel release
> binaries may follow.

## Building from source

Shuffle is written in **Rust** using [GPUI](https://www.gpui.rs/) (the GPU UI
framework behind the Zed editor).

### Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- Xcode Command Line Tools
- The **Metal Toolchain** (GPUI compiles its shaders at build time):

  ```sh
  xcodebuild -downloadComponent MetalToolchain
  ```

### Build & run

```sh
# Clone
git clone <your-fork-url> shuffle
cd shuffle

# Quick run (debug)
cargo run

# Optimized build
cargo build --release

# Assemble a proper Shuffle.app bundle (signed, with icon)
./make_app.sh
open ./Shuffle.app
```

Running as a real `.app` bundle (rather than the bare binary) gives the process
normal OS scheduling priority and lets macOS remember granted folder/privacy
permissions across launches.

## Keyboard shortcuts

| Shortcut      | Action                                |
| ------------- | ------------------------------------- |
| `Cmd+P`       | Open the command palette / fuzzy find |
| `Cmd+,`       | Open Settings                         |
| `Cmd+Q`       | Quit                                  |
| `↑` / `↓`     | Move selection in the palette         |
| `Enter`       | Open the selected item / path         |
| `Esc`         | Close palette / cancel path edit      |

## Tech stack

- **Rust** — single-binary, memory-safe core.
- **GPUI** — GPU-accelerated, Metal-backed UI.
- **AppKit (via objc2)** — real system icons (`NSWorkspace`), Trash, etc.
- **jwalk + rayon** — parallel directory walking and fuzzy ranking for the
  in-memory search index.

## Status

Early but usable. Active development. Contributions, issues, and ideas are
welcome.

## License

Open source. See [LICENSE](LICENSE) (add your preferred license — MIT or
Apache-2.0 recommended).
