//! `pipeline`: JSONL text corpus indexer for training tiny language models.
//!
//! The binary is a three-line shim over [`run`]; everything is in the library
//! so tests can drive every command with captured output.

pub mod check;
pub mod cli;
pub mod count;
pub mod delete;
pub mod error;
pub mod explore;
pub mod export;
pub mod expr;
pub mod features;
pub mod hash;
pub mod import;
pub mod info;
pub mod jsonl;
pub mod meta;
pub mod query;
pub mod recompute;
pub mod show;
pub mod stats;
pub mod store;
pub mod tokenizer;
pub mod util;

use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::process::ExitCode;

use clap::Parser;

use cli::{
    CheckArgs, Cli, Command, CompactArgs, CountArgs, DeleteArgs, ExploreArgs, ExportArgs, ImportArgs, InfoArgs, QueryArgs, RecomputeArgs,
    ShowArgs, StatsArgs,
};
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

/// Run the CLI with explicit output streams and stdin for prompts. `err`
/// must be `Send` because import and recompute print progress from their
/// worker threads.
pub fn run_with<I, T>(args: I, out: &mut dyn Write, err: &mut (dyn Write + Send)) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_with_input(args, &mut std::io::stdin().lock(), out, err)
}

/// Run the CLI with explicit streams; `input` answers confirmation prompts.
pub fn run_with_input<I, T>(args: I, input: &mut dyn BufRead, out: &mut dyn Write, err: &mut (dyn Write + Send)) -> u8
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
    match run_cli(cli, input, out, err) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "Error: {e}");
            e.exit_code()
        }
    }
}

pub fn run_cli(cli: Cli, input: &mut dyn BufRead, out: &mut dyn Write, err: &mut (dyn Write + Send)) -> Result<()> {
    match cli.cmd {
        Command::Import(a) => cmd_import(a, out, err),
        Command::Stats(a) => cmd_stats(a, out),
        Command::Count(a) => cmd_count(a, out),
        Command::Query(a) => cmd_query(a, out, err),
        Command::Export(a) => cmd_export(a, out, err),
        Command::Check(a) => cmd_check(a, out),
        Command::Info(a) => cmd_info(a, out),
        Command::Show(a) => cmd_show(a, out),
        Command::Compact(a) => cmd_compact(a, out),
        Command::Delete(a) => cmd_delete(a, input, out, err),
        Command::Explore(a) => cmd_explore(a, out),
        Command::Recompute(a) => cmd_recompute(a, out, err),
    }
}

fn threads(requested: Option<usize>) -> usize {
    requested.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get())).max(1)
}

fn cmd_import(a: ImportArgs, out: &mut dyn Write, err: &mut (dyn Write + Send)) -> Result<()> {
    let mut opts = import::ImportOptions::new(a.store.db, a.inputs);
    opts.source = a.source;
    opts.source_key = a.source_key;
    opts.text_key = a.text_key;
    opts.extra = import::parse_extra(&a.extra)?;
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
    opts.tokenizer = a.tokenizer;
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

fn cmd_query(a: QueryArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let filter = compile_opt(a.expr.as_deref())?;
    let sort = a.sort.as_deref().map(|s| query::SortBy::parse(s, a.asc)).transpose()?;
    let start = a.random.then(|| {
        query::start_key(a.seed.unwrap_or_else(|| std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), 0u8)))
    });
    let opts = query::QueryOptions { limit: a.limit, sort, start, threads: threads(a.threads) };
    let sel = query::select(store.as_ref(), &sources, filter.as_ref(), &opts)?;
    let output = if a.ids {
        query::Output::Ids
    } else if a.json {
        query::Output::Json
    } else if a.full {
        query::Output::Readable(show::Clip::Full)
    } else {
        query::Output::Readable(a.head.map_or(show::Clip::Ends(query::DEFAULT_ENDS), show::Clip::Head))
    };
    query::print(store.as_ref(), &sources, &sel, sort, output, out)?;
    write!(err, "{}", query::summary(&sel, &opts))?;
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

fn cmd_delete(a: DeleteArgs, input: &mut dyn BufRead, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let opts = delete::DeleteOptions {
        db: a.store.db,
        expr: a.expr,
        ids: a.ids,
        limit: a.limit,
        threads: threads(a.threads),
        backup: a.backup,
        force: a.force,
        batch_size: delete::DEFAULT_BATCH,
    };
    opts.validate()?;
    let store = store::open(&opts.db, false)?;
    let sources = store::Sources::load(store.as_ref())?;
    let sel = delete::select(store.as_ref(), &sources, &opts)?;
    if a.dry_run {
        for key in &sel.keys {
            writeln!(out, "{}", hash::key_to_hex(key))?;
        }
        writeln!(
            err,
            "Dry run: {} documents would be deleted ({} scanned); nothing changed",
            util::fmt_int(sel.keys.len() as u64),
            util::fmt_int(sel.scanned)
        )?;
        return Ok(());
    }
    if sel.keys.is_empty() {
        writeln!(out, "Nothing to delete: 0 documents selected ({} scanned)", util::fmt_int(sel.scanned))?;
        return Ok(());
    }
    if !a.yes {
        let scope = if sel.scanned > 0 { format!(" ({} scanned)", util::fmt_int(sel.scanned)) } else { String::new() };
        write!(
            err,
            "About to delete {} documents from {}{scope}. Type yes to continue: ",
            util::fmt_int(sel.keys.len() as u64),
            opts.db.display()
        )?;
        err.flush()?;
        let mut line = String::new();
        input.read_line(&mut line)?;
        if line.trim() != "yes" {
            return Err(Error::Cancelled(format!("nothing deleted (answer was {:?}, not \"yes\")", line.trim())));
        }
    }
    let report = delete::run(store.as_ref(), &sources, &opts, sel, err)?;
    write!(out, "{}", report.render())?;
    Ok(())
}

fn cmd_explore(a: ExploreArgs, out: &mut dyn Write) -> Result<()> {
    let opts = explore::ExploreOptions { db: a.store.db, bind: a.bind, port: a.port, threads: threads(a.threads) };
    let explorer = explore::Explorer::open(&opts)?;
    let server = explore::bind(explorer, &opts.bind, opts.port)?;
    write!(out, "{}", explore::banner(&server)?)?;
    out.flush()?;
    server.serve(explore::WORKERS);
    Ok(())
}

fn cmd_recompute(a: RecomputeArgs, out: &mut dyn Write, err: &mut (dyn Write + Send)) -> Result<()> {
    let store = store::open(&a.store.db, false)?;
    let opts = recompute::RecomputeOptions {
        threads: threads(a.threads),
        batch_size: a.batch_size.max(1),
        dry_run: a.dry_run,
        force: a.force,
        progress_every: recompute::PROGRESS_EVERY,
        tokenizer: a.tokenizer,
    };
    let rep = recompute::recompute(store.as_ref(), &opts, err)?;
    write!(out, "{}", rep.render())?;
    Ok(())
}
