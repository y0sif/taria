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
//! [`bound_root`] makes the tree fit before it is published. It cuts what
//! costs the most first and stops as soon as the tree fits:
//!
//! 1. the longest values, down to one shared length, so the biggest strings
//!    shrink first and shorter ones are untouched;
//! 2. values and labels together, the same way, once values alone are not
//!    enough;
//! 3. the trailing children of the widest nodes;
//! 4. last, every child of the root.
//!
//! Ids and custom action names are never changed, because they are what an
//! agent sends back: a node it can read but not address is worse than one it
//! cannot read at all.
//!
//! Everything here is pure: no I/O, nothing the layer's threads share.

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

/// What a node costs before its strings and its children: the braces, the
/// `id` and `role` keys, and `focused` at its longer spelling.
const NODE_FRAME_BYTES: usize = r#"{"id":,"role":,"focused":false}"#.len();

/// The key a present label adds.
const LABEL_KEY_BYTES: usize = r#","label":"#.len();

/// The key a present value adds.
const VALUE_KEY_BYTES: usize = r#","value":"#.len();

/// The key and brackets a non-empty action list adds, before its entries and
/// the commas between them.
const ACTIONS_FRAME_BYTES: usize = r#","actions":[]"#.len();

/// The key and brackets a non-empty child list adds, before its entries and
/// the commas between them.
const CHILDREN_FRAME_BYTES: usize = r#","children":[]"#.len();

/// What a [`Action::Custom`] adds around its quoted name.
const CUSTOM_FRAME_BYTES: usize = r#"{"custom":}"#.len();

/// Upper bound on a role, or on an action that is not
/// [`Action::Custom`], quotes included.
///
/// Both are short snake_case names, the longest today `"progress_bar"` at 14
/// bytes, and both vocabularies are `#[non_exhaustive]` in another crate, so
/// no `match` here can name every variant and stay exhaustive as they grow.
/// An allowance with room to spare is what the per-frame estimate charges
/// instead. It never needs to be exact, only never short, and
/// `every_role_and_built_in_action_fits_the_name_allowance` checks every name
/// this build knows against it.
const NAME_BYTES_MAX: usize = 32;

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
    /// The report for a root [`bound_root`] cut against [`ROOT_BUDGET`].
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

    /// Id of the node holding the longest label or value before the cut,
    /// measured as JSON. Several nodes can tie; this is the first in tree
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

/// What [`bound_root`] did to a root that did not fit.
///
/// Kept apart from [`SnapshotTooLarge`] so the bound can be run against any
/// budget, which is what lets its tests work with trees of a few kilobytes
/// rather than a megabyte each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bounded {
    /// The root's exact serialized length before the cut.
    pub(crate) root_bytes: usize,
    pub(crate) strings_trimmed: usize,
    pub(crate) nodes_dropped: usize,
    pub(crate) largest: NodeId,
}

