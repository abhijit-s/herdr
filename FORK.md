# Fork changes

This fork of [herdrdev/herdr](https://github.com/herdrdev/herdr) carries local
features and fixes that are not upstream. It tracks upstream via periodic
`Merge upstream herdrdev/herdr into fork master` commits.

This file is the inventory of what diverges. Update it in the same commit that
adds, removes, or upstreams a fork-only change, so the divergence never has to
be reconstructed from `git log`.

## Why this file exists

`CHANGELOG.md` interleaves fork entries with upstream's, so it cannot answer
"what is ours?". Reconstructing that from history is unreliable because upstream
merges rewrite context and some fork commits are later superseded upstream.

## Conventions

- Every fork-only feature is gated behind a config key wherever practical, so
  the default build stays close to upstream and merges stay cheap.
- Fork releases use a semver prerelease suffix (`0.8.3-abhi.1`) so they sort
  below the equivalent upstream version and never squat an upstream tag.
- `just release` rejects prerelease versions, so fork releases bump
  `Cargo.toml`, curate the changelog, tag, and push by hand.

## Divergence summary

As of `v0.8.3-abhi.1`: 41 non-merge commits, 62 files, +5430/-383 vs
`upstream/master`.

Regenerate with:

```bash
git fetch upstream
git log --oneline --no-merges upstream/master..master
git diff --stat upstream/master...master
```

## Features

### Command palette

Native floating command palette with fuzzy matching, jump navigation,
wrap-around, and a position counter. Assembles its catalog from built-in
navigate actions, plugin entries, and custom commands, with identity dedup and
ranked filtering.

- Config: `[command_palette]`
- Files: `src/app/command_palette.rs`, `src/ui/command_palette.rs`
- Supersedes the `jt.command-palette` plugin

### Status strip

tmux-style right-aligned status strip in the tab bar, with `#[...]` style
directives, theme token resolution, powerline separator support, and a
socket-fed push lane exposing `#{slot:NAME}` tokens.

- Config: `[ui.status]` (`status_right`, `status_right_length`, `status_interval`)
- Files: `src/ui/status_right.rs`, `src/app/api/status.rs`, `src/api/schema/status.rs`
- Hung strip commands time out rather than blocking the tab bar

### Rounded borders

Rounded corners for pane borders, modals, menus, the navigator, the command
palette, and toast notifications. Pane borders are drawn cell-by-cell, so
rounding swaps the four true-corner glyphs; junctions keep square glyphs
because Unicode has no rounded variants for them.

- Config: `ui.rounded_borders` (default `true`)
- Files: `src/ui/panes.rs`, `src/ui/widgets.rs`, `src/ui/status.rs`

### Tab edge styles

Tabs draw a cap glyph at each end in the tab's own colour, so a single-row tab
reads as a shaped tile. Caps sit inside the padding `tab_width` already
reserves, so no style changes tab widths, hit areas, or scroll math.

- Config: `ui.tab_style` — `block` (default), `round`, `slant`, `powerline`
- `powerline` needs a Nerd Font; the others render in any font
- Files: `src/ui/tabs.rs`

### Configurable pane resize step

- Config: `keys.resize_step`

## Fixes

- Tab labels sit on the exact centre column. `MIN_TAB_WIDTH` pads short labels
  out to eight columns, which left an odd padding budget with no centre column.
- Inline Kitty images no longer blank in panes via the re-emit path.
- Config diagnostics render as a bottom-right toast instead of a persistent
  top-right banner, and `toast_deadline` is set at startup so the warning
  auto-dismisses.
- Worktree dialog errors render as toasts, in the high-contrast title row.
- Tab clicks are no longer swallowed by drag-reorder jitter.

## Local tooling

- `just install-local` builds, ad-hoc signs, and swaps the local binary into the
  Homebrew keg. This overwrites the keg in place, so a `brew upgrade` or
  reinstall of herdr silently reverts it — reinstall from the tap instead.
- Zig toolchain pinned via `mise.toml`; see the note on the Xcode 26.4+ zig
  linker bug and the `zig@0.15` workaround.
- Plugin relink hint points at `just setup herdr-plugins`.

## Deliberate upstream reverts

- `Revert "fix: support non-us shifted keybindings (#1876)"` (`3c150694`).
  Re-evaluate on each upstream merge; drop the revert if upstream reworks it.

## Distribution

- Tap: `abhijit-s/homebrew-tap` → `brew install abhijit-s/tap/herdr`
- Releases: GitHub Releases on this fork, built by `.github/workflows/release.yml`
  on a `v*` tag. Two jobs (`close-released-issues`, `update-latest-json`) need
  upstream-only secrets and fail here; they run after the release publishes, so
  assets are unaffected.
- Linux binaries can also be built without tagging via the
  "Build artifacts (manual)" workflow.
