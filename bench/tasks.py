"""Benchmark tasks: what the agent is told, and how its result is graded.

A task is graded on the demo's state dump (`TARIA_DEMO_STATE`, see
examples/demo-app/src/state.rs), never on the screen or the tree: both arms
are judged by what the app actually holds when the agent stops. Every check
holds the dump against the seed (examples/demo-app/src/app.rs, SEED_TASKS), so
a task passes only if the asked-for change happened and nothing else did.

Python 3 stdlib only. Run this file to self-test the checkers.
"""

from dataclasses import dataclass
from typing import Callable, Optional

# The demo's seed, as the state dump spells it. Ids are reproducible across
# restarts (app.rs: FIRST_TASK_ID plus a fixed table), so these are stable.
SEED_TASKS = (
    {"id": "task-1", "title": "Write the taria README", "done": False},
    {"id": "task-2", "title": "Sketch the protocol wire types", "done": True},
    {"id": "task-3", "title": "Wire up the MCP bridge", "done": False},
    {"id": "task-4", "title": "Record the killer demo", "done": False},
    {"id": "task-5", "title": "Publish a snapshot per frame", "done": True},
)
SEED_STATE = {
    "tasks": [dict(task) for task in SEED_TASKS],
    "tab": "active",
    "dialog": None,
    "draft": "",
    "quit": False,
}
STATE_KEYS = frozenset(SEED_STATE)


@dataclass(frozen=True)
class Task:
    """One benchmark task, identical for both arms.

    prompt  what the agent is told, phrased without naming either arm's tools
    check   check(state) -> (passed, reason) over the final state dump
    setup   optional scripted prelude run before the agent starts, called with
            the harness's DemoRun; it must leave the app in a state the dump
            reflects. None means the agent starts on the seed.
    """

    id: str
    prompt: str
    check: Callable[[dict], "tuple[bool, str]"]
    setup: Optional[Callable] = None


def _describe(task):
    return f"{task['id']} {task['title']!r}"


def unintended_changes(state, expected_tasks):
    """Every way `state` differs from the seed beyond `expected_tasks`.

    Returns a list of human-readable problems, empty when the state is the
    seed with exactly the expected task list. Data (the tasks), an open
    dialog, a leftover draft and a quit app all count; which tab is showing
    does not, since looking at the other tab changes nothing an agent was
    asked to leave alone.
    """
    problems = []
    if not isinstance(state, dict) or set(state) != STATE_KEYS:
        got = sorted(state) if isinstance(state, dict) else type(state).__name__
        return [f"state dump has keys {got}, expected {sorted(STATE_KEYS)}"]
    if state["quit"]:
        problems.append("the demo was quit")
    if state["dialog"] is not None:
        problems.append(f"a delete dialog is still open for {state['dialog'].get('task')}")
    if state["draft"]:
        problems.append(f"the new-task input holds a leftover draft {state['draft']!r}")

    got = {task["id"]: task for task in state["tasks"]}
    want = {task["id"]: task for task in expected_tasks}
    for task_id, task in want.items():
        if task_id not in got:
            problems.append(f"{_describe(task)} was deleted")
            continue
        for field in ("title", "done"):
            if got[task_id][field] != task[field]:
                problems.append(
                    f"{task_id} {field} changed: {task[field]!r} -> {got[task_id][field]!r}"
                )
    for task_id, task in got.items():
        if task_id not in want:
            seeded = any(seed["id"] == task_id for seed in SEED_TASKS)
            problems.append(
                f"{_describe(task)} is still there"
                if seeded
                else f"unexpected new task {_describe(task)}"
            )
    return problems


def check_t3(state):
    """T3 passes when task-3 is gone and nothing else moved."""
    expected = [task for task in SEED_TASKS if task["id"] != "task-3"]
    problems = unintended_changes(state, expected)
    if problems:
        return False, "; ".join(problems)
    return True, "task-3 deleted; the other four seed tasks intact; no dialog; not quit"


TASKS = {
    "T3": Task(
        id="T3",
        prompt="Delete the task 'Wire up the MCP bridge'.",
        check=check_t3,
    ),
}


def _self_test():
    import copy

    done = copy.deepcopy(SEED_STATE)
    done["tasks"] = [t for t in done["tasks"] if t["id"] != "task-3"]
    assert check_t3(done)[0], check_t3(done)

    on_other_tab = copy.deepcopy(done)
    on_other_tab["tab"] = "done"
    assert check_t3(on_other_tab)[0], "looking at the other tab is not a change"

    cases = {
        "untouched": (copy.deepcopy(SEED_STATE), "is still there"),
        "wrong task": (
            {**copy.deepcopy(SEED_STATE), "tasks": [dict(t) for t in SEED_TASKS if t["id"] != "task-1"]},
            "task-1 'Write the taria README' was deleted",
        ),
        "dialog open": ({**copy.deepcopy(done), "dialog": {"task": "task-4"}}, "dialog is still open"),
        "quit": ({**copy.deepcopy(done), "quit": True}, "the demo was quit"),
        "draft": ({**copy.deepcopy(done), "draft": "d"}, "leftover draft"),
        "toggled": (
            {**copy.deepcopy(done), "tasks": [{**t, "done": not t["done"]} if t["id"] == "task-4" else t for t in done["tasks"]]},
            "task-4 done changed",
        ),
        "added": (
            {**copy.deepcopy(done), "tasks": done["tasks"] + [{"id": "task-6", "title": "x", "done": False}]},
            "unexpected new task task-6",
        ),
        "bad shape": ({"tasks": []}, "state dump has keys"),
    }
    for name, (state, needle) in cases.items():
        passed, reason = check_t3(state)
        assert not passed, f"{name}: passed"
        assert needle in reason, f"{name}: {reason!r} lacks {needle!r}"
    print(f"tasks.py: check_t3 passes 2 good states and fails {len(cases)} bad ones")


if __name__ == "__main__":
    _self_test()
