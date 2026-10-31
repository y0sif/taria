# The README demo GIF. Regenerate it from the repository root with:
#
#   vhs demo/demo.tape
#
# Left pane: the real taria-demo app. Right pane: demo/drive.py, a scripted
# MCP client (no model) that drives it through the real taria-mcp bridge.
# Both find the app's socket through TARIA_SOCK, set below, so the recording
# never touches a taria-demo you already have running.

Output demo/demo.gif

Set Shell bash
Set Width 1200
Set Height 700
Set FontSize 16
Set FontFamily "JetBrains Mono"
Set Theme "Catppuccin Mocha"
Set Padding 16
Set Framerate 20
Set TypingSpeed 5ms
Set CursorBlink false

Hide

# Clear a server left by an interrupted run, then build into target/.
Type "tmux -L taria-vhs kill-server 2>/dev/null; cargo build -q -p taria-demo -p taria-mcp && clear"
Enter
Wait@600s

# A socket of its own, cleared first so the client waits for this run's.
Type `export TARIA_SOCK="${XDG_RUNTIME_DIR:-$(mktemp -d)}/taria-gif.sock"; rm -f "$TARIA_SOCK"`
Enter

# Two panes side by side on a private tmux server, with no user config. The
# session dies with its client, so an aborted recording leaves nothing behind.
Type `tmux -L taria-vhs -f /dev/null new-session -s gif target/debug/taria-demo \; set -g status off \; set -g destroy-unattached on \; set -g pane-border-status top \; set -g pane-border-style fg=brightblack \; set -g pane-active-border-style fg=brightblack \; set -g pane-border-format "#[fg=white] #{pane_title} " \; select-pane -T "taria-demo: the ratatui app" \; split-window -h -l 61 "python3 demo/drive.py --hold" \; select-pane -T "demo/drive.py: speaks MCP to taria-mcp"`
Enter
Wait+Screen@30s /scripted MCP client/

Show

Wait+Screen@60s /(done|failed): /
Sleep 4s

Hide

# Stop the private server, and with it the app and the client, then remove
# the socket the killed app could not.
Ctrl+B
Type ":kill-server"
Enter
Wait@10s
Type `rm -f "$TARIA_SOCK"`
Enter
Wait@10s
