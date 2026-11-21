# FerrisExplorer — improvement log

Iterative pass over the codebase. Each iteration: pick the single
highest-impact improvement, implement it, then gate on
`cargo build` → `cargo test` → `cargo clippy -- -D warnings` → `cargo fmt`.

## Baseline (before iteration 1)

| Gate | State |
| --- | --- |
| `cargo build` | passes, 5 warnings (`f32`/`f64` trait-bound fallback) |
| `cargo test` | **0 tests** |
| `cargo clippy -- -D warnings` | **20 errors** |
| `cargo fmt` | never run — whole tree unformatted |

---

## Iteration 1 — Search query parsing correctness + first test suite

**Why this first.** Search is the app's headline feature and its query parser
was silently wrong. With zero tests, no later refactor could be verified, so
the safety net had to come first.

### Bugs fixed

1. **Operator-precedence bug in `parse_size`** (the big one).

   ```rust
   val.parse::<f64>().unwrap_or(0.0) as u64 * 1_000_000
   ```

   The cast to `u64` binds tighter than `*`, so the fractional part was
   discarded *before* scaling: `size:>1.5mb` meant **1 MB**, `size:>0.5kb`
   meant **0 bytes** (i.e. matched everything).

2. **Malformed filters silently matched everything.** `parse_size` returned
   `unwrap_or(0)`, so `size:>garbage` became `size:>0`. `parse_size` now
   returns `Option<u64>` and an unparseable filter is *dropped* rather than
   applied as a wrong constraint. Also rejects negatives, `nan`, `inf`.

3. **Filter keywords were matched case-sensitively** — found by a test that
   was written expecting the fix. `TYPE:PDF` was not recognised as a filter
   and leaked into the fuzzy-match text, so the query matched nothing. Tokens
   are now lowercased before prefix matching; free text keeps its casing.

4. **Size units disagreed with the UI.** Parsing used decimal (1 MB =
   1_000_000) while `format_size` renders binary (1 MB = 1_048_576), so a file
   shown as "1.2 MB" did not match `size:>1mb`. Parsing is now binary
   throughout, and `kib`/`mib`/`gib`/`tib` are accepted as explicit spellings.

5. **Overflow.** `99999999999tb` wrapped; it now saturates to `u64::MAX`.

### Also added

- Fractional sizes (`1.5gb`) and fractional durations (`1.5d`).
- Duration units `w`/`d`/`h`/`m`/`s` (was `d`/`h`/bare only).
- **25 unit tests** covering size parsing, duration parsing, query parsing,
  filter matching, and ranking order. Tests use a fixed `NOW` constant so age
  assertions don't depend on the wall clock.

### Clippy — 20 → 0

All pre-existing. Notables beyond mechanical fixes:

- `walk_root` took `&PathBuf` and `&Arc<AtomicBool>` → now `&Path` / `&AtomicBool`.
- `IndexWorker::start` took `&PathBuf` → `&Path`.
- Extracted `SpaceHoggerResult` type alias (was a `type_complexity` error).
- Two `collapsible_if` fixes in `handle_keyboard_shortcuts` changed behaviour
  *for the better*: Escape and Ctrl+W are now only **consumed** when they can
  actually act, so they fall through to other widgets otherwise.

### Formatting

`cargo fmt` had never been run, so this commit also normalises the whole tree.
That inflates the diff — subsequent commits will be formatting-clean.

---

## Iteration 2 — File list hid files, and re-ranked everything every frame

**The bug.** The file list pushed the plain directory listing through
`filter_and_rank` using the *empty* search query. Filtering and ranking were
therefore no-ops that `sort_items` immediately overwrote — except for the
`truncate(200)`, which **silently hid entries in any directory with more than
200 of them**, keeping an arbitrary 200 chosen by a search heuristic.

Extracted `Tab::display_items` so the listing/search-results choice is one
testable place, and only search results are capped.

