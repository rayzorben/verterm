# Vendored crates

## eframe-0.36.1 (+ upstream PR #8398)

`eframe` 0.36.1 exactly as published on crates.io, except `src/native/run.rs`, which carries the
backport of [emilk/egui#8398](https://github.com/emilk/egui/pull/8398) "Don't busy-loop a CPU
core while waiting for a redraw" (merged 2026-08-11; not in any release as of 2026-09-05, the
newest being 0.36.1 from 2026-08-07). It is wired in through `[patch.crates-io]` in the root
`Cargo.toml`. The exact change is `eframe-0.36.1-pr8398.patch` beside this file.

**Why.** eframe 0.36.1 switched winit to `ControlFlow::Poll` after every `request_redraw()`, and
only left Poll once a `RedrawRequested` event was processed. winit's Wayland backend delivers that
event only after the compositor answers the surface's frame callback, and Niri does not draw (so
does not answer) a window on a workspace that is not shown. verterm is a summoned scratch
terminal that is hidden most of the time and asks for repaints constantly (proc scanner every
500 ms, PTY output, the 120 ms running-command timer), so its GUI thread spun a whole core whenever
it was hidden. Measured on 2026-09-05 against PID 547819: 26.0 h of CPU over 26.8 h alive, ~500k
empty `read()`s per second (the `polling` crate clearing its eventfd after each zero-timeout
`epoll_wait`), 4 voluntary context switches per 2 s, no GPU work.

**Remove when eframe ≥ 0.37 is published.** Delete this directory and the `[patch.crates-io]`
section, bump `eframe`/`egui`, and confirm the release's `check_redraw_requests` no longer sets
`Poll`. Tracked in `todo/90-eframe-037-drop-vendored-patch.md`.
