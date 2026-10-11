"""Scripted, non-LLM solvers for the dry run, one per arm.

Each solver drives the demo through the same MCP server an agent would get,
using only what that arm's tools show: the taria solver reads node ids and
advertised actions from the JSON tree, the outline solver reads the same
from the outline text (parse_outline), the tmux solver reads the screen
text and presses keys. A task's plan is a list of steps every solver
implements; the sabotage plan is a plausible wrong one, which the task's
check must fail.

The solvers verify little on purpose. The check on the state dump is the
arbiter, and a solver that quietly did the wrong thing fails there.

Python 3 stdlib only.
"""

import json
import re
from collections import Counter

from tasks import T1_TITLE, T2_TITLE, T3_TITLE, T4_SUFFIX, T5_DELETE, T5_PREFIX, T6_TITLE


class SolverError(Exception):
    """The scripted solver saw something it did not expect."""


class CountingSession:
    """Every tools/call a solver makes goes through here and is counted."""

    def __init__(self, client):
        self.client = client
        self.calls = Counter()

    def call(self, tool, arguments=None):
        self.calls[tool] += 1
        return self.client.call(tool, arguments)


# --- taria: the semantic tree --------------------------------------------------------


def flatten(node):
    yield node
    for child in node.get("children", []):
        yield from flatten(child)


def find(tree, node_id):
    return next((n for n in flatten(tree["root"]) if n.get("id") == node_id), None)


def tree_in(text):
    """The tree an act/key/type_text result carries, or None.

    A tree on its own, or on the second line after a note (an Ignored ack);
    the no-change and no-ack notes carry none, and the caller re-reads.
    """
    for candidate in (text, text.partition("\n")[2]):
        if candidate.lstrip().startswith("{"):
            try:
                return json.loads(candidate)
            except ValueError:
                pass
    return None


class TariaSolver:
    TABS = {"active": ("tab-active", "Active"), "done": ("tab-done", "Done")}

    def __init__(self, session):
        self.s = session

    def tree(self):
        return json.loads(self.s.call("read_tree"))

    def act(self, node, action, value=None):
        args = {"node": node, "action": action}
        if value is not None:
            args["value"] = value
        tree = tree_in(self.s.call("act", args))
        return tree if tree is not None else self.tree()

    @staticmethod
    def items(tree):
        tasks = find(tree, "tasks")
        return [] if tasks is None else tasks.get("children", [])

    def on_tab(self, tab, tree=None):
        tree = tree or self.tree()
        node_id, label = self.TABS[tab]
        if find(tree, "tabs").get("value") != label:
            tree = self.act(node_id, "select")
        return tree

    def locate(self, title):
        """The list item labelled `title`, switching tabs if it is on the other one."""
        tree = self.tree()
        for _ in range(2):
            item = next((n for n in self.items(tree) if n.get("label") == title), None)
            if item is not None:
                return item
            other = "done" if find(tree, "tabs").get("value") == "Active" else "active"
            tree = self.on_tab(other, tree)
        raise SolverError(f"no list item labelled {title!r} on either tab")

    def add(self, title):
        self.act("input", "set_value", title)
        tree = self.act("input", "activate")
        if not any(n.get("label") == title for n in self.items(tree)):
            raise SolverError(f"{title!r} not listed after submitting it")

    def toggle(self, title):
        self.act(self.locate(title)["id"], "toggle")

    def delete(self, title):
        item = self.locate(title)
        tree = self.act(item["id"], "delete")
        if find(tree, "dialog-confirm") is None:
            raise SolverError(f"delete on {item['id']} opened no dialog")
        self.confirm_dialog()

    def confirm_dialog(self):
        self.act("dialog-confirm", "activate")

    def dismiss_dialog(self):
        if find(self.act("dialog", "dismiss"), "dialog") is not None:
            raise SolverError("the dialog is still open after dismissing it")

    def add_from(self, tab, position, suffix):
        items = self.items(self.on_tab(tab))
        if len(items) <= position:
            raise SolverError(f"the {tab} tab has {len(items)} items, no position {position}")
        self.add(items[position]["label"] + suffix)

    def toggle_matching(self, tab, prefix):
        # By node id: an id names its task however the list shifts.
        ids = [n["id"] for n in self.items(self.on_tab(tab)) if n.get("label", "").startswith(prefix)]
        for node_id in ids:
            self.act(node_id, "toggle")

    def toggle_at(self, tab, positions):
        # By position, re-read each time: the mistake identity exists to stop.
        for position in positions:
            self.act(self.items(self.on_tab(tab))[position]["id"], "toggle")


