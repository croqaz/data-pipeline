//! `pipeline`: JSONL text corpus indexer for training tiny language models.
//!
//! The binary is a three-line shim over [`run`]; everything is in the library
//! so tests can drive every command with captured output.

pub mod check;
pub mod cli;
pub mod count;
pub mod error;
pub mod export;
pub mod expr;
pub mod features;
pub mod hash;
pub mod import;
pub mod info;
pub mod jsonl;
pub mod meta;
pub mod show;
pub mod stats;
pub mod store;
pub mod util;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use cli::{CheckArgs, Cli, Command, CompactArgs, CountArgs, ExportArgs, ImportArgs, InfoArgs, ShowArgs, StatsArgs};
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
        Command::Export(a) => cmd_export(a, out, err),
        Command::Check(a) => cmd_check(a, out),
        Command::Info(a) => cmd_info(a, out),
        Command::Show(a) => cmd_show(a, out),
        Command::Compact(a) => cmd_compact(a, out),
    }
}

fn threads(requested: Option<usize>) -> usize {
    requested.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get())).max(1)
}

fn cmd_import(a: ImportArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let mut opts = import::ImportOptions::new(a.store.db, a.inputs);
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
    let store = store::open(&a.store.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let filter = compile_opt(a.expr.as_deref())?;
    let cfg = stats::ScanConfig { filter: filter.as_ref(), threads: threads(a.threads), limit: a.limit };
    let rep = stats::collect(store.as_ref(), &sources, &cfg)?;
    write!(out, "{}", stats::render(&rep, &sources, a.expr.as_deref()))?;
    Ok(())
}

fn cmd_count(a: CountArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let filter = compile_opt(a.expr.as_deref())?;
    let cfg = stats::ScanConfig { filter: filter.as_ref(), threads: threads(a.threads), limit: None };
    let rep = count::count(store.as_ref(), &sources, &cfg)?;
    write!(out, "{}", rep.render())?;
    Ok(())
}

fn cmd_export(a: ExportArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let mut opts = export::ExportOptions::new(a.store.db, a.out);
    opts.expr = a.expr;
    opts.fields = a.fields;
    opts.limit = a.limit;
    opts.sample = a.sample;
    opts.seed = a.seed;
    opts.top = a.top;
    opts.header = match (a.header, a.header_file) {
        (Some(h), _) => Some(export::unescape(&h)),
        (None, Some(path)) => {
            let raw = std::fs::read_to_string(&path).map_err(|e| Error::Usage(format!("--header-file {}: {e}", path.display())))?;
            let trimmed = raw.strip_suffix('\n').unwrap_or(&raw);
            Some(trimmed.strip_suffix('\r').unwrap_or(trimmed).to_string())
        }
        (None, None) => None,
    };
    opts.header_sep = export::unescape(&a.header_sep);
    opts.shard = a.shard;
    opts.zstd = a.zstd;
    opts.zstd_level = a.zstd_level;
    opts.threads = threads(a.threads);
    opts.verify = a.verify;
    opts.force = a.force;
    let to_stdout = opts.to_stdout();
    let report = export::export(&opts, out, err)?;
    let rendered = report.render();
    if to_stdout {
        write!(err, "{rendered}")?;
    } else {
        write!(out, "{rendered}")?;
    }
    Ok(())
}

fn cmd_check(a: CheckArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let opts = check::CheckOptions { deep: a.deep, threads: threads(a.threads), examples: a.examples };
    let rep = check::check(store.as_ref(), &sources, &opts)?;
    write!(out, "{}", rep.render())?;
    if rep.problems() > 0 {
        return Err(check::failure(&rep));
    }
    Ok(())
}

fn cmd_info(a: InfoArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let rep = info::info(store.as_ref(), &a.store.db)?;
    write!(out, "{}", rep.render())?;
    Ok(())
}

fn cmd_compact(a: CompactArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let rep = info::compact(store.as_ref())?;
    write!(out, "{}", rep.render())?;
    Ok(())
}

fn cmd_show(a: ShowArgs, out: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    for (i, id) in a.ids.iter().enumerate() {
        let key = show::resolve_id(store.as_ref(), id)?;
        let doc = show::fetch(store.as_ref(), &key)?;
        if a.json {
            write!(out, "{}", show::render_json(&doc, &sources)?)?;
        } else {
            if i > 0 {
                writeln!(out)?;
            }
            write!(out, "{}", show::render(&doc, &sources, a.head))?;
        }
    }
    Ok(())
}
