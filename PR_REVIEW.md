# Thermonuclear Code Review — herdr-muster fork (tb/organize-fuzzy-search)

**Scope reviewed:** every change added to the fork since it diverged from upstream
`marcoskichel/herdr-muster` at `9a1e42e` ("docs: enhance README with Tmux Sesh reference").

**Fork commits under review (5):**

| Commit | Subject |
|---|---|
| `b879cb4` | feat: harden muster switcher filtering |
| `95afd13` | fix: harden shutdown bounds, terminal injection, and state file safety |
| `12a0306` | fix: avoid cross-field fuzzy matches |
| `9efc5ff` | feat: sort picker open group by most recently used |
| `03d1f2b` | feat: alt+digit quick jump to recent workspaces |
| `a85059d` | *(review-fix commit from this review: rustfmt, deterministic state file, corrupt-state warning, display sanitization)* |

**Delta:** 12 files, ~2,935 insertions / 472 deletions (src: `herdr.rs`, `main.rs`,
`model.rs`, `picker.rs`, `refresh.rs` (new), `registry.rs`, `sources.rs`; plus `tests/pty_lifecycle.rs`
(new), `README.md`, `CONTEXT.md`, `Cargo.toml`, `Cargo.lock`).

---

## Round 1 — Architecture & Data Flow

### 1.1 Module boundaries — PASS
Clean layering: `main` (orchestration loop) · `config` (TOML) · `sources` (project
discovery) · `registry` (identity + recency persistence) · `herdr` (CLI wrapper,
JSON parsing, process safety) · `model` (row assembly/sorting) · `picker` (TUI) ·
`refresh` (async snapshot pipeline). The `Herdr` trait keeps the CLI swappable,
which the test suite exploits (FakeHerdr + fake shell scripts). No layering
violations, no globals, no `unsafe` beyond the deliberate `libc`/`fcntl`/
`setsid` process-control calls in `herdr.rs`, each with a Unix-only `cfg` gate.

### 1.2 Concurrency model — PASS
One background worker per picker session; communication is an `mpsc` channel
(`Partial` → `Ready` → disconnect) plus an `Arc<AtomicBool>` cancellation token.
Ownership moves eliminate shared mutable state: the worker receives *copies* of
the registry live-map and MRU, and the main thread mutates the registry only
after the picker (and its worker) have shut down. `Updates::drop` sets
cancellation, waits ≤200 ms for cooperative exit, and deliberately **detaches**
a worker stuck in a filesystem syscall (e.g. an unavailable automount) so picker
teardown never blocks. Trade-off documented; a permanently stuck worker leaks a
thread for the process lifetime — acceptable for a short-lived picker process.

### 1.3 Persistence & crash safety — PASS (2 improvements applied)
`Registry::save` is atomic and durable: temp file in the same directory →
`write_all` → `sync_all` → `persist` (rename) → fsync of the parent directory.
A dedicated test proves the predictable temp path cannot be symlink-attacked.
Loading canonicalizes keys and collapses symlink aliases *deterministically*
(non-symlink spelling first, then lexical order) so migration never depends on
`HashMap` iteration order.

Two gaps closed in this review:
- **Deterministic serialization:** the `map` field was a `HashMap`, so
  `state.json` key order (and thus file diff) was nondeterministic across saves.
  Switched to `BTreeMap` (same JSON schema, stable key order).
- **Silent data loss:** a corrupt state file fell back to a default registry
  with *no indication* — every project identity and the MRU order vanished
  without a trace. Now warns on stderr ("state file … is corrupt (…); starting fresh").

### 1.4 Subprocess hygiene — EXEMPLARY
Every `herdr`/`zoxide` invocation: spawned in its own session (`setsid`),
polled via `try_wait` + nonblocking drains, bounded at 30 s and 8 MiB, and torn
down as a **process group** (SIGTERM → 50 ms grace → SIGKILL) on
cancellation/timeout — and on the success path when a shell left background
descendants holding the pipes. `kill(process_group, 0)` probes avoid the grace
delay when no descendants exist. The PTY test proves descendants and pipe-holding
escaped processes are reaped and the terminal is restored on both exit paths.

### 1.5 Terminal-safety architecture — PASS (1 gap closed)
All terminal-originated text (pane labels, terminal titles, workspace labels,
cwd, agent names) passes through `clean()`: C0/DEL/C1 control characters are
stripped before Ratatui renders the string (Ratatui writes strings raw, so this
is the injection boundary).

