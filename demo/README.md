Regenerate `demo.gif` from the repository root with `vhs demo/demo.tape` (needs vhs, ttyd, tmux and python3; the tape builds the binaries into `target/`).
The GIF shows the real `taria-demo` app on the left and, on the right, `drive.py` driving it through the real `taria-mcp` bridge with semantic acts on node ids (plus one `type_text`), no raw keys.
`drive.py` is a scripted MCP client with six fixed steps, not an agent or a model; every tree line it prints comes from the bridge's answer.
