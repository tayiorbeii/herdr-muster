# muster

![muster switcher](docs/screenshot.png)

An agent-aware project switcher for [herdr](https://herdr.dev/) inspired by [Tmux Sesh](https://github.com/joshmedeski/sesh)

Hit one key and you get a fuzzy list of live workspaces, tabs, named panes, and
project directories. Live targets appear first; project directories are grouped
below them. Directories observed in Herdr stay in recent project history after
their workspace closes, with configured paths, scanned roots, and optional
zoxide suggestions filling out the list. The workspace you were in before
opening the switcher is ranked last among live workspaces because Escape
already returns there. Enter focuses a live target or opens a workspace for a
project directory, and the footer tells you which action applies to the selected
row.

Each project maps to exactly one workspace. muster remembers that pairing from
the moment it creates the workspace, so it never guesses the project from
whatever directory a pane happens to be sitting in, and you never end up with
two workspaces for the same repo.

## Install

You'll need a Rust toolchain, since `herdr plugin install` compiles the binary
from source when it sets the plugin up.

    herdr plugin install marcoskichel/herdr-muster

### Working on it locally

    cargo build --release
    herdr plugin link /path/to/herdr-muster   # e.g. ~/dev/herdr-muster

## Configure

    herdr plugin config-dir kichel.muster   # prints the config dir

Copy `config.toml.example` into that directory as `config.toml` and edit it:

- `paths` lists directories you always want to see.
- `roots` gets scanned one level deep for git repos.
- `use_zoxide` folds in your `zoxide query -l` results when zoxide is installed.

## Jump Pane mode

Jump Pane is a dedicated existing-pane picker. It searches live pane IDs,
paths, labels, titles, workspace/tab context, agents, status, and optional
foreground applications, then focuses the selected pane using Herdr's absolute
`pane.focus` socket API. It never scans projects or creates/binds workspaces.

Tab names are searchable too: each target's `herdr tab list` is read once per
snapshot and joined onto its panes by exact `tab_id`, so typing a tab name (for
example `api server`) surfaces every pane in that tab and still focuses that
exact pane. Tab labels are best-effort display/search metadata only — a missing
or failing `tab list` degrades to unlabeled panes, and identity stays the
runtime-qualified pane ID.

Open it with the `jump-pane` plugin entrypoint (or `herdr-muster --jump-pane`).
Optional named or remote enumeration targets can be configured in `config.toml`:

    [[jump_pane.sessions]]
    name = "work"
    command = "herdr"
    args = ["--session", "work"]
    socket = "~/.config/herdr/work.sock"

Each configured target is invoked as an executable plus argument prefix (never
through a shell). Exact focus requires that target's explicit Unix socket; a
target without one is shown as focus-unsupported rather than misrouted to the
default socket. The default local target may use `~/.config/herdr/herdr.sock`.
Remote targets use `remote = "host"` for enumeration and still require a
forwarded socket for focus.

## Bind it to a key

Add this to your herdr `config.toml`, then run `herdr server reload-config`:

    [[keys.command]]
    key = "prefix+space"
    type = "shell"
    command = "herdr plugin pane open --plugin kichel.muster --entrypoint picker"

## Keys inside the switcher

The legacy project/workspace picker keeps its existing controls. Jump Pane has
only typing, arrows, Enter (exact `pane.focus`), and Escape; it has no create,
bind, close, filesystem-scan, or canonicalization path. Rows include runtime,
workspace/tab, agent/status, application (when lazily available), and a short
pane ID; home paths collapse to `~` and sensitive full paths are not displayed
unless needed to disambiguate.

- Start typing immediately, even while workspaces and projects load. Fuzzy
  filtering searches pane labels, terminal-title fallbacks, and full paths for
  open workspaces; open workspaces stay above projects. Arrow keys move the
  selection.
- Open rows show the workspace's unnamed pane names and its tab names as inline
  context (`· tabs: …`). Tabs and **renamed** panes are also rows of their own —
  sections `TABS` and `PANES` — so typing a tab name or a pane you renamed
  filters straight to that row and Enter jumps to it (`tab.focus` for a tab,
  `pane.focus` for a pane). Unnamed panes get no row; they stay searchable
  through their workspace row. Tabs Herdr still labels with their own number
  (never renamed) are hidden from both the `TABS` section and the inline tab
  context. Alt+digit quick jumps still count workspaces
  only, and Ctrl-N / Ctrl-X still apply to workspace/project rows only.
  Tab metadata is best-effort: a missing or failing `herdr tab list` leaves rows
  without tab context instead of failing the list.
- Enter jumps to the selected project. The open group is sorted
  most-recently-used first with the switcher's own workspace last, so Enter
  on the top row fast-tracks you back to where you were before opening the
  switcher. If the project
  isn't open yet, Enter musters a fresh workspace for it.
- Hold Alt and press a number to jump straight to the matching open
  workspace: Alt+1 is the most recently used workspace — where you were
  before opening the switcher — Alt+2 the one before that, and so on through
  Alt+9, with Alt+0 as the tenth; the switcher's own workspace carries the
  last number. Open rows
  show their number up front (1–9, 0), so the shortcut is always visible.
- Ctrl-N forces a brand new workspace for the selected directory.
- Ctrl-X closes the selected open workspace.
- Esc or Ctrl-C backs out.