**Gap found:** project *display* strings sourced from config paths, zoxide, and
git-repo directory names were **not** sanitized — a directory literally named
with ESC sequences (hostile checkout, crafted zoxide entry) would reach the
terminal. Commit `95afd13`'s hardening covered pane/workspace text only.
**Fixed in this review:** a shared `sanitize_text()` (extracted from `clean()`)
now also strips control characters from candidate display strings and row names;
the identity *path* is preserved untouched, so workspace creation and registry
keys are unaffected. New unit test covers it.

### 1.6 UX architecture — PASS
Recency-first open group (origin → persisted MRU → never-focused by agent-state
priority) is coherent, and the Alt+digit quick-jump reuses the *same* filtered
ordering as the rendered row numbers, so the visible 0–9 labels always match the
jump targets by construction. Partial/Ready snapshots let the user type before
discovery finishes without rows flickering away.

---

## Round 2 — Line-by-line Correctness & Edge Cases

### Verified correct (with test/evidence)
- **C1 Alt+digit ↔ numbering agreement.** `build()` numbers open rows by their
  position among *visible open matches*; `nth_open_jump()` finds the Nth open
  row in the *same* filtered order. No divergence between label and target.
- **C2 Root-pane selection.** `Pane::number()` decodes herdr's public pane IDs
  (bijective base-32 over `123456789ABCDEFGHJKMNPQRSTVWXYZ0`) with
  `checked_mul/checked_add` (overflow-safe); malformed suffixes tie-break by
  pane id. Unit tests cover `p1`, `pA`, `p0`→32, `p11`→33, invalid `pI`.
- **C3 Selection identity across refreshes.** `apply_snapshot` preserves the
  selected row by `RowId` (`Open(ws_id)`/`Project(path)`), falling back to row 0;
  `apply_partial` retains last-known dormant rows so a loading refresh cannot
  make projects disappear.
- **C4 Close→reopen loop.** Closing removes the row from carried-over picker
  state and re-spawns a fresh refresh; the loop continues inside the switcher.
- **C5 Registry invariants.** `bind`/`unbind_if_bound`/`reconcile`/`touch`/
  `forget` are all conditional and return whether state changed; `unbind_if_bound`
  refuses to unbind a *newer* rebinding (Force-New case). Tested.
- **C6 Failed refresh never reconciles.** `live_workspace_ids` is
  `Option<HashSet>`; `None` short-circuits `registry.reconcile`, so a herdr
  query failure cannot clear persisted bindings. Tested.
- **C7 Reconcile ordering.** Reconcile runs before the outcome action; Close
  additionally unbinds + forgets the closed workspace. No stale bindings.
- **C8 Fuzzy matches never cross metadata fields** (commit `12a0306`): each
  field is scored independently and the max taken; the regression test
  (`deadpo` no longer matches `instructional-design-agent`) pins this.
- **C9 Unicode-safe rendering.** Grapheme-aware truncation (`e\u301` kept whole),
  display-width budgeting, and a property test asserting `row_line` never exceeds
  width 0..80 for wide CJK pane names.
- **C10 `clean()` covers C0 + DEL + C1 ranges** (0x00–1F, 0x7F, 0x80–9F) before
  trim; pane titles like `\x1b]0;build\x07` become inert text.

