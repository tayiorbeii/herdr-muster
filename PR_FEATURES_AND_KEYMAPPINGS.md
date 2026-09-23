# muster — Features & Keymappings (PR reference)

Complete, implementation-verified inventory of what the fork's PR adds to
herdr-muster, for the PR description and release notes.

---

## 1. Feature summary

### 1.0 Jump Pane
Jump Pane is isolated from project mode: it indexes only live `pane list`
records, keeps runtime/session-qualified pane identities, and routes Enter via
the absolute Herdr socket method `pane.focus`. No directory scan, bind, create,
or directional/workspace fallback is reachable from this mode. Named session
sockets may be configured under `[[jump_pane.sessions]]`; each target uses an
executable plus argument prefix, and unavailable or focus-unsupported targets
are reported independently without default-socket misrouting. Each target's
`tab list` is read once per snapshot and joined onto panes by exact `tab_id`, so
panes can also be found by human-readable tab name; tab labels are optional
metadata that degrade to unlabeled panes on error and never participate in pane
identity, validation, or focus.

muster is an agent-aware project switcher for herdr: one keypress opens a fuzzy
list of your projects; the ones already running appear first, tagged with what
their agent is doing, ordered by how recently you were in them. Each project
maps to exactly one workspace, remembered from the moment muster creates it.

### 1.1 Project discovery
- **Sources.** Projects come from four sources, merged and de-duplicated:
  - recent Herdr history — up to 100 absolute workspace directories observed by Muster, retained after closure; legacy registry bindings seed the history on upgrade.
  - `paths` — explicit directories from `config.toml`; always shown (user opted in by naming them).
  - `roots` — each root is scanned **one level deep** for git repositories.
  - `zoxide` — `zoxide query -l` results, folded in when `use_zoxide = true` (the default) and zoxide is installed.
- **Repo-root filtering.** Recent history and `paths` bypass the filter;
  `roots` and zoxide results are kept only if the entry is an existing
  directory whose basename is not hidden and whose `.git` is a **directory** —
  linked worktrees and submodules (`.git` is a *file*) and hidden dirs
  (`.claude`, `.git`, …) are excluded.
- **Normalization.** `~` expansion on config paths; missing directories dropped;
  duplicates collapsed by canonical path; display shows `~/…` collapsed paths.
- **Cancellable discovery.** Filesystem traversal is checkpointed against a
  cancellation token so picker teardown is prompt even on slow mounts.

### 1.2 Agent-aware open group
- **Per-workspace agent state**, color-coded with a glyph:

  | State | Glyph | Color | Meaning |
  |---|---|---|---|
  | blocked | `●` | red | waiting on you |
  | working | `◐` | cyan | running |
  | done | `✓` | green | finished, awaiting review |
  | idle | `○` | muted | no agent activity |
  | unknown | `·` | muted | herdr reported nothing |

- **Identity never guessed from cwd.** Each open workspace shows the directory
  muster bound it to when it created the workspace (stored in a muster-owned
  registry, survives `cd`); for workspaces herdr created directly it falls back
  to the workspace's root-pane cwd. Open rows show their pane label(s)
  (explicit pane name, else terminal title; deduplicated — `a · b +3` for more),
  agent name, and state word.
- **Root-pane selection** uses herdr's public pane IDs (bijective base-32) so
  the lowest-numbered pane decides agent/cwd even for alphabetic suffixes.

### 1.3 Recency ordering (open group)
1. Persisted **most-recently-used** workspaces, most recent first (Enter
   fast-tracks to where you were before opening the picker).
2. Workspaces never focused through muster, by agent-state priority
   (blocked > working > done > idle > unknown), then name.
3. The workspace the picker was **opened from**, last — Escape already
   returns to it.
- Dormant projects sort below all live workspaces, tabs, and renamed panes;
  Herdr history appears newest-first, followed by configured paths, sorted root
  discoveries, and zoxide's own result order. History survives closure independently
  of workspace bindings.

### 1.4 Fuzzy filtering
- Type immediately — even while workspaces and projects are still loading.
- Search fields per row: name, display path, **full path, pane labels,
  agent name, and agent-state word** (open rows only); each field scored
  independently and the strongest kept — a fuzzy subsequence can never cross
  metadata boundaries (e.g. `deadpo` cannot match a row whose *name* is
  `instructional-design-agent` via its path).
- Open rows show the workspace's **tab names** as `· tabs: <labels>` next to the
  collapsed path, and tabs plus renamed panes are rows in their own sections:
  `TABS` lists one row per tab and `PANES` lists only panes the user renamed.
  Both are searchable by label and jumpable with Enter — `tab.focus` for a tab,
  `pane.focus` for a renamed pane. Unnamed panes have no row of their own and
  stay searchable through their workspace row. Tabs whose label is still just
  their position number (never renamed) are hidden from the `TABS` section and
  from the inline tab context. Alt+digit quick jumps and
  Ctrl-N / Ctrl-X remain workspace/project-only. Tab metadata comes from one
  best-effort `herdr tab list` per refresh; a missing or failing call leaves
  rows without tab context instead of failing the snapshot.
- Open rows always rank above projects, with or without a query.
- Smart case + normalization (nucleo); selection resets to the top on each keystroke.

### 1.5 Alt+digit quick jump
- Hold **Alt** and press **0–9** to jump straight to the Nth open workspace in
  the current view. With no query, **Alt+0** = the workspace you opened the
  picker from, **Alt+1** = the one before that, …, **Alt+9** = the tenth.
