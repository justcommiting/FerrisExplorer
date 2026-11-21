# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2] - 2026-11-14

### Fixed
- Downloads and the update feed now come from the public FerrisExplorer-releases repository; v0.3.1 installs must be updated manually once.

## [0.3.1] - 2026-11-14

### Added
- In-app auto-updater (click to install, signature-verified).
- Persistent log files and a crash recovery screen.
- SHA256SUMS.txt published with releases.

### Changed
- Releases now require CI to pass and the tag to match the app version.

## [0.3.0] - 2026-11-14

### Added
- Miller columns view mode per tab with keyboard navigation and preview column.
- Rich file previews supporting syntax highlighting, markdown rendering, bounded media playback, and hex view.
- Document content search inside PDF, Word (docx), Excel (xlsx), PowerPoint (pptx), and OpenDocument files.
- Keyboard power-user layer with type-ahead list navigation, quick filter, frecency Jump to Folder, and shortcut cheat sheet.
- Run Command on Selection with placeholder expansion and streaming terminal panel.
- Built-in Git support for staging, unstaging, discarding changes, committing, blame viewer, and ignored file dimming.
- Workspace layouts to save, restore, and manage custom multi-tab and split window configurations.
- Plugin system supporting custom file actions and previewers configured via TOML manifests.
- Duplicate Finder view with content-confirmed matching and automated cleanup helpers.
- Folder comparison and two-way sync with interactive diff previews.
- Smart Folders in the sidebar for pinned saved searches that refresh automatically on index changes.
- Space usage history tracking with "What Grew Recently" comparisons across days, weeks, or months.
- Bulk rename dialog with token patterns, regex find/replace, live preview, and collision avoidance.
- Virtual folder browsing and safe extraction for zip, tar, tar.gz, tar.bz2, and tar.xz archives.
- Drag-and-drop file transfers to and from external applications.
- Multi-step undo and redo support for file moves, copies, duplicates, trash actions, and renames.

### Changed
- File deletion now moves items to the OS trash by default, retaining permanent deletion via Shift+Delete.
- Streamed directory listings for responsive navigation in folders with hundreds of thousands of files.
- Atomic rename-without-replace across macOS, Linux, and Windows to eliminate rename race conditions.

### Fixed
- Fixed potential data loss when renaming a symlink onto its own target.
- Display clear, user-friendly error messages when directories are unreadable.
- Fixed window race conditions and directory chunk routing across multiple tabs and windows.
- Ensured superseded directory navigations and searches cancel cleanly when switching tabs.

### Security
- Replaced direct shell string interpolation in terminal launching and file opening with secure system APIs.
- Restricted shell command placeholders to reject backticks, heredocs, and embedded user quotes.
- Enforced SHA-256 integrity validation on plugin manifests before enabling or executing actions.
- Hardened file copy and archive extraction to never write through or overwrite existing symlinks.
- Added decompression limits and path traversal guards against parent directory escapes in archive browsing.

## [0.2.1] - 2026-11-13

### Added
- Initial release built on Tauri 2 and React with native multi-window and tab management.
- Live SQLite-backed file index with real-time filesystem change tracking.
- Fast disk search supporting filename, path, size, and date filters.
- Space usage analyzer to identify large files and folders.
- Automatic folder size calculation with background scanning and cancellation.
- Basic file operations including copy, move, duplicate, and compress with progress tracking.
- Integrated title bar into the tab strip on macOS and Windows.
- Keyboard-first navigation and shortcuts throughout the interface.

### Changed
- Migrated user interface framework from egui to Tauri 2 and React.

### Fixed
- Fixed Space Usage Finder hanging indefinitely on index analysis.
- Fixed directory listing performance and scroll snapping in large folders.
- Fixed search query parsing bugs and tab routing for background scan results.

[0.3.2]: https://github.com/justcommiting/FerrisExplorer-releases/releases/tag/v0.3.2
[0.3.1]: https://github.com/justcommiting/FerrisExplorer-releases/releases/tag/v0.3.1
[0.3.0]: https://github.com/justcommiting/FerrisExplorer-releases/releases/tag/v0.3.0
[0.2.1]: https://github.com/justcommiting/FerrisExplorer-releases/releases/tag/v0.2.1
