//! `pipeline`: JSONL text corpus indexer for training tiny language models.
//!
//! The binary is a three-line shim over [`run`]; everything is in the library
//! so tests can drive every command with captured output.

pub mod cli;
pub mod count;
pub mod error;
pub mod expr;
pub mod features;
pub mod hash;
pub mod import;
pub mod jsonl;
pub mod meta;
pub mod stats;
pub mod store;
pub mod util;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command, CountArgs, ImportArgs, StatsArgs};
use error::{Error, Result};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Entry point used by `main`: stdout/stderr, process exit code.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let code = run_with(args, &mut std::io::stdout(), &mut std::io::stderr());
    ExitCode::from(code)
}

/// Run the CLI with explicit output streams. Returns the exit code.
pub fn run_with<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            if e.use_stderr() {
                let _ = write!(err, "{e}");
                return 2;
            }
            let _ = write!(out, "{e}");
            return 0;
        }
    };
    match run_cli(cli, out, err) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "Error: {e}");
            e.exit_code()
        }
    }
}

pub fn run_cli(cli: Cli, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    match cli.cmd {
        Command::Import(a) => cmd_import(a, out, err),
        Command::Stats(a) => cmd_stats(a, out),
        Command::Count(a) => cmd_count(a, out),
    }
}

fn threads(requested: Option<usize>) -> usize {
    requested.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get())).max(1)
}

fn cmd_import(a: ImportArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let mut opts = import::ImportOptions::new(a.store.db, a.inputs);
    opts.backend = a.store.backend;
    opts.source = a.source;
    opts.source_key = a.source_key;
    opts.text_key = a.text_key;
    opts.extra = a.extra;
    opts.max_len = a.max_len;
    opts.workers = threads(a.workers);
    opts.offset = a.offset;
    opts.limit = a.limit;
    opts.rejects = a.rejects;
    opts.on_error = a.on_error;
    opts.nocow = a.nocow;
    opts.chunk_bytes = a.chunk_mb.max(1) << 20;
    opts.batch_size = a.batch_size.max(1);
    opts.dry_run = a.dry_run;
    if opts.max_len <= features::MIN_LEN {
        return Err(Error::Usage(format!("--max-len must be greater than {}", features::MIN_LEN)));
    }
    let report = import::import(&opts, err)?;
    write!(out, "{}", report.render())?;
    Ok(())
}

fn compile_opt(expr: Option<&str>) -> Result<Option<expr::Compiled>> {
    expr.map(expr::compile).transpose()
}

fn cmd_stats(a: StatsArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, a.store.backend, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let filter = compile_opt(a.expr.as_deref())?;
    let cfg = stats::ScanConfig { filter: filter.as_ref(), threads: threads(a.threads), limit: a.limit };
    let rep = stats::collect(store.as_ref(), &sources, &cfg)?;
    write!(out, "{}", stats::render(&rep, &sources, a.expr.as_deref()))?;
    Ok(())
}

fn cmd_count(a: CountArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, a.store.backend, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let filter = compile_opt(a.expr.as_deref())?;
    let cfg = stats::ScanConfig { filter: filter.as_ref(), threads: threads(a.threads), limit: None };
    let rep = count::count(store.as_ref(), &sources, &cfg)?;
    write!(out, "{}", rep.render())?;
    Ok(())
}