**Ranking.** `rank_result` ran *inside* a `sort_by` comparator, so every
comparison re-ran `SystemTime::now()` and allocated a lowercased name, a
lowercased query, and up to three `format!` needles for the word-boundary test
— O(n log n) allocations for an O(n) job. Now:

- `RankContext` hoists the query-wide state (lowered text, reference `now`).
- `sort_by_cached_key` scores each node exactly once.
- The word-boundary test inspects the character preceding each match rather
  than building `" x"` / `"_x"` / `"-x"` needles.
- A test asserts the new sort orders *identically* to the old comparator.

**Crawler.** `walk_dir_search` cloned and re-ranked the whole result set every
20 matches (quadratic); progressive updates are now throttled to 150 ms. It
also called `path.is_dir()` — an extra `stat` that **follows symlinks**, so a
link pointing at an ancestor made the search recurse forever. It now descends
using the directory entry's own file type, as the indexer already did, and
both walkers share one skip-list.

---

## Iteration 3 — Single-pass directory listing

`LocalFileProvider::read_dir` awaited a separate `tokio::fs::symlink_metadata`
per entry — each a round trip through the blocking pool — when the directory
entry it had just read already carried that metadata. All of it ran on the UI
thread under `futures::executor::block_on`.

Measured before/after (warm local SSD, mean of 5, macOS):

| entries | before | after | speedup |
| ---: | ---: | ---: | ---: |
| 100 | 1.86 ms | 0.37 ms | 5.0× |
| 1,000 | 9.46 ms | 1.20 ms | 7.9× |
| 10,000 | 99.36 ms | 12.67 ms | 7.8× |

`Tab::refresh` now calls the synchronous `core::list_dir` and no longer spins
up an executor to await a future that never yields. `node_from_metadata`
becomes the single place a `VNode` is built from filesystem metadata —
`walk_dir_search` had its own copy that reported every file as
`application/octet-stream`.

---

## Iteration 4 — Background results went to the wrong tab

Search batches and folder-scan results were written to
`tabs[active_tab_idx]` — whichever tab was active *when the result arrived*,
not the one that requested it. Switching tabs during a search or scan wrote
one tab's results into another, or dropped them and left the originating tab
spinning on "Scanning…" forever, because `analyzing` was cleared on the wrong
tab.

- `TabId`, assigned at construction and preserved across cloning, so a
  detached tab keeps its identity.
- Search updates and analyzer workers carry the requesting id; lookup spans
  docked *and* detached tabs.
- Freshness rules moved onto `Tab` (`apply_search_results`,
  `apply_scan_result`) so they sit next to the state they guard and are
  testable without standing up the app: a search batch is dropped unless it
  answers the query currently shown; a scan result is dropped unless the tab
  still points at the scanned directory.
- `analyzer_worker: Option<_>` → `Vec<_>`: starting a scan in a second tab
  used to overwrite and abandon the first tab's worker.

---

## Iteration 5 — Displayed dates were wrong

`format_timestamp` derived the calendar date by dividing days-since-epoch by
*average* year and month lengths (365.25, 30.4375). Sampling seven dates, four
were wrong:

| actual | was shown |
| --- | --- |
| 2000-01-01 | **12/30/1999** (a full year out) |
| 1999-03-05 | 03/02/1999 |
| 2025-12-31 | 12/30/2025 |
| 2026-07-28 | 07/26/2026 |

Replaced with Hinnant's `civil_from_days`, exact for the proleptic Gregorian
calendar. Tested against known dates, leap days, year/month boundaries, the
2100 non-leap case, and a round trip over every day from 1970–2040 against the
inverse algorithm. The relative-vs-absolute decision moved into
`format_timestamp_at(secs, now)` so the "Today"/"Yesterday"/"N days ago"
labels are testable without the wall clock — including a clock-skewed future
mtime, which must not underflow.

Dates remain UTC; local time would need a timezone database.

---

## Iteration 6 — Bounded pool for directory sizes

