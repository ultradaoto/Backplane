# Performance notes: what has been measured

A survey of where a picture's time goes, made a path at a time, with the number for each. Every figure here was measured on one Windows machine in debug builds unless said otherwise, so treat the *relationships* as the finding and the absolute numbers as the shape of this machine. The work that came out of it is on branches (see [Where the work is](#where-the-work-is)); nothing here is merged.

## What a picture is, and why it matters

A picture is the JSON object the presenter builds for a view: a thread's page, or the viewer page inside it. **It is built on every state change of the app**, and an agent's turn is a stream of state changes — a tool call, each chunk of a reply, each ask opening and closing. So anything a picture does *per picture* is multiplied by the turn.

That is the recurring bug this survey found, in five different places: work done per picture that is not per picture.

## The measurements

| path | what it cost | what it costs now | branch |
|---|---|---|---|
| a layer press (client) | re-tessellated the whole board: ~1.4 ms on the stand-in, tens on a real one | a re-composite of kept paths: 0.002 ms | `perf/plot-layer-toggle` |
| a save while watched (client) | re-tessellated every chunk: 17.6 ms on a 24-chunk stand-in | only the chunks that changed: 0.04 ms | `perf/plot-layer-toggle` |
| a Gerber walk (hub) | 2.15 ms an ask, a `canonicalize` per file listed | 0.72 ms | `perf/gerber-walk-no-canonicalize` |
| the rail sources, per picture (hub) | 2.96 ms an ask; the newest set found on pages that show none | 1.19 ms; the walk only where a set is drawn | `perf/rail-sources-lazy-gerbers` |
| the Files tab's folder (hub) | 62 ms a listing of 3000 files, per picture **and** per 400 ms watch poll | one walk a burst; the watch still lists fresh | `perf/files-listing-shared` |
| the project's own files (hub) | 2.32 ms: a walk of the whole folder tree for every `.kicad_pro` in it | 0.07 ms: the three names the picture needs | `perf/project-read-without-tree` |
| "is there hardware here" (hub) | 2.45 ms: the whole tree listed twice to answer yes or no | 0.05 ms: the folder's own read first, then a walk that stops at the first project | `perf/hardware-probe-early-out` |
| the Gerbers page's set (hub) | a project-wide walk per picture for a time the set's own listing already had | the set's path kept while the folders it is found in have not moved | `perf/gerbers-page-no-hint`, `perf/gerbers-newest-set-cached` |
| a save being noticed (hub) | a 150 ms poll, always | fast while a file changes, slower when quiet | `perf/watch-adaptive-poll` |
| the plot itself (hub) | 104 ms for a 16 MB, 120k-item board | unchanged: 88% of it is already parallel, and the rest is per-item building | — measured, not changed |
| the client's decode + tessellation | 16 ms on a real board, cold | unchanged; the pieces scale with the board, and the layers' work sits on top of it | — measured, not changed |
| startup replay of `events.jsonl` | 0.11 s for 60,000 events, 1 MB | unchanged: 540k lines/s, ~17 bytes a line | — measured, not changed |
| the reply being streamed (hub) | 1.1 ms at 2 KB of reply, 20.7 ms at 40 KB, rendered whole per picture | unchanged: the hub paces streamed notices by what the last picture cost (`wire::live_gap`), which bounds the share it takes | — measured, not changed |

Together, on the same project and machine, a picture before and after the per-picture work:

    picture   before   after
    idle       3.03 ms  0.74 ms
    board      3.83     1.87
    schematic  3.78     1.77
    split      4.51     2.21
    Gerbers    5.14     2.79

## How to see these numbers again

Each figure came from a test that prints it, so a regression shows up as a number. **The tests below are on the branches named at the end, not on main yet** — they land with the work they measure.

    cd native/host
    cargo test a_picture_costs_a_millisecond -- --nocapture     # a picture, per page
    cargo test a_picture_does_not_search -- --nocapture         # the project's files
    cargo test hardware_is_a_yes_or_no -- --nocapture           # the hardware probe
    cargo test a_gerbers_page_keeps_the_newest_set -- --nocapture
    cargo test a_burst_of_pictures_walks_a_big_folder_once -- --nocapture
    cargo test a_projects_rail_sources_are_cheap -- --nocapture

    cd native/gpui
    cargo test a_real_board_tessellates -- --nocapture          # tessellation of a real board's frame
    cargo test a_plot_decodes_in_a_few_milliseconds -- --nocapture

    cd native/host
    cargo run --bin state-bench -- LOG                          # startup fold: time, memory, tick
    cargo run --bin bp-plot -- 0 BOARD                          # the plot reader, for timing it

`bp-plot 0 BOARD > frame.plot` also produces a client frame from any board, which `BP_FRAME_PROBE=frame.plot` feeds to the client tests above — that is how a big board is measured without a hub running.

## The one rule the survey keeps finding

**A picture must not walk the filesystem to answer a question no one is looking at.** Five of the fixes above are that sentence in different places. When adding a field to a picture, ask in this order:

1. is it needed for the page on show? (the lazy rail sources, the Gerbers set)
2. is it a name or a time? (a name changes when a folder's own time moves; a time needs the file)
3. if it must be read, can a cheap stamp (four folder times) stand in for the walk?
4. if it must be walked every time, can the thing that *changes* refresh it (a watch start), rather than a timer?

## Where the work is

Nothing is merged: this checkout has no write access to the upstream repository, so each round published a branch on the fork (`github.com/ultradaoto/Backplane`) and a comment on `BackplaneWorks/Backplane#1691`.

- **`perf/per-picture`** is the one to review: eight commits off `d1da7c7df`, in dependency order, superseding seven branches that each did one of them.
- **`perf/plot-layer-toggle`** and **`perf/watch-adaptive-poll`** are independent of it (the client's canvas, and the watch's cadence).

## Two decisions this survey did not take

- **the folder watch re-lists up to 32 folders every 400 ms** to hash them (`files::watch_dir`), a stat per entry per folder per poll. Hashing a kept listing when the folder's own time has not moved would be cheap, but a file growing *in place* would then be noticed one poll later. That is a behaviour decision.
- **the Gerbers page's discovery walk** could be dropped entirely by refreshing a kept newest-set from the watch instead of the folder times. The cache in `perf/gerbers-newest-set-cached` already keeps it for a page that names no file; a watch-driven refresh would cover the case where a *different* set becomes newest while the page is open.
