//! Command-line definitions (clap derive).

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::import::OnError;

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
    /// Write matching documents as JSONL (the only way out).
    Export(ExportArgs),
    /// Verify that the store is internally consistent.
    Check(CheckArgs),
    /// Cheap facts about a store: size, count, sources, engine details.
    Info(InfoArgs),
    /// Print one or more documents by id (or unambiguous id prefix).
    Show(ShowArgs),
    /// Rewrite every keyspace into its most compact form.
    Compact(CompactArgs),
    /// Remove documents by id or by expression; asks before doing it.
    Delete(DeleteArgs),
    /// Browse the store in a web browser: random, next, prev, query, count. Read-only.
    Explore(ExploreArgs),
}

#[derive(Args, Debug, Clone)]
pub struct StoreArgs {
    /// Store directory.
    #[arg(short, long)]
    pub db: PathBuf,
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

#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Only documents matching this expression.
    pub expr: Option<String>,
    /// Output file (`-` for stdout). A `.zst` suffix turns on zstd.
    #[arg(short, long)]
    pub out: PathBuf,
    /// Comma-separated fields: id, source, text, features, <feature>, extra, extra.*, extra.<key>.
    #[arg(short, long, value_delimiter = ',')]
    pub fields: Vec<String>,
    /// Export at most N documents (first N in key order).
    #[arg(short, long)]
    pub limit: Option<u64>,
    /// Keep this fraction of the matches (0 < f <= 1), chosen per document.
    #[arg(long)]
    pub sample: Option<f64>,
    /// Seed for --sample.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Keep the N highest-scoring matches, best first.
    #[arg(long)]
    pub top: Option<u64>,
    /// Header template with {placeholders}, prepended to text (`\n` escapes allowed).
    #[arg(long, allow_hyphen_values = true)]
    pub header: Option<String>,
    /// Read the header template from a file.
    #[arg(long, conflicts_with = "header")]
    pub header_file: Option<PathBuf>,
    /// Separator between the header and the text.
    #[arg(long, default_value = "\\n\\n", allow_hyphen_values = true)]
    pub header_sep: String,
    /// Split the output into files of N documents (`name-0001.jsonl`, ...).
    #[arg(long)]
    pub shard: Option<u64>,
    /// Compress with zstd (multi-threaded).
    #[arg(long)]
    pub zstd: bool,
    /// zstd compression level (1-22).
    #[arg(long, default_value_t = 3)]
    pub zstd_level: i32,
    /// Fetch threads (default: all logical CPUs).
    #[arg(short, long)]
    pub threads: Option<usize>,
    /// Re-read the output and check that every line parses.
    #[arg(long)]
    pub verify: bool,
    /// Overwrite existing output files.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Also re-read every text: UTF-8, key hash and feature recomputation.
    #[arg(long)]
    pub deep: bool,
    /// Scan threads (default: all logical CPUs).
    #[arg(short, long)]
    pub threads: Option<usize>,
    /// Example ids to list per problem kind.
    #[arg(long, default_value_t = 5)]
    pub examples: usize,
}

