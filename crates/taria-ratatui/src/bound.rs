//! Bounding a snapshot to the one line a bridge will read.
//!
//! A bridge reads each app line up to [`MAX_LINE_BYTES`] and treats a longer
//! one as a broken connection, because past that point it can no longer find
//! where the next message starts. A snapshot is one line, so a tree whose JSON
//! runs over is not merely lost. The bridge drops the connection, reconnects,
//! is sent the same latest snapshot again, and drops that too, for as long as
//! the app holds the tree, with nothing on either side saying why. One
//! unbounded `value` is enough: a draft, a log tail, a transcript.
//!
//! [`cut_to_fit`] makes the tree fit before it is published. It cuts what
//! costs the most first and stops as soon as the tree fits:
//!
//! 1. the longest values, down to one shared length, so the biggest strings
//!    shrink first and shorter ones are untouched;
//! 2. values and labels together, the same way, once values alone are not
//!    enough;
//! 3. nodes: first any node that could not fit with nothing beside it, then
//!    the trailing children of the widest nodes, then, shallowest first, as
//!    many of those as still fit, each node's next children in their order
//!    until one does not;
//! 4. last, every child of the root.
//!
//! Ids and custom action names are never changed, because they are what an
//! agent sends back: a node it can read but not address is worse than one it
//! cannot read at all.
//!
//! Everything here is pure: no I/O, nothing the layer's threads share.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io;

use taria::wire::MAX_LINE_BYTES;
use taria::{Action, Node, NodeId, Role};

/// Bytes a snapshot line spends around its root, at the largest `seq`.
///
/// Spelled out as the literal it measures, the line with the root left out:
/// the seq is the only part that varies, and it is written here at
/// `u64::MAX`, so a root within [`ROOT_BUDGET`] fits at any seq the layer
/// can reach. Budgeting for the worst seq rather than the next one costs at
/// most 19 bytes of a megabyte, and buys a bound that depends on the tree
/// alone: the same tree is cut the same way whenever it is published, so
/// deduplicating a redrawn frame still works on a tree that had to be cut.
///
/// Nothing derives this from the wire types, so
/// `the_envelope_is_measured_not_assumed` holds it against a real
/// serialization, which is what catches a change to the envelope.
const SNAPSHOT_ENVELOPE_BYTES: usize =
    r#"{"type":"snapshot","protocol_version":1,"seq":18446744073709551615,"root":}"#.len();

/// Longest serialized root that still leaves its snapshot line within
/// [`MAX_LINE_BYTES`] at any `seq`.
pub(crate) const ROOT_BUDGET: usize = MAX_LINE_BYTES - SNAPSHOT_ENVELOPE_BYTES;

/// Shortest a trimmed string is cut to, in escaped JSON bytes, marker
/// included.
///
/// A floor rather than whatever length the arithmetic asks for, because the
/// arithmetic only knows about bytes. Past a point a cut string stops
/// telling an agent what the field holds, and a tree that still does not fit
/// with every string at this length has too many nodes rather than too much
/// text, which is what the later stages are for.
const STRING_FLOOR: usize = 256;

/// Marks where a trimmed string was cut. One character, three bytes, never
/// escaped, so what it costs is known exactly.
const MARKER: &str = "\u{2026}";

/// Escaped bytes [`MARKER`] costs.
const MARKER_BYTES: usize = MARKER.len();

/// A snapshot too large for one taria line, and what was cut to make it fit.
///
/// Keeps the size the line would have had and the node holding the longest
/// string, because the fix belongs to the app and the app needs to know where
/// to look. A tree built from data usually runs away in one place: a draft
/// the user is still typing, a log nobody bounded, a list holding every row
/// instead of the rows on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SnapshotTooLarge {
    bytes: usize,
    limit: usize,
    strings_trimmed: usize,
    nodes_dropped: usize,
    largest: NodeId,
}

impl SnapshotTooLarge {
    /// The report for a root [`cut_to_fit`] cut against [`ROOT_BUDGET`].
    pub(crate) fn new(bounded: Bounded) -> Self {
        Self {
            bytes: bounded.root_bytes.saturating_add(SNAPSHOT_ENVELOPE_BYTES),
            limit: MAX_LINE_BYTES,
            strings_trimmed: bounded.strings_trimmed,
            nodes_dropped: bounded.nodes_dropped,
            largest: bounded.largest,
        }
    }

    /// Length of the snapshot line as built, before anything was cut, in
    /// bytes, not counting the terminating `\n`.
    ///
    /// Counted with `seq` at its largest, the way the bound counts it. A tree
    /// is cut when its line would not fit at every seq, not merely at the one
    /// it went out under, so that whether a tree is cut never depends on how
    /// many snapshots came before it. That keeps this always over
    /// [`limit`](Self::limit), and at most 19 bytes over the line actually
    /// built, which spelled its seq in fewer digits.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The line limit it ran over, [`taria::wire::MAX_LINE_BYTES`].
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// How many labels and values were shortened among the nodes that were
    /// published. A string in a node that was then dropped is counted in
    /// [`nodes_dropped`](Self::nodes_dropped) instead.
    pub fn strings_trimmed(&self) -> usize {
        self.strings_trimmed
    }

    /// How many nodes were left out, counting every descendant of a dropped
    /// node. Zero when shortening strings was enough.
    pub fn nodes_dropped(&self) -> usize {
        self.nodes_dropped
    }

    /// Id of the node holding the longest string before the cut, measured as
    /// JSON: its id, its label, its value or one of its custom action names.
    /// Ids and custom action names count although they are never cut,
    /// because when one of them is what ran over, that node is where the
    /// app has to look. Several nodes can tie; this is the first in tree
    /// order. It names where the bytes went, which is not always why the
    /// tree ran over: when nodes were dropped, the cause was how many there
    /// were rather than how long any one of them was.
    pub fn largest(&self) -> &NodeId {
        &self.largest
    }
}

impl fmt::Display for SnapshotTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "snapshot is {} bytes with its longest string at node `{}`, over the {} byte line \
             limit for a taria snapshot; {} string(s) trimmed and {} node(s) dropped to fit",
            self.bytes, self.largest.0, self.limit, self.strings_trimmed, self.nodes_dropped
        )
    }
}

impl Error for SnapshotTooLarge {}

/// What [`cut_to_fit`] did to a root that did not fit.
///
/// Kept apart from [`SnapshotTooLarge`] so the bound can be run against any
/// budget, which is what lets its tests work with trees of a few kilobytes
/// rather than a megabyte each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bounded {
    /// The root's exact serialized length before the cut, or `usize::MAX`
    /// for a root that could not be measured.
    pub(crate) root_bytes: usize,
    pub(crate) strings_trimmed: usize,
    pub(crate) nodes_dropped: usize,
    pub(crate) largest: NodeId,
}

/// What the bound has been asked to do on this thread, so a test can prove a
/// frame never reached it: roots measured by [`measure_root`], roots cut by
/// [`cut_to_fit`], and cuts that fell back to [`bare_root`].
///
/// Per thread because the layer bounds on the thread that publishes, which in
/// a test is the test's own, so tests running side by side cannot see each
/// other's counts.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Runs {
    pub(crate) measured: usize,
    pub(crate) cut: usize,
    pub(crate) bare: usize,
}

#[cfg(test)]
thread_local! {
    static RUNS: std::cell::Cell<Runs> = const {
        std::cell::Cell::new(Runs { measured: 0, cut: 0, bare: 0 })
    };
}

