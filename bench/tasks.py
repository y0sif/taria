"""Benchmark tasks: what the agent is told, and how its result is graded.

A task is graded on the demo's state dump (`TARIA_DEMO_STATE`, see
examples/demo-app/src/state.rs), never on the screen or the tree: every arm
is judged by what the app actually holds when the agent stops. Every check
holds the dump against the seed (examples/demo-app/src/app.rs, SEED_TASKS), so
a task passes only if the asked-for change happened and nothing else did.

Prompts are identical across arms and name no arm's tools, node ids or
key bindings.

Python 3 stdlib only. Run this file to self-test the checkers.
"""

import copy
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
SEED_IDS = frozenset(task["id"] for task in SEED_TASKS)
SEED_STATE = {
    "tasks": [dict(task) for task in SEED_TASKS],
    "tab": "active",
    "dialog": None,
    "draft": "",
    "quit": False,
}
STATE_KEYS = frozenset(SEED_STATE)


def seed_active():
    """The seed's Active tab, in the order the demo shows it."""
    return [dict(task) for task in SEED_TASKS if not task["done"]]


@dataclass(frozen=True)
class Task:
    """One benchmark task, identical for every arm.

    prompt  what the agent is told, phrased without naming any arm's tools
    check   check(state) -> (passed, reason) over the final state dump
    setup   optional prelude run on the demo before the agent's MCP server
            starts, called with the harness's DemoRun. It drives the demo
            through its own tmux session, which every arm runs in, so it is
            arm-neutral. None means the agent starts on the seed.
    """

    id: str
    prompt: str
    check: Callable[[dict], "tuple[bool, str]"]
    setup: Optional[Callable] = None


def _describe(task):
    return f"{task.get('id')} {task.get('title')!r}"


_ABSENT = "<absent>"


def task_differences(want, got):
    """Every field on which two task objects differ, `want` first.

    All fields, not a fixed list: a field the dump grows later differs from
    the seed by being present, and is reported rather than ignored.
    """
    problems = []
    for field in sorted(set(want) | set(got)):
        before, after = want.get(field, _ABSENT), got.get(field, _ABSENT)
        if before != after:
            problems.append(
                f"{want.get('id', got.get('id'))} {field} is {after!r}, expected {before!r}"
            )
    return problems


def _without_id(task):
    return {k: v for k, v in task.items() if k != "id"}


def unintended_changes(state, expected_tasks, new_tasks=()):
    """Every way `state` differs from what the task asked for.

    expected_tasks  the seed tasks that should still exist, each exactly as
                    given (every field compared)
    new_tasks       the tasks the agent should have created, each spelled
                    without its id (the demo assigns that); every field of a
                    created task must match one of these exactly

    Returns human-readable problems, empty when the state is exactly right.
    Data (the tasks), an open dialog, a leftover draft and a quit app all
    count; which tab is showing does not, since looking at the other tab
    changes nothing an agent was asked to leave alone.
    """
    if not isinstance(state, dict) or set(state) != STATE_KEYS:
        got = sorted(state) if isinstance(state, dict) else type(state).__name__
        return [f"state dump has keys {got}, expected {sorted(STATE_KEYS)}"]
    tasks = state["tasks"]
    if not isinstance(tasks, list) or not all(
        isinstance(t, dict) and isinstance(t.get("id"), str) for t in tasks
    ):
        return [f"state dump's tasks are not a list of objects with ids: {tasks!r}"]

    problems = []
    if state["quit"] is not False:
        problems.append("the demo was quit")
    if state["dialog"] is not None:
        target = state["dialog"].get("task") if isinstance(state["dialog"], dict) else state["dialog"]
        problems.append(f"a delete dialog is still open for {target}")
    if state["draft"] != "":
        problems.append(f"the new-task input holds a leftover draft {state['draft']!r}")

    ids = [t["id"] for t in tasks]
    if len(ids) != len(set(ids)):
        problems.append(f"task ids repeat: {ids}")
    got = {t["id"]: t for t in tasks}
    want = {t["id"]: t for t in expected_tasks}
    for task_id, task in want.items():
        if task_id not in got:
            problems.append(f"{_describe(task)} was deleted")
        else:
            problems.extend(task_differences(task, got[task_id]))
    for task_id, task in got.items():
        if task_id in SEED_IDS and task_id not in want:
            problems.append(f"{_describe(task)} is still there")

    created = [t for t in tasks if t["id"] not in SEED_IDS]
    unmatched = list(created)
    for spec in new_tasks:
        match = next((t for t in unmatched if _without_id(t) == spec), None)
        if match is None:
            problems.append(f"no new task {spec}")
        else:
            unmatched.remove(match)
    for task in unmatched:
        problems.append(f"unexpected new task {task['id']} {_without_id(task)}")
    return problems


def _verdict(problems, success):
    return (False, "; ".join(problems)) if problems else (True, success)


def _new(title):
    return {"title": title, "done": False}


# --- The tasks -------------------------------------------------------------------

