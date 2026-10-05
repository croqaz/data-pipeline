//! The filter expression language shared by stats, count, export and delete.
//!
//! Syntax is deliberately close to JavaScript:
//!
//! ```text
//! words >= 50 && vowel > 95 && source == "gutenberg"
//! uniqWords / words < 0.2 || sentences < 2
//! extra.year < 1850 && has(dictHit) && text.contains("Lorem")
//! source in ["EEBO", "ECCO"]
//! ```
//!
//! Semantics that matter for a growing schema: a feature the record does not
//! have is `null`; any comparison or arithmetic with `null` is `null`; `&&`
//! and `||` treat `null` as false; a `null` result means "does not match".
//! Unknown names are rejected at parse time.

pub mod compile;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod value;

pub use compile::{Compiled, Slot, compile};
pub use eval::{RecordRow, Row};
pub use value::Value;