/// This thread's [`Runs`] so far.
#[cfg(test)]
pub(crate) fn runs() -> Runs {
    RUNS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn count(update: impl FnOnce(&mut Runs)) {
    RUNS.with(|runs| {
        let mut now = runs.get();
        update(&mut now);
        runs.set(now);
    });
}

/// Exact length of `root`'s JSON, or `None` if it could not be measured,
/// which serializing a [`Node`] into a counter has no way to produce.
///
/// The one measure a changed frame pays whether or not it fits, by
/// serializing it into a counter that keeps nothing, and the one an
/// unchanged frame does not pay at all: the layer compares the tree with the
/// previous frame's before it gets here. Exact rather than estimated,
/// because an estimate has to know every field a [`Node`] writes, and a
/// field a later taria adds would be one it undercounts. The tree must
/// already be within [`taria::MAX_NODE_DEPTH`], because serializing it
/// recurses and the depth cut is what bounds that.
pub(crate) fn measure_root(root: &Node) -> Option<usize> {
    #[cfg(test)]
    count(|runs| runs.measured += 1);
    measure(root)
}

/// Cut `root` until its JSON is at most `budget` bytes, and say what was cut.
///
/// `before` is what [`measure_root`] made of the root, which the caller
/// needed to know whether to cut at all, so the measure is not paid twice: a
/// length over `budget`, or `None` for a root that could not be measured,
/// which goes straight to the bare root rather than to a guess.
///
/// The stages run in the order the module documentation gives, each only when
/// the one before could not make the tree fit, and whatever they leave is
/// measured exactly again before it is trusted. The tree must already be
/// within [`taria::MAX_NODE_DEPTH`].
pub(crate) fn cut_to_fit(root: &mut Node, budget: usize, before: Option<usize>) -> Bounded {
    #[cfg(test)]
    count(|runs| runs.cut += 1);
    let mut survey = Survey::of(root);
    let Some(before) = before else {
        return bare_root(root, &survey, usize::MAX);
    };
    let excess = before.saturating_sub(budget);

    let trimmed = match water_level(&mut survey.values, excess) {
        Some(level) => Some(trim_strings(root, level, Strings::Values)),
        None => {
            // A fresh fill over both sets, not labels on top of values
            // already at the floor: with labels in play the shared level can
            // sit above the floor, and values are then cut less.
            let mut both = survey.values.clone();
            both.extend_from_slice(&survey.labels);
            water_level(&mut both, excess)
                .map(|level| trim_strings(root, level, Strings::ValuesAndLabels))
        }
    };
    let (strings_trimmed, nodes_dropped) = match trimmed {
        Some(strings) => (strings, 0),
        None => match drop_to_fit(root, budget) {
            Some(counts) => counts,
            None => return bare_root(root, &survey, before),
        },
    };
    match measure(root) {
        Some(after) if after <= budget => Bounded {
            root_bytes: before,
            strings_trimmed,
            nodes_dropped,
            largest: survey.largest,
        },
        // The stages plan on what each node and each child list costs, and
        // the tests hold those plans to the real serialization. If the two
        // ever part ways, the root still goes out in a form that fits rather
        // than in one that might not.
        _ => bare_root(root, &survey, before),
    }
}

/// Which strings a water-fill cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strings {
    Values,
    ValuesAndLabels,
}

/// Which end of a string survives a cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The start, with [`MARKER`] after it.
    Head,
    /// The end, with [`MARKER`] in front.
    Tail,
}

/// Which end of a node's value survives a cut.
///
/// A [`Role::Log`] grows at the end and a [`Role::Terminal`] has its cursor
/// near the bottom, so for both the newest content is the tail, and that is
/// what an agent reading one is after. Everything else is read from the
/// start. Labels always keep the head: a label is a name, not content that
/// grows.
fn keep_for(role: Role) -> Keep {
    if matches!(role, Role::Log | Role::Terminal) {
        Keep::Tail
    } else {
        Keep::Head
    }
}

/// What a byte costs inside a JSON string as `serde_json` writes it.
///
/// A quote, a backslash and the five control characters with a short escape
/// (`\b`, `\t`, `\n`, `\f`, `\r`) cost two bytes; every other control
/// character is written `\u00XX` and costs six. Everything else is written
/// as itself, including each byte of a multibyte character, which is why a
/// character costs its UTF-8 length.
fn byte_cost(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
        0x00..=0x1f => 6,
        _ => 1,
    }
}

/// What a character costs inside a JSON string.
fn char_cost(c: char) -> usize {
    if c.is_ascii() {
        byte_cost(c as u8)
    } else {
        c.len_utf8()
    }
}

/// Length of `s` as the contents of a JSON string, quotes excluded.
fn escaped_len(s: &str) -> usize {
    s.bytes()
        .fold(0usize, |len, byte| len.saturating_add(byte_cost(byte)))
}

/// Counts the bytes written to it and keeps none, so a tree can be measured
/// exactly without building its JSON.
struct ByteCount(usize);

impl io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Exact length of `node`'s JSON, or `None` if serializing it failed.
fn measure(node: &Node) -> Option<usize> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, node).ok()?;
    Some(count.0)
}

/// Exact length of `node`'s JSON without its children: the part of it a cap
/// on children cannot shrink.
///
/// Measured by setting the children aside rather than modelling the fields,
/// so the one model the drop stage still plans with is how a child list is
/// framed, and [`Framing`] measures that too.
fn measure_own(node: &mut Node) -> Option<usize> {
    let children = std::mem::take(&mut node.children);
    let own = measure(node);
    node.children = children;
    own
}

/// What a root that did not fit holds, read once before anything is cut.
struct Survey {
    /// Escaped lengths of the values over [`STRING_FLOOR`], the only ones a
    /// cut can touch.
    values: Vec<usize>,
    /// Escaped lengths of the labels over [`STRING_FLOOR`].
    labels: Vec<usize>,
    /// The node holding the longest string: id, label, value or custom
    /// action name.
    largest: NodeId,
    /// Every node in the tree, the root included.
    nodes: usize,
    /// The root's own label and value over [`STRING_FLOOR`], which are what
    /// the bare root ends up cutting whatever ran before it.
    root_over_floor: usize,
}

impl Survey {
    fn of(root: &Node) -> Self {
        let over_floor =
            |text: Option<&str>| text.is_some_and(|text| escaped_len(text) > STRING_FLOOR);
        let mut values = Vec::new();
        let mut labels = Vec::new();
        let mut largest = (0, &root.id);
        let mut nodes = 0usize;
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            nodes = nodes.saturating_add(1);
            // The strings no cut touches compete for `largest` too: when an
            // id or a custom action name is what ran over, it is the only
            // place worth sending the app to look.
            let fixed = node.actions.iter().filter_map(|action| match action {
                Action::Custom(name) => Some(name.as_str()),
                _ => None,
            });
            for text in std::iter::once(node.id.0.as_str()).chain(fixed) {
                let len = escaped_len(text);
                if len > largest.0 {
                    largest = (len, &node.id);
                }
            }
            for (text, lengths) in [(&node.value, &mut values), (&node.label, &mut labels)] {
                let Some(text) = text else { continue };
                let len = escaped_len(text);
                if len > largest.0 {
                    largest = (len, &node.id);
                }
                if len > STRING_FLOOR {
                    lengths.push(len);
                }
            }
            // Reversed so the first child is walked next, which keeps ties
            // for `largest` going to the first node in tree order.
            pending.extend(node.children.iter().rev());
        }
        Self {
            values,
            labels,
            largest: largest.1.clone(),
            nodes,
            root_over_floor: usize::from(over_floor(root.label.as_deref()))
                + usize::from(over_floor(root.value.as_deref())),
        }
    }
}

/// The level to cut strings to so that `excess` bytes come off, or `None`
/// when even cutting every one to [`STRING_FLOOR`] falls short.
///
/// A water-fill: every string longer than the level is cut down to it and
/// nothing shorter is touched, so the biggest strings shrink first and a
/// short one is only reached once the long ones are down to its length. The
/// level is the highest that frees enough, which makes the cut the smallest
/// that fits. `lengths` are escaped lengths over the floor, and are sorted
/// here, longest first.
fn water_level(lengths: &mut [usize], excess: usize) -> Option<usize> {
    lengths.sort_unstable_by(|a, b| b.cmp(a));
    let mut sum = 0usize;
    for (index, &len) in lengths.iter().enumerate() {
        if len <= STRING_FLOOR {
            break;
        }
        sum = sum.saturating_add(len);
        let count = index + 1;
        // How low these `count` strings can go before the next one has to be
        // cut too, or before they reach the floor.
        let below = lengths.get(count).copied().unwrap_or(0).max(STRING_FLOOR);
        if sum.saturating_sub(below.saturating_mul(count)) >= excess {
            // At or above `below`, and below the shortest of the `count`, so
            // exactly those are cut and each frees at least what the sum
            // promised.
            return Some(sum.saturating_sub(excess) / count);
        }
    }
    None
}

