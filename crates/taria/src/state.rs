//! The value types behind a [`Node`](crate::Node)'s typed state, and the
//! lenient reading every state field gets.
//!
//! The state fields were added in 0.4.0, inside the frozen version 1, so a
//! peer built before them must keep parsing every node, and a peer built
//! after them must not let one of them cost a whole snapshot. A field that
//! fails its parse fails the node, the node fails the snapshot, and the
//! reader skips the line, which leaves an agent on a stale tree with nothing
//! saying why. So a state field in the wrong type or shape, `"selected":"yes"`,
//! `"checked":2`, a negative `row`, a `NaN` spelled as a string, reads as not
//! stated, exactly as if it were absent, and the rest of the node is kept.

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Whether a checkable node is checked: a checkbox, a done task, a toggled
/// menu entry.
///
/// On the wire `true`, `false` or `"mixed"`, the third for a parent whose
/// children are partly checked. `#[non_exhaustive]` so a state added later is
/// not a break for a caller that matches on this; a reader that does not know
/// a value reads it as not stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Checked {
    /// Checked: `true` on the wire.
    True,
    /// Not checked: `false` on the wire.
    False,
    /// Partly checked, for a node whose children disagree: `"mixed"`.
    Mixed,
}

impl From<bool> for Checked {
    fn from(checked: bool) -> Self {
        if checked { Self::True } else { Self::False }
    }
}

impl Serialize for Checked {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::True => serializer.serialize_bool(true),
            Self::False => serializer.serialize_bool(false),
            Self::Mixed => serializer.serialize_str("mixed"),
        }
    }
}

/// Strict: anything but `true`, `false` or `"mixed"` is an error. A
/// [`Node`](crate::Node) does not read its `checked` field this way; it reads
/// a wrong shape as not stated.
impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Shape::read(deserializer)?
            .checked()
            .ok_or_else(|| de::Error::custom("expected true, false or \"mixed\""))
    }
}

/// A finite number: a progress bar's value, a slider's bounds.
///
/// A newtype over `f64` that cannot hold `NaN` or an infinity, which is what
/// lets a [`Node`](crate::Node) keep `Eq`. An adapter compares each frame's
/// tree with the last one to skip publishing an unchanged frame, and a `NaN`
/// is unequal to itself, so a tree holding one would never compare equal and
/// would be published on every frame. JSON cannot spell either value anyway,
/// so one that reached the wire would fail the line.
///
/// `-0.0` is stored as `0.0`, so equal numbers hash alike. Serialized as a
/// plain JSON number, and as an integer when it is one, so `42.0` goes out as
/// `42`.
#[derive(Debug, Clone, Copy)]
pub struct Number(f64);

impl Number {
    /// `value` as a `Number`, or `None` when it is `NaN` or infinite.
    pub fn new(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        // Adding positive zero turns -0.0 into 0.0 and leaves every other
        // finite value as it is.
        Some(Self(value + 0.0))
    }

    /// The number as an `f64`, always finite.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl From<Number> for f64 {
    fn from(number: Number) -> Self {
        number.0
    }
}

/// By bit pattern, which for a value that is never `NaN` and never `-0.0` is
/// numeric equality, and the one [`Hash`] agrees with.
impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Number {}

impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

/// Integers from here to its negation are exactly representable as `f64`,
/// so one in this range can be written as an integer without changing it.
const EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

impl Serialize for Number {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.fract() == 0.0 && value.abs() <= EXACT_INTEGER {
            // In range and whole, so the cast is exact.
            serializer.serialize_i64(value as i64)
        } else {
            serializer.serialize_f64(value)
        }
    }
}

/// Strict: anything but a JSON number is an error. A
/// [`Node`](crate::Node) reads its number fields leniently instead.
impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Shape::read(deserializer)?
            .number()
            .ok_or_else(|| de::Error::custom("expected a finite number"))
    }
}

/// What one state field held, read without failing on any shape.
///
/// Only the shapes some field accepts are told apart; everything else is
/// [`Other`](Self::Other), consumed whole so the parser can go on to the next
/// field.
enum Shape {
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    Text(String),
    Other,
}

impl Shape {
    /// Read one value of any shape. The only error is the one the
    /// underlying parser reports for input that is not well formed at all,
    /// which fails the line whatever field it is in.
    fn read<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ShapeVisitor)
    }

    fn bool(self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(value),
            _ => None,
        }
    }

    fn checked(self) -> Option<Checked> {
        match self {
            Self::Bool(value) => Some(Checked::from(value)),
            Self::Text(text) if text == "mixed" => Some(Checked::Mixed),
            _ => None,
        }
    }

    fn number(self) -> Option<Number> {
        match self {
            // Through `f64`, as JSON numbers are: an integer past 2^53 is
            // rounded, never refused.
            Self::Unsigned(value) => Number::new(value as f64),
            Self::Signed(value) => Number::new(value as f64),
            Self::Float(value) => Number::new(value),
            _ => None,
        }
    }

    /// A whole number of at most `u32::MAX` and at least `min`.
    fn whole(self, min: u32) -> Option<u32> {
        let value = match self {
            Self::Unsigned(value) => u32::try_from(value).ok()?,
            Self::Signed(value) => u32::try_from(value).ok()?,
            _ => return None,
        };
        (value >= min).then_some(value)
    }

    fn text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
}

struct ShapeVisitor;

