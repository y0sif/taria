# Competitive Landscape (Phase 0, researched 2026-09-04)

Live-web research done before scaffolding. It is where the README intro, the
positioning the rest of the docs assume, and the project's keywords come from.

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

## Role vocabulary census (researched 2026-09-07)

Done before freezing the wire format, to answer one question: how often
would a real ratatui app have no role to publish? The v0 vocabulary had 18
roles and was drawn from an HTML forms model rather than from what terminal
UIs put on screen.

Method: 15 apps read at pinned commits (gitui, bottom, yazi, atuin,
television, the OpenAI Codex TUI, xplr, kdash, impala, bandwhich, oha,
rainfrog, systemctl-tui, joshuto, csvlens), sampling roughly 188 distinct
main-screen widgets from their actual draw paths. Each was judged by what a
user would call the thing, not by which ratatui primitive drew it. Counts
are per widget kind, not per node instance, so a 500-row table counts once.

| Bucket | Share |
|---|---|
| A named role carries the widget | 53% |
| A named role is defensible but drops something an agent needs | 26% |
| No named role is defensible | 21% |

Eight of ratatui's 15 publishable built-in widgets had no role: Scrollbar,
Sparkline, Chart, BarChart, Canvas, Monthly, and the two logo widgets.
`Scrollbar` appears in 6 of the 15 apps, more than `Tabs`, which did have a
role. Meanwhile `Button` appeared zero times across all 15 and `Checkbox`
twice: what looks like a button in a TUI is almost always hint text beside a
key-driven dialog. The vocabulary was well shaped for modal, dialog-driven
apps and under-shaped for viewer apps (file managers, monitors, log readers,
chat transcripts), which is most of the ratatui showcase and includes Codex.

In the third-party ecosystem, 40% of widget crates and 51% of downloads
landed on categories with no word. The two most informative data points are
other people's attempts at the same vocabulary: the ratatui org's own
`tui-widgets` bundle fits 4 of its 10 widgets, and `tui-realm-stdlib` 12 of
its 20.

What changed as a result: 11 roles added (Tree, TreeItem, Image, Chart,
Scrollbar, Log, Status, Terminal, Link, Select, Option), taking the
vocabulary to 29.

What did not change, and why. The tempting fix was to let apps name their
own roles, as `Other(String)`. Measured, it was nearly free: the wire bytes
are identical and nothing in the workspace branches on a role, so apps are
producers and the consumer is a model reading JSON. It was still rejected on
two grounds. It reaches only the 21% and can do nothing for the larger 26%,
where the app already has a role worth keeping and only needs to say more
about it. And it is the one design this problem space has tried and
abandoned: AT-SPI shipped exactly it as `ROLE_EXTENDED` and deprecated it,
with ATK's maintainer calling it "a hack so applications can register an
element that doesn't fit to a existing role, and then don't worry about
asking to update the accessibility spec"; IAccessible2 specified
`extendedRole` and no browser implemented it; Firefox's free-form MSAA role
string was removed after 20 years unread; AccessKit deleted its equivalent
with "the Role enum should be comprehensive enough" and added
`Role::Terminal` instead. ARIA discards an unrecognized role outright and
forbids `aria-roledescription` on its semantics-free fallback. Every one of
these systems separates which role (closed, degradable) from what to call it
(open, optional, alongside a real role). Adding the roles is what the prior
art tells you to do.

The larger finding is deferred, not dismissed: most of the 26% is not
role-shaped at all. It wants node attributes, `selected` distinct from
`focused`, position in a list, scroll offset, `expanded` and `level` for
trees, and `mode` for modal editors. That last one is a safety issue rather
than a completeness one: without it an agent typing into a vim-modal editor
runs commands instead of typing. Attributes are additive under the freeze,
so they belong to a version that can design them properly.