/// Cut every string in `which` that is longer than `level` down to it, and
/// return how many were cut.
fn trim_strings(root: &mut Node, level: usize, which: Strings) -> usize {
    let mut trimmed = 0usize;
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let keep = keep_for(node.role);
        if let Some(value) = node.value.as_mut()
            && escaped_len(value) > level
        {
            trim_to(value, level, keep);
            trimmed = trimmed.saturating_add(1);
        }
        if which == Strings::ValuesAndLabels
            && let Some(label) = node.label.as_mut()
            && escaped_len(label) > level
        {
            trim_to(label, level, Keep::Head);
            trimmed = trimmed.saturating_add(1);
        }
        pending.extend(node.children.iter_mut());
    }
    trimmed
}

/// Cut `node`'s own label and value down to [`STRING_FLOOR`], returning how
/// many it cut.
fn trim_to_floor(node: &mut Node) -> usize {
    let keep = keep_for(node.role);
    let mut trimmed = 0;
    if let Some(value) = node.value.as_mut()
        && escaped_len(value) > STRING_FLOOR
    {
        trim_to(value, STRING_FLOOR, keep);
        trimmed += 1;
    }
    if let Some(label) = node.label.as_mut()
        && escaped_len(label) > STRING_FLOOR
    {
        trim_to(label, STRING_FLOOR, Keep::Head);
        trimmed += 1;
    }
    trimmed
}

/// Cut `s` to at most `target` escaped bytes, [`MARKER`] included.
///
/// Walks characters, never bytes, so a cut can only land between two
/// characters, and counts each in escaped bytes, so a run of control
/// characters is charged what it costs on the line rather than what it costs
/// in memory. The kept part is the longest that leaves room for the marker,
/// which may be a little under `target` when the next character costs more
/// than the room left.
fn trim_to(s: &mut String, target: usize, keep: Keep) {
    let room = target.saturating_sub(MARKER_BYTES);
    let mut used = 0usize;
    match keep {
        Keep::Head => {
            let mut end = 0;
            for (at, c) in s.char_indices() {
                used = used.saturating_add(char_cost(c));
                if used > room {
                    break;
                }
                end = at + c.len_utf8();
            }
            // `end` is just past a character, so this cannot split one.
            s.truncate(end);
            s.push_str(MARKER);
        }
        Keep::Tail => {
            let mut start = s.len();
            for (at, c) in s.char_indices().rev() {
                used = used.saturating_add(char_cost(c));
                if used > room {
                    break;
                }
                start = at;
            }
            // `start` is where a character begins, so neither can this.
            s.replace_range(..start, MARKER);
        }
    }
}

/// How `serde_json` frames a node's children, measured from its own output
/// rather than written down here.
///
/// The drop stage adds a tree up from what each node costs without its
/// children, [`measure_own`], and these two numbers, so they are the whole of
/// its model of the format. Taking them from two tiny trees on every cut
/// keeps that model in step with whatever `serde_json` and [`Node`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Framing {
    /// What a non-empty child list adds besides its entries: the key, the
    /// brackets, and the comma that separates it from the field before.
    list: usize,
    /// What each child after the first adds besides itself: the comma.
    comma: usize,
}

impl Framing {
    /// Measure both from a node with no children, one child, and two.
    fn measured() -> Option<Self> {
        let leaf = Node::new("", Role::Other);
        let alone = measure(&leaf)?;
        let one = measure(&leaf.clone().child(leaf.clone()))?;
        let two = measure(&leaf.clone().children([leaf.clone(), leaf.clone()]))?;
        Some(Self {
            // The parent is the leaf again, so `one` holds the leaf twice.
            list: one.checked_sub(alone.checked_mul(2)?)?,
            comma: two.checked_sub(one.checked_add(alone)?)?,
        })
    }

    /// What a child list of `kept` entries costs besides the entries.
    fn around(self, kept: usize) -> usize {
        match kept {
            0 => 0,
            _ => self
                .list
                .saturating_add(self.comma.saturating_mul(kept - 1)),
        }
    }
}

/// One node of a tree flattened in breadth-first order, as the drop stage
/// needs it.
///
/// Breadth-first puts every node after its parent and before everything
/// deeper, and a node's children side by side in their own order. The first
/// is what lets [`size_at_cap`] add a tree up in one backwards pass and the
/// refill reach shallow nodes first; the second is what lets a node's
/// children be found, and kept or dropped, as one range.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    /// Index of the parent slot. Unused on the root, slot 0.
    parent: usize,
    /// Exact JSON length without its children; see [`measure_own`].
    own: usize,
    /// How many of its own strings were cut to the floor.
    trimmed: usize,
    /// Index of its first child's slot; the rest follow it.
    first_child: usize,
    /// How many children it has.
    children: usize,
    /// The smallest tree that holds this node: it, every ancestor, and the
    /// child lists between them, with nothing else.
    reach: usize,
    /// Whether `reach` is within the budget. One that is not can never be
    /// published, so it is dropped with everything under it before the cap
    /// is searched for.
    admissible: bool,
    /// Position among its admissible siblings, which is what a cap on
    /// children counts.
    rank: usize,
    /// How many of its children are admissible.
    admissible_children: usize,
}

/// Drop nodes until the tree fits, after cutting every string to the floor.
/// Returns how many published strings were cut and how many nodes were
/// dropped, or `None` when not even the root alone fits or a node could not
/// be measured.
///
/// Three passes over the tree flattened breadth-first:
///
/// 1. Any node that could not fit with nothing but its ancestors beside it
///    goes, with everything under it. Such a node is one huge id or a long
///    run of custom actions, which no cut shortens, and left in it would
///    hold the cap below at zero for its whole parent: its siblings would go
///    with it, a status bar, a help line, the `quit` that is an agent's way
///    out.
/// 2. One cap on how many children any node keeps, the widest that fits,
///    found by halving. Every node wider than the cap loses its tail and
///    every narrower one keeps everything, so a status bar beside a runaway
///    list, one of a handful of the root's children, stays well inside any
///    cap the list's rows leave room for. Searching adds the tree up once
///    per halving, a few dozen times at most, rather than once per node
///    dropped.
/// 3. One cap for the whole tree is coarse: where the deepest branch is the
///    last child of a wide node, any cap that keeps that branch keeps
///    everything, and the widest cap that fits can leave most of the budget
///    unused. So what the cap dropped is offered back, breadth-first so
///    shallower nodes come first and siblings in their own order, and each
///    is taken when its parent is kept and it fits in what is left. The
///    first that does not fit ends the offer for its parent: none of its
///    later siblings is taken, though a smaller one might fit, so what a
///    node keeps is always its first children, with no gap. A gap would
///    tell an agent reading a list that the row after it is the next one.
///    Other parents' children are still offered after it.
///
/// Every pass is linear in the tree and the search runs a logarithmic number
/// of them, so a list of two hundred thousand rows costs a few dozen walks
/// over a flat array rather than anything quadratic.
fn drop_to_fit(root: &mut Node, budget: usize) -> Option<(usize, usize)> {
    let framing = Framing::measured()?;
    let mut slots = flatten_to_floor(root)?;
    if !mark_admissible(&mut slots, framing, budget) {
        return None;
    }

    let widest = slots
        .iter()
        .map(|slot| slot.admissible_children)
        .max()
        .unwrap_or(0);
    let mut scratch = vec![0usize; slots.len()];
    let mut size = |cap| size_at_cap(&slots, framing, cap, &mut scratch);
    // Invariant: a cap of `fits` fits and one of `over` does not. A cap of
    // zero is the root alone, which `mark_admissible` found fits.
    let mut fits = 0;
    let mut used = size(0);
    let all = size(widest);
    if all <= budget {
        fits = widest;
        used = all;
    } else {
        let mut over = widest;
        while over - fits > 1 {
            let mid = fits + (over - fits) / 2;
            let bytes = size(mid);
            if bytes <= budget {
                fits = mid;
                used = bytes;
            } else {
                over = mid;
            }
        }
    }

    let kept = refill(&slots, framing, fits, budget, used);
    keep_only(root, &slots, &kept);
    let (mut strings, mut published) = (0usize, 0usize);
    for (slot, _) in slots.iter().zip(&kept).filter(|(_, kept)| **kept) {
        strings = strings.saturating_add(slot.trimmed);
        published += 1;
    }
    Some((strings, slots.len().saturating_sub(published)))
}