impl<'de> Visitor<'de> for ShapeVisitor {
    type Value = Shape;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Shape, E> {
        Ok(Shape::Bool(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Shape, E> {
        Ok(Shape::Unsigned(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Shape, E> {
        Ok(Shape::Signed(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Shape, E> {
        Ok(Shape::Float(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Shape, E> {
        Ok(Shape::Text(value.to_string()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Shape, E> {
        Ok(Shape::Text(value))
    }

    fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Shape, E> {
        Ok(Shape::Other)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Shape, E> {
        Ok(Shape::Other)
    }

    fn visit_none<E: de::Error>(self) -> Result<Shape, E> {
        Ok(Shape::Other)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Shape, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Shape, D::Error> {
        deserializer.deserialize_any(self)
    }

    /// Drained rather than refused: serde_json rejects a sequence the
    /// visitor left half-read.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Shape, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Shape::Other)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shape, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Shape::Other)
    }
}

/// `selected` and `expanded`: a boolean, or not stated.
pub(crate) fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(Shape::read(d)?.bool())
}

/// `disabled`: only `true` is a statement. `false` reads as not stated,
/// which is what it means, so a parsed node never holds `Some(false)`.
pub(crate) fn lenient_disabled<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(Shape::read(d)?.bool().filter(|disabled| *disabled))
}

/// `checked`: `true`, `false` or `"mixed"`, or not stated.
pub(crate) fn lenient_checked<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Checked>, D::Error> {
    Ok(Shape::read(d)?.checked())
}

/// `value_now`, `value_min`, `value_max`: a finite number, or not stated.
pub(crate) fn lenient_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Number>, D::Error> {
    Ok(Shape::read(d)?.number())
}

/// `pos_in_set`, `row`, `column`: a whole number from 1, or not stated.
pub(crate) fn lenient_position<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Shape::read(d)?.whole(1))
}

/// `set_size`, `row_count`, `column_count`: a whole number from 0, or not
/// stated.
pub(crate) fn lenient_count<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Shape::read(d)?.whole(0))
}

/// `description`: a string, or not stated.
pub(crate) fn lenient_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Shape::read(d)?.text())
}

/// Whether `disabled` stays off the wire: it is sent only as `true`.
pub(crate) fn not_disabled(disabled: &Option<bool>) -> bool {
    *disabled != Some(true)
}

/// A snapshot's `after_input`: an unsigned 64-bit integer, or not stated.
///
/// Read like the node's state, for the same reason: the field was added
/// within version 1, and a reader that failed the line over it would leave
/// the agent on a stale tree over one field. Not stated is what an
/// unstamped peer sends, so the bridge treats the tree as one it cannot
/// order against its inputs, which is the safe reading.
pub(crate) fn lenient_input_id<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<crate::wire::InputId>, D::Error> {
    Ok(match Shape::read(d)? {
        Shape::Unsigned(n) => Some(crate::wire::InputId::new(n)),
        _ => None,
    })
}

/// A hello's `stamps`: only `true` is a statement, and anything else reads
/// as a peer that does not stamp, which is what an older peer is.
pub(crate) fn lenient_flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Shape::read(d)?.bool() == Some(true))
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;

    use super::*;

    fn hash_of(number: Number) -> u64 {
        let mut hasher = DefaultHasher::new();
        number.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn a_number_is_finite_and_negative_zero_is_zero() {
        assert!(Number::new(f64::NAN).is_none());
        assert!(Number::new(f64::INFINITY).is_none());
        assert!(Number::new(f64::NEG_INFINITY).is_none());
        assert_eq!(Number::new(42.5).map(Number::get), Some(42.5));

        let zero = Number::new(0.0).unwrap();
        let negative = Number::new(-0.0).unwrap();
        assert_eq!(zero, negative);
        assert_eq!(hash_of(zero), hash_of(negative));
        assert!(negative.get().is_sign_positive());
        assert_eq!(f64::from(negative), 0.0);
    }

    #[test]
    fn a_whole_number_serializes_as_an_integer() {
        let json = |value: f64| serde_json::to_string(&Number::new(value).unwrap()).unwrap();
        assert_eq!(json(42.0), "42");
        assert_eq!(json(-3.0), "-3");
        assert_eq!(json(-0.0), "0");
        assert_eq!(json(0.25), "0.25");
        assert_eq!(json(EXACT_INTEGER), "9007199254740992");
        // Past 2^53 a whole number may not be the integer it prints as, so
        // it goes out as a float, in serde_json's spelling.
        assert!(json(EXACT_INTEGER * 4.0).contains('e'));
        assert!(json(1e300).contains('e'));
        for value in [42.0, -3.0, 0.25, 1e300, EXACT_INTEGER * 4.0] {
            let back: Number = serde_json::from_str(&json(value)).unwrap();
            assert_eq!(back.get(), value);
        }
    }

    #[test]
    fn checked_spells_true_false_and_mixed() {
        for (checked, json) in [
            (Checked::True, "true"),
            (Checked::False, "false"),
            (Checked::Mixed, r#""mixed""#),
        ] {
            assert_eq!(serde_json::to_string(&checked).unwrap(), json);
            assert_eq!(serde_json::from_str::<Checked>(json).unwrap(), checked);
        }
        assert_eq!(Checked::from(true), Checked::True);
        assert_eq!(Checked::from(false), Checked::False);
    }

    /// Standalone, both types are strict; the leniency belongs to the node.
    #[test]
    fn standalone_the_value_types_refuse_a_wrong_shape() {
        for json in ["2", r#""yes""#, "null", "[]", "{}"] {
            assert!(serde_json::from_str::<Checked>(json).is_err(), "{json}");
        }
        for json in [r#""42""#, "true", "null", "[1]", "{}"] {
            assert!(serde_json::from_str::<Number>(json).is_err(), "{json}");
        }
    }
}
