//! Integer codecs used by the posting lists.
//!
//! Two schemes live side by side. Full blocks of 128 doc-id deltas / term
//! frequencies use frame-of-reference bit packing (`bitpack`), which decodes
//! with a tight loop and no per-value branching on the value size. The ragged
//! tail of a list and the position streams use variable-byte (`vbyte`), where
//! block framing would waste more than it saves.

pub mod bitpack;
pub mod crc32;
pub mod vbyte;