/// Cut every string in the tree to the floor and flatten it breadth-first
/// into [`Slot`]s, or `None` if a node could not be measured.
fn flatten_to_floor(root: &mut Node) -> Option<Vec<Slot>> {
    let mut slots = Vec::new();
    let mut queue: VecDeque<(&mut Node, usize)> = VecDeque::from([(root, 0)]);
    while let Some((node, parent)) = queue.pop_front() {
        let index = slots.len();
        let trimmed = trim_to_floor(node);
        let own = measure_own(node)?;
        slots.push(Slot {
            parent,
            own,
            trimmed,
            // Everything still queued takes the indices after this one, and
            // the children are queued behind it, so they come next, side by
            // side.
            first_child: index.saturating_add(queue.len()).saturating_add(1),
            children: node.children.len(),
            ..Slot::default()
        });
        queue.extend(node.children.iter_mut().map(|child| (child, index)));
    }
    Some(slots)
}

/// Work out which nodes could fit at all, and rank each admissible node
/// among its admissible siblings. Returns false when the root itself does
/// not fit, which nothing short of the bare root can help.
///
/// Parent before child, which breadth-first order guarantees, so a node's
/// reach is its parent's plus a child list and itself.
fn mark_admissible(slots: &mut [Slot], framing: Framing, budget: usize) -> bool {
    let Some(root) = slots.first_mut() else {
        return false;
    };
    root.reach = root.own;
    root.admissible = root.reach <= budget;
    if !root.admissible {
        return false;
    }
    for index in 0..slots.len() {
        let Some(&parent) = slots.get(index) else {
            break;
        };
        let range = parent.first_child..parent.first_child.saturating_add(parent.children);
        let mut admissible = 0;
        if let Some(children) = slots.get_mut(range) {
            for child in children {
                child.reach = parent
                    .reach
                    .saturating_add(framing.list)
                    .saturating_add(child.own);
                child.admissible = parent.admissible && child.reach <= budget;
                if child.admissible {
                    child.rank = admissible;
                    admissible += 1;
                }
            }
        }
        if let Some(slot) = slots.get_mut(index) {
            slot.admissible_children = admissible;
        }
    }
    true
}

/// Exact JSON length of the flattened tree if no node kept more than `cap`
/// of its admissible children. `kept_bytes` is scratch space, one entry per
/// slot.
///
/// Backwards over breadth-first order, so every node is reached after all of
/// its descendants: by then the children it keeps have added themselves to
/// its entry in `kept_bytes`, and it adds itself to its parent's if its
/// parent keeps it. A node its parent drops adds nothing, and nor, through
/// it, does anything under it.
fn size_at_cap(slots: &[Slot], framing: Framing, cap: usize, kept_bytes: &mut [usize]) -> usize {
    kept_bytes.fill(0);
    for (index, slot) in slots.iter().enumerate().rev() {
        let bytes = slot
            .own
            .saturating_add(framing.around(slot.admissible_children.min(cap)))
            .saturating_add(kept_bytes.get(index).copied().unwrap_or(0));
        if index == 0 {
            return bytes;
        }
        if slot.admissible
            && slot.rank < cap
            && let Some(parent) = kept_bytes.get_mut(slot.parent)
        {
            *parent = parent.saturating_add(bytes);
        }
    }
    0
}

/// Which slots are kept: those `cap` keeps, which come to `used` bytes, and
/// then whichever the cap dropped still fit, offered breadth-first.
///
/// A node offered back costs itself and a comma, or a whole child list when
/// it is the first child its parent keeps; the children it comes back
/// without are offered in their turn, deeper down the order, and only once
/// it is kept. Once one child of a parent does not fit, that parent is
/// closed and its later children are not offered, so every kept node keeps
/// an unbroken run of its admissible children from the first. A child that
/// is not admissible is passed over without closing its parent: it was never
/// going to fit, and the rows after it are as much the next ones as it was.
fn refill(
    slots: &[Slot],
    framing: Framing,
    cap: usize,
    budget: usize,
    mut used: usize,
) -> Vec<bool> {
    let mut kept = vec![false; slots.len()];
    // How many children each node keeps so far, which decides what the next
    // one costs.
    let mut kept_children = vec![0usize; slots.len()];
    // Parents a child of which did not fit, whose later children are not
    // offered.
    let mut closed = vec![false; slots.len()];
    if let Some(root) = kept.first_mut() {
        *root = true;
    }
    let is_kept = |kept: &[bool], index: usize| kept.get(index).copied().unwrap_or(false);

    // What the cap keeps, which is what `used` measured: a chain of
    // admissible nodes within the cap all the way up from the root.
    for (index, slot) in slots.iter().enumerate().skip(1) {
        if slot.admissible && slot.rank < cap && is_kept(&kept, slot.parent) {
            if let Some(entry) = kept.get_mut(index) {
                *entry = true;
            }
            if let Some(count) = kept_children.get_mut(slot.parent) {
                *count += 1;
            }
        }
    }

    // What the cap dropped, offered back. Breadth-first, so every parent has
    // been decided by the time its children are reached.
    for (index, slot) in slots.iter().enumerate().skip(1) {
        if !slot.admissible
            || is_kept(&kept, index)
            || !is_kept(&kept, slot.parent)
            || closed.get(slot.parent).copied().unwrap_or(true)
        {
            continue;
        }
        let Some(siblings) = kept_children.get_mut(slot.parent) else {
            continue;
        };
        let frame = if *siblings == 0 {
            framing.list
        } else {
            framing.comma
        };
        let cost = slot.own.saturating_add(frame);
        if used.saturating_add(cost) <= budget {
            used = used.saturating_add(cost);
            *siblings += 1;
            if let Some(entry) = kept.get_mut(index) {
                *entry = true;
            }
        } else if let Some(parent) = closed.get_mut(slot.parent) {
            *parent = true;
        }
    }
    kept
}

/// Remove from the tree every node `kept` does not keep, leaving the rest in
/// their order.
fn keep_only(root: &mut Node, slots: &[Slot], kept: &[bool]) {
    let mut pending = vec![(root, 0usize)];
    while let Some((node, index)) = pending.pop() {
        let Some(slot) = slots.get(index) else {
            continue;
        };
        let first = slot.first_child;
        let mut at = first;
        // `retain` visits every child once, in order, which is what keeps
        // `at` on the slot of the child it is looking at.
        node.children.retain(|_| {
            let keep = kept.get(at).copied().unwrap_or(false);
            at += 1;
            keep
        });
        let kept_slots = (first..first.saturating_add(slot.children))
            .filter(|&child| kept.get(child).copied().unwrap_or(false));
        pending.extend(node.children.iter_mut().zip(kept_slots));
    }
}