`DirSizeWorker::spawn` started a dedicated `std::thread` **per request**, and
the file list requested a size for every directory row it painted. Opening a
folder with a thousand subdirectories spawned a thousand OS threads at once,
each starting its own recursive walk — thrashing the disk and finishing none
of them sooner. Requests are now queued to four long-lived workers via the
standard `Arc<Mutex<Receiver>>` pattern.

The walk also returned `Ok` unconditionally, so `DirectorySizeState::Failed`
was **unreachable** and an unreadable directory showed a confident "0 B".
`directory_size` now probes the root and reports an error for it, while still
skipping unreadable *subdirectories* so the total remains a best-effort lower
bound. The running total saturates instead of overflowing.

Added `src/testutil.rs` with a self-cleaning `TempDir` that restores
permissions on drop — otherwise the permission-denied test would leave behind
a directory the harness could not remove.

---

## Iteration 7 — Virtualized the file list

`TableBody::row` invokes every row's content closure unconditionally, so the
list built all rows every frame — 10,000 row closures per frame in a
10,000-entry directory, each formatting a size, timestamp and kind label for
something almost always off screen. `TableBody::rows` builds only the visible
rows and pads the scroll area for the rest.

That change alone would have **broken the size column**: requests were issued
from inside the paint loop, so an off-screen directory would never be sized
and sorting by size would never settle. Queueing moved out of the paint loop
into `queue_dir_sizes`, which walks the whole listing; it stays cheap because
the `Calculating` marker means each directory is queued once, and iteration
6's pool bounds how many run at a time.

Verified by source inspection of egui_extras 0.29.1 (`rows` iterates only
`min_row..max_row`) plus a smoke test against a 5,003-entry directory. Not
benchmarked: eframe only repaints on demand, so an idle CPU comparison would
not have measured anything meaningful.

---

## Iteration 8 — Stopped deep-cloning the file list every frame, and an indexer crash

**The bigger hotpath.** `show_file_list` ran unconditionally on every repaint
(not just when something changed) and, before doing anything else, called
`display_items().to_vec()` — a deep clone of every `VNode` (two `String`s and
a `PathBuf` each) in the directory, followed by a full re-sort. Iteration 7's
virtualization only stopped *rendering* off-screen rows; this clone-and-sort
still ran over the entire listing 60 times a second regardless of scrolling.
`sorted_items` is now `Vec<&VNode>` — borrowed pointers into the tab's own
listing — so a frame allocates one vector of pointers instead of `n` heap
copies. The borrow lives only for the frame (all reads of it happen before any
`&mut self.tabs[..]` access), so nothing needed to change about when
selection/navigation actually mutate the tab.

`sort_items`'s Name comparator had the same bug iteration 2 fixed in the
ranker: `a.name.to_lowercase().cmp(&b.name.to_lowercase())` inside `sort_by`
lowercases both names on every comparison, `O(n log n)` allocations for an
`O(n)` job. Switched to `sort_by_cached_key`, which lowercases each name once.
Descending order used to be `cmp.reverse()` inside the comparator; naively
reversing the sorted slice instead would have reversed the relative order of
tied entries (equal sizes, equal mtimes) too, so each column now sorts by
`Reverse(key)` when descending, which flips the comparison but not tie order —
covered by a new test.

**Indexer crash.** Hit live while testing this: "Index failed: UNIQUE
constraint failed: directories.parent_id, directories.name". `roots` (one
thread per drive) can overlap on disk — on macOS `/Volumes` typically contains
an entry that is the boot volume mounted a second time — so two independent
walks can reach the very same `(parent, name)` pair. `dir_map` resolves each
child's `parent_id` by looking up its parent's *path string* in a table both
walk threads write into via a single sequential writer; if the two roots'
inserts for a shared parent interleave, both children end up inserting the
identical `(parent_id, name)`, and the second one hits the unique index and
aborts the whole index build. The insert is now an upsert
(`ON CONFLICT(parent_id, name) DO UPDATE ... RETURNING id`): a colliding
directory is treated as already indexed rather than crashing. Regression test
indexes the same root twice to force the overlap.