### Findings (severity-ordered)
| # | Severity | Finding | Disposition |
|---|---|---|---|
| R2-1 | **Medium** | `cargo fmt --check` failed on fork files (`model.rs`, `picker.rs`, new `refresh.rs`) — new code was not rustfmt-clean. | **Fixed** (`a85059d`); remaining drift is pre-existing on `main` (`config.rs`, untouched by the fork). |
| R2-2 | **Low** | Terminal-injection gap for project display strings (see §1.5). | **Fixed** (`sanitize_text` + test). |
| R2-3 | **Low** | Corrupt state file silently reset identities/MRU. | **Fixed** (stderr warning). |
| R2-4 | **Low** | `state.json` serialization order nondeterministic. | **Fixed** (`BTreeMap`). |
| R2-5 | **Low** | `close_workspace` / `focus_workspace` / `create_workspace` failures abort the whole picker (error printed, pane closed) instead of returning to the switcher. | Accepted for v1: simple, observable, recoverable. Not changed. |
| R2-6 | **Low** | `apply_partial` can transiently show a stale dormant row for a project that just opened (Open vs Project `RowId`s differ, so the stale row isn't deduped during the partial phase). Replaced by `Ready`. | Accepted: visual only, rare, self-healing. |
| R2-7 | **Info** | `command_output` non-Unix fallback reads pipes only after `wait()` — could deadlock >64 KB output. Compile-only path; declared platforms are linux/macos (Unix path always used). | Noted only. |
| R2-8 | **Info** | `filter()` recomputed on every 50 ms redraw (20 fps of nucleo scoring). Fine for <500 rows; a cache would be an optimization, not a fix. | Noted only. |
| R2-9 | **Info** | Alt+letter is intentionally a no-op (only Alt+digit is bound); macOS Option glyphs (`º¡™£¢∞§¶•ª`) are mapped to digits for terminals that send symbols instead of ESC-prefixed digits (documented in code comments). | By design. |
| R2-10 | **Info** | `clean()` also removes TAB/NL from titles (single-line titles). | By design. |

### Panic/error-path audit
- All `unwrap`/`expect` in production code are locally justified: `stdout`/`stderr`
  are configured `piped` before `take()`; `stop_command`'s kills are best-effort
  (`let _`); `try_wait` errors are mapped; serde failures become `InvalidJson`
  errors with context. `usize::MAX` guards keep arithmetic overflow-free
  (`saturating_*` throughout picker layout).
- `Updates::drop` joins only a finished worker; never blocks teardown beyond
  200 ms. `Registry::load`/`save` handle missing dirs, read-only parents, and
  corrupt files without panicking.

---

## Round 3 — Integration, Test, & PR Readiness

### Validation matrix (all on the fork branch, after review fixes)
| Check | Result |
|---|---|
| `cargo build` / `cargo build --release` | ✅ clean |
| `cargo test --all-targets` | ✅ 56 unit + 1 PTY E2E, 0 failures |
| `cargo clippy --all-targets -- -D warnings` | ✅ clean |
| `cargo fmt --check` | ✅ fork files clean (only pre-existing `config.rs` drift from `main` remains) |
| PTY lifecycle test | ✅ stable across 4 consecutive runs (both Esc and Ctrl-C paths) |

### Test-suite strengths
- **Real controlling terminal E2E** (`tests/pty_lifecycle.rs`): `pty.fork()` gives
  the binary a genuine tty; verifies raw-mode entry, typing *before* discovery
  completes, stable cursor-position rendering of the query, prompt appearance in
  <1 s, both exit paths, termios restoration (ICANON/ECHO), picker exit in <1 s,
  and reaping of both the fake herdr and its `sleep 5` descendant.
- **Cancellation depth:** worker thread + `sleep` subprocess; setsid-escaped
  pipe-holder (python fork + setsid) reaped <1 s; unbounded output bounded at
  8 MiB; `Updates::drop` never waits for a stuck worker.
- **Persistence:** atomic save/load round-trip, symlink-alias collapse, temp-symlink
  attack, unbind-preserves-newer-rebinding, canonical key normalization.
- **Model/picker:** recency sort (origin-first and no-origin), cross-field fuzzy
  regression, selection identity across snapshots, dormant retention, quick-jump
  numbering, width safety for wide text, macOS glyph mapping.
- **Sources:** repo-root rules (worktrees/submodules excluded via `.git`-is-file),
  dedup, cancellation mid-traversal, display sanitization (new).

### PR hygiene
- Diff confined to fork-touched files; the review-fix commit `a85059d` is small
  and separately reviewable (6 files, +88/−17).
- Docs updated consistently: README keymap/recency prose matches implementation;
  CONTEXT.md gains the Recency term.
- `Cargo.toml`: `tempfile` correctly moved from dev-dependencies to
  dependencies (registry save uses it at runtime); `unicode-segmentation`,
  `unicode-width`, `libc` are runtime deps of the picker/process layer.

### Outstanding (out of scope, pre-existing on `main`)
- `src/config.rs` is not rustfmt-clean on `main`; left untouched to keep the PR
  diff focused. A follow-up "cargo fmt whole repo" commit could be proposed
  separately.

---

## Verdict

The fork is **PR-ready**. Round 1 found a sound architecture with exemplary
subprocess hygiene; Round 2 found four real issues (rustfmt drift, a terminal
injection gap for project display strings, silent corrupt-state reset, and
nondeterministic state serialization) — all fixed and regression-tested; Round 3
validates build, tests (57), clippy, fmt, and a genuinely hard PTY E2E test.
Remaining notes are Low/Info and either deliberate design choices or
pre-existing upstream drift.