# --- outline: the same tree, one line per node ------------------------------------------
#
# The grammar is taria-mcp's --tree-format outline (docs/cli.md, The outline;
# crates/taria-mcp/src/tree_format.rs): a header line of top-level
# `key=<compact json>` fields, then one node per line, indented two spaces
# per level,
#
#   <id> <role>[ "<label>"][ = "<value>"][ *focused][ <key>=<json> ...][ act: <action>|<action>...]
#
# for example `input text_input "New task" = "x" *focused act: set_value|activate|dismiss`.
# An id, role, action or key is bare when made only of [A-Za-z0-9_.:/@+-] and
# not the marker `act:` itself, and JSON-quoted otherwise; label and value are
# always JSON-quoted. The actions end the line, after the bare marker `act: `,
# joined by `|`, absent when a node has none; a value no parse reads as an
# action comes as compact JSON. This parser reads that back into the JSON
# tree's shape, so the outline solver runs the taria solver's steps on what
# it parsed.
#
# It also reads the outline of batches before that (outline1, probe3), whose
# actions came as ` [<action> <action> ...]` right after the value: no token
# of the current grammar starts with `[`, so the two cannot be confused.

BARE_RE = re.compile(r"[A-Za-z0-9_.:/@+-]+")
ACT_MARKER = "act:"
_DECODER = json.JSONDecoder()


def _quoted(line, pos, what):
    """(the JSON string at `pos`, the position after it)."""
    try:
        value, end = _DECODER.raw_decode(line, pos)
    except ValueError as err:
        raise SolverError(f"bad {what} at column {pos} of outline line {line!r}: {err}")
    if not isinstance(value, str):
        raise SolverError(f"{what} at column {pos} of outline line {line!r} is not a string")
    return value, end


def _name(line, pos, what):
    """(a bare or JSON-quoted name at `pos`, the position after it)."""
    if line.startswith('"', pos):
        return _quoted(line, pos, what)
    match = BARE_RE.match(line, pos)
    if match is None:
        raise SolverError(f"no {what} at column {pos} of outline line {line!r}")
    return match.group(0), match.end()


def _field(line, pos):
    """(key, value, position after) for one ` key=<compact json>` field."""
    key, pos = _name(line, pos, "field key")
    if not line.startswith("=", pos):
        raise SolverError(f"field {key!r} has no '=' in outline line {line!r}")
    try:
        value, end = _DECODER.raw_decode(line, pos + 1)
    except ValueError as err:
        raise SolverError(f"field {key!r} is not JSON in outline line {line!r}: {err}")
    return key, value, end


def _action_list(line, pos):
    """The actions after the `act: ` marker, from `pos` to the end of the
    line: names joined by `|`, each bare, JSON-quoted, or (a value no parse
    reads as an action) compact JSON."""
    actions = []
    while True:
        if line.startswith(("{", "["), pos):
            try:
                action, pos = _DECODER.raw_decode(line, pos)
            except ValueError as err:
                raise SolverError(f"bad action at column {pos} of outline line {line!r}: {err}")
        else:
            action, pos = _name(line, pos, "action")
        actions.append(action)
        if pos == len(line):
            return actions
        if not line.startswith("|", pos):
            raise SolverError(f"expected '|' or the line's end at column {pos} of outline line {line!r}: "
                              "the actions end the line")
        pos += 1


def _node_line(line):
    """One node line, indent stripped, as the JSON tree's node dict (no children)."""
    node, pos = {}, 0
    node["id"], pos = _name(line, pos, "id")
    if not line.startswith(" ", pos):
        raise SolverError(f"no role after the id in outline line {line!r}")
    if line.startswith(("{", "["), pos + 1):  # a role in a shape the line has no name for
        try:
            node["role"], pos = _DECODER.raw_decode(line, pos + 1)
        except ValueError as err:
            raise SolverError(f"bad role in outline line {line!r}: {err}")
    else:
        node["role"], pos = _name(line, pos + 1, "role")
    while pos < len(line):
        if line[pos] != " ":
            raise SolverError(f"expected a space at column {pos} of outline line {line!r}")
        pos += 1
        if line.startswith('"', pos):
            # A quoted token followed directly by `=` is a field's key; the
            # label is the quoted token right after the role, and is never
            # followed by `=` (a value comes after a space, ` = "..."`).
            text, end = _quoted(line, pos, "label or field key")
            if line.startswith("=", end):
                key, value, pos = _field(line, pos)
                node[key] = value
            elif len(node) == 2:
                node["label"], pos = text, end
            else:
                raise SolverError(f"a quoted {text!r} at column {pos} of outline line {line!r} is "
                                  "neither the label (right after the role) nor a field key")
        elif line.startswith('= "', pos):
            node["value"], pos = _quoted(line, pos + 2, "value")
        elif line.startswith(ACT_MARKER + " ", pos) or line.startswith("[", pos):
            if "actions" in node:
                raise SolverError(f"a second action list at column {pos} of outline line {line!r}")
            if line.startswith("[", pos):  # the outline before act: (see above)
                actions, pos = [], pos + 1
                while not line.startswith("]", pos):
                    action, pos = _name(line, pos, "action")
                    actions.append(action)
                    if line.startswith(" ", pos):
                        pos += 1
                node["actions"], pos = actions, pos + 1
            else:
                node["actions"], pos = _action_list(line, pos + len(ACT_MARKER) + 1), len(line)
        elif line.startswith("*focused", pos) and line[pos + 8:pos + 9] in ("", " "):
            node["focused"], pos = True, pos + 8
        else:
            key, value, pos = _field(line, pos)
            node[key] = value
    return node


