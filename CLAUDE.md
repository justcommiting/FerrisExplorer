# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Frontend (run from repo root):
- `npm install`
- `npm run tauri dev` - app with hot reload (Vite on port 1420)
- `npm run tauri build` - release bundle in `src-tauri/target/release/bundle`
- `npm run build` - `tsc -b` typecheck, then `vite build` into `dist/`. Run this before any cargo command: `tauri::generate_context!` embeds `dist/`, so it must exist.
- `npm run lint` - oxlint (`.oxlintrc.json`: react hooks rules, only-export-components)
- There is no frontend test runner; no test files exist under `src/`.

Rust (run from `src-tauri/`, or pass `--manifest-path src-tauri/Cargo.toml`):
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- Single test: `cargo test <name_substring>` (e.g. `cargo test git_commit`)
- Ignored perf tests (in `core.rs`): `cargo test --release -- --ignored perf_ --nocapture`
- `cargo deny check --all-features` (CI runs this; policy in `src-tauri/deny.toml`)

CI (`.github/workflows/ci.yml`) runs, in order: `npm run build`, `npm run lint`, `cargo fmt --check`, clippy with `-D warnings`, `cargo test`, and cargo-deny, on ubuntu-22.04, macOS and Windows. Make all of these pass before reporting work as done.

## Architecture

**Frontend to backend.** `src/lib/api.ts` exports an `api` object of typed `invoke(...)` wrappers, one per command (e.g. `listDir: (path) => invoke('list_dir', { path })`). The commands are `#[tauri::command]` functions in `src-tauri/src/api.rs`. Adding a command needs both: the function in `api.rs` and its entry in the `generate_handler![...]` list in `src-tauri/src/main.rs`. A command missing from that list fails only at runtime.

**Blocking work.** Commands that touch disk run on Tauri's blocking pool (`api.rs` uses `spawn_blocking`; the `blocking()` helper wraps it). Keep sync commands off the main thread; `watch_dirs` and `request_dir_sizes` were made `async` for this reason.

**Background results to the UI.** Workers report over `mpsc` channels. `state::spawn_pump` (in `state.rs`) runs one thread that drains those channels every `PUMP_INTERVAL` and converts each result into a Tauri event. Events include `index-changed`, `index-status`, `dir-changed`, `op-progress`, `duplicates-done`, `sync-progress`/`sync-done`, `shell-lines`/`shell-exit`/`shell-done`. Some events are emitted directly from the command, not through the pump: streamed listings emit `listing-chunk`.

**Streamed listings.** `api::list_dir_stream` lists a folder on the blocking pool and emits `listing-chunk` batches (`core.rs`: first batch 2,000 entries, then 5,000 each). `ListingRegistry` (in `state.rs`) keys cancel flags by tab and request id; starting a listing cancels the tab's previous one. The frontend drops batches whose request id is stale. Errors arrive as a final chunk carrying the error text.

**Shared state.** `AppState` (`state.rs`) is registered with `.manage()` in `main.rs`. It holds `Mutex`-guarded registries (listings, watcher, op cancels, duplicate and space scans, index workers) and `Arc<AtomicBool>` cancel flags. Cancellation is cooperative: long jobs check their flag between entries.

**Workers** (`workers.rs`): `DirSizeWorker` (a small fixed pool, `DIR_SIZE_WORKERS = 4`, fed by a priority `SizeQueue`), `IndexWorker`, `IndexMaintainer` (live index: `start_live` runs its own thread), `AnalyzerWorker`.

**File watching** (`watch.rs`): one long-lived `notify` thread. The UI sends `WatchCmd::Set(paths)` with the visible folders, and debounced changed-parent paths come back. `is_own_file` filters out FerrisExplorer's own files (`.ferris_explorer*`) so they do not trigger refreshes.

**Persistence.**
- Search index: SQLite at `~/.ferris_explorer_index.db`, managed by `db.rs` (`DbManager`, `SCHEMA_SQL`, `migrate_schema`). Foreign keys must be enabled on every connection (`open_db`). `index_generation()` is bumped after each write and drives the UI's `index-changed` event. Corrupt databases are quarantined (`quarantine_index`). Delete with `remove_index_files` so `-wal`/`-shm` go too.
- Other state: JSON stores in the home directory, one `*Store` struct per file in `persistence.rs` (favorites, saved searches, search and shell history, frecency, key prefs, workspaces, plugin trust). Paths come from `sys::get_home_dir()`.

**Plugins** (`plugins.rs`, format in the README "Plugins" section, sample at `docs/plugins/example.toml`):
- Manifests are TOML files in `~/.ferris_explorer/plugins/`. Limits: 64 KiB each, 50 files. Unknown keys are errors, and an invalid file loads nothing.
- A manifest holds no code that runs in the webview. Actions are shell templates; `{}`, `{+}`, `{dir}`, `{name}`, `{stem}`, `{ext}` are quoted by `shell.rs`, and a template that quotes a placeholder is rejected at load.
- Trust: each plugin is disabled on load. `PluginTrust` (`persistence.rs`) stores the SHA-256 of the exact enabled bytes; any change disables the plugin again. Enable and run calls must pass the hash the user was shown.
- Previewers run in the background on selection, are cancelled when the selection changes, and have their process group killed after `timeout_ms`.

**Shell execution** (`shell.rs`): placeholders are replaced with shell-quoted paths and everything else goes through verbatim. On Unix the line runs via `$SHELL -c` (only sh, bash, zsh, dash or ksh, since their single quotes are literal) in its own process group; on Windows via `cmd`. Output is capped (`OUTPUT_CAP`, 2 MiB), one run at a time, and `refuse_archive` blocks commands run inside an archive or on archive entries (archives are read-only). Plugin actions use this same planning path.

**Security model.**
- `src-tauri/capabilities/default.json` grants only `core:default` and a few window permissions to `main` and `explorer-*` windows. File access is not granted to the webview; it goes through Rust commands.
- CSP in `src-tauri/tauri.conf.json` restricts scripts and resources to `'self'` and IPC.
- Rename, move and undo refuse to overwrite. Deleting a filesystem root or the home directory is refused, whether to the Trash or permanently (see README "Data and files").

## Tests

- Unit tests live in `#[cfg(test)] mod tests` blocks inside each module; there is no `tests/` directory.
- Use `crate::testutil::TempDir` (`src-tauri/src/testutil.rs`) for temporary trees: `TempDir::new("tag")`, then `.file(name, bytes)` and `.dir(name)`. It is removed on drop, and permissions are restored first so unreadable test directories still get cleaned up.
- Tests that touch git use real repositories created in a `TempDir` (see `git.rs`).

## Gotchas

- `claude-notes.md` is an improvement log, not a spec. Its "Remaining findings" are open items to verify before acting on them.
- `claude-notes.md` covers several architectures (the early iterations predate the Tauri/React rewrite), so check that a named type still exists before following a note.
- Sorting stays in JS on purpose (100k rows takes about 93 ms, once per listing). `FileList` and `ColumnView` memoise on items, sort, filter and sizes; while streaming, sort once at the end, not after each batch.
- Linux CI needs the webkit2gtk-4.1 system packages listed in `ci.yml`.