#[derive(Args, Debug, Clone)]
pub struct InfoArgs {
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Document ids: 32 hex characters, or an even-length prefix of at least 4.
    #[arg(required = true)]
    pub ids: Vec<String>,
    /// Print the export JSON line instead of the readable layout.
    #[arg(long)]
    pub json: bool,
    /// Show only the first N characters of the text.
    #[arg(long)]
    pub head: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub struct CompactArgs {
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Args, Debug, Clone)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Delete every document matching this expression.
    pub expr: Option<String>,
    /// Delete this document (32 hex characters or an unambiguous prefix); repeatable.
    #[arg(long = "id", value_name = "ID")]
    pub ids: Vec<String>,
    /// Print the ids that would be deleted and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Delete at most N documents (the first N in key order).
    #[arg(short, long)]
    pub limit: Option<u64>,
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Export the selected documents (all fields) to this JSONL file first.
    #[arg(long)]
    pub backup: Option<PathBuf>,
    /// Overwrite an existing backup file.
    #[arg(long)]
    pub force: bool,
    /// Scan threads (default: all logical CPUs).
    #[arg(short, long)]
    pub threads: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub struct ExploreArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// TCP port to listen on (0 picks a free one).
    #[arg(short, long, default_value_t = crate::explore::DEFAULT_PORT)]
    pub port: u16,
    /// Address to bind; keep the default unless you want other machines in.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Scan threads for count (default: all logical CPUs).
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
        let cli = Cli::try_parse_from(["pipeline", "stats", "--db", "x", "words > 1"]).unwrap();
        let Command::Stats(a) = cli.cmd else { panic!() };
        assert_eq!(a.expr.as_deref(), Some("words > 1"));
        assert!(
            Cli::try_parse_from(["pipeline", "stats", "--db", "x", "--backend", "fjall"]).is_err(),
            "the engine is not a choice any more"
        );
        let cli = Cli::try_parse_from(["pipeline", "count", "--db", "x", "-t", "2"]).unwrap();
        let Command::Count(a) = cli.cmd else { panic!() };
        assert_eq!(a.threads, Some(2));
        assert!(a.expr.is_none());
        assert!(Cli::try_parse_from(["pipeline", "import", "--db", "x"]).is_err());
        assert!(Cli::try_parse_from(["pipeline", "nope"]).is_err());
        let cli = Cli::try_parse_from([
            "pipeline",
            "export",
            "--db",
            "x",
            "-o",
            "o.jsonl",
            "-f",
            "id,text",
            "--sample",
            "0.5",
            "--header",
            "T: {extra.t}",
            "words > 1",
        ])
        .unwrap();
        let Command::Export(a) = cli.cmd else { panic!() };
        assert_eq!(a.fields, vec!["id", "text"]);
        assert_eq!(a.sample, Some(0.5));
        assert_eq!(a.header_sep, "\\n\\n");
        assert_eq!(a.header.as_deref(), Some("T: {extra.t}"));
        assert_eq!(a.expr.as_deref(), Some("words > 1"));
        assert_eq!(a.zstd_level, 3);
        assert!(Cli::try_parse_from(["pipeline", "export", "--db", "x"]).is_err(), "--out is required");
        assert!(Cli::try_parse_from(["pipeline", "export", "--db", "x", "-o", "o", "--header", "a", "--header-file", "f"]).is_err());
        let cli = Cli::try_parse_from([
            "pipeline",
            "export",
            "--db",
            "x",
            "-o",
            "o",
            "--header",
            "---\\ntitle = {extra.title}\\n---",
            "--header-sep",
            "-",
        ])
        .unwrap();
        let Command::Export(a) = cli.cmd else { panic!() };
        assert_eq!(a.header.as_deref(), Some("---\\ntitle = {extra.title}\\n---"), "a template may start with a dash");
        assert_eq!(a.header_sep, "-");
        let cli = Cli::try_parse_from(["pipeline", "check", "--db", "x", "--deep", "--examples", "2"]).unwrap();
        let Command::Check(a) = cli.cmd else { panic!() };
        assert!(a.deep && a.examples == 2 && a.threads.is_none());
        let cli = Cli::try_parse_from(["pipeline", "show", "--db", "x", "abcd", "ef01", "--head", "40"]).unwrap();
        let Command::Show(a) = cli.cmd else { panic!() };
        assert_eq!(a.ids, vec!["abcd", "ef01"]);
        assert_eq!(a.head, Some(40));
        assert!(!a.json);
        assert!(Cli::try_parse_from(["pipeline", "show", "--db", "x"]).is_err(), "an id is required");
        assert!(matches!(Cli::try_parse_from(["pipeline", "info", "--db", "x"]).unwrap().cmd, Command::Info(_)));
        assert!(matches!(Cli::try_parse_from(["pipeline", "compact", "--db", "x"]).unwrap().cmd, Command::Compact(_)));
        let cli =
            Cli::try_parse_from(["pipeline", "delete", "--db", "x", "--id", "abcd", "--id", "ef01", "-y", "--backup", "b.jsonl"]).unwrap();
        let Command::Delete(a) = cli.cmd else { panic!() };
        assert_eq!(a.ids, vec!["abcd", "ef01"]);
        assert!(a.yes && !a.dry_run && a.expr.is_none());
        assert_eq!(a.backup.as_deref(), Some(Path::new("b.jsonl")));
        let cli = Cli::try_parse_from(["pipeline", "delete", "--db", "x", "--dry-run", "--limit", "3", "words < 5"]).unwrap();
        let Command::Delete(a) = cli.cmd else { panic!() };
        assert_eq!((a.dry_run, a.limit, a.expr.as_deref()), (true, Some(3), Some("words < 5")));
        let cli = Cli::try_parse_from(["pipeline", "explore", "--db", "x"]).unwrap();
        let Command::Explore(a) = cli.cmd else { panic!() };
        assert_eq!((a.port, a.bind.as_str(), a.threads), (8080, "127.0.0.1", None));
        let cli = Cli::try_parse_from(["pipeline", "explore", "--db", "x", "-p", "0", "--bind", "0.0.0.0", "-t", "2"]).unwrap();
        let Command::Explore(a) = cli.cmd else { panic!() };
        assert_eq!((a.port, a.bind.as_str(), a.threads), (0, "0.0.0.0", Some(2)));
        assert!(Cli::try_parse_from(["pipeline", "explore", "--db", "x", "-p", "70000"]).is_err(), "a port fits in 16 bits");
    }
}