def parse_outline(text):
    """The tree an outline shows, as {**header fields, "root": node}, every
    node with its children. Raises SolverError on text that is not one."""
    lines = text.split("\n")
    if len(lines) < 2:
        raise SolverError(f"an outline has a header line and a root line, not {text[:200]!r}")
    tree, header, pos = {}, lines[0], 0
    while pos < len(header):
        key, value, pos = _field(header, pos)
        tree[key] = value
        if header.startswith(" ", pos):
            pos += 1
    stack = []  # (depth, node) from the root down to the last line's node
    for line in lines[1:]:
        body = line.lstrip(" ")
        indent = len(line) - len(body)
        if not body or indent % 2:
            raise SolverError(f"outline line {line!r} is empty or indented by an odd count")
        depth, node = indent // 2, _node_line(body)
        while stack and stack[-1][0] >= depth:
            stack.pop()
        if not stack:
            if depth or "root" in tree:
                raise SolverError(f"outline line {line!r} has no parent")
            tree["root"] = node
        elif depth != stack[-1][0] + 1:
            raise SolverError(f"outline line {line!r} skips a level")
        else:
            stack[-1][1].setdefault("children", []).append(node)
        stack.append((depth, node))
    return tree


def outline_in(text):
    """The tree an outline-mode result carries, or None, as tree_in does for
    JSON: the outline on its own, or on the second line after a note. A tree
    the bridge could not express as an outline comes as JSON, and is read so."""
    for candidate in (text, text.partition("\n")[2]):
        if candidate.lstrip().startswith("{"):
            tree = tree_in(candidate)
            if tree is not None:
                return tree
            continue
        try:
            return parse_outline(candidate)
        except SolverError:
            pass
    return None


class OutlineSolver(TariaSolver):
    """The taria solver's steps, on the tree read from the outline text."""

    def tree(self):
        text = self.s.call("read_tree")
        tree = outline_in(text)
        if tree is None:
            raise SolverError(f"read_tree returned no outline: {text[:300]!r}")
        return tree

    def act(self, node, action, value=None):
        args = {"node": node, "action": action}
        if value is not None:
            args["value"] = value
        tree = outline_in(self.s.call("act", args))
        return tree if tree is not None else self.tree()


# --- tmux: the screen ----------------------------------------------------------------

# One task row as the demo draws it: two spaces, the cursor marker or two more
# spaces, the checkbox, the title.
ROW_RE = re.compile(r"^  (>| ) \[([ x])\] (.*\S)\s*$")
CONFIRM_RE = re.compile(r"\[(\w)\] Delete")
DIALOG_MARK = "Confirm delete"
INPUT_FOCUSED_MARK = "[Enter] add task"
EMPTY_MARK = "(no tasks here)"


def screen_rows(screen):
    """[(has_cursor, done, title)] for every task row on screen, top to bottom."""
    rows = []
    for line in screen.splitlines():
        match = ROW_RE.match(line)
        if match:
            rows.append((match.group(1) == ">", match.group(2) == "x", match.group(3)))
    return rows