---

## Iteration 9 — The search overlay hit SQLite every repaint; detached windows cloned the whole tab every frame

**Global search overlay (Ctrl+P).** `show_global_search_overlay` opened the
SQLite index, ran a `LIKE` query and re-ranked up to 500 rows, unconditionally,
on *every frame* the overlay was open with non-empty text. A repaint isn't
only triggered by typing — cursor blink and hover repaint too — so this was a
synchronous disk + SQL round trip on the UI thread far more often than the
query text actually changed. The response already exposed `.changed()` and it
was even used two lines down to reset the selected index, just not to gate the
query itself.

Gating on `.changed()` directly turns out to be the wrong fix, though: clicking
a "recent search" entry sets `overlay_query` from code, not through the text
edit widget's own interaction, so `changed()` never fires for it and results
would never populate. Added `overlay_last_query`, the query the current
`overlay_results` actually answers, and rerun only when `overlay_query` drifts
from it — this covers both typing and programmatic sets, and is exactly the
"stale cache" test the DB-backed search path (`execute_search`) doesn't need
here because it runs once per Enter press, not once per frame.

**Detached viewports.** `show_detached_viewports` ran the same two problems
iterations 8 and 7 had already fixed in the main window, in a worse form,
once per detached window per frame:

- `let mut tab = tab.clone();` deep-cloned the *entire* `Tab` — full directory
  listing, search results, selection — every repaint, then shipped the
  mutated copy back to `self.detached_tabs` over an mpsc channel
  (`ViewportMessage::UpdateTab`) so the next frame could clone it again.
- The row list used a `for` loop calling `body.row()` per filtered entry
  instead of `body.rows()`, so it wasn't virtualized at all: every row's
  widgets were built every frame regardless of scroll position.

Checked egui's `show_viewport_immediate` source (0.29.1): its callback has no
`'static` bound and runs synchronously within the call, so the tab can be
borrowed (`&mut Tab`) straight out of `self.detached_tabs` instead of cloned.
`navigate_to`/`launch_path` are deferred past the borrow with local `Option`s,
the same pattern the main file list already used, so double-clicking a row no
longer needs its own clone of the tab to call back into — which also let the
filtered list become `Vec<&VNode>` instead of `Vec<VNode>`. This removed every
use of `ViewportMessage::UpdateTab`, so the variant was deleted along with its
handler in `background.rs`; `clippy::large_enum_variant` then flagged the
remaining `Remerge(Tab)` against a bare `Close(ViewportId)`, fixed by boxing
(`Remerge(Box<Tab>)`) — that clone still only happens on the "Merge Back"
button click, not per frame.

---

## Iteration 10 — `code-review --focus efficiency`, and the ones worth acting on

Ran the `code-review` skill against `src/` focused on efficiency, in the
background, to catch systematic hotpaths eyeballing had missed. It returned 8
findings; four were real and worth fixing immediately, the rest are recorded
below as accepted trade-offs rather than silently dropped.

**Fixed:**

1. **File preview decoded/re-read on every frame.** `show_file_preview` ran
   unconditionally from `show_details_panel` (which runs every frame). For a
   selected image it called `image::open` + `to_rgba8` + `ctx.load_texture`
   — full decode and a fresh GPU upload — 60 times a second for as long as
   the image stayed selected. For a selected text file it called
   `std::fs::read_to_string` and then, worse, iterated `content.lines()`
   *twice*: once to take the first 30, once more over the *entire file* just
   to decide whether to print "...". Replaced with a `PreviewCache` keyed by
   path: decode/read once when the selection changes, and the truncation
   check now stops after the 31st line instead of walking the rest of the
   file. Passed as `&mut Option<PreviewCache>` rather than made a `&mut
   self` method, so it coexists with the live `&mut self.tabs[idx]` borrow
   already open in `show_details_panel` — same disjoint-field-borrow pattern
   used elsewhere in this codebase.