- Open rows display their number (0–9) up front, so the shortcut is always
  visible; numbers follow the *filtered* view when a query is active.
- macOS terminals that send Option symbols instead of ESC-prefixed digits are
  supported: `º ¡ ™ £ ¢ ∞ § ¶ • ª` map to `0–9`.

### 1.6 Workspace actions
- **Enter** — the footer names the selected row’s action: focus an open workspace,
  tab, or pane; or *muster* a fresh workspace (create with cwd+label, focus, bind
  identity) for a dormant project.
- **Ctrl-N** — force new: create a fresh workspace for the selected project
  even though one is open, rebinding identity to the new workspace.
- **Ctrl-X** — close the selected open workspace (closes it in herdr, unbinds
  identity, forgets recency); the picker stays open so you can keep going.
- **Esc / Ctrl-C** — cancel; the picker pane closes itself.

### 1.7 Background refresh & resilience
- **Two-phase snapshots:** open workspaces render as soon as herdr data is
  ready (`Partial`); project discovery completes in the background (`Ready`).
  Last-known dormant rows are retained during loading so nothing flickers away.
- **Graceful degradation:** if the workspace query fails, projects still appear
  as dormant entries and persisted bindings are **not** reconciled against the
  incomplete snapshot; a failed refresh is surfaced in the UI (red badge / error row).
- **Process safety:** every herdr/zoxide call is bounded (30 s timeout, 8 MiB
  output cap), runs in its own session, and is torn down as a process group
  (SIGTERM → 50 ms → SIGKILL) on cancel/timeout — no orphaned `herdr` or
  backgrounded shell descendants, verified by a controlling-PTY end-to-end test.
- **State durability:** registry writes are atomic (temp file + fsync + rename
  + parent-dir fsync) with deterministic serialization; a corrupt state file
  now warns instead of silently resetting.
- **Terminal-injection hardening:** all rendered text (pane labels, terminal
  titles, workspace labels, cwds, agent names, and project display names/paths)
  is stripped of C0/DEL/C1 control characters before reaching Ratatui.

### 1.8 UI chrome
- Header: *"one terminal for the whole herd"* + live counts
  (`N open · M projects`) + workspace-loading, project-searching,
  refresh-failed, or zoxide-unavailable status.
- Amber `›` caret prompt with `type to fuzzy-filter…` placeholder and a live
  match count; grouped rows under **OPEN (LIVE WORKSPACES)** and
  **PROJECTS (NOT OPEN YET)** headers; tokyo-night palette, rounded border,
  amber-tinted selection; grapheme-aware, display-width-safe truncation at any
  terminal width.

---

## 2. Keymappings (complete)

| Key | Action | Notes |
|---|---|---|
| any printable char | Fuzzy-filter the list | Works immediately, even while loading; selection resets to top |
| `↑` / `↓` | Move selection | Clamped at list bounds |
| `Enter` | **Focus** a live workspace/tab/pane, or **open project** for a dormant directory | Footer label reflects the selected row; project activation may create/bind a workspace |
| `Alt+0 … Alt+9` | **Quick jump** to the Nth open workspace in the current view | `Alt+1` = most recent, `Alt+0` = tenth (no query); numbers shown on open rows |
| `Option+0 … Option+9` (macOS glyphs `º¡™£¢∞§¶•ª`) | Same as Alt+digit | For terminals that send the symbol instead of ESC+digit |
| `Ctrl-N` | **Force new** workspace for the selected row | Rebinds identity to the new workspace |
| `Ctrl-X` | **Close** the selected open workspace | Stays in the picker; unbinds + forgets recency |
| `Esc` | Cancel and close the picker pane | |
| `Ctrl-C` | Cancel and close the picker pane | |
| `Backspace` | Delete last query character | |
| `Alt+letter` | (intentional no-op) | Only Alt+digit is bound |

> Binding outside the switcher (herdr config.toml):
> `[[keys.command]] key = "prefix+space" type = "shell" command = "herdr plugin pane open --plugin kichel.muster --entrypoint picker"`

---

## 3. Configuration (config.toml)

```toml
paths      = ["~/dev/api", "~/notes"]   # always shown
roots      = ["~/dev"]                  # scanned 1 level deep for git repos
use_zoxide = true                       # fold in `zoxide query -l` (default true)
```

- Config dir: `herdr plugin config-dir kichel.muster`; missing file ⇒ defaults
  (`paths`/`roots` empty, `use_zoxide = true`); malformed file ⇒ clear error.

---

## 4. Behavior notes worth knowing

- **One project ↔ one workspace:** muster binds a project directory to the
  workspace the moment it creates it; a pane moved elsewhere later still belongs
  to its original project, and you never get two workspaces for the same repo
  (dormant rows whose path resolves to an open workspace are dropped).
- **Force-new rebinds:** after Ctrl-N, the *new* workspace owns the identity;
  closing the old one later won't unbind the new binding.
- **Recency persistence:** MRU order is stored in `state.json` next to the
  bindings and survives restarts; workspaces closed outside muster are pruned on
  the next successful refresh.
- **No workspace left unreachable:** a workspace whose directory is unknown
  falls back to its (sanitized) herdr label, so it still appears and can be
  focused; malformed public pane IDs cannot panic the root-pane selection.
