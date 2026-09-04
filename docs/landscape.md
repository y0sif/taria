# Competitive Landscape (Phase 0, researched 2026-09-04)

Live-web research done before scaffolding. This document feeds the README intro,
`docs/comparison.md`, `docs/faq.md`, and SEO keywords.

## The original idea, and why it pivoted

The starting idea was an agent-side driver that lets AI agents use TUIs by
taking over input and reading the screen. Research showed that side of the
problem is already well-served, so taria pivoted to the unclaimed framework
side: a semantic accessibility layer that TUI apps opt into, so agents read a
widget tree instead of scraping a screen.

## Competitive matrix

| Name | Type | Status | Approach | Gaps taria fills |
|---|---|---|---|---|
| [ht](https://github.com/andyk/ht) | OSS, Rust, Apache-2.0 | Active, ~903 stars | Headless VT100 wrapper, JSON stdin + WebSocket API, built explicitly "to make terminals easy for LLMs" | Screen-level only, no semantic widget understanding, no Windows |
| tmux + send-keys | Incumbent workaround | Universal, 18+ years | Detached sessions, virtual keystrokes, capture-pane | Screen-level, brittle waits, agents guess at structure |
| tmux/PTY MCP servers ([bnomei/tmux-mcp](https://github.com/bnomei/tmux-mcp), [lox/tmux-mcp-server](https://github.com/lox/tmux-mcp-server), [terminal-control-mcp](https://github.com/wehnsdaefflae/terminal-control-mcp), pty-mcp) | OSS MCP servers | Active, fragmented | Wrap tmux/PTY as MCP tools | Screen-level; taria-mcp exposes semantics through the same MCP channel |
| [agent-tui](https://github.com/pproenca/agent-tui) | OSS, Rust, MIT | Active, ~117 stars | PTY daemon, screenshots + input via CLI/JSON-RPC | Unix-only, screen-level |
| [tui-use](https://github.com/onesuper/tui-use) | OSS, [Show HN 2026](https://news.ycombinator.com/item?id=47692661) | Just shipped | Screen capture + keystrokes for agents | HN's top criticism was "agents can just use tmux"; screen-level |
| [agent-terminal](https://github.com/jasonkneen/agent-terminal) | OSS, node-pty | Active | Headless terminal automation | Screen-level |
| tuibot / tuicov ([arXiv 2608.03743](https://arxiv.org/abs/2608.03743)) | Academic OSS | Released 2026 | Instrumented LLM testing of TUIs across ratatui, bubbletea, textual, ink | Testing-focused; its finding that LLMs are weak at raw screen interaction is taria's motivation |
| Claude Code tmux skills ([obra/superpowers-lab](https://claudemarketplaces.com/skills/obra/superpowers-lab/using-tmux-for-interactive-commands)) | Harness-level pattern | Shipping today | Skills teaching agents to drive vim/REPLs via tmux | Proof harnesses want this; still screen-level underneath |

## Platform threats

- Harnesses already reach TUIs "well enough" through tmux skills and MCP
  servers. A first-party interactive-terminal tool in Claude Code or Codex
  would flatten every agent-side driver at once. taria is deliberately not an
  agent-side driver: a native harness tool would still be screen-level and
  could itself consume taria semantics.
- Adoption is the main risk: a protocol without framework buy-in is dead.
  Mitigation: demo-first, prove a measurable win over tmux scraping, then
  approach ratatui maintainers (their community is AI-friendly; OpenAI Codex
  is built on ratatui).

## Differentiation statement

Given that ht, tmux+send-keys, and a crowd of PTY/MCP drivers already let
agents scrape TUI screens, taria earns its existence by working on the other
side of the terminal: a framework-level semantic protocol that lets TUI apps
expose their widget tree, focus state, and available actions directly to
agents. It is the accessibility-tree moment for terminals, which no existing
tool provides, starting as a ratatui integration with an MCP bridge so any
harness can consume it unchanged.

## Supporting research signals

- arXiv 2608.03743 ("Can LLMs Test Terminal User Interfaces?", Chao Peng et
  al.): frontier LLMs do not beat random exploration at screen-level TUI
  interaction; input semantics are where they win. Screen-scraping is a weak
  agent interface.
- CI4A (arXiv 2601.14790) and LUMOS (arXiv 2606.30697): semantic component
  interfaces beat raw observation for web and OS agents. No TUI equivalent
  exists.
- "Terminal Is All You Need" (arXiv 2603.10664): terminal-based agent tools
  dominate in practice; representational compatibility between agent and
  interface is a core design property.