class TmuxSolver:
    """Reads the screen as text. Which tab is showing is drawn only as bold
    against dim, so the solver infers it from the checkboxes (a tab holds only
    todo or only done tasks) and falls back to its own count of Tab presses on
    an empty tab, where the text alone cannot say."""

    def __init__(self, session):
        self.s = session
        self.tab = "active"  # the demo starts there; tracked through Tab presses

    def screen(self):
        return self.s.call("screen")

    def keys(self, keys):
        screen = self.s.call("send_keys", {"keys": keys})
        if "Tab" in keys.split():
            self.tab = "done" if self.tab == "active" else "active"
        return screen

    def type(self, text):
        return self.s.call("send_keys", {"keys": text, "literal": True})

    def tab_of(self, screen):
        rows = screen_rows(screen)
        if rows:
            self.tab = "done" if rows[0][1] else "active"
        return self.tab

    def on_tab(self, tab, screen=None):
        screen = screen or self.screen()
        if self.tab_of(screen) != tab:
            screen = self.keys("Tab")
            self.tab_of(screen)
        return screen

    def cursor_to(self, screen, index):
        rows = screen_rows(screen)
        cursor = next((i for i, row in enumerate(rows) if row[0]), None)
        if cursor is None:
            raise SolverError(f"no cursor row on screen:\n{screen}")
        steps = index - cursor
        if steps:
            screen = self.keys(" ".join(["Down" if steps > 0 else "Up"] * abs(steps)))
            rows = screen_rows(screen)
            if index >= len(rows) or not rows[index][0]:
                raise SolverError(f"cursor did not reach row {index}: {rows}")
        return screen

    def goto(self, title):
        """Put the cursor on the row titled `title`, switching tabs if needed."""
        screen = self.screen()
        for _ in range(2):
            titles = [t for _, _, t in screen_rows(screen)]
            if title in titles:
                return self.cursor_to(screen, titles.index(title))
            screen = self.keys("Tab")
        raise SolverError(f"no row titled {title!r} on either tab")

    def add(self, title):
        screen = self.screen()
        if INPUT_FOCUSED_MARK not in screen:
            self.keys("i")
        self.type(title)
        screen = self.keys("Enter")
        self.tab = "active"  # submitting shows the tab the new task lands on
        if title not in [t for _, _, t in screen_rows(screen)]:
            raise SolverError(f"{title!r} not on screen after submitting it:\n{screen}")

    def toggle(self, title):
        self.goto(title)
        self.keys("Space")

    def delete(self, title):
        self.goto(title)
        screen = self.keys("d")
        if f"Delete task '{title}'?" not in screen:
            raise SolverError(f"d did not open a dialog for {title!r}:\n{screen}")
        self.confirm_dialog(screen)

    def confirm_dialog(self, screen=None):
        screen = screen or self.screen()
        confirm = CONFIRM_RE.search(screen)
        if confirm is None:
            raise SolverError(f"no confirm key on the dialog:\n{screen}")
        self.keys(confirm.group(1))

    def dismiss_dialog(self):
        if DIALOG_MARK not in self.screen():
            raise SolverError("no dialog on screen to dismiss")
        if DIALOG_MARK in self.keys("Escape"):
            raise SolverError("the dialog is still on screen after Escape")

    def add_from(self, tab, position, suffix):
        rows = screen_rows(self.on_tab(tab))
        if len(rows) <= position:
            raise SolverError(f"the {tab} tab shows {len(rows)} rows, no position {position}")
        self.add(rows[position][2] + suffix)

    def toggle_matching(self, tab, prefix):
        # By title, re-reading the screen before each: rows shift as tasks leave.
        titles = [t for _, _, t in screen_rows(self.on_tab(tab)) if t.startswith(prefix)]
        for title in titles:
            self.toggle(title)

    def toggle_at(self, tab, positions):
        for position in positions:
            self.cursor_to(self.on_tab(tab), position)
            self.keys("Space")


# --- Plans ----------------------------------------------------------------------------

THIRD = 2  # zero-based position of "the third task"

# task id -> (plan, sabotage plan). Steps are (method, *args) on either solver.
PLANS = {
    "T1": ([("add", T1_TITLE)], [("add", T1_TITLE + "s")]),
    "T2": ([("toggle", T2_TITLE)], [("toggle", T3_TITLE)]),
    "T3": ([("delete", T3_TITLE)], [("delete", "Write the taria README")]),
    "T4": (
        [("add_from", "active", THIRD, T4_SUFFIX)],
        [("add_from", "active", THIRD - 1, T4_SUFFIX)],  # off by one
    ),
    "T5": (
        [("toggle_matching", "active", T5_PREFIX), ("delete", T5_DELETE)],
        # Positions 0 and 1 as first read: the second toggle lands on the task
        # that slid into position 1, not the second match.
        [("toggle_at", "active", [0, 1]), ("delete", T5_DELETE)],
    ),
    "T6": (
        [("dismiss_dialog",), ("add", T6_TITLE)],
        [("confirm_dialog",), ("add", T6_TITLE)],
    ),
}

SOLVERS = {"taria": TariaSolver, "tmux": TmuxSolver, "outline": OutlineSolver}


def solve(task_id, arm, session, sabotage=False):
    plan, wrong = PLANS[task_id]
    solver = SOLVERS[arm](session)
    for method, *args in wrong if sabotage else plan:
        getattr(solver, method)(*args)
