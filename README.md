# BetterAgents

A terminal dashboard for [Claude Code](https://claude.com/claude-code) background agents, a better `claude agents` view.

- Sessions grouped by status (needs input, working, idle, offline) or by your own categories
- Open any session's chat inside the dashboard, or in fullscreen
- Colors follow your Hyprland/Noctalia wallpaper theme; the whole UI goes monochrome when the window loses focus
- Start/stop/delete sessions without leaving the list

Requires the `claude` CLI on your `PATH`. Linux; Rust 2024 toolchain.

## Install

```
cargo install --path .
betteragents
```

## Keys

| Where | Key | Action |
|---|---|---|
| List | `j`/`k`, arrows | move |
| List | `Enter` / `→` | open chat |
| List | `f` | open chat fullscreen |
| List | `Tab` | switch status / category view |
| List | `c` | categorize (`Tab`/arrows pick an existing one) |
| List | `o` / `s` | bring online / take offline |
| List | `d` | delete session (asks first) |
| List | `x` | close the chat view |
| List | `r`, `q` | refresh, quit |
| Chat | `←` (empty prompt) / `F2` | back to list |
| Chat | `F3` | toggle fullscreen |
| Chat | `Shift+Enter` | newline |
| Chat | `Ctrl+click` | open a link |

Categories are stored in `~/.config/betteragents/categories.json`.

## License

MIT