2. **"Scan Folder Size" had no re-entrancy guard.** `analyzer_workers` became
   a `Vec` in iteration 4 specifically so a second tab's scan wouldn't
   discard the first's — but nothing stopped the *same* tab from queuing a
   second, independent recursive walk if the button was clicked again before
   the first finished, each fanning out through rayon's shared pool and
   competing with the other rather than finishing any sooner. Exactly the
   unbounded-spawn shape iteration 6 fixed for `DirSizeWorker`. Guarded on
   `tab.analyzing`, which was already tracked (it drives the spinner shown a
   few lines below) — the button simply doesn't fire while a scan for that
   tab is in flight.

3. **`execute_search` blocked the UI thread once an index existed.** The
   no-index fallback already ran `walk_dir_search` on a background thread;
   the indexed path called `DbManager::query_disk_index` — open SQLite, run
   a `LIKE` query, rank — inline, synchronously, and `show_navigation_bar`
   triggers a search on every `TextEdit::changed()`, i.e. every keystroke.
   Moved onto a thread using the exact same `SearchUpdate` channel the
   no-index path already had; `apply_search_results` already discards a
   result that lands after the query moved on, so this didn't need new
   staleness handling, just routing through the existing one.

4. **Indexing overlapping roots double-counted files.** Iteration 9 stopped
   the directory-level crash with an upsert, but the finder pointed out that
   `files` had no equivalent: two walks reaching the same file (now sharing
   the same, upserted `directory_id`) still inserted two rows for it,
   doubling its size in both the raw row and the running `dir_sizes` total
   fed into the space-hogger scan. Added a `UNIQUE(directory_id, name)` index
   and switched to `INSERT OR IGNORE`, using the row-count `execute` already
   returns to skip the size/progress accounting for a row that was ignored
   rather than inserted. Regression test indexes the same root twice and
   asserts both the file count and the summed size are un-doubled — writing
   it caught an unrelated test-hygiene bug (the previous overlap test put the
   database file *inside* the directory it indexed, so the walk was counting
   `index.db`/`-wal`/`-shm` as extra "files"; both overlap tests now write
   their database to a separate temp directory).

**Reviewed, left as-is (recorded so they're not silently dropped):**

- **`sort_items` still re-sorts the full (non-virtualized) list every
  frame**, not just when something changed. Iteration 8 fixed the clone and
  the comparator's allocation; a cache invalidated on
  listing/sort-column/`dir_sizes` change would remove the sort itself, but
  needs a dirty flag threaded through several mutation sites, so it's
  deferred rather than done under this pass.
- **`dir_sizes` grows without bound** for the life of the process — already
  called out after iteration 6 (item 5 below); still true, still low
  practical impact next to the per-frame costs fixed above.
- **The detached-viewport file list re-scans `fuzzy_match` over every item
  every frame.** Much cheaper now that iteration 9 stopped cloning the whole
  `Tab` around it, and a substring scan with no per-item allocation over a
  single directory's contents is unlikely to be felt even at tens of
  thousands of entries — a dirty-check would need a cache slot on `Tab`
  itself, which felt like the wrong place to add one for a rarely-used
  window. Worth revisiting if detached-window scanning shows up in profiling.

---

## Iteration 11 — The full index (and so "Space Usage Finder") walked the boot volume twice

**Reported symptom:** "The space usage finder takes ages to finish
processing." Space Usage Finder itself only runs one bounded, indexed SQL
query (`ORDER BY direct_size DESC LIMIT 50`) — the slow part is the full
disk index it depends on (`start_indexing`), built by walking every root
`sys::get_drives()` returns, one thread per root.

Checked this machine directly: `/Volumes/Macintosh HD` is a symlink to `/`,
and `stat -f "%d"` confirms both report the *same device id* (16777231).
This is standard on modern macOS (APFS always exposes the boot volume a
second time under `/Volumes`), and `get_drives()` returned both unfiltered —
so `index_multiple_to_sqlite` spawned a walker thread for `/` and a second,
entirely redundant one for `/Volumes/Macintosh HD`, walking (and inserting)
the whole boot volume twice. Iteration 9's directory upsert and iteration
10's file dedup stopped this from corrupting the resulting counts, but did
nothing about the wasted I/O of actually performing the second walk — this
is very likely the direct cause of "ages": a full-disk index build was
taking roughly double what it needed to.

Fixed at the source instead of downstream: `get_drives()` now deduplicates
roots by device id (`st_dev` on Unix), keeping the first occurrence, so `/`
wins and its `/Volumes` twin is dropped before any walking starts. Verified
against this machine's real `/Volumes` (a throwaway test, removed after
confirming): before, `get_drives()` returned `/` *and*
`/Volumes/Macintosh HD`; after, just `/`, plus the three genuinely distinct
mounted disk images (`Bambu Studio`, `CLion`, `Infuse Pro 8.5`), which have
different device ids and are correctly kept. Unit-tested with a fake
device-id lookup so the dedup logic itself doesn't depend on any particular
machine's real volumes.

