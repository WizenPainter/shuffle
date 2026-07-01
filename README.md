<div align="center">

<img src="logo2%20Background%20Removed.png" width="120" alt="Shuffle logo" />

# Shuffle

**A modern file explorer for macOS, built for speed.**

Inspired by [File Pilot](https://filepilot.tech) on Windows: an open source,
GPU rendered file manager focused on the best performance, the smoothest feel,
and a deep set of power user features and customization.

</div>

## Philosophy

macOS file managers are either too limited or too slow. Shuffle is a single,
small, native app that stays instant no matter how big the directory or how fast
you move, and gives power users real tools instead of getting in the way.

* **Fast.** GPU rendered (Metal), fully virtualized lists. A folder with 100,000
  items scrolls as smoothly as one with 10.
* **Smooth.** Native drag and drop, no jank, no Electron.
* **Powerful and customizable.** The features below, plus a theme system to make
  it yours.

## Highlights

* 🪟 **Dual panes / split canvas.** Drag a tab to the edge to split the window
  into two side by side panes, each with its own tabs, history, and filter, and a
  draggable divider resizes them.
* 🗂️ **Tabs.** `Cmd+T` or `+`, with smooth drag to reorder and drag between panes.
* ⚡ **Command palette (`Cmd+P`).** Millisecond fuzzy search over your home
  directory with typo tolerance, live path browsing, and quick commands.
* ⌨️ **Terminal mode.** An optional command bar at the bottom to move through the
  explorer like a shell (`cd` navigates, Tab autocompletes paths and commands).
* 👁️ **Preview and Information.** Single click a file to preview it (QuickLook:
  images, PDFs, docs) and inspect its details in a side inspector.
* 🔎 **In-place filter (`/`).** Instantly narrow the current folder, typo tolerant.
* ☁️ **Cloud and servers.** Dropbox, Google Drive, OneDrive, and iCloud show up
  automatically, alongside mounted volumes and a Connect to Server dialog.
* 🎨 **Deep theming.** Dozens of preset palettes (Catppuccin, Dracula, Nord,
  Gruvbox, Solarized, Tokyo Night, and bold single hue themes) plus per color
  customization, applied live.

Optional features (Terminal, Preview, Information) live in **Settings › General**
and are off by default.

## Platform support

Built for **macOS**, tuned for **Apple Silicon (M series)**. It is a standard
Metal app and runs on any modern Mac (macOS 12+). Intel builds from source.

## Building from source

Written in **Rust** with [GPUI](https://www.gpui.rs/) (the GPU UI framework
behind the Zed editor).

```sh
# Prerequisites: Rust (rustup), Xcode CLT, and the Metal toolchain:
xcodebuild -downloadComponent MetalToolchain

cargo run                 # debug
cargo build --release     # optimized
./make_app.sh && open ./Shuffle.app   # packaged .app bundle
```

## Keyboard shortcuts

| Shortcut | Action                       |
| :------- | :--------------------------- |
| `Cmd+P`  | Command palette / fuzzy find |
| `/`      | Filter the current directory |
| `Cmd+T`  | New tab                      |
| `Cmd+W`  | Close tab                    |
| `Cmd+,`  | Settings                     |

## Status

Early but usable, under active development. Contributions and ideas welcome.

## License

[MIT](LICENSE) © Jaime Guzman
