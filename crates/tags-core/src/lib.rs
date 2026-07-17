//! `tags-core` — the typed tag layer between the raw poller cache and the
//! OPC UA / MQTT publishers (design PLAN.md §4, phase 2).
//!
//! - [`value`] — [`TypedValue`]: decoded engineering values;
//! - [`decode`] — raw registers/bits -> typed (and the inverse encode for
//!   the phase-4 write path): word/byte order, BCD, ASCII, bitfields.

pub mod decode;
pub mod engine;
pub mod formula;
pub mod persist;
pub mod store;
pub mod value;

pub use decode::{decode, encode, DecodeError, DecodeMeta};
pub use engine::{EngineError, EngineHandle, TagEngine};
pub use formula::{check_config_formulas, Formula, FormulaError, TagLookup, Transform};
pub use persist::{Persist, PersistError, StoredKind, StoredValue};
pub use store::{TypedBatch, TypedReader, TypedSnapshot, TypedStore, TYPED_CHANGE_CAPACITY};
pub use value::TypedValue;
