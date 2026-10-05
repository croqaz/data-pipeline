//! JSONL input: opening files (plain, gzip, zstd) and cutting them into
//! chunks of whole lines for the worker threads.

pub mod reader;

pub use reader::{Chunk, Chunker, open_input};
