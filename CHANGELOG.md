# Changelog

## 0.2.0 — 2026-09-24

First fork release. Forked from
[marcoskichel/herdr-muster](https://github.com/marcoskichel/herdr-muster)
(July 2026); all upstream history is preserved and credited.

### Migration (breaking)

- The plugin ID changed from `kichel.muster` to `tayiorbeii.muster`. After
  upgrading, move your `config.toml` from the directory printed by
  `herdr plugin config-dir kichel.muster` to the one printed by
  `herdr plugin config-dir tayiorbeii.muster`, and update any herdr
  keybindings that reference `--plugin kichel.muster`.

### Added

- **Jump Pane mode** — a dedicated existing-pane picker that searches live
  pane IDs, paths, labels, titles, workspace/tab context, agents, status, and
  optional foreground applications, then focuses the exact pane via Herdr's
  absolute `pane.focus` socket API. Tab names are searchable and joined onto
  panes by `tab_id`. Optional named/remote enumeration targets with explicit
  sockets.
- **Sectioned picker** — results are grouped into OPEN / TABS / PANES /
  PROJECTS sections; Tab jumps the selection to the first result of the next
  non-empty section, skipping filtered-out sections and wrapping.
- **Alt+digit quick jump** — Alt+1…Alt+9 (and Alt+0) jump straight to open
  workspaces in most-recently-used order; the switcher's own workspace gets
  the last number. Row numbers are displayed up front.
- **Parent spaces in the picker** — tabs and renamed panes appear as grouped
  rows with their parent space context; duplicate space labels disambiguate
  via Git branch, distinct directory name, or workspace number.
- **Project discovery sources** — recent Herdr history, configured `paths`,
  one-level `roots` scans filtered to git repo roots, and optional `zoxide`
  suggestions (including Homebrew install locations), merged and
  de-duplicated by canonical path.
- End-to-end test suites for Jump Pane, picker tab/pane behavior, and PTY
  process lifecycle.

### Changed

- The open-workspace group is sorted most-recently-used first, with the
  switcher's own workspace ranked last (Escape already returns there).
- Fuzzy matching is scoped per field so a query term can no longer match
  across unrelated row fields.

### Fixed

- Hardened shutdown bounds for spawned processes and pipe readers, including
  prompt cancellation when a `setsid` descendant streams output.
- Terminal control-character injection stripped from labels and titles.
- State file writes are deterministic; a corrupt state file warns instead of
  crashing and is rewritten.
- Display sanitization for pane/workspace labels.