T1_TITLE = "Benchmark the bridge"
T2_TITLE = "Record the killer demo"
T3_TITLE = "Wire up the MCP bridge"
T4_SUFFIX = " (follow-up)"
T5_PREFIX = "W"
T5_DELETE = "Sketch the protocol wire types"
T6_TITLE = "Check the dialog"


def check_t1(state):
    """T1: one new todo task titled T1_TITLE; the seed untouched."""
    return _verdict(
        unintended_changes(state, SEED_TASKS, [_new(T1_TITLE)]),
        f"one new task {T1_TITLE!r}; seed intact; no dialog; not quit",
    )


def expected_t2():
    return [{**t, "done": True} if t["title"] == T2_TITLE else dict(t) for t in SEED_TASKS]


def check_t2(state):
    """T2: the task titled T2_TITLE is done; nothing else changed."""
    return _verdict(
        unintended_changes(state, expected_t2()),
        f"{T2_TITLE!r} done; nothing else changed",
    )


def expected_t3():
    return [dict(t) for t in SEED_TASKS if t["title"] != T3_TITLE]


def check_t3(state):
    """T3: the task titled T3_TITLE is gone; nothing else moved."""
    return _verdict(
        unintended_changes(state, expected_t3()),
        f"{T3_TITLE!r} deleted; the other four seed tasks intact; no dialog; not quit",
    )


def expected_t4_title():
    """The third task on the seed's Active tab, plus the suffix."""
    return seed_active()[2]["title"] + T4_SUFFIX


def check_t4(state):
    """T4: one new todo task named after the third Active task; seed untouched."""
    title = expected_t4_title()
    return _verdict(
        unintended_changes(state, SEED_TASKS, [_new(title)]),
        f"one new task {title!r}; seed intact",
    )


def expected_t5():
    """Derived from the seed: every Active task starting with the prefix is
    done, and the named task is gone. Identity, not position: completing the
    first match shifts the Active tab, so an agent working by position marks
    the wrong second task."""
    marked = {t["id"] for t in seed_active() if t["title"].startswith(T5_PREFIX)}
    return [
        {**t, "done": True} if t["id"] in marked else dict(t)
        for t in SEED_TASKS
        if t["title"] != T5_DELETE
    ]


def check_t5(state):
    """T5: exactly expected_t5(), nothing more, nothing less."""
    marked = [t["title"] for t in seed_active() if t["title"].startswith(T5_PREFIX)]
    return _verdict(
        unintended_changes(state, expected_t5()),
        f"{marked} done, {T5_DELETE!r} deleted, nothing else changed",
    )


def setup_t6(run):
    """Open the delete dialog on the first Active task, as a person pressing d
    on the list the demo starts with would. Arm-neutral: it goes through the
    demo's tmux session, before any arm's MCP server starts."""
    first = seed_active()[0]["id"]
    run.send("d")
    run.wait_state(
        lambda s: s.get("dialog") == {"task": first},
        what=f"an open delete dialog for {first}",
    )
    run.wait_screen_settled()


def check_t6(state):
    """T6: one new task titled T6_TITLE, every seed task intact, no dialog."""
    return _verdict(
        unintended_changes(state, SEED_TASKS, [_new(T6_TITLE)]),
        f"one new task {T6_TITLE!r}; every seed task intact; dialog closed",
    )


TASKS = {
    "T1": Task("T1", f"Add a task titled '{T1_TITLE}'.", check_t1),
    "T2": Task("T2", f"Mark the task '{T2_TITLE}' as done.", check_t2),
    "T3": Task("T3", f"Delete the task '{T3_TITLE}'.", check_t3),
    "T4": Task(
        "T4",
        "Add a new task whose title is the title of the third task on the Active "
        f"tab, followed by '{T4_SUFFIX}'.",
        check_t4,
    ),
    "T5": Task(
        "T5",
        f"Mark every task on the Active tab whose title starts with '{T5_PREFIX}' as "
        f"done, then delete the task '{T5_DELETE}'.",
        check_t5,
    ),
    "T6": Task(
        "T6",
        f"Add a task titled '{T6_TITLE}'. Do not delete anything.",
        check_t6,
        setup=setup_t6,
    ),
}


# --- Self-test ---------------------------------------------------------------------


def _state(tasks, **over):
    state = copy.deepcopy(SEED_STATE)
    state["tasks"] = copy.deepcopy(list(tasks))
    state.update(over)
    return state


def _with_new(tasks, title, task_id="task-6", **extra):
    return list(tasks) + [{"id": task_id, "title": title, "done": False, **extra}]


def _toggled(tasks, title):
    return [{**t, "done": not t["done"]} if t["title"] == title else dict(t) for t in tasks]


def _without(tasks, title):
    return [dict(t) for t in tasks if t["title"] != title]


