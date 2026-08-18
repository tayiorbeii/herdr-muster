# muster

![muster switcher](docs/screenshot.png)

An agent-aware project switcher for [herdr](https://herdr.dev/) inspired by [Tmux Sesh](https://github.com/joshmedeski/sesh)

Hit one key and you get a fuzzy list of your projects. The ones already running
show up first, tagged with what their agent is doing (blocked, working, done, or
idle), ordered by how recently you were in them: the workspace you were in
before opening the switcher sits at the top, so Enter jumps you straight back
to it, while the switcher's own workspace sinks to the bottom — Escape already
returns there. Everything else sits below, one
keypress away from a fresh workspace.

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

## Bind it to a key

Add this to your herdr `config.toml`, then run `herdr server reload-config`:

    [[keys.command]]
    key = "prefix+space"
    type = "shell"
    command = "herdr plugin pane open --plugin kichel.muster --entrypoint picker"

## Keys inside the switcher

- Start typing immediately, even while workspaces and projects load. Fuzzy
  filtering searches pane labels, terminal-title fallbacks, and full paths for
  open workspaces; open workspaces stay above projects. Arrow keys move the
  selection.
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
