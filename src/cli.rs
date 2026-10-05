//! Command-line definitions (clap derive).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::import::OnError;
use crate::store::Backend;

#[derive(Parser, Debug)]
#[command(name = "pipeline", version, about = "JSONL text corpus indexer for training tiny language models")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Index JSONL files into a store (the only way in).
    Import(ImportArgs),
    /// Per-feature distribution table over the store.
    Stats(StatsArgs),
    /// Count documents matching an expression.
    Count(CountArgs),
}

#[derive(Args, Debug, Clone)]
pub struct StoreArgs {
    /// Store directory.
    #[arg(short, long)]
    pub db: PathBuf,
    /// Storage engine; required only when creating a store, must match afterwards.
    #[arg(long, value_enum)]
    pub backend: Option<Backend>,
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Source label for documents whose record has no source key.
    #[arg(short, long, default_value = "cli")]
    pub source: String,
    /// JSON key that overrides the source label per record ("" to disable).
    #[arg(long, default_value = "source")]
    pub source_key: String,
    /// JSON key holding the document text.
    #[arg(short = 'k', long, default_value = "text")]
    pub text_key: String,
    /// Comma-separated JSON keys to keep as metadata.
    #[arg(short, long, value_delimiter = ',')]
    pub extra: Vec<String>,
    /// Drop documents longer than this many characters.
    #[arg(short, long, default_value_t = crate::features::DEFAULT_MAX_LEN)]
    pub max_len: u32,
    /// Feature-extraction threads (default: all logical CPUs).
    #[arg(short, long)]
    pub workers: Option<usize>,
    /// Skip the first N lines (counted across files, in order).
    #[arg(short, long, default_value_t = 0)]
    pub offset: u64,
    /// Stop after N lines (after the offset).
    #[arg(short, long)]
    pub limit: Option<u64>,
    /// Write every dropped line with its reason to this JSONL file.
    #[arg(long)]
    pub rejects: Option<PathBuf>,
    /// What to do with lines that are not usable JSON records.
    #[arg(long, value_enum, default_value_t = OnError::Skip)]
    pub on_error: OnError,
    /// Set the btrfs nodatacow attribute on a new store directory.
    #[arg(long)]
    pub nocow: bool,
    /// Reader chunk size in MiB.
    #[arg(long, default_value_t = 2)]
    pub chunk_mb: usize,
    /// Records per atomic store write.
    #[arg(long, default_value_t = 2000)]
    pub batch_size: usize,
    /// Parse and count everything, write nothing, never open the store.
    #[arg(long)]
    pub dry_run: bool,
    /// JSONL files (.jsonl, .jsonl.gz, .jsonl.zst).
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct StatsArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Only documents matching this expression.
    pub expr: Option<String>,
    /// Scan at most N documents (single-threaded).
    #[arg(short, long)]
    pub limit: Option<u64>,
    /// Scan threads (default: all logical CPUs).
    #[arg(short, long)]
    pub threads: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub struct CountArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Expression to count (default: every document).
    pub expr: Option<String>,
    /// Scan threads (default: all logical CPUs).
    #[arg(short, long)]
    pub threads: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_subcommand() {
        let cli = Cli::try_parse_from(["pipeline", "import", "--db", "x", "-e", "a,b", "--limit", "5", "f.jsonl"]).unwrap();
        let Command::Import(a) = cli.cmd else { panic!() };
        assert_eq!(a.extra, vec!["a", "b"]);
        assert_eq!(a.limit, Some(5));
        assert_eq!(a.store.db, PathBuf::from("x"));
        assert_eq!(a.on_error, OnError::Skip);
        let cli = Cli::try_parse_from(["pipeline", "stats", "--db", "x", "--backend", "rocksdb", "words > 1"]).unwrap();
        let Command::Stats(a) = cli.cmd else { panic!() };
        assert_eq!(a.store.backend, Some(Backend::Rocksdb));
        assert_eq!(a.expr.as_deref(), Some("words > 1"));
        let cli = Cli::try_parse_from(["pipeline", "count", "--db", "x", "-t", "2"]).unwrap();
        let Command::Count(a) = cli.cmd else { panic!() };
        assert_eq!(a.threads, Some(2));
        assert!(a.expr.is_none());
        assert!(Cli::try_parse_from(["pipeline", "import", "--db", "x"]).is_err());
        assert!(Cli::try_parse_from(["pipeline", "nope"]).is_err());
    }
}
