# FerrisExplorer

A fast, keyboard-first file explorer: a Rust core in a [Tauri](https://tauri.app) shell with a React UI,
with a SQLite-backed search index that stays current while the app runs.

## Features

- Tabs, split pane, file preview, and a command palette (`⌘K` / `Ctrl+K`)
- Search across the whole disk from an index, with filters (below)
- Live index: filesystem watchers keep it fresh, and it re-syncs changes made while the app was closed
- Space usage finder for finding what is eating a disk
- Copy / move / duplicate / compress with progress and cancel; git status marks

## Install

Download from the [Releases](https://github.com/justcommiting/FerrisExplorer-releases/releases) page.

Builds are unsigned:
- **macOS**: Right-click the app, Open, the first time (or `xattr -dr com.apple.quarantine /Applications/FerrisExplorer.app`).
- **Windows**: SmartScreen, More info, Run anyway.

Verify downloads against `SHA256SUMS.txt` (`shasum -a 256 -c SHA256SUMS.txt --ignore-missing` on macOS/Linux, `Get-FileHash` on Windows).

## Build and run

Requires a stable Rust toolchain and Node.js 20+. On Windows, the WebView2 runtime
(preinstalled on Windows 11) and the MSVC build tools.

```sh
npm install
npm run tauri dev     # development, with hot reload
npm run tauri build   # release bundle in src-tauri/target/release/bundle
```

## Layout

| Path | What lives there |
| --- | --- |
| `src-tauri/src` | Rust core: index, search, watchers, file ops, git. `api.rs` is the command surface; `state.rs` forwards background results to the UI as events |
| `src` | React UI (Vite, Tailwind, shadcn/ui on Base UI). `store/explorer.ts` holds tab and app state; `commands.ts` is the command catalog shared by the palette, menus and shortcuts |

## Searching

Type in the search box. Free text matches names; `key:value` filters combine:

| Filter | Example |
| --- | --- |
| Extension / type | `ext:rs`, `type:pdf` |
| Size | `size:>1.5gb`, `size:<100kb` |
| Modified | `modified:<7d`, `modified:today` |
| Name | `name:"Cargo"` |
| Contents | `content:"TODO"` |
| Git state | `git:modified` |
| Duplicates | `duplicate:true` |
| Current folder only | `here:` |

Unknown or malformed filters are reported rather than silently ignored.

## Data and files

Everything lives in your home directory:

| File | Purpose |
| --- | --- |
| `.ferris_explorer_index.db` | search index (safe to delete; rebuild with *Rebuild Index*) |
| `.ferris_explorer_index.db.corrupt` | a damaged index, moved aside automatically |
| `.ferris_explorer_saved_searches.json`, favorites, etc. | settings |
| `.ferris_explorer_crash.log` | panic reports, including backtraces; attach to bug reports |
| `.ferris_explorer_plugins.json` | which plugin files are enabled, by content hash |
| `plugins/*.toml` in the same folder | plugin manifests, see [Plugins](#plugins) |

Logs go to the OS app-log folder: `~/Library/Logs/dev.ferrisexplorer.app` on macOS,
`%LOCALAPPDATA%\dev.ferrisexplorer.app\logs` on Windows, `~/.local/share/dev.ferrisexplorer.app/logs`
on Linux (each file is capped at 5 MB, and only the latest is kept). A crash log is also written to
`~/.ferris_explorer_crash.log`. The app sends no telemetry; its only network request is the update
check against GitHub Releases.

Delete moves items to the system Trash (Shift+Delete, or Shift+⌘⌫ on macOS, deletes permanently
with a confirmation). Undo (⌘Z / Ctrl+Z) reverses renames, new folders, moves, copies, duplicates
and trashing; Redo (⇧⌘Z / Ctrl+Y) reapplies them. Undo restores trashed items on Windows and
Linux; on macOS it cannot put them back, so restore those from the Trash in Finder. Permanent
deletes cannot be undone. Renames, moves and undo refuse to overwrite an existing file, and the
app refuses to delete a filesystem root or your home directory, whether to the Trash or for good.

## Updates

On start, FerrisExplorer checks GitHub Releases for a newer version and shows a bar if one exists.
Nothing installs until you click *Update and restart*, and every update is signature-verified
against the public key built into the app. The first install of an unsigned build still triggers
Gatekeeper (macOS) or SmartScreen (Windows). Updates come from the public
[FerrisExplorer-releases](https://github.com/justcommiting/FerrisExplorer-releases/releases) repository;
v0.3.1 cannot self-update because its feed URL is private, so install v0.3.2 manually once.

## Plugins

A plugin is a TOML manifest in `~/.ferris_explorer/plugins/`, one file per plugin. It adds
commands to the command palette and file previews to the Details panel. A manifest contains no
code that runs in the window: each command is a shell template that runs through the same
planning, quoting, cancel and output limits as *Run Command on Selection*. See
[`docs/plugins/example.toml`](docs/plugins/example.toml) for a complete file.

```toml
name = "Examples"          # required, up to 80 bytes
version = "1.0.0"          # required
description = "..."        # optional, up to 500 bytes

[[action]]
id = "sha256"              # required: a-z, 0-9, _ or -; unique within the plugin
label = "SHA-256 checksum" # required, shown in the palette as "Examples: SHA-256 checksum"
command = "shasum -a 256 {}"
when = { extensions = [], kind = "files", min = 1 }   # all optional, see below
confirm = false            # true: show the exact command lines and wait for Run
shortcut = "Ctrl+Shift+H"  # optional; bound unless it collides with a built-in

[[previewer]]
extensions = ["json"]      # required, letters and digits, up to 50 entries
command = "jq . {}"        # required, must contain {}
timeout_ms = 3000          # optional, 1 to 10000, default 3000
max_output = 262144        # optional, 1 to 2097152 bytes, default 256 KiB
```

Command templates use the same placeholders as *Run Command on Selection*: `{}` (each path),
`{+}` (all paths in one run), `{dir}`, `{name}`, `{stem}` and `{ext}`. Paths are quoted for you,
so do not put quotes around a placeholder; a manifest that does is rejected. Each command must
fit on one line.

`when` decides when an action appears and runs:

- `kind`: `"any"` (default), `"files"` or `"dirs"`. Every selected item must be of that kind.
- `extensions`: when not empty, every selected item must be a file with one of these extensions
  (case does not matter; the leading dot is optional).
- `min` (default 1) and `max` (no limit by default): how many items are selected.

**Trust.** A plugin does nothing until you enable it. On first load, every plugin is disabled.
Open *Plugins* (Ctrl+K, then "Plugins…") to read its commands and enable it. The app stores a
SHA-256 hash of the enabled file's exact bytes. If the file changes in any way, the plugin is
disabled again until you review and enable it. Enabling is refused if the file changed after
the window showed it. Use *Reload Plugins* to pick up new or edited files.

**Limits.** A manifest is at most 64 KiB, and at most 50 plugin files load (any others are listed
as errors). A manifest that fails to parse or validate is listed in the Plugins window with its
error and loads nothing; the rest still load. Unknown keys are errors, so typos are caught.

**Previewers.** An enabled previewer whose extensions match the selected file runs in the
background when you select it, and its output replaces the built-in preview as plain text. A
selection change cancels a previewer that is still running, and a previewer that runs past
`timeout_ms` has its process group killed. Archives are not previewed by plugins.

Matching actions also appear in the file context menu. A `shortcut` (Ctrl/⌘ plus a letter, digit,
F1–F12, Enter, Space, Delete or Backspace) is bound unless a built-in or earlier plugin already uses
it; the Plugins overlay shows which shortcuts were bound.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check        # dependency advisories; policy in deny.toml
```

## Privacy

FerrisExplorer contains no telemetry or analytics. The only network request the app makes on its own is the update check to GitHub Releases. All data stays in local files, see [Data and files](#data-and-files).

## License

[MIT](LICENSE)