/// Cut `root` until its JSON is at most `budget` bytes, and say what was cut.
///
/// `None` means the root fits and was not touched. A tree that fits costs
/// one walk over it, adding up an upper bound on its size with neither
/// serialization nor allocation, because every frame of every app passes
/// through here and almost none of them are anywhere near the limit.
///
/// Past the estimate the tree is measured exactly, by serializing it into a
/// counter, and every stage after that is measured exactly again before it is
/// trusted. The stages run in the order the module documentation gives, each
/// only when the one before could not make the tree fit. A failure to measure
/// at any point, which serializing a [`Node`] into a counter has no way to
/// produce, falls back to the bare root rather than to a guess.
///
/// The tree must already be within [`taria::MAX_NODE_DEPTH`]: the estimate
/// recurses, as `serde_json` does, and the depth cut is what bounds both.
pub(crate) fn bound_root(root: &mut Node, budget: usize) -> Option<Bounded> {
    if estimate(root, budget) <= budget {
        return None;
    }
    let Some(before) = measure(root) else {
        let survey = Survey::of(root);
        let before = estimate(root, usize::MAX);
        return Some(bare_root(root, &survey, before));
    };
    // The estimate charges every role and built-in action its allowance, so
    // a tree of many small nodes can be over it and still fit.
    if before <= budget {
        return None;
    }
    let excess = before - budget;
    let mut survey = Survey::of(root);

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
            None => return Some(bare_root(root, &survey, before)),
        },
    };
    match measure(root) {
        Some(after) if after <= budget => Some(Bounded {
            root_bytes: before,
            strings_trimmed,
            nodes_dropped,
            largest: survey.largest,
        }),
        // The stages plan on a model of what `serde_json` writes, and the
        // tests hold that model to it. If the two ever part ways, the root
        // still goes out in a form that fits rather than in one that might
        // not.
        _ => Some(bare_root(root, &survey, before)),
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

/// Length of `s` as a JSON string, quotes included.
fn quoted_len(s: &str) -> usize {
    escaped_len(s).saturating_add(2)
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
/// so the one place a model is still trusted is how a child list is framed,
/// which `the_size_model_matches_serde_at_full_width` pins.
fn measure_own(node: &mut Node) -> Option<usize> {
    let children = std::mem::take(&mut node.children);
    let own = measure(node);
    node.children = children;
    own
}

/// An upper bound on [`measure`], adding up no further once it passes
/// `stop_above`.
fn estimate(root: &Node, stop_above: usize) -> usize {
    let mut total = 0;
    add_estimate(root, &mut total, stop_above);
    total
}

/// Add `node` and its subtree to `total`, returning false as soon as the sum
/// passes `stop_above`.
///
/// Recursive so that it allocates nothing, which is safe only because the
/// tree has already been cut to [`taria::MAX_NODE_DEPTH`].
fn add_estimate(node: &Node, total: &mut usize, stop_above: usize) -> bool {
    *total = total.saturating_add(own_bytes_upper(node));
    if !node.children.is_empty() {
        *total = total
            .saturating_add(CHILDREN_FRAME_BYTES)
            .saturating_add(node.children.len() - 1);
    }
    if *total > stop_above {
        return false;
    }
    node.children
        .iter()
        .all(|child| add_estimate(child, total, stop_above))
}

/// An upper bound on what `node` costs without its children.
///
/// Exact for the strings, ids and custom action names included, and for the
/// keys; a role or built-in action is charged [`NAME_BYTES_MAX`] and
/// `focused` its longer spelling, which is where the estimate runs over.
fn own_bytes_upper(node: &Node) -> usize {
    let mut bytes = NODE_FRAME_BYTES
        .saturating_add(quoted_len(&node.id.0))
        .saturating_add(NAME_BYTES_MAX);
    if let Some(label) = &node.label {
        bytes = bytes
            .saturating_add(LABEL_KEY_BYTES)
            .saturating_add(quoted_len(label));
    }
    if let Some(value) = &node.value {
        bytes = bytes
            .saturating_add(VALUE_KEY_BYTES)
            .saturating_add(quoted_len(value));
    }
    if !node.actions.is_empty() {
        bytes = bytes
            .saturating_add(ACTIONS_FRAME_BYTES)
            .saturating_add(node.actions.len() - 1);
        for action in &node.actions {
            bytes = bytes.saturating_add(match action {
                Action::Custom(name) => CUSTOM_FRAME_BYTES.saturating_add(quoted_len(name)),
                _ => NAME_BYTES_MAX,
            });
        }
    }
    bytes
}

/// What a root that did not fit holds, read once before anything is cut.
struct Survey {
    /// Escaped lengths of the values over [`STRING_FLOOR`], the only ones a
    /// cut can touch.
    values: Vec<usize>,
    /// Escaped lengths of the labels over [`STRING_FLOOR`].
    labels: Vec<usize>,
    /// The node holding the longest label or value.
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

/// One node of a tree flattened in pre-order, as the cap search needs it.
///
/// Pre-order puts every node after its parent and a subtree's nodes after
/// its root, which is what lets [`size_at_cap`] add a tree up in one
/// backwards pass.
struct Slot {
    /// Index of the parent slot. Unused on the root, slot 0.
    parent: usize,
    /// Position among its parent's children.
    ordinal: usize,
    /// Exact JSON length without its children; see [`measure_own`].
    own: usize,
    /// How many children it has.
    children: usize,
    /// How many of its own strings were cut to the floor.
    trimmed: usize,
}

/// Drop the trailing children of the widest nodes until the tree fits, after
/// cutting every string to the floor. Returns how many published strings were
/// cut and how many nodes were dropped, or `None` when not even the root
/// alone fits or a node could not be measured.
///
/// Dropping one trailing child at a time from whichever node has the most
/// children comes to the same thing as one cap on how many children any node
/// keeps, give or take one child on nodes that tie at the end: every node
/// wider than the cap loses its tail, every narrower node keeps everything,
/// and the cap is the widest that fits. Searching for the cap adds the tree
/// up once per halving, a few dozen times at most, instead of once per
/// dropped node, which for a list of fifty thousand rows is the difference
/// that matters. It is also what keeps a short sibling of a long list: a
/// status bar or a dialog beside a runaway list is one of a handful of the
/// root's children, well inside any cap the list's rows leave room for.
fn drop_to_fit(root: &mut Node, budget: usize) -> Option<(usize, usize)> {
    let slots = flatten_to_floor(root)?;
    let widest = slots.iter().map(|slot| slot.children).max().unwrap_or(0);
    let mut scratch = vec![0usize; slots.len()];
    let mut size = |cap| size_at_cap(&slots, cap, &mut scratch);

    // Invariant: a cap of `fits` fits and one of `over` does not.
    if size(0) > budget {
        return None;
    }
    let mut fits = 0;
    if size(widest) <= budget {
        fits = widest;
    } else {
        let mut over = widest;
        while over - fits > 1 {
            let mid = fits + (over - fits) / 2;
            if size(mid) <= budget {
                fits = mid;
            } else {
                over = mid;
            }
        }
    }
    cap_children(root, fits);

    let mut kept: Vec<bool> = Vec::with_capacity(slots.len());
    let (mut strings, mut dropped) = (0usize, 0usize);
    for (index, slot) in slots.iter().enumerate() {
        let is_kept =
            index == 0 || (slot.ordinal < fits && kept.get(slot.parent).copied().unwrap_or(false));
        kept.push(is_kept);
        if is_kept {
            strings = strings.saturating_add(slot.trimmed);
        } else {
            dropped = dropped.saturating_add(1);
        }
    }
    Some((strings, dropped))
}

/// Cut every string in the tree to the floor and flatten it into
/// [`Slot`]s, or `None` if a node could not be measured.
fn flatten_to_floor(root: &mut Node) -> Option<Vec<Slot>> {
    let mut slots = Vec::new();
    let mut pending: Vec<(&mut Node, usize, usize)> = vec![(root, 0, 0)];
    while let Some((node, parent, ordinal)) = pending.pop() {
        let index = slots.len();
        let trimmed = trim_to_floor(node);
        let own = measure_own(node)?;
        slots.push(Slot {
            parent,
            ordinal,
            own,
            children: node.children.len(),
            trimmed,
        });
        // Reversed so the first child is popped next, keeping the slots in
        // pre-order.
        pending.extend(
            node.children
                .iter_mut()
                .enumerate()
                .rev()
                .map(|(ordinal, child)| (child, index, ordinal)),
        );
    }
    Some(slots)
}

/// Exact JSON length of the flattened tree if no node kept more than `cap`
/// children. `kept_bytes` is scratch space, one entry per slot.
///
/// Backwards over pre-order, so every node is reached after all of its
/// descendants: by then the children it keeps have added themselves to its
/// entry in `kept_bytes`, and it adds itself to its parent's if its parent
/// keeps it. A node its parent drops adds nothing, and nor, through it, does
/// anything under it.
fn size_at_cap(slots: &[Slot], cap: usize, kept_bytes: &mut [usize]) -> usize {
    kept_bytes.fill(0);
    for (index, slot) in slots.iter().enumerate().rev() {
        let kept = slot.children.min(cap);
        let mut bytes = slot.own;
        if kept > 0 {
            bytes = bytes
                .saturating_add(CHILDREN_FRAME_BYTES)
                .saturating_add(kept - 1)
                .saturating_add(kept_bytes.get(index).copied().unwrap_or(0));
        }
        if index == 0 {
            return bytes;
        }
        if slot.ordinal < cap
            && let Some(parent) = kept_bytes.get_mut(slot.parent)
        {
            *parent = parent.saturating_add(bytes);
        }
    }
    0
}

/// Keep at most `cap` children on every node of the tree.
fn cap_children(root: &mut Node, cap: usize) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        node.children.truncate(cap);
        pending.extend(node.children.iter_mut());
    }
}

/// The last resort: the root with no children, its own label and value cut
/// to the floor.
///
/// Always fits for the root the layer builds, whose id is `app` and which
/// carries no actions: what is left is a few hundred bytes. Its actions and
/// id are kept as they are, like everywhere else.
fn bare_root(root: &mut Node, survey: &Survey, before: usize) -> Bounded {
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

    #[test]
    fn every_role_and_built_in_action_fits_the_name_allowance() {
        for role in ROLES {
            let len = serde_json::to_vec(&role).unwrap().len();
            assert!(len <= NAME_BYTES_MAX, "{role:?} is {len} bytes");
        }
        for action in built_in_actions() {
            let len = serde_json::to_vec(&action).unwrap().len();
            assert!(len <= NAME_BYTES_MAX, "{action:?} is {len} bytes");
        }
    }

    /// The fast path skips the exact measure whenever the estimate fits, so
    /// an estimate that ever comes in under the real length lets an oversized
    /// line through. Checked node by node as well as for the whole tree,
    /// because an undercount in one node can hide behind an allowance in
    /// another.
    #[test]
    fn the_estimate_never_undercounts() {
        let tree = varied_tree();
        assert!(estimate(&tree, usize::MAX) >= json_len(&tree));

        let mut pending = vec![&tree];
        while let Some(node) = pending.pop() {
            let estimated = estimate(node, usize::MAX);
            let exact = json_len(node);
            assert!(estimated >= exact, "{}: {estimated} < {exact}", node.id.0);
            pending.extend(&node.children);
        }

        // Each string the estimate counts exactly, it counts exactly.
        for text in [ESCAPE_HEAVY, "", "plain", "\u{2026}", "\x1f\x20"] {
            assert_eq!(
                quoted_len(text),
                serde_json::to_vec(text).unwrap().len(),
                "{text:?}"
            );
        }
    }

    /// The drop stage plans with the children framing modelled rather than
    /// measured, so the model has to be exact at full width, where nothing
    /// is dropped and the real serialization is there to compare against.
    #[test]
    fn the_size_model_matches_serde_at_full_width() {
        let mut tree = varied_tree();
        // Nothing in the varied tree is over the floor, so flattening trims
        // nothing and the tree it measures is the one serialized.
        let before = tree.clone();
        let slots = flatten_to_floor(&mut tree).unwrap();
        assert_eq!(tree, before);

        let mut scratch = vec![0; slots.len()];
        assert_eq!(
            size_at_cap(&slots, usize::MAX, &mut scratch),
            json_len(&tree)
        );
        // And at a cap, against the tree actually capped.
        for cap in [0, 1, 2, 5] {
            let mut capped = tree.clone();
            cap_children(&mut capped, cap);
            assert_eq!(
                size_at_cap(&slots, cap, &mut scratch),
                json_len(&capped),
                "cap {cap}"
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
        let rows = (0..50_000).map(|n| {
            Node::new(format!("row-{n:05}"), Role::ListItem)
                .label(format!("Task {n:05}"))
                .action(Action::Select)
        });
        let mut root = Node::new("app", Role::App).label("huge").children([
            Node::new("list", Role::List).label("Tasks").children(rows),
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
        // The rows kept are the first ones, in order.
        for (n, row) in list.children.iter().enumerate() {
            assert_eq!(row.id.0, format!("row-{n:05}"));
        }
        // And the cap is the widest that fits: one more row would not.
        let mut one_more = root.clone();
        one_more.children[0].children.push(
            Node::new(format!("row-{kept:05}"), Role::ListItem)
                .label(format!("Task {kept:05}"))
                .action(Action::Select),
        );
        assert!(json_len(&one_more) > ROOT_BUDGET);
    }

    #[test]
    fn the_bare_root_fits_when_nothing_else_does() {
        // A child whose id alone is over the budget. Ids are never cut, so
        // the only way to fit is to drop it, and with it every child.
        let mut root = Node::new("app", Role::App)
            .label("r".repeat(10_000))
            .children([
                Node::new("i".repeat(5000), Role::Text),
                Node::new("second", Role::Text),
            ]);
        let bounded = bound_root(&mut root, 2000).expect("over budget");
        assert!(root.children.is_empty());
        assert_eq!(bounded.nodes_dropped, 2);
        assert_eq!(bounded.strings_trimmed, 1, "the root's label");
        assert!(json_len(&root) <= 2000);
        assert_eq!(root.id.0, "app");

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
}