def self_test_cases():
    """{task id: (good states, {name: (bad state, needle)})}."""
    seed = list(SEED_TASKS)
    t4 = expected_t4_title()
    t5 = expected_t5()
    return {
        "T1": (
            [_state(_with_new(seed, T1_TITLE)), _state(_with_new(seed, T1_TITLE, "task-9"), tab="done")],
            {
                "untouched": (_state(seed), "no new task"),
                "wrong title": (_state(_with_new(seed, T1_TITLE + "s")), "unexpected new task task-6"),
                "added twice": (
                    _state(_with_new(_with_new(seed, T1_TITLE), T1_TITLE, "task-7")),
                    "unexpected new task task-7",
                ),
                "added done": (
                    _state(seed + [{"id": "task-6", "title": T1_TITLE, "done": True}]),
                    "no new task",
                ),
                "extra field on new": (_state(_with_new(seed, T1_TITLE, prio=1)), "unexpected new task"),
                "draft left": (_state(_with_new(seed, T1_TITLE), draft="x"), "leftover draft"),
                "seed toggled": (_state(_with_new(_toggled(seed, T2_TITLE), T1_TITLE)), "task-4 done is True, expected False"),
            },
        ),
        "T2": (
            [_state(expected_t2()), _state(expected_t2(), tab="done")],
            {
                "untouched": (_state(seed), "task-4 done is False, expected True"),
                "wrong task": (_state(_toggled(seed, T3_TITLE)), "task-3 done is True, expected False"),
                "both": (_state(_toggled(expected_t2(), T3_TITLE)), "task-3 done is True, expected False"),
                "retitled": (
                    _state([{**t, "title": "x"} if t["id"] == "task-4" else t for t in expected_t2()]),
                    "task-4 title is",
                ),
                "new field": (
                    _state([{**t, "starred": True} if t["id"] == "task-4" else t for t in expected_t2()]),
                    "task-4 starred is True, expected '<absent>'",
                ),
                "quit": (_state(expected_t2(), quit=True), "the demo was quit"),
            },
        ),
        "T3": (
            [_state(expected_t3()), _state(expected_t3(), tab="done")],
            {
                "untouched": (_state(seed), "is still there"),
                "wrong task": (_state(_without(seed, "Write the taria README")), "task-1 'Write the taria README' was deleted"),
                "dialog open": (_state(expected_t3(), dialog={"task": "task-4"}), "dialog is still open"),
                "quit": (_state(expected_t3(), quit=True), "the demo was quit"),
                "draft": (_state(expected_t3(), draft="d"), "leftover draft"),
                "toggled": (_state(_toggled(expected_t3(), T2_TITLE)), "task-4 done is True, expected False"),
                "added": (_state(_with_new(expected_t3(), "x")), "unexpected new task task-6"),
                "bad shape": ({"tasks": []}, "state dump has keys"),
            },
        ),
        "T4": (
            [_state(_with_new(seed, t4))],
            {
                "second task": (_state(_with_new(seed, "Wire up the MCP bridge" + T4_SUFFIX)), "no new task"),
                "third overall": (_state(_with_new(seed, "Wire up the MCP bridge" + T4_SUFFIX)), "unexpected new task"),
                "no suffix": (_state(_with_new(seed, seed_active()[2]["title"])), "no new task"),
                "renamed instead": (
                    _state([{**t, "title": t4} if t["id"] == "task-4" else t for t in seed]),
                    "task-4 title is",
                ),
            },
        ),
        "T5": (
            [_state(t5), _state(t5, tab="done")],
            {
                "by position": (
                    _state(_without(_toggled(_toggled(seed, "Write the taria README"), "Record the killer demo"), T5_DELETE)),
                    "task-3 done is False, expected True",
                ),
                "not deleted": (_state(_toggled(_toggled(seed, "Write the taria README"), T3_TITLE)), "is still there"),
                "only first": (_state(_without(_toggled(seed, "Write the taria README"), T5_DELETE)), "task-3 done is False, expected True"),
                "deleted the wrong one": (
                    _state(_without(_toggled(_toggled(seed, "Write the taria README"), T3_TITLE), "Publish a snapshot per frame")),
                    "was deleted",
                ),
            },
        ),
        "T6": (
            [_state(_with_new(seed, T6_TITLE))],
            {
                "confirmed the dialog": (_state(_with_new(_without(seed, "Write the taria README"), T6_TITLE)), "task-1 'Write the taria README' was deleted"),
                "dialog left open": (_state(_with_new(seed, T6_TITLE), dialog={"task": "task-1"}), "dialog is still open for task-1"),
                "nothing added": (_state(seed), "no new task"),
            },
        ),
    }


def _self_test():
    good_total = bad_total = 0
    for task_id, (good, bad) in self_test_cases().items():
        check = TASKS[task_id].check
        for state in good:
            passed, reason = check(state)
            assert passed, f"{task_id} good state failed: {reason}"
            good_total += 1
        for name, (state, needle) in bad.items():
            passed, reason = check(state)
            assert not passed, f"{task_id} {name}: passed"
            assert needle in reason, f"{task_id} {name}: {reason!r} lacks {needle!r}"
            bad_total += 1
    # The seed itself must fail every task: none of them asks for nothing.
    for task_id, task in TASKS.items():
        assert not task.check(copy.deepcopy(SEED_STATE))[0], f"{task_id} passes on the seed"
    print(
        f"tasks.py: {len(TASKS)} checkers pass {good_total} good states and fail "
        f"{bad_total} bad ones (and the untouched seed)"
    )


if __name__ == "__main__":
    _self_test()
