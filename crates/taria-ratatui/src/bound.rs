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
//! 1. nodes that could never fit are dropped, with everything under them,
//!    before any string is cut. Such a node is one whose id, role and
//!    actions, with its strings at the floor, do not fit with only its
//!    ancestors beside it: one huge id or a long run of custom actions,
//!    which no cut shortens. Left in, it would have every later stage cut
//!    the rest of the tree to make room it can never be given;
//! 2. strings are cut: the longest values and descriptions first, down to
//!    one shared length, so the biggest strings shrink first and shorter
//!    ones are untouched, then labels with them, the same way, once values
//!    and descriptions alone are not enough;
//! 3. every node keeps at most one shared number of children, the largest
//!    that fits, so a list of fifty thousand rows loses its last rows while
//!    a status bar beside it keeps its place;
//! 4. shallowest first, the children that cap removed go back while they
//!    fit, each parent keeping a run of its first children with no gap,
//!    apart from a node that could never fit. The run ends at the first
//!    child too large for the room left, which closes its parent's list
//!    although a later, smaller sibling might still fit. So two children
//!    that each fit alone but not together keep only the first, and every
//!    sibling after them goes too: a status bar and a `quit` that follow
//!    two children with half-megabyte ids are dropped, and the line is left
//!    half full. Only absurd sizes reach this, and a gap would be worse: it
//!    would tell an agent reading a list that the row after it is the next
//!    one;
//! 5. if all else fails, a bare root, which always fits: every child of the
//!    root goes.
//!
//! The third and fourth stages plan with every string at the floor, which
//! gives the nodes all the room a cut can make, and the strings of the nodes
//! kept are then cut only as far as the kept tree, measured as it is, still
//! runs over: not at all when what ran over went with the nodes dropped. The
//! fourth stage only gives back what the third took beyond what fitting
//! needed, so it never runs on a tree an earlier stage made fit.
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

/// Bytes a snapshot line spends around its root, at the largest `seq` and
/// the largest stamp.
///
/// Spelled out as the literal it measures, the line with the root left out:
/// the seq and the stamp (`after_input`) are the only parts that vary, and
/// both are written here at `u64::MAX`, so a root within [`ROOT_BUDGET`]
/// fits at any seq the layer can reach and under any stamp. Budgeting for
/// the worst case rather than the next line costs at most 19 bytes of a
/// megabyte for the seq and 35 for the stamp, which an unstamped line does
/// not carry at all, and buys a bound that depends on the tree alone: the
/// same tree is cut the same way whenever it is published, so deduplicating
/// a redrawn frame still works on a tree that had to be cut, and a snapshot
/// sent again with only its stamp moved still fits without being cut again.
///
/// Nothing derives this from the wire types, so
/// `the_envelope_is_measured_not_assumed` holds it against a real
/// serialization, which is what catches a change to the envelope.
const SNAPSHOT_ENVELOPE_BYTES: usize = concat!(
    r#"{"type":"snapshot","protocol_version":1,"seq":18446744073709551615,"#,
    r#""after_input":18446744073709551615,"root":}"#
)
.len();