/// The last resort: the root with no children, its own label and value cut
/// to the floor.
///
/// Always fits for the root the layer builds, whose id is `app` and which
/// carries no actions: what is left is a few hundred bytes. Its actions and
/// id are kept as they are, like everywhere else.
fn bare_root(root: &mut Node, survey: &Survey, before: usize) -> Bounded {
    #[cfg(test)]
    count(|runs| runs.bare += 1);
    root.children.clear();
    trim_to_floor(root);
    Bounded {
        root_bytes: before,
        strings_trimmed: survey.root_over_floor,
        nodes_dropped: survey.nodes.saturating_sub(1),
        largest: survey.largest.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use taria::Snapshot;
    use taria::wire::AppToBridge;

    use super::*;

    /// Every role this build knows, for the tests that have to cover the
    /// vocabulary. `Role` is `#[non_exhaustive]` in another crate, so this
    /// cannot be an exhaustive `match`; core's own role table is what keeps
    /// the vocabulary itself honest.
    const ROLES: [Role; 29] = [
        Role::App,
        Role::Pane,
        Role::List,
        Role::ListItem,
        Role::Tree,
        Role::TreeItem,
        Role::Table,
        Role::Row,
        Role::Cell,
        Role::TextInput,
        Role::Button,
        Role::Checkbox,
        Role::Select,
        Role::Option,
        Role::Tabs,
        Role::Tab,
        Role::Link,
        Role::Text,
        Role::Log,
        Role::Terminal,
        Role::Image,
        Role::Chart,
        Role::ProgressBar,
        Role::Status,
        Role::Scrollbar,
        Role::Dialog,
        Role::Menu,
        Role::MenuItem,
        Role::Other,
    ];

    fn built_in_actions() -> [Action; 7] {
        [
            Action::Activate,
            Action::Focus,
            Action::Select,
            Action::Toggle,
            Action::Scroll,
            Action::SetValue,
            Action::Dismiss,
        ]
    }

    fn json_len(node: &Node) -> usize {
        serde_json::to_vec(node).unwrap().len()
    }

    /// The layer's two steps in one: measure, and cut only a root that does
    /// not fit.
    fn bound_root(root: &mut Node, budget: usize) -> Option<Bounded> {
        let before = measure_root(root);
        if before.is_some_and(|bytes| bytes <= budget) {
            return None;
        }
        Some(cut_to_fit(root, budget, before))
    }

    /// Keep at most `cap` children on every node, which is what the cap
    /// search models when nothing is too large to fit alone.
    fn cap_children(root: &mut Node, cap: usize) {
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            node.children.truncate(cap);
            pending.extend(node.children.iter_mut());
        }
    }

    fn count_nodes(root: &Node) -> usize {
        let mut count = 0;
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            count += 1;
            pending.extend(&node.children);
        }
        count
    }

    /// Characters that cost more on the line than in memory, in every
    /// escape class `serde_json` has, beside multibyte ones.
    const ESCAPE_HEAVY: &str = "\x1b[31m\"quoted\"\\path\n\t\r\x08\x0c\x00\x7f é€😀";

    /// A tree with every field set somewhere, every role, every action,
    /// custom actions with awkward names, and strings full of escapes.
    fn varied_tree() -> Node {
        let mut root = Node::new("app", Role::App)
            .label("varied")
            .actions(built_in_actions())
            .action(Action::Custom(format!("custom {ESCAPE_HEAVY}")));
        for (index, role) in ROLES.into_iter().enumerate() {
            let mut node = Node::new(format!("node-{index}-{ESCAPE_HEAVY}"), role)
                .focused(index == 3)
                .label(ESCAPE_HEAVY.repeat(index % 4))
                .value(format!("{index}:{}", ESCAPE_HEAVY.repeat(index % 3)));
            if index % 2 == 0 {
                node = node
                    .actions(built_in_actions())
                    .actions((0..index).map(|n| Action::Custom(format!("act-{n}\u{1}"))));
            }
            if index % 5 == 0 {
                node = node.children(
                    (0..index).map(|n| Node::new(format!("leaf-{index}-{n}"), Role::Cell)),
                );
            }
            root = root.child(node);
        }
        root
    }

    /// A root shaped like the layer's, with one child holding `value`.
    fn one_value(role: Role, value: &str) -> Node {
        Node::new("app", Role::App)
            .label("bound")
            .child(Node::new("field", role).value(value))
    }

    fn value_of(root: &Node, index: usize) -> &str {
        root.children[index].value.as_deref().unwrap()
    }

    /// One row of a long list, all of them the same size.
    fn row(n: usize) -> Node {
        Node::new(format!("row-{n:06}"), Role::ListItem)
            .label(format!("Task {n:06}"))
            .action(Action::Select)
    }

    #[test]
    fn the_envelope_is_measured_not_assumed() {
        let root = Node::new("app", Role::App).label("envelope");
        let root_len = json_len(&root);
        let line_len = |seq| {
            serde_json::to_vec(&AppToBridge::Snapshot(Snapshot::new(seq, root.clone())))
                .unwrap()
                .len()
        };

        assert_eq!(line_len(u64::MAX) - root_len, SNAPSHOT_ENVELOPE_BYTES);
        // And it is the worst case: no seq spends more, and the smallest
        // spends exactly the 19 digits less that the budget gives away.
        for seq in [0, 1, 9, 10, 99, 100, 12_345, u64::MAX / 10] {
            assert!(
                line_len(seq) - root_len <= SNAPSHOT_ENVELOPE_BYTES,
                "seq {seq}"
            );
        }
        assert_eq!(line_len(1) - root_len, SNAPSHOT_ENVELOPE_BYTES - 19);
        assert_eq!(ROOT_BUDGET + SNAPSHOT_ENVELOPE_BYTES, MAX_LINE_BYTES);
    }

    /// The drop stage plans with each node's own size measured and the child
    /// lists between them modelled, so the model has to be exact at full
    /// width, where nothing is dropped and the real serialization is there
    /// to compare against, and at every cap, against the tree really capped.
    #[test]
    fn the_size_model_matches_serde_at_full_width() {
        let framing = Framing::measured().unwrap();
        assert_eq!(
            framing,
            Framing {
                list: r#","children":[]"#.len(),
                comma: 1
            },
            "what serde_json writes today"
        );

        let mut tree = varied_tree();
        // Nothing in the varied tree is over the floor, so flattening trims
        // nothing and the tree it measures is the one serialized.
        let before = tree.clone();
        let mut slots = flatten_to_floor(&mut tree).unwrap();
        assert_eq!(tree, before);
        assert_eq!(slots.len(), count_nodes(&tree));
        assert!(mark_admissible(&mut slots, framing, usize::MAX));

        let mut scratch = vec![0; slots.len()];
        assert_eq!(
            size_at_cap(&slots, framing, usize::MAX, &mut scratch),
            json_len(&tree)
        );
        for cap in [0, 1, 2, 5] {
            let mut capped = tree.clone();
            cap_children(&mut capped, cap);
            assert_eq!(
                size_at_cap(&slots, framing, cap, &mut scratch),
                json_len(&capped),
                "cap {cap}"
            );
        }
        // And escaped lengths, which the string stages plan with, are what
        // serde_json writes between the quotes.
        for text in [ESCAPE_HEAVY, "", "plain", MARKER, "\x1f\x20"] {
            assert_eq!(
                escaped_len(text) + 2,
                serde_json::to_vec(text).unwrap().len(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_tree_at_the_budget_is_untouched_and_one_byte_over_is_trimmed() {
        let base = json_len(&one_value(Role::TextInput, ""));
        let at = "x".repeat(ROOT_BUDGET - base);

        let mut root = one_value(Role::TextInput, &at);
        assert_eq!(json_len(&root), ROOT_BUDGET);
        let untouched = root.clone();
        assert_eq!(bound_root(&mut root, ROOT_BUDGET), None);
        assert_eq!(root, untouched);

        let mut root = one_value(Role::TextInput, &format!("{at}x"));
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("one byte over is cut");
        assert_eq!(bounded.root_bytes, ROOT_BUDGET + 1);
        let report = SnapshotTooLarge::new(bounded.clone());
        assert_eq!(report.bytes(), MAX_LINE_BYTES + 1, "one byte over the line");
        assert_eq!(report.limit(), MAX_LINE_BYTES);
        assert_eq!(bounded.strings_trimmed, 1);
        assert_eq!(bounded.nodes_dropped, 0);
        assert_eq!(bounded.largest, NodeId("field".into()));
        assert!(value_of(&root, 0).ends_with(MARKER), "the cut is marked");
        // One byte over costs four characters of the value, not the value:
        // the three-byte marker takes their place, and the root lands back
        // on the budget exactly.
        assert_eq!(json_len(&root), ROOT_BUDGET);
        assert_eq!(value_of(&root, 0), format!("{}{MARKER}", &at[3..]));
    }

    #[test]
    fn the_longest_values_are_cut_first_and_short_ones_are_untouched() {
        let lengths = [5000, 3000, 400, 100];
        let root =
            Node::new("app", Role::App)
                .label("fill")
                .children(lengths.iter().enumerate().map(|(index, &len)| {
                    Node::new(format!("v{index}"), Role::Text).value("y".repeat(len))
                }));
        // Three thousand bytes over: more than the longest alone can give
        // before it reaches the second, so both come down to one level.
        let budget = json_len(&root) - 3000;
        let mut bounded_root = root.clone();
        let bounded = bound_root(&mut bounded_root, budget).expect("over budget");

        assert_eq!(bounded.strings_trimmed, 2);
        assert_eq!(bounded.largest, NodeId("v0".into()));
        assert!(json_len(&bounded_root) <= budget);
        let (first, second) = (value_of(&bounded_root, 0), value_of(&bounded_root, 1));
        assert!(first.ends_with(MARKER) && second.ends_with(MARKER));
        // (5000 + 3000 - 3000) / 2: one shared level for both.
        assert!(escaped_len(first) <= 2500 && escaped_len(first) >= 2496);
        assert_eq!(escaped_len(first), escaped_len(second));
        assert_eq!(value_of(&bounded_root, 2), value_of(&root, 2));
        assert_eq!(value_of(&bounded_root, 3), value_of(&root, 3));
    }

    #[test]
    fn log_and_terminal_keep_the_tail_and_other_roles_keep_the_head() {
        let text: String = (0..2000)
            .map(|n| char::from(b'a' + (n % 26) as u8))
            .collect();
        for role in ROLES {
            let mut root = one_value(role, &text);
            let budget = json_len(&root) - 1000;
            bound_root(&mut root, budget).expect("over budget");
            let value = value_of(&root, 0);
            let kept = value.trim_start_matches(MARKER).trim_end_matches(MARKER);
            if matches!(role, Role::Log | Role::Terminal) {
                assert!(value.starts_with(MARKER), "{role:?}: {value}");
                assert!(text.ends_with(kept), "{role:?} keeps the tail");
            } else {
                assert!(value.ends_with(MARKER), "{role:?}: {value}");
                assert!(text.starts_with(kept), "{role:?} keeps the head");
            }
            assert!(json_len(&root) <= budget, "{role:?}");
        }
    }

    #[test]
    fn escapes_cost_what_they_cost_on_the_line_and_cuts_land_between_chars() {
        // Each control character is six bytes on the line and one in memory:
        // cut by memory, this would run nearly six times over.
        for (piece, keep) in [
            ("\x1b", Keep::Head),
            ("\n", Keep::Head),
            ("\"", Keep::Tail),
            ("a\x1b[0m\"\n", Keep::Head),
            ("é€😀", Keep::Head),
            ("é€😀", Keep::Tail),
        ] {
            let original = piece.repeat(500);
            let mut text = original.clone();
            trim_to(&mut text, STRING_FLOOR, keep);
            let quoted = serde_json::to_vec(&text).unwrap().len();
            assert!(quoted - 2 <= STRING_FLOOR, "{piece:?}: {quoted}");
            assert_eq!(quoted - 2, escaped_len(&text), "{piece:?}");
            // Within one character's cost of the target: the cut stops at the
            // last character that fits, not some way before it.
            assert!(escaped_len(&text) > STRING_FLOOR - 6, "{piece:?}");
            match keep {
                Keep::Head => assert!(original.starts_with(text.strip_suffix(MARKER).unwrap())),
                Keep::Tail => assert!(original.ends_with(text.strip_prefix(MARKER).unwrap())),
            }
        }

        // A four-byte character straddling the cut is left out whole. The
        // room is the floor less the marker, 253: one `a` and 63 emoji are
        // 253, and an `aa` prefix pushes the 63rd emoji across the line.
        let mut text = format!("aa{}", "😀".repeat(100));
        trim_to(&mut text, STRING_FLOOR, Keep::Head);
        assert_eq!(text, format!("aa{}{MARKER}", "😀".repeat(62)));

        // Through the whole bound, with every escape class in one value.
        let mut root = one_value(Role::Terminal, &ESCAPE_HEAVY.repeat(200));
        let budget = json_len(&root) - 2000;
        bound_root(&mut root, budget).expect("over budget");
        assert!(json_len(&root) <= budget);
        assert!(value_of(&root, 0).starts_with(MARKER));
    }

    #[test]
    fn labels_are_trimmed_only_when_values_are_not_enough() {
        let root = Node::new("app", Role::App).label("labels").child(
            Node::new("both", Role::Text)
                .label("l".repeat(3000))
                .value("v".repeat(3000)),
        );
        let label_of = |root: &Node| root.children[0].label.clone().unwrap();

        // Well within what the value can give on its own.
        let mut values_only = root.clone();
        let budget = json_len(&root) - 1000;
        let bounded = bound_root(&mut values_only, budget).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 1);
        assert_eq!(
            label_of(&values_only),
            label_of(&root),
            "the label is untouched"
        );
        assert!(value_of(&values_only, 0).ends_with(MARKER));

        // More than the value can give before it reaches the floor, so both
        // are filled together, and to one level rather than the value first.
        let mut both = root.clone();
        let budget = json_len(&root) - 4000;
        let bounded = bound_root(&mut both, budget).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 2);
        assert!(json_len(&both) <= budget);
        let (label, value) = (label_of(&both), value_of(&both, 0).to_string());
        assert!(label.ends_with(MARKER) && value.ends_with(MARKER));
        assert_eq!(escaped_len(&label), escaped_len(&value));
        assert!(
            escaped_len(&value) > STRING_FLOOR,
            "the shared level sits above the floor"
        );
    }

    #[test]
    fn a_huge_list_drops_trailing_rows_and_keeps_its_sibling_status_bar() {
        let mut root = Node::new("app", Role::App).label("huge").children([
            Node::new("list", Role::List)
                .label("Tasks")
                .children((0..50_000).map(row)),
            Node::new("status", Role::Status).value("50000 tasks"),
        ]);
        assert!(json_len(&root) > ROOT_BUDGET);

        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");
        assert!(json_len(&root) <= ROOT_BUDGET);
        assert_eq!(bounded.strings_trimmed, 0, "no string was over the floor");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["list", "status"], "the status bar survives the list");
        let list = &root.children[0];
        let kept = list.children.len();
        assert!(kept > 1000 && kept < 50_000, "kept {kept} rows");
        assert_eq!(bounded.nodes_dropped, 50_000 - kept);
        // The rows kept are the first ones, in order: every row costs the
        // same, so none past the cap fits in what the cap left.
        for (n, kept_row) in list.children.iter().enumerate() {
            assert_eq!(kept_row.id, row(n).id);
        }
        // And the cap is the widest that fits: one more row would not.
        let mut one_more = root.clone();
        one_more.children[0].children.push(row(kept));
        assert!(json_len(&one_more) > ROOT_BUDGET);
    }

    /// The cap is one number for every node, but a node of three children is
    /// never wide enough to reach it, so beside a runaway list only the list
    /// loses anything, and what it keeps fills the line.
    #[test]
    fn a_huge_list_beside_tabs_is_capped_alone_and_fills_the_budget() {
        let tabs = ["Inbox", "Today", "Done"].map(|name| {
            Node::new(format!("tab-{name}"), Role::Tab)
                .label(name)
                .action(Action::Select)
        });
        let mut root = Node::new("app", Role::App).label("tabs").children([
            Node::new("list", Role::List)
                .label("Tasks")
                .children((0..50_000).map(row)),
            Node::new("tabs", Role::Tabs).children(tabs.clone()),
        ]);
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        let len = json_len(&root);
        assert!(len <= ROOT_BUDGET);
        assert!(
            ROOT_BUDGET - len < 4096,
            "{} bytes of the budget left unused",
            ROOT_BUDGET - len
        );
        assert_eq!(root.children[1].children, tabs, "every tab is kept");
        let kept = root.children[0].children.len();
        assert!(kept < 50_000);
        assert_eq!(bounded.nodes_dropped, 50_000 - kept, "only rows went");
    }

    /// The refill never leaves a gap. Here the cap keeps ten long rows, the
    /// eleventh does not fit in what is left, and every row after it is
    /// short enough that it would: taking one would show an agent row 10
    /// followed by row 50, as though nothing lay between them.
    #[test]
    fn the_refill_keeps_each_list_a_run_of_its_first_rows() {
        let long = "r".repeat(200);
        let all: Vec<Node> = (0..100)
            .map(|n| {
                let id = if n < 50 {
                    format!("row-{n:03}-{long}")
                } else {
                    format!("row-{n:03}")
                };
                Node::new(id, Role::ListItem)
            })
            .collect();
        let tree = |rows: &[Node]| {
            Node::new("app", Role::App).label("mixed").children([
                Node::new("list", Role::List).children(rows.to_vec()),
                Node::new("status", Role::Status).value("100 rows"),
            ])
        };
        let mut root = tree(&all);
        // Ten long rows and room to spare, short of an eleventh but well
        // over what one short row costs.
        let budget = json_len(&tree(&all[..10])) + 100;
        assert!(json_len(&all[10]) > 101 && json_len(&all[50]) < 99);

        let bounded = bound_root(&mut root, budget).expect("over budget");
        assert!(json_len(&root) <= budget);
        assert_eq!(
            root.children[0].children,
            all[..10],
            "the first ten, no gap"
        );
        assert_eq!(root.children[1].id.0, "status");
        assert_eq!(bounded.nodes_dropped, 90);
    }

    /// A node no cut can make small enough, one huge id or a long run of
    /// custom actions, goes on its own. Kept in, it would hold the cap at
    /// zero for its parent, and its siblings would go with it: here the
    /// status bar, the help line, and the `quit` that is the advertised way
    /// out of the app.
    #[test]
    fn a_child_too_large_to_fit_alone_is_dropped_without_its_siblings() {
        let chrome = [
            Node::new("status", Role::Status).value("3 tasks"),
            Node::new("help", Role::Text).label("q quit, ? help"),
            Node::new("quit", Role::Button)
                .label("Quit")
                .action(Action::Activate),
        ];
        let huge_actions = Node::new("actions", Role::Button)
            .actions((0..500).map(|n| Action::Custom(format!("action-{n}"))));
        for huge in [Node::new("i".repeat(5000), Role::Text), huge_actions] {
            let name = huge.id.0.chars().take(8).collect::<String>();
            let huge = huge.children((0..3).map(|n| Node::new(format!("under-{n}"), Role::Text)));
            let mut root = Node::new("app", Role::App)
                .label("chrome")
                .child(huge)
                .children(chrome.clone());
            let bounded = bound_root(&mut root, 2000).expect("over budget");

            assert_eq!(root.children, chrome, "{name}: the siblings survive whole");
            assert_eq!(
                bounded.nodes_dropped, 4,
                "{name}: the node and everything under it"
            );
            assert!(json_len(&root) <= 2000, "{name}");
        }
    }

    /// Where the deepest branch is the last child of a wide node, any one
    /// cap that keeps the branch keeps everything, and the widest cap that
    /// fits keeps one level of it. What the cap dropped is offered back,
    /// shallowest first, until the line is full.
    #[test]
    fn a_deep_branch_behind_wide_siblings_is_refilled_to_the_budget() {
        let leaves = |level: usize| {
            (0..40).map(move |n| {
                Node::new(format!("leaf-{level}-{n}"), Role::Text).label("x".repeat(200))
            })
        };
        let mut chain = Node::new("n20", Role::TreeItem).children(leaves(20));
        for level in (1..20).rev() {
            chain = Node::new(format!("n{level}"), Role::TreeItem)
                .children(leaves(level))
                .child(chain);
        }
        let mut root = Node::new("app", Role::App).label("deep").child(chain);
        let budget = json_len(&root) / 3;

        let bounded = bound_root(&mut root, budget).expect("over budget");
        let len = json_len(&root);
        assert!(len <= budget);
        assert!(
            len * 10 >= budget * 9,
            "{len} of {budget} bytes used, {} nodes dropped",
            bounded.nodes_dropped
        );
        // Shallowest first: the branch is followed down as far as it fits,
        // so the first level is kept whole, branch and all.
        assert_eq!(root.children[0].children.len(), 41);
    }

    #[test]
    fn largest_names_a_long_id_or_custom_action_name() {
        let label = || Node::new("label", Role::Text).label("l".repeat(300));

        let long_id = "x".repeat(3000);
        let mut root = Node::new("app", Role::App)
            .label("largest")
            .children([label(), Node::new(long_id.clone(), Role::Text)]);
        let bounded = bound_root(&mut root, 2000).expect("over budget");
        assert_eq!(bounded.largest.0, long_id, "the id is what ran over");

        let mut root = Node::new("app", Role::App).label("largest").children([
            label(),
            Node::new("act", Role::Button).action(Action::Custom("a".repeat(3000))),
        ]);
        let bounded = bound_root(&mut root, 2000).expect("over budget");
        assert_eq!(bounded.largest.0, "act", "the action name is what ran over");
        let message = SnapshotTooLarge::new(bounded).to_string();
        assert!(
            message.contains("longest string at node `act`"),
            "{message}"
        );
    }

    #[test]
    fn the_bare_root_is_the_last_resort() {
        // A root that cannot fit even alone, with its label at the floor:
        // nothing under it can be kept, so it goes out bare, which is the
        // most any cut can do for it.
        let mut root = Node::new("app", Role::App)
            .label("r".repeat(10_000))
            .children([
                Node::new("first", Role::Text),
                Node::new("second", Role::Text),
            ]);
        let bounded = bound_root(&mut root, 200).expect("over budget");
        assert!(root.children.is_empty());
        assert_eq!(bounded.nodes_dropped, 2);
        assert_eq!(bounded.strings_trimmed, 1, "the root's label");
        assert_eq!(root.id.0, "app");

        // So does a root that could not be measured.
        let mut root = Node::new("app", Role::App).child(Node::new("child", Role::Text));
        let bounded = cut_to_fit(&mut root, ROOT_BUDGET, None);
        assert!(root.children.is_empty());
        assert_eq!(bounded.root_bytes, usize::MAX);

        // The fallback itself, from the worst root the layer can build: an
        // absurd app label and a tree far over the line.
        let mut root = Node::new("app", Role::App)
            .label(format!("{}\n", "\x1b".repeat(100_000)))
            .focused(true)
            .children(
                (0..1000).map(|n| Node::new(format!("{n}"), Role::Text).value("v".repeat(5000))),
            );
        let survey = Survey::of(&root);
        let before = json_len(&root);
        let bounded = bare_root(&mut root, &survey, before);
        assert_eq!(bounded.root_bytes, before);
        assert!(root.children.is_empty());
        assert_eq!(bounded.nodes_dropped, 1000);
        assert_eq!(bounded.strings_trimmed, 1);
        let len = json_len(&root);
        assert!(len <= 512, "a bare root is a few hundred bytes, not {len}");
        assert!(len <= ROOT_BUDGET);
    }

    #[test]
    fn ids_and_custom_action_names_are_never_changed() {
        let long = "k".repeat(2000);
        let mut root = Node::new("app", Role::App)
            .label("ids")
            .children((0..300).map(|n| {
                Node::new(format!("{long}-{n}"), Role::Button)
                    .label(long.clone())
                    .value(long.clone())
                    .action(Action::Custom(format!("{long}-act-{n}")))
            }));
        let original = root.clone();
        let bounded = bound_root(&mut root, 200_000).expect("over budget");
        assert!(bounded.nodes_dropped > 0, "the drop stage ran");
        assert!(json_len(&root) <= 200_000);

        assert_eq!(root.id, original.id);
        assert!(!root.children.is_empty());
        for (kept, was) in root.children.iter().zip(&original.children) {
            assert_eq!(kept.id, was.id);
            assert_eq!(kept.actions, was.actions);
            assert_eq!(kept.role, was.role);
            assert_ne!(kept.value, was.value, "the strings were what gave way");
        }
    }

    #[test]
    fn the_water_level_is_the_highest_that_frees_enough() {
        // Exactly enough at the floor.
        let mut lengths = vec![STRING_FLOOR + 10, STRING_FLOOR + 5];
        assert_eq!(water_level(&mut lengths, 15), Some(STRING_FLOOR));
        // One byte more than the floor can give.
        let mut lengths = vec![STRING_FLOOR + 10, STRING_FLOOR + 5];
        assert_eq!(water_level(&mut lengths, 16), None);
        // Only the longest is needed, and it stops at the second's length.
        let mut lengths = vec![1000, 600, 300];
        assert_eq!(water_level(&mut lengths, 400), Some(600));
        assert_eq!(water_level(&mut lengths, 401), Some(599));
        assert_eq!(water_level(&mut Vec::new(), 1), None);
    }

    /// Linear-ish: every pass is linear and the cap search runs about
    /// eighteen of them. Quadratic work on two hundred thousand rows would
    /// be tens of billions of steps, which no machine finishes inside this
    /// bound.
    #[test]
    fn a_list_of_two_hundred_thousand_rows_is_bounded_in_linear_time() {
        let mut times = Vec::new();
        for rows in [50_000, 200_000] {
            let mut root = Node::new("app", Role::App).label("rows").children([
                Node::new("list", Role::List).children((0..rows).map(row)),
                Node::new("status", Role::Status).value(format!("{rows} rows")),
            ]);
            let start = Instant::now();
            let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");
            let elapsed = start.elapsed();
            eprintln!("{rows} rows bounded in {elapsed:?}");
            assert!(json_len(&root) <= ROOT_BUDGET);
            assert_eq!(root.children.len(), 2, "the status bar is kept");
            assert_eq!(
                bounded.nodes_dropped,
                rows - root.children[0].children.len()
            );
            times.push(elapsed);
        }
        assert!(
            times[1] < Duration::from_secs(30),
            "200000 rows took {:?}",
            times[1]
        );
    }

    /// A small xorshift generator, so the fuzz test needs no dependency and
    /// every failure names a seed that reproduces it.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn one_in(&mut self, n: usize) -> bool {
            self.below(n) == 0
        }

        /// A string heavy in escapes and multibyte characters, mostly short
        /// and now and then far over the floor. Built from runs of one piece
        /// each, so a long one costs a few copies rather than a draw per
        /// character.
        fn text(&mut self) -> String {
            const PIECES: [&str; 15] = [
                "a", "Z", "0", " ", "\"", "\\", "\n", "\t", "\x1b", "\x00", "\x7f", "é", "€", "😀",
                MARKER,
            ];
            let max = [2, 12, 60, 400][self.below(4)];
            let mut text = String::new();
            for _ in 0..self.below(8) {
                let piece = PIECES[self.below(PIECES.len())];
                text.push_str(&piece.repeat(self.below(max + 1)));
            }
            text
        }
    }

    /// A random subtree, its ids unique by the counter that leads them.
    fn random_node(rng: &mut Rng, depth: usize, ids: &mut usize, room: &mut usize) -> Node {
        *ids += 1;
        let long = if rng.one_in(10) {
            rng.text()
        } else {
            String::new()
        };
        let mut node = Node::new(format!("n{ids}:{long}"), ROLES[rng.below(ROLES.len())])
            .focused(rng.one_in(20));
        if rng.one_in(2) {
            node = node.label(rng.text());
        }
        if rng.one_in(2) {
            node = node.value(rng.text());
        }
        if rng.one_in(3) {
            let actions: Vec<Action> = built_in_actions()
                .into_iter()
                .filter(|_| rng.one_in(2))
                .collect();
            node = node.actions(actions);
        }
        if rng.one_in(4) {
            for _ in 0..1 + rng.below(3) {
                node = node.action(Action::Custom(format!("c:{}", rng.text())));
            }
        }
        if depth > 0 {
            let fanout = match rng.below(4) {
                0 => 0,
                1 => rng.below(4),
                2 => rng.below(20),
                _ => rng.below(120),
            };
            for _ in 0..fanout {
                if *room == 0 {
                    break;
                }
                *room -= 1;
                let child = random_node(rng, depth - 1, ids, room);
                node = node.child(child);
            }
        }
        node
    }

    /// Ids of the nodes the drop stage finds admissible in `original` at
    /// `budget`: those that could fit with nothing beside them but their
    /// ancestors.
    fn admissible_ids(original: &Node, budget: usize) -> HashSet<String> {
        let mut tree = original.clone();
        let mut slots = flatten_to_floor(&mut tree).unwrap();
        mark_admissible(&mut slots, Framing::measured().unwrap(), budget);
        // The same breadth-first order the slots were flattened in.
        let mut ids = HashSet::new();
        let mut slots = slots.iter();
        let mut queue = VecDeque::from([&tree]);
        while let Some(node) = queue.pop_front() {
            if slots.next().is_some_and(|slot| slot.admissible) {
                ids.insert(node.id.0.clone());
            }
            queue.extend(&node.children);
        }
        ids
    }

    /// Whether `kept` is `original` with some entries left out and the rest
    /// in their order.
    fn is_subsequence(kept: &[&str], original: &[&str]) -> bool {
        let mut rest = original.iter();
        kept.iter().all(|id| rest.any(|was| was == id))
    }

    /// Random trees against random budgets. Whatever comes out fits, nothing
    /// panics, a tree that fits comes back untouched, the bare root is never
    /// needed (so the stages' model of the format held), and everything kept
    /// is what was there: the same ids, roles, actions and focus, children
    /// in their order, and strings either whole or cut where marked.
    #[test]
    fn random_trees_always_fit_and_keep_what_they_keep_intact() {
        // How many trees fit, were cut in their strings only, and lost nodes,
        // so the test can show it reached every stage.
        let (mut fitted, mut strings_only, mut dropped) = (0, 0, 0);
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut ids = 0;
            let mut room = 150;
            let mut root = Node::new("app", Role::App)
                .label(rng.text())
                .focused(rng.one_in(2));
            for _ in 0..1 + rng.below(6) {
                let child = random_node(&mut rng, 4, &mut ids, &mut room);
                root = root.child(child);
            }
            let original = root.clone();
            let original_len = json_len(&original);
            // The bare root of a root like the layer's is about 300 bytes,
            // so every budget leaves room for it.
            let budget = 400 + rng.below(original_len * 2);
            let bare_before = runs().bare;

            let Some(bounded) = bound_root(&mut root, budget) else {
                assert!(original_len <= budget, "seed {seed}");
                assert_eq!(root, original, "seed {seed}: a tree that fits is untouched");
                fitted += 1;
                continue;
            };
            if bounded.nodes_dropped == 0 {
                strings_only += 1;
            } else {
                dropped += 1;
            }
            let len = json_len(&root);
            assert!(original_len > budget, "seed {seed}");
            assert!(len <= budget, "seed {seed}: {len} over {budget}");
            assert_eq!(runs().bare, bare_before, "seed {seed}: no bare root");
            assert_eq!(bounded.root_bytes, original_len, "seed {seed}");
            assert_eq!(
                bounded.nodes_dropped,
                count_nodes(&original) - count_nodes(&root),
                "seed {seed}"
            );

            let mut by_id = HashMap::new();
            let mut pending = vec![&original];
            while let Some(node) = pending.pop() {
                by_id.insert(node.id.0.as_str(), node);
                pending.extend(&node.children);
            }
            let admissible = (bounded.nodes_dropped > 0).then(|| admissible_ids(&original, budget));
            let mut trimmed = 0;
            let mut pending = vec![&root];
            while let Some(node) = pending.pop() {
                let was = by_id[node.id.0.as_str()];
                assert_eq!(node.role, was.role, "seed {seed}");
                assert_eq!(node.actions, was.actions, "seed {seed}");
                assert_eq!(node.focused, was.focused, "seed {seed}");
                for (now, then) in [(&node.label, &was.label), (&node.value, &was.value)] {
                    if now == then {
                        continue;
                    }
                    trimmed += 1;
                    let (now, then) = (now.as_deref().unwrap(), then.as_deref().unwrap());
                    let kept_head = now
                        .strip_suffix(MARKER)
                        .is_some_and(|head| then.starts_with(head));
                    let kept_tail = now
                        .strip_prefix(MARKER)
                        .is_some_and(|tail| then.ends_with(tail));
                    assert!(kept_head || kept_tail, "seed {seed}: {now:?}");
                    assert!(escaped_len(now) < escaped_len(then), "seed {seed}");
                }
                let kept: Vec<&str> = node.children.iter().map(|c| c.id.0.as_str()).collect();
                let all: Vec<&str> = was.children.iter().map(|c| c.id.0.as_str()).collect();
                assert!(
                    is_subsequence(&kept, &all),
                    "seed {seed}: children reordered"
                );
                // And with no gap: what a node keeps is its first children,
                // passing over only those that could never fit.
                if let Some(admissible) = &admissible {
                    let fit: Vec<&str> = all
                        .iter()
                        .copied()
                        .filter(|id| admissible.contains(*id))
                        .collect();
                    assert!(
                        fit.starts_with(&kept),
                        "seed {seed}: a gap in the children of {:?}",
                        node.id.0
                    );
                }
                pending.extend(&node.children);
            }
            assert_eq!(bounded.strings_trimmed, trimmed, "seed {seed}");
        }
        eprintln!("{fitted} fitted, {strings_only} cut in their strings, {dropped} lost nodes");
        for (stage, count) in [
            ("fit", fitted),
            ("strings", strings_only),
            ("nodes", dropped),
        ] {
            assert!(count >= 30, "only {count} of 300 trees reached `{stage}`");
        }
    }
}