Known remaining gap: this catches the guaranteed, universal case (the boot
volume's `/Volumes` twin), not every possible overlap. A separate APFS data
volume mounted under `/Volumes` with its *own* device id, whose content is
also reachable through `/` via a firmlink, would still be walked twice —
detecting that needs per-file device tracking during the walk itself, not
just a root-list dedup, and was left out of scope here.

**Found while verifying this:** the file-count regression test added in
iteration 10 failed once out of many runs of the full suite (never in
isolation). Root cause was in `index_multiple_to_sqlite` itself, not the
test: `dir_map.insert(path, id)` was last-write-wins, so when two
overlapping root walks (two threads) both produce a `Dir` event for the same
path — guaranteed at the root, since a `NULL parent_id` never collides with
another `NULL` under the unique index, so the DB-level upsert can't dedupe
two root-level inserts the way it does for real, non-null parents — the id
that path resolved to could change mid-stream depending on thread
interleaving. A child processed against the id in place *before* the
overwrite and a sibling from the other walk processed *after* would resolve
the same logical parent to two different directory rows, splitting one real
file across both and slipping past the `(directory_id, name)` dedup.
Changed to `dir_map.entry(path).or_insert(id)`: first write wins, so once a
path resolves to an id that assignment is stable for the rest of the run
regardless of interleaving — by induction this makes every deeper duplicate
resolve consistently too, since a path's children are only ever looked up
after its own (now-fixed) entry exists. 30 isolated runs plus 8 full-suite
runs afterward, all green (this doesn't prove the race can't recur, but the
fix is structural, not probabilistic — argued through above, not just
observed).

---

## Iteration 12 — Space Usage Finder never stopped showing "Analysing index..."

**Reported symptom:** "the space usage finder really wastes time when
analysing, it takes ages." Reproduced live: opened the panel, watched it sit
on "Analysing index..." for 90+ seconds with nothing rendering.

The query it runs (`query_space_hoggers_from_sqlite`, a single indexed
`SELECT ... ORDER BY direct_size DESC LIMIT 50`) finishes in milliseconds, so
it wasn't actually slow — `process_background_messages` set
`space_hoggers_results`/`space_hoggers_error` when the worker's result
arrived but never cleared `space_hoggers_in_progress`, which is set `true` in
exactly one place (`start_space_hogger_scan`) and nowhere else. The overlay
gates its spinner on that flag, so it stayed stuck showing "Analysing
index..." forever after the first open, regardless of how fast the query
actually was.

Fixed by clearing the flag alongside both completion branches. Verified
red/green: removed the fix, confirmed the new regression tests fail with the
exact old symptom (`assertion failed: !app.space_hoggers_in_progress`),
restored it, confirmed green. Also rebuilt and drove the real app end to end
— the panel now populates (e.g. `~/Downloads` at 70.3 GB) instead of hanging.

---

## Final state

| Gate | Before | After |
| --- | --- | --- |
| `cargo test` | 0 tests | **72 passing** |
| `cargo clippy -- -D warnings` | 20 errors | clean |
| `cargo fmt --check` | unformatted | clean |

---

## Remaining findings

Reviewed 2026-11-16 against the Tauri/React app. The six findings that were
listed here came from the earlier egui version and are all resolved or gone:

1. Navigation blocking the UI: listings stream from the blocking pool
   (`api::list_dir_stream`, `listing-chunk`).
2. Re-sorting every frame: `FileList` and `ColumnView` memoise the sort.
3. Eager folder sizing: only visible rows are sized unless sorting by size
   (`sizeRequests` in `src/lib/listing.ts`).
4. Unreadable folder shown as empty: `tab.listingError` renders "Can't read
   this folder" (`FileList.tsx`).
5. Unbounded `dir_sizes`: the store prunes sizes (`pruneSizes`).
6. `update_node_size` and `search_service.rs` no longer exist.

Fixed in the same review: favorites, saved searches, search and shell
history, frecency, key prefs and plugin trust read an unparsable file as
empty, and the next save overwrote it. They now share `load_json` /
`move_aside` in `persistence.rs`, which keeps the file as `<name>.corrupt`,
as the workspaces store already did.

No open findings.

## Performance

Measured with `cargo test --release -- --ignored perf_ --nocapture` (`perf_list_dir_and_serialize`,
`perf_first_chunk_latency` in `core.rs`), on macOS, 100,000 empty files in one folder. The 100k
"before" figure was measured on a quiet machine (217 ms); the other rows were measured while another
build was running, so the first-paint row is compared against the same-conditions full listing (284 ms).

| Measure | Before | After |
|---|---|---|
| Full listing, `list_dir` + JSON encode (100k) | 217 ms quiet / 284 ms under load | 279 ms total, now delivered in 21 batches |
| Time to first paint (100k) | 217 ms (nothing sent until all entries are read) | 4.7 ms quiet / 5.4 ms under load (first 2,000 entries) |
| Time to first paint (10k) | 19 ms quiet / 25 ms under load | 4.6 ms quiet / 5.0 ms under load |
| First IPC payload (100k) | 28.7 MB (whole folder) | 0.58 MB (first batch) |
| Re-sort per selection or cursor change (100k, name sort) | 93 ms, every store change | 0 ms: `FileList` and `ColumnView` memoise on items, sort, filter and (for size sort only) sizes |
| Re-sort while streaming (100k, JS) | not streamed | 0 per batch; one sort at the end (93 ms). Sorting every batch would cost ~985 ms |
| Folder-size walks queued when a 100k folder opens | one per subfolder, all at once | visible rows only, unless sorting by size (then all, behind visible rows) |

JS timings come from `displayItems` bundled and run in Node (V8) with 100k synthetic rows, so
`Intl.Collator` costs will differ in WebKit. Other sorts at 100k: size 20 ms, modified 23 ms, kind 174 ms.

Notes:
- Listing already ran on the blocking pool (`list_dir` is `async` + `spawn_blocking`); the streamed
  listing (`list_dir_stream`) uses the same pool and returns immediately. Directory watching runs on
  its own thread, and `watch_dirs` and `request_dir_sizes` were made `async` so no sync command runs on
  the main thread.
- A stale listing (the user navigated away) is cancelled on the Rust side between entries and its
  batches are dropped on the JS side by request id. A read that is already blocked inside the OS on a
  dead network mount cannot be interrupted; its result is simply discarded.
- The Cancel button on "Waiting for <volume>…" goes back in history (or up) and cancels the listing.
- Not done: sorting in Rust. Sorting 100k rows in JS is 93 ms, once per listing, so the measured gain
  did not justify moving it.