/// Longest serialized root that still leaves its snapshot line within
/// [`MAX_LINE_BYTES`] at any `seq` and under any stamp.
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

    /// Length of the snapshot line as built, before the size bound cut
    /// anything, in bytes, not counting the terminating `\n`. For a tree
    /// deeper than [`taria::MAX_NODE_DEPTH`] that is the tree after the
    /// depth cut, which runs first: the tree as built is never serialized,
    /// because serializing recurses and the depth cut is what bounds that.
    ///
    /// Counted with `seq` and the stamp at their largest, the way the bound
    /// counts them. A tree is cut when its line would not fit at every seq
    /// and under every stamp, not merely as it went out, so that whether a
    /// tree is cut never depends on how many snapshots or inputs came before
    /// it. That keeps this always over [`limit`](Self::limit), and at most
    /// 54 bytes over the line actually built: 19 for a seq spelled in fewer
    /// digits, and 35 for a stamp spelled shorter or not carried.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The line limit it ran over, [`taria::wire::MAX_LINE_BYTES`].
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// How many labels, values and descriptions were shortened among the
    /// nodes that were published. A node that was dropped is counted in
    /// [`nodes_dropped`](Self::nodes_dropped) instead, whatever it held.
    pub fn strings_trimmed(&self) -> usize {
        self.strings_trimmed
    }

    /// How many nodes were left out, counting every descendant of a dropped
    /// node. Zero when shortening strings was enough.
    pub fn nodes_dropped(&self) -> usize {
        self.nodes_dropped
    }

    /// Id of the node holding the longest string before the cut, measured as
    /// JSON: its id, its label, its value, its description or one of its
    /// custom action names.
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
    let Some(framing) = Framing::measured() else {
        return bare_root(root, &survey, before);
    };

    // Nodes that could never fit go first, before any string is cut, so
    // that no string anywhere is cut to make room one of them will never
    // be given.
    let Some(mut slots) = flatten(root) else {
        return bare_root(root, &survey, before);
    };
    let Some(unfit) = mark_reach(&mut slots, framing, budget) else {
        return bare_root(root, &survey, before);
    };
    let mut now = before;
    // Handed on to the drop stage, which then need not flatten the tree
    // again, for as long as it still describes the tree.
    let mut flattened = Some(slots);
    if unfit > 0 {
        if let Some(slots) = flattened.take() {
            let fits: Vec<bool> = slots.iter().map(|slot| slot.fits).collect();
            keep_only(root, &slots, &fits);
        }
        // Measured again rather than worked out from what went: this runs
        // only for a tree holding an absurd node, and measuring is the one
        // count of what is left that cannot be wrong.
        let Some(after) = measure(root) else {
            return bare_root(root, &survey, before);
        };
        now = after;
        survey.recount_strings(root);
    }

    let (strings_trimmed, nodes_dropped) = if now <= budget {
        (0, unfit)
    } else {
        match cut_strings(root, &mut survey, now - budget) {
            Some(strings) => (strings, unfit),
            None => match drop_to_fit(root, flattened, framing, budget, &mut survey) {
                Some((strings, dropped)) => (strings, unfit.saturating_add(dropped)),
                None => return bare_root(root, &survey, before),
            },
        }
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

/// Cut strings until `excess` bytes come off, and return how many were cut,
/// or `None`, with nothing cut, when cutting every one to [`STRING_FLOOR`]
/// would not be enough.
///
/// Values and descriptions first, and labels with them only once those
/// alone fall short.
fn cut_strings(root: &mut Node, survey: &mut Survey, excess: usize) -> Option<usize> {
    if let Some(level) = water_level(&mut survey.values, excess) {
        return Some(trim_strings(root, level, Strings::Values));
    }
    // A fresh fill over both sets, not labels on top of values already at
    // the floor: with labels in play the shared level can sit above the
    // floor, and values are then cut less.
    let mut both = survey.values.clone();
    both.extend_from_slice(&survey.labels);
    let level = water_level(&mut both, excess)?;
    Some(trim_strings(root, level, Strings::ValuesAndLabels))
}

/// Which strings a water-fill cuts. A description is cut with the values,
/// because like a value it is content, while a label is a name.
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

/// Exact length of `node`'s JSON without its children, with its label,
/// value and description as they would be cut to [`STRING_FLOOR`]: the part
/// of it no stage but dropping it can shrink.
///
/// Measured by setting the children aside rather than modelling the fields,
/// less what [`floor_saving`] says the cut would take off, so nothing is cut
/// to learn it and the one model the node stages still plan with is how a
/// child list is framed, which [`Framing`] measures too.
fn measure_own_at_floor(node: &mut Node) -> Option<usize> {
    let children = std::mem::take(&mut node.children);
    let own = measure(node);
    node.children = children;
    let keep = keep_for(node.role);
    let saving = [
        (&node.value, keep),
        (&node.description, Keep::Head),
        (&node.label, Keep::Head),
    ]
    .into_iter()
    .filter_map(|(text, keep)| Some(floor_saving(text.as_deref()?, keep)))
    .fold(0usize, usize::saturating_add);
    own?.checked_sub(saving)
}

/// What a root that did not fit holds, read once before anything is cut.
///
/// The lengths of the strings are read again once nodes that could never fit
/// are dropped, so the string stages plan on what is left. Everything else
/// describes the tree as it came, because that is what the report is about.
struct Survey {
    /// Escaped lengths of the values and descriptions over [`STRING_FLOOR`],
    /// the only ones a cut can touch, which the first string tier cuts.
    values: Vec<usize>,
    /// Escaped lengths of the labels over [`STRING_FLOOR`].
    labels: Vec<usize>,
    /// The node holding the longest string: id, label, value, description
    /// or custom action name.
    largest: NodeId,
    /// Every node in the tree, the root included.
    nodes: usize,
    /// The root's own label, value and description over [`STRING_FLOOR`],
    /// which are what the bare root ends up cutting whatever ran before it.
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
            // A description is cut in the values' tier, so it is counted
            // with them.
            let strings = [
                (&node.value, false),
                (&node.description, false),
                (&node.label, true),
            ];
            for (text, is_label) in strings {
                let Some(text) = text else { continue };
                let lengths = if is_label { &mut labels } else { &mut values };
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
                + usize::from(over_floor(root.value.as_deref()))
                + usize::from(over_floor(root.description.as_deref())),
        }
    }

    /// Read the lengths of the strings again from what is left of `root`
    /// once nodes were dropped, so the string stages plan on the nodes kept.
    fn recount_strings(&mut self, root: &Node) {
        let rest = Self::of(root);
        self.values = rest.values;
        self.labels = rest.labels;
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
        if let Some(description) = node.description.as_mut()
            && escaped_len(description) > level
        {
            trim_to(description, level, Keep::Head);
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

/// Cut `node`'s own label, value and description down to [`STRING_FLOOR`],
/// returning how many it cut.
fn trim_to_floor(node: &mut Node) -> usize {
    let keep = keep_for(node.role);
    let mut trimmed = 0;
    if let Some(value) = node.value.as_mut()
        && escaped_len(value) > STRING_FLOOR
    {
        trim_to(value, STRING_FLOOR, keep);
        trimmed += 1;
    }
    if let Some(description) = node.description.as_mut()
        && escaped_len(description) > STRING_FLOOR
    {
        trim_to(description, STRING_FLOOR, Keep::Head);
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
    let (at, _) = cut_point(s, target, keep);
    match keep {
        // `at` is just past a character, so this cannot split one.
        Keep::Head => {
            s.truncate(at);
            s.push_str(MARKER);
        }
        // `at` is where a character begins, so neither can this.
        Keep::Tail => s.replace_range(..at, MARKER),
    }
}

/// Where [`trim_to`] cuts `s`: the byte offset the kept part ends at, for
/// [`Keep::Head`], or starts at, for [`Keep::Tail`], and the escaped bytes
/// the kept part costs, [`MARKER`] not included.
///
/// Apart from [`trim_to`] so that what a cut would leave can be known
/// without making it: the first stage has to know what each node would cost
/// with its strings at the floor before it decides whether any string is cut.
fn cut_point(s: &str, target: usize, keep: Keep) -> (usize, usize) {
    let room = target.saturating_sub(MARKER_BYTES);
    let mut used = 0usize;
    // Takes `c` into the kept part if it still fits. The first character
    // that does not is where the cut lands.
    let mut take = |c: char| {
        let next = used.saturating_add(char_cost(c));
        if next > room {
            return false;
        }
        used = next;
        true
    };
    let at = match keep {
        Keep::Head => s
            .char_indices()
            .find(|&(_, c)| !take(c))
            .map_or(s.len(), |(at, _)| at),
        Keep::Tail => s
            .char_indices()
            .rev()
            .find(|&(_, c)| !take(c))
            .map_or(0, |(at, c)| at + c.len_utf8()),
    };
    (at, used)
}

/// Escaped bytes cutting `s` to [`STRING_FLOOR`] would take off it: none
/// for a string already within the floor.
fn floor_saving(s: &str, keep: Keep) -> usize {
    let len = escaped_len(s);
    if len <= STRING_FLOOR {
        return 0;
    }
    let (_, kept) = cut_point(s, STRING_FLOOR, keep);
    len.saturating_sub(kept.saturating_add(MARKER_BYTES))
}

/// How `serde_json` frames a node's children, measured from its own output
/// rather than written down here.
///
/// The node stages add a tree up from what each node costs without its
/// children, [`measure_own_at_floor`], and these two numbers, so they are the
/// whole of their model of the format. Taking them from two tiny trees on
/// every cut keeps that model in step with whatever `serde_json` and [`Node`]
/// write.
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

/// One node of a tree flattened in breadth-first order, as the node stages
/// need it.
///
/// Breadth-first puts every node after its parent and before everything
/// deeper, and a node's children side by side in their own order. The first
/// is what lets [`mark_reach`] work down a tree in one forward pass,
/// [`size_at_cap`] add it up in one backwards pass, and the refill reach
/// shallow nodes first; the second is what lets a node's children be found,
/// and kept or dropped, as one range.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    /// Index of the parent slot. Unused on the root, slot 0.
    parent: usize,
    /// Exact JSON length without its children, its strings at the floor;
    /// see [`measure_own_at_floor`].
    own: usize,
    /// Index of its first child's slot; the rest follow it.
    first_child: usize,
    /// How many children it has.
    children: usize,
    /// Position among its siblings, which is what a cap on children counts.
    rank: usize,
    /// The smallest tree that holds this node, every string at the floor:
    /// it, every ancestor, and the child lists between them, with nothing
    /// else. Set by [`mark_reach`].
    reach: usize,
    /// Whether `reach` is within the budget. A node for which it is not
    /// could never be published, and goes with everything under it before
    /// any string is cut. Set by [`mark_reach`].
    fits: bool,
}

/// Drop nodes until the tree fits with every string at the floor, and then
/// cut the strings of the nodes kept only as far as they still run over.
/// Returns how many published strings were cut and how many nodes were
/// dropped, or `None` when not even the root alone fits or a node could not
/// be measured.
///
/// `slots` is the tree as the first stage flattened it, while that still
/// describes it, and the tree is flattened afresh when it is `None`. Every
/// node in it could fit with nothing but its ancestors beside it: the first
/// stage took out those that could not.
///
/// Two passes over the tree flattened breadth-first, each planning with every
/// string at the floor:
///
/// 1. One cap on how many children any node keeps, the widest that fits,
///    found by halving. Every node wider than the cap loses its tail and
///    every narrower one keeps everything, so a status bar beside a runaway
///    list, one of a handful of the root's children, stays well inside any
///    cap the list's rows leave room for. Searching adds the tree up once
///    per halving, a few dozen times at most, rather than once per node
///    dropped.
/// 2. One cap for the whole tree is coarse: where the deepest branch is the
///    last child of a wide node, any cap that keeps that branch keeps
///    everything, and the widest cap that fits can leave most of the budget
///    unused. So what the cap dropped is offered back, breadth-first so
///    shallower nodes come first and siblings in their own order, and each
///    is taken when its parent is kept and it fits in what is left. The
///    first that does not fit ends the offer for its parent: none of its
///    later siblings is taken, though a smaller one might fit, so what a
///    node keeps is always its first children, with no gap apart from a
///    node that could never fit, which the first stage took out. A gap
///    would tell an agent reading a list that the row after it is the next
///    one. Other parents' children are still offered after it.
///
/// The strings are cut last, and only in the nodes kept, which is all a
/// string cut in a dropped node would have been spent on. Planning with them
/// at the floor gives the nodes all the room a cut can make, but the floor
/// is the most a cut can take, not what the kept tree needs: the nodes that
/// went are often what ran over. So the tree kept is measured as it is, and
/// its strings are cut, by [`cut_strings`], only by what it is still over,
/// which is often nothing. That cut, or failing it every string at the
/// floor, always fits, because every string at the floor is the plan.
///
/// Every pass is linear in the tree and the search runs a logarithmic number
/// of them, so a list of two hundred thousand rows costs a few dozen walks
/// over a flat array rather than anything quadratic.
fn drop_to_fit(
    root: &mut Node,
    slots: Option<Vec<Slot>>,
    framing: Framing,
    budget: usize,
    survey: &mut Survey,
) -> Option<(usize, usize)> {
    let slots = match slots {
        Some(slots) => slots,
        None => flatten(root)?,
    };

    let widest = slots.iter().map(|slot| slot.children).max().unwrap_or(0);
    let mut scratch = vec![0usize; slots.len()];
    let mut size = |cap| size_at_cap(&slots, framing, cap, &mut scratch);
    // Invariant: a cap of `fits` fits and one of `over` does not. A cap of
    // zero is the root alone, which the first stage found fits; checked
    // again here rather than taken on trust.
    let mut fits = 0;
    let mut used = size(0);
    if used > budget {
        return None;
    }
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
    let published = kept.iter().filter(|kept| **kept).count();
    let dropped = slots.len().saturating_sub(published);
    let now = measure(root)?;
    if now <= budget {
        return Some((0, dropped));
    }
    survey.recount_strings(root);
    // The fill counts each cut string as freeing its length less the level,
    // and a cut that stops short of a wide character frees a few bytes more,
    // so it can come up short of a plan that fits by those few bytes. Every
    // string at the floor is then what the plan measured.
    let strings = cut_strings(root, survey, now - budget)
        .unwrap_or_else(|| trim_strings(root, STRING_FLOOR, Strings::ValuesAndLabels));
    Some((strings, dropped))
}

/// Flatten the tree breadth-first into [`Slot`]s, each measured as it would
/// be with its strings at the floor, or `None` if a node could not be
/// measured. Nothing in the tree is cut.
fn flatten(root: &mut Node) -> Option<Vec<Slot>> {
    let mut slots = Vec::new();
    let mut queue: VecDeque<(&mut Node, usize, usize)> = VecDeque::from([(root, 0, 0)]);
    while let Some((node, parent, rank)) = queue.pop_front() {
        let index = slots.len();
        let own = measure_own_at_floor(node)?;
        slots.push(Slot {
            parent,
            own,
            // Everything still queued takes the indices after this one, and
            // the children are queued behind it, so they come next, side by
            // side.
            first_child: index.saturating_add(queue.len()).saturating_add(1),
            children: node.children.len(),
            rank,
            ..Slot::default()
        });
        queue.extend(
            node.children
                .iter_mut()
                .enumerate()
                .map(|(rank, child)| (child, index, rank)),
        );
    }
    Some(slots)
}

/// Work out which nodes could ever fit: those whose reach is within the
/// budget. Returns how many could not, or `None` when the root itself could
/// not, which nothing short of the bare root can help.
///
/// Parent before child, which breadth-first order guarantees, so a node's
/// reach is its parent's plus a child list and itself, and a node under one
/// that could never fit could never fit either.
fn mark_reach(slots: &mut [Slot], framing: Framing, budget: usize) -> Option<usize> {
    let root = slots.first_mut()?;
    root.reach = root.own;
    root.fits = root.reach <= budget;
    if !root.fits {
        return None;
    }
    let mut unfit = 0usize;
    for index in 0..slots.len() {
        let Some(&parent) = slots.get(index) else {
            break;
        };
        let range = parent.first_child..parent.first_child.saturating_add(parent.children);
        let Some(children) = slots.get_mut(range) else {
            continue;
        };
        for child in children {
            child.reach = parent
                .reach
                .saturating_add(framing.list)
                .saturating_add(child.own);
            child.fits = parent.fits && child.reach <= budget;
            if !child.fits {
                unfit = unfit.saturating_add(1);
            }
        }
    }
    Some(unfit)
}

/// Exact JSON length of the flattened tree, every string at the floor, if no
/// node kept more than `cap` of its children. `kept_bytes` is scratch space,
/// one entry per slot.
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
            .saturating_add(framing.around(slot.children.min(cap)))
            .saturating_add(kept_bytes.get(index).copied().unwrap_or(0));
        if index == 0 {
            return bytes;
        }
        if slot.rank < cap
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
/// an unbroken run of its children from the first.
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

    // What the cap keeps, which is what `used` measured: a chain of nodes
    // within the cap all the way up from the root.
    for (index, slot) in slots.iter().enumerate().skip(1) {
        if slot.rank < cap && is_kept(&kept, slot.parent) {
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
        if is_kept(&kept, index)
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

/// The last resort: the root with no children, its own label, value and
/// description cut to the floor.
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
    use taria::wire::{AppToBridge, InputId};

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
                .relayed(index % 4 == 1)
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
            // The typed state, a different part of it on each node.
            node = match index % 4 {
                0 => node
                    .description(ESCAPE_HEAVY.repeat(index % 5))
                    .selected(index % 8 == 0)
                    .checked(taria::Checked::Mixed),
                1 => node
                    .expanded(false)
                    .disabled(true)
                    .range(-0.5, -1e300, 1e300),
                2 => node
                    .range(index as f64, 0.0, 29.0)
                    .pos_in_set(1 + index as u32),
                _ => node
                    .set_size(u32::MAX)
                    .cell(index as u32, 2)
                    .table_size(0, 7),
            };
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
        let line_len = |seq, stamp: Option<u64>| {
            let mut snapshot = Snapshot::new(seq, root.clone());
            if let Some(stamp) = stamp {
                snapshot = snapshot.after(InputId::new(stamp));
            }
            serde_json::to_vec(&AppToBridge::Snapshot(snapshot))
                .unwrap()
                .len()
        };

        assert_eq!(
            line_len(u64::MAX, Some(u64::MAX)) - root_len,
            SNAPSHOT_ENVELOPE_BYTES
        );
        // And it is the worst case: no seq or stamp spends more, the smallest
        // seq spends exactly the 19 digits less that the budget gives away,
        // and an unstamped line the 35 bytes of the whole field besides.
        for seq in [0, 1, 9, 10, 99, 100, 12_345, u64::MAX / 10, u64::MAX] {
            for stamp in [None, Some(0), Some(1), Some(12_345), Some(u64::MAX)] {
                assert!(
                    line_len(seq, stamp) - root_len <= SNAPSHOT_ENVELOPE_BYTES,
                    "seq {seq}, stamp {stamp:?}"
                );
            }
        }
        assert_eq!(
            line_len(1, Some(u64::MAX)) - root_len,
            SNAPSHOT_ENVELOPE_BYTES - 19
        );
        assert_eq!(
            line_len(1, Some(1)) - root_len,
            SNAPSHOT_ENVELOPE_BYTES - 19 - 19
        );
        let field = r#""after_input":18446744073709551615,"#.len();
        assert_eq!(field, 35);
        assert_eq!(
            line_len(u64::MAX, None) - root_len,
            SNAPSHOT_ENVELOPE_BYTES - field
        );
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
        // Nothing in the varied tree is over the floor, so the tree the
        // slots plan for is the one serialized.
        let before = tree.clone();
        let mut slots = flatten(&mut tree).unwrap();
        assert_eq!(tree, before, "flattening cuts nothing");
        assert_eq!(slots.len(), count_nodes(&tree));
        assert_eq!(mark_reach(&mut slots, framing, usize::MAX), Some(0));

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

        // With strings over the floor, every escape class among them, the
        // slots plan for the tree as the floor would leave it, worked out
        // without cutting anything: the same bytes as cutting it and
        // measuring.
        let mut long = varied_tree();
        let mut pending = vec![&mut long];
        while let Some(node) = pending.pop() {
            node.label = Some(ESCAPE_HEAVY.repeat(30));
            if node.value.is_some() {
                node.value = Some(format!("{}tail", ESCAPE_HEAVY.repeat(40)));
            }
            pending.extend(node.children.iter_mut());
        }
        let untouched = long.clone();
        let slots = flatten(&mut long).unwrap();
        assert_eq!(long, untouched, "flattening cuts nothing");
        let mut at_floor = long.clone();
        let cut = trim_strings(&mut at_floor, STRING_FLOOR, Strings::ValuesAndLabels);
        assert!(cut > count_nodes(&long), "labels and values were over");
        let mut scratch = vec![0; slots.len()];
        assert_eq!(
            size_at_cap(&slots, framing, usize::MAX, &mut scratch),
            json_len(&at_floor)
        );
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

    /// A description is content, like a value, so it is cut in the first
    /// tier with the values, and to the same shared level, while the label,
    /// a name, waits for the second.
    #[test]
    fn descriptions_are_cut_with_values_before_labels() {
        let root = Node::new("app", Role::App).label("described").child(
            Node::new("field", Role::TextInput)
                .label("l".repeat(3000))
                .value("v".repeat(3000))
                .description("d".repeat(3000))
                .checked(true)
                .range(1.0, 0.0, 2.0),
        );
        let field = |root: &Node| root.children[0].clone();

        let mut first = root.clone();
        let budget = json_len(&root) - 4000;
        let bounded = bound_root(&mut first, budget).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 2);
        assert!(json_len(&first) <= budget);
        let cut = field(&first);
        assert_eq!(cut.label, field(&root).label, "the label is untouched");
        let (value, description) = (cut.value.unwrap(), cut.description.unwrap());
        assert!(value.ends_with(MARKER) && description.ends_with(MARKER));
        assert_eq!(escaped_len(&value), escaped_len(&description));
        assert_eq!(cut.checked, Some(taria::Checked::True));
        assert_eq!(cut.value_now.map(taria::Number::get), Some(1.0));

        // More than the two can give before the floor: the label joins them.
        let mut all = root.clone();
        let budget = json_len(&root) - 7000;
        let bounded = bound_root(&mut all, budget).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 3);
        assert!(json_len(&all) <= budget);
        let cut = field(&all);
        let label = cut.label.unwrap();
        assert!(label.ends_with(MARKER));
        assert_eq!(escaped_len(&label), escaped_len(&cut.description.unwrap()));
    }

    /// A long description is a string a cut can shorten, so a node holding
    /// one is cut, never dropped as a node that could never fit, and it is
    /// what the report names as the largest.
    #[test]
    fn a_long_description_is_cut_not_dropped_and_named_largest() {
        let root = Node::new("app", Role::App).label("described").children([
            Node::new("help", Role::Text).description("d".repeat(100_000)),
            Node::new("status", Role::Status).value("ok"),
        ]);
        let mut cut = root.clone();
        let budget = 2000;
        let bounded = bound_root(&mut cut, budget).expect("over budget");
        assert!(json_len(&cut) <= budget);
        assert_eq!(bounded.nodes_dropped, 0);
        assert_eq!(bounded.strings_trimmed, 1);
        assert_eq!(bounded.largest, NodeId("help".into()));
        let description = cut.children[0].description.as_deref().unwrap();
        assert!(description.ends_with(MARKER));
        assert!(
            escaped_len(description) > STRING_FLOOR,
            "cut only as far as needed"
        );

        // The bare root cuts its own description to the floor too.
        let mut bare = Node::new("app", Role::App).description("d".repeat(5000));
        let bounded = bound_root(&mut bare, 1000).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 1);
        assert!(json_len(&bare) <= 1000);
    }

    /// A cut shortens the text a node relays and says nothing new about who
    /// wrote it: a relayed log cut in both its value and its label still goes
    /// out marked, and the nodes around it stay unmarked.
    #[test]
    fn a_relayed_node_cut_to_fit_keeps_its_mark() {
        let root = Node::new("app", Role::App).label("relayed").children([
            Node::new("reply", Role::Log)
                .label("l".repeat(3000))
                .value("v".repeat(3000))
                .relayed(true),
            Node::new("status", Role::Status).value("streaming"),
        ]);
        let mut cut = root.clone();
        let budget = json_len(&root) - 4000;
        let bounded = bound_root(&mut cut, budget).expect("over budget");
        assert_eq!(bounded.strings_trimmed, 2);
        assert!(json_len(&cut) <= budget);

        let reply = &cut.children[0];
        assert!(
            value_of(&cut, 0).starts_with(MARKER),
            "a log keeps its tail"
        );
        assert!(reply.label.as_deref().is_some_and(|l| l.ends_with(MARKER)));
        assert!(reply.relayed, "cut or not, it is still relayed text");
        let json = serde_json::to_string(reply).unwrap();
        assert!(json.contains(r#""relayed":true"#), "{json}");
        assert!(!cut.relayed && !cut.children[1].relayed);
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

    /// The edge the no-gap rule costs, pinned so the module documentation
    /// stays true of it: two children that each fit alone but not together,
    /// ahead of the chrome. The second does not fit in the room the first
    /// leaves, which closes the root's list, so the chrome after it goes too
    /// although it would fit.
    #[test]
    fn a_child_too_large_for_the_room_left_closes_its_parents_list() {
        let mut root = Node::new("app", Role::App)
            .label("closed")
            .child(Node::new("a".repeat(800), Role::Text))
            .child(Node::new("b".repeat(900), Role::Text))
            .children(status_and_quit());
        let bounded = bound_root(&mut root, 1500).expect("over budget");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["a".repeat(800)], "the run ends at the second");
        assert_eq!(bounded.nodes_dropped, 3);
        assert!(json_len(&root) <= 1500);
    }

    /// The chrome a real app publishes beside whatever runs away.
    fn status_and_quit() -> [Node; 2] {
        [
            Node::new("status", Role::Status).value("3 tasks"),
            Node::new("quit", Role::Button)
                .label("Quit")
                .action(Action::Activate),
        ]
    }

    /// A node that could never fit is dropped before any string is cut, so
    /// it costs its siblings nothing. Cut later, it had every label and
    /// value in the tree cut to the floor first: a 300 KB draft that fits
    /// whole went out at 256 bytes, in a line of 590.
    #[test]
    fn a_node_that_could_never_fit_costs_its_siblings_nothing() {
        let draft = "d".repeat(300_000);
        let mut root = Node::new("app", Role::App)
            .label("unfit")
            .child(Node::new("I".repeat(2_000_000), Role::Text))
            .child(Node::new("draft", Role::TextInput).value(draft.clone()))
            .children(status_and_quit());
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["draft", "status", "quit"]);
        assert_eq!(value_of(&root, 0), draft, "the draft goes out whole");
        assert_eq!(bounded.strings_trimmed, 0, "no string was cut");
        assert_eq!(bounded.nodes_dropped, 1);
        assert_eq!(bounded.largest.0, "I".repeat(2_000_000));
        assert!(json_len(&root) > 300_000);
    }

    /// Past the node that could never fit, the string stages run on what is
    /// left, so a draft too long for the line is cut to what the line has
    /// room for rather than to the floor.
    #[test]
    fn a_long_draft_beside_a_node_that_could_never_fit_fills_the_line() {
        let mut root = Node::new("app", Role::App)
            .label("unfit")
            .child(Node::new("I".repeat(1_100_000), Role::Text))
            .child(Node::new("draft", Role::TextInput).value("d".repeat(1_500_000)))
            .children(status_and_quit());
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["draft", "status", "quit"]);
        assert_eq!(root.children[1..], status_and_quit());
        let kept = escaped_len(value_of(&root, 0));
        assert!(
            kept > ROOT_BUDGET - 1024,
            "{kept} bytes of draft in a line of {ROOT_BUDGET}"
        );
        assert!(json_len(&root) <= ROOT_BUDGET);
        assert_eq!(bounded.strings_trimmed, 1);
        assert_eq!(bounded.nodes_dropped, 1);
    }

    /// The same for a run of custom actions too long for the line: the log
    /// beside it fits, so it is kept whole, tail and all, not cut to the
    /// floor's last 256 bytes.
    #[test]
    fn a_log_beside_custom_actions_that_could_never_fit_keeps_its_tail_whole() {
        let log: String = (0..526_000)
            .map(|n| if n % 80 == 79 { '\n' } else { 'l' })
            .collect();
        let actions = Node::new("actions", Role::Menu)
            .actions((0..80_000).map(|n| Action::Custom(format!("action-{n:06}"))));
        assert!(json_len(&actions) > 2_000_000, "{}", json_len(&actions));
        let mut root = Node::new("app", Role::App)
            .label("unfit")
            .child(actions)
            .child(Node::new("log", Role::Log).value(log.clone()))
            .children(status_and_quit());
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["log", "status", "quit"]);
        assert_eq!(value_of(&root, 0), log, "the log goes out whole");
        assert_eq!(bounded.strings_trimmed, 0);
        assert_eq!(bounded.nodes_dropped, 1);
    }

    /// Nodes are dropped on a plan that puts every string at the floor, but
    /// the strings of the nodes kept are cut only by what the kept tree is
    /// still over. Here the last child fits alone and not beside the one
    /// before it, so it goes, and what is left fits with the draft whole.
    /// Cut to the floor whatever room was left, the draft went out at 256
    /// bytes in a line of 600,480, with room for all of it.
    #[test]
    fn a_draft_beside_nodes_that_must_go_is_kept_whole() {
        let draft = "d".repeat(300_000);
        let mut root = Node::new("app", Role::App).label("drop").children([
            Node::new("draft", Role::TextInput).value(draft.clone()),
            Node::new("A".repeat(600_000), Role::Text),
            Node::new("B".repeat(600_000), Role::Text),
        ]);
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["draft".to_string(), "A".repeat(600_000)]);
        assert_eq!(value_of(&root, 0), draft, "the draft goes out whole");
        assert_eq!(bounded.strings_trimmed, 0, "no string was cut");
        assert_eq!(bounded.nodes_dropped, 1);
        assert!(json_len(&root) > 900_000);
    }

    /// And where the kept tree does not fit as it is, its strings are cut
    /// to what the line has room for, not to the floor: cut to the floor, a
    /// log of 941 KB went out as its last 227 bytes in a line of 322 KB.
    #[test]
    fn a_log_beside_nodes_that_must_go_keeps_most_of_its_tail() {
        let log: String = (0..941_000)
            .map(|n| if n % 80 == 79 { '\n' } else { 'l' })
            .collect();
        let mut root = Node::new("app", Role::App).label("drop").children([
            Node::new("log", Role::Log).value(log.clone()),
            Node::new("A".repeat(322_000), Role::Text),
            Node::new("B".repeat(800_000), Role::Text),
        ]);
        let bounded = bound_root(&mut root, ROOT_BUDGET).expect("over budget");

        assert_eq!(root.children.len(), 2, "the last child went");
        assert_eq!(bounded.nodes_dropped, 1);
        assert_eq!(bounded.strings_trimmed, 1);
        let kept = value_of(&root, 0);
        let tail = kept.strip_prefix(MARKER).expect("the cut is marked");
        assert!(log.ends_with(tail), "a log keeps its tail");
        assert!(
            escaped_len(kept) > ROOT_BUDGET - 322_000 - 1024,
            "{} bytes of log kept",
            escaped_len(kept)
        );
        let len = json_len(&root);
        assert!(
            len <= ROOT_BUDGET && ROOT_BUDGET - len < 6,
            "{len} of {ROOT_BUDGET}"
        );
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
        if rng.one_in(4) {
            node = node
                .description(rng.text())
                .checked(rng.one_in(2))
                .range(rng.below(1000) as f64 / 7.0, 0.0, 1e9)
                .pos_in_set(rng.below(100) as u32);
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

    /// Ids of the nodes in `original` that could ever fit at `budget`: those
    /// that fit with nothing beside them but their ancestors.
    ///
    /// Worked out apart from the first stage, which this is the oracle for,
    /// by building that tree: the node and each of its ancestors with no
    /// other children and their strings cut to the floor, each the child of
    /// the next, serialized by `serde_json` and measured. None of the
    /// planning the stage does (flattening, the framing model, what a cut
    /// would save) is asked.
    fn fitting_ids(original: &Node, budget: usize) -> HashSet<String> {
        fn walk(node: &mut Node, path: &mut Vec<Node>, budget: usize, ids: &mut HashSet<String>) {
            let children = std::mem::take(&mut node.children);
            let mut alone = node.clone();
            node.children = children;
            trim_to_floor(&mut alone);
            path.push(alone);
            // Built out of the path and taken apart into it again, so no
            // node is copied twice.
            let chain = path
                .drain(..)
                .rev()
                .reduce(|child, parent| parent.child(child))
                .unwrap();
            let fits = json_len(&chain) <= budget;
            let mut next = Some(chain);
            while let Some(mut link) = next {
                next = link.children.pop();
                path.push(link);
            }
            // A node under one that could never fit could never fit either:
            // its chain holds its parent's.
            if fits {
                ids.insert(node.id.0.clone());
                for child in &mut node.children {
                    walk(child, path, budget, ids);
                }
            }
            path.pop();
        }
        let mut ids = HashSet::new();
        walk(&mut original.clone(), &mut Vec::new(), budget, &mut ids);
        ids
    }

    /// Make one node of `root` too large to fit beside the rest, by its id
    /// or by a run of custom actions, which no cut shortens: `extra` bytes
    /// of them or more. Picks the node by the counter that leads its id.
    fn make_one_huge(rng: &mut Rng, root: &mut Node, nodes: usize, extra: usize) {
        let prefix = format!("n{}:", 1 + rng.below(nodes));
        let by_id = rng.one_in(2);
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if node.id.0.starts_with(&prefix) {
                if by_id {
                    node.id.0.push_str(&"U".repeat(extra));
                } else {
                    // Long names, so the run costs a few hundred actions
                    // rather than a few hundred thousand.
                    let name = "U".repeat(1000);
                    node.actions.extend(
                        (0..=extra / 1000).map(|n| Action::Custom(format!("huge-{n}-{name}"))),
                    );
                }
                return;
            }
            pending.extend(node.children.iter_mut());
        }
    }

    /// Whether `kept` is `original` with some entries left out and the rest
    /// in their order.
    fn is_subsequence(kept: &[&str], original: &[&str]) -> bool {
        let mut rest = original.iter();
        kept.iter().all(|id| rest.any(|was| was == id))
    }

    /// Random trees against random budgets. Whatever comes out fits, nothing
    /// panics, a tree that fits comes back untouched, the bare root is never
    /// needed (so the stages' model of the format held), a tree that fits
    /// once the nodes that could never fit are gone loses nothing else, a
    /// cut string leaves the line full, and everything kept is what was
    /// there: the same ids, roles, actions and focus, children in their
    /// order with no gap, and strings either whole or cut where marked.
    #[test]
    fn random_trees_always_fit_and_keep_what_they_keep_intact() {
        // How many trees fit, were cut in their strings only, and lost nodes,
        // so the test can show it reached every stage; how many of those
        // that lost nodes lost only the ones that could never fit; and how
        // many lost nodes to the drop stage and then had strings cut.
        let (mut fitted, mut strings_only, mut dropped, mut unfit_only) = (0, 0, 0, 0);
        let mut dropped_then_cut = 0;
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            // One tree in three has a node made too large to fit at a budget
            // drawn below what it adds, so the first stage has a node to
            // drop, and as often as not that is all it has to do. Those trees
            // are kept smaller, so the node that runs over costs the test
            // what a whole tree does rather than several times that.
            let huge = rng.one_in(3);
            let mut ids = 0;
            let mut room = if huge { 40 } else { 150 };
            let mut root = Node::new("app", Role::App)
                .label(rng.text())
                .focused(rng.one_in(2));
            for _ in 0..1 + rng.below(6) {
                let child = random_node(&mut rng, 4, &mut ids, &mut room);
                root = root.child(child);
            }
            // The bare root of a root like the layer's is about 300 bytes,
            // so every budget leaves room for it.
            let budget = if huge {
                let extra = json_len(&root) * (2 + rng.below(4));
                make_one_huge(&mut rng, &mut root, ids, extra);
                400 + rng.below(extra)
            } else {
                400 + rng.below(json_len(&root) * 2)
            };
            let original = root.clone();
            let original_len = json_len(&original);
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
            let fitting = fitting_ids(&original, budget);
            // The first stage's plan agrees with the tree it plans for, node
            // for node, in the breadth-first order it was flattened in.
            let mut flat = original.clone();
            let mut slots = flatten(&mut flat).unwrap();
            mark_reach(&mut slots, Framing::measured().unwrap(), budget).unwrap();
            let mut planned = HashSet::new();
            let mut queue = VecDeque::from([&flat]);
            let mut slots = slots.iter();
            while let Some(node) = queue.pop_front() {
                if slots.next().is_some_and(|slot| slot.fits) {
                    planned.insert(node.id.0.clone());
                }
                queue.extend(&node.children);
            }
            assert_eq!(
                planned, fitting,
                "seed {seed}: the first stage and the oracle disagree"
            );
            let unfit = count_nodes(&original) - fitting.len();
            if bounded.nodes_dropped > unfit && bounded.strings_trimmed > 0 {
                dropped_then_cut += 1;
            }
            // Strings are cut only by what the tree is still over, after a
            // node drop as much as before one: whenever one was cut, the
            // line is within six bytes a string of the budget, which is what
            // sharing one level and stopping short of a wide character can
            // leave.
            assert!(
                bounded.strings_trimmed == 0 || len + 6 * bounded.strings_trimmed > budget,
                "seed {seed}: {len} of {budget} bytes with {} string(s) cut",
                bounded.strings_trimmed
            );
            // Stopped as soon as the tree fit: when dropping the nodes that
            // could never fit is enough, that is all that happened, and no
            // string anywhere was cut.
            let mut pruned = original.clone();
            let mut pending = vec![&mut pruned];
            while let Some(node) = pending.pop() {
                node.children
                    .retain(|child| fitting.contains(child.id.0.as_str()));
                pending.extend(node.children.iter_mut());
            }
            if json_len(&pruned) <= budget {
                assert_eq!(root, pruned, "seed {seed}: cut more than the unfit nodes");
                assert_eq!(bounded.strings_trimmed, 0, "seed {seed}");
                unfit_only += 1;
            }
            let mut trimmed = 0;
            let mut pending = vec![&root];
            while let Some(node) = pending.pop() {
                let was = by_id[node.id.0.as_str()];
                assert_eq!(node.role, was.role, "seed {seed}");
                assert_eq!(node.actions, was.actions, "seed {seed}");
                assert_eq!(node.focused, was.focused, "seed {seed}");
                let strings = [
                    (&node.label, &was.label),
                    (&node.value, &was.value),
                    (&node.description, &was.description),
                ];
                for (now, then) in strings {
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
                let fit: Vec<&str> = all
                    .iter()
                    .copied()
                    .filter(|id| fitting.contains(*id))
                    .collect();
                assert!(
                    fit.starts_with(&kept),
                    "seed {seed}: a gap in the children of {:?}",
                    node.id.0
                );
                pending.extend(&node.children);
            }
            assert_eq!(bounded.strings_trimmed, trimmed, "seed {seed}");
        }
        eprintln!(
            "{fitted} fitted, {strings_only} cut in their strings, {dropped} lost nodes, \
             {unfit_only} only those that could never fit, {dropped_then_cut} strings cut \
             after the drop stage"
        );
        for (stage, count) in [
            ("fit", fitted),
            ("strings", strings_only),
            ("nodes", dropped),
            ("unfit nodes only", unfit_only),
            ("strings after the drop stage", dropped_then_cut),
        ] {
            assert!(count >= 30, "only {count} of 300 trees reached `{stage}`");
        }
    }
}
