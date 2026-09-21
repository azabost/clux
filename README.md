# clux

[![MIT License][license-shield]][license-url]

A tmux plugin that shows the status of Claude Code sessions running in your tmux
sessions. Also works as a standalone CLI.

Written in Rust.

> **This is a fork** of [calthejuggler/clux](https://github.com/calthejuggler/clux).
> See the [changelog](CHANGELOG.md) for what changed.

## What it does

clux looks at `~/.claude/sessions/`, works out which Claude Code sessions are
running, and maps each one to the tmux pane it lives in. It then shows their
status right in your tmux session picker.

For each session it tells you:

- **State** -- active (Claude is working) or idle (Claude finished and is waiting for you)
- **Claude session** -- the name Claude Code gave the conversation
- **Summary** -- what Claude is working on, from its recap or your last prompt

When you hit `prefix + s` to switch sessions you can see which ones have Claude
running without checking each one by hand. There is also a dedicated Claude
picker (`prefix + a`) showing only sessions with Claude, sorted by most recent
activity.

![Session picker with Claude status](assets/session-picker.png)

![Claude picker with fzf](assets/claude-picker.png)

## Getting started

You need [tmux](https://github.com/tmux/tmux) and [Claude Code](https://claude.ai/code).

### Install as a tmux plugin (recommended)

Add this to your `.tmux.conf`:

```sh
set -g @plugin 'azabost/clux'
```

Then press `prefix + I` to install. The plugin downloads a pre-built binary for
your platform on first load, and again whenever the version changes.

### Install as a standalone CLI

**From the latest release:**

```sh
curl -fsSL https://raw.githubusercontent.com/azabost/clux/main/scripts/install.sh | bash
```

This downloads a pre-built binary to `~/.local/bin/clux`. You can pass a custom
path:

```sh
curl -fsSL https://raw.githubusercontent.com/azabost/clux/main/scripts/install.sh | bash -s -- /usr/local/bin/clux
```

Pre-built binaries are available for Linux and macOS, x86_64 and aarch64.

**From source:**

```sh
cargo install --git https://github.com/azabost/clux
```

## CLI usage

```
clux <COMMAND>

Commands:
  update  Update tmux session variables with Claude Code status
  list    List Claude Code sessions (tab-separated)
  select  Open tmux choose-tree with Claude Code status
  pick    Open a Claude-only session picker (fzf or tmux menu)
```

`clux list` is the most useful one outside tmux. It prints a tab-separated table
of every Claude Code session it can find, with its state, Claude session name,
summary, working directory and tmux session name.

`clux update`, `clux select` and `clux pick` all require tmux to be running.

Each command that accepts a filter argument supports these values:

| Filter | Shows |
|--------|-------|
| `all` | All sessions (default) |
| `has-claude` | Only sessions with Claude running |
| `active` | Only sessions where Claude is working |
| `idle` | Only sessions where Claude finished and is waiting for you |

## Configuration

These options go in your `.tmux.conf` and only apply when using clux as a tmux
plugin.

| Option | Default | Description |
|--------|---------|-------------|
| `@clux-key` | `s` | Key to bind the session picker (after prefix) |
| `@clux-claude-key` | `a` | Key to bind the Claude-only picker (after prefix) |
| `@clux-format` | ` \| {total} ({detail})` | Format string for session status |
| `@clux-filter-binds` | _(none)_ | Comma-separated `key:filter` pairs for filtered pickers |
| `@clux-fzf` | _(on)_ | Set to `off` to use tmux menus instead of fzf in the Claude picker |
| `@clux-recaps` | _(on)_ | Set to `off` to use history summaries instead of Claude Code recaps |
| `@clux-sort` | `recent` | Sort order for `list` and `pick` commands |

### The Claude picker

The Claude picker (`prefix + a`) gives you a focused view of just your Claude
sessions: state, the Claude session name, a summary of what Claude is doing, and
the working directory. Sessions are sorted by most recently switched-to by
default, with the current session pinned to the bottom (like harpoon).

If Claude Code has produced a recap and no conversation message has been written
after it, clux uses that recap as the summary. Set `@clux-recaps` to `off` to
always use the older history-based summary.

If you have `fzf-tmux` installed it is used for fuzzy finding. Otherwise clux
falls back to a tmux display-menu, which you can force with
`set -g @clux-fzf 'off'`.

### Sort order

Both the Claude picker and `clux list` support a configurable sort order. Set
`@clux-sort` in your `.tmux.conf` or use the `--sort` CLI flag, which takes
precedence.

| Value | Description |
|-------|-------------|
| `recent` | Most recently switched-to first, current session last (default) |
| `timestamp-desc` | Most recent activity first |
| `timestamp-asc` | Oldest activity first |
| `status` | Idle first, then active |
| `status-rev` | Active first, then idle |

Ties break by timestamp descending, except for `status-rev` which uses timestamp
ascending.

```sh
set -g @clux-sort 'status'
# or via CLI
clux list --sort status
clux pick --sort status-rev
```

### Format placeholders

| Placeholder | Description | Example |
|-------------|-------------|---------|
| `{total}` | Total Claude sessions | `3` |
| `{active}` | Sessions where Claude is working | `2` |
| `{idle}` | Sessions where Claude finished | `1` |
| `{detail}` | Smart summary (omits zero counts) | `2 active, 1 idle` |

### Example

```sh
set -g @clux-key 's'
set -g @clux-claude-key 'a'
set -g @clux-format ' | {active}/{total}'
set -g @clux-filter-binds 'S:has-claude,A:active,I:idle'
set -g @clux-sort 'status'
```

This binds `prefix + s` to the full session picker, `prefix + a` to the Claude
picker, `prefix + S` to show only sessions with Claude, `prefix + A` for active
sessions and `prefix + I` for idle ones, and sorts idle sessions first.

## Credits

Original work by Cal Courtney ([@calthejuggler](https://github.com/calthejuggler))
— [calthejuggler/clux](https://github.com/calthejuggler/clux). MIT licensed; this
fork keeps that licence.

- [Claude Code](https://claude.ai/code)
- [tmux](https://github.com/tmux/tmux)

[license-shield]: https://img.shields.io/github/license/azabost/clux.svg?style=for-the-badge
[license-url]: https://github.com/azabost/clux/blob/main/LICENSE
