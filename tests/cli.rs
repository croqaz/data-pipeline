//! End-to-end tests: every subcommand and error path, in-process.

use std::path::{Path, PathBuf};

use pipeline::store;

const PROSE: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

fn run(args: &[&str]) -> (u8, String, String) {
    run_in(args, "")
}

/// Like `run`, with `input` answering any confirmation prompt.
fn run_in(args: &[&str], input: &str) -> (u8, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let full: Vec<&str> = std::iter::once("pipeline").chain(args.iter().copied()).collect();
    let mut stdin = std::io::Cursor::new(input.as_bytes().to_vec());
    let code = pipeline::run_with_input(full, &mut stdin, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn doc(n: usize) -> String {
    serde_json::json!({ "text": format!("[{n}] {PROSE}") }).to_string()
}

fn write(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, lines.join("\n") + "\n").unwrap();
    p
}

fn mixed_file(dir: &Path) -> PathBuf {
    let lines = vec![
        doc(1),
        doc(2),
        String::new(),
        "{not json".to_string(),
        "[1, 2, 3]".to_string(),
        serde_json::json!({ "other": "no text key here" }).to_string(),
        serde_json::json!({ "text": "too short" }).to_string(),
        doc(3),
        // same normalized text as doc 3: punctuation differs only
        serde_json::json!({ "text": format!("[3] {PROSE}").replace('.', "...") }).to_string(),
    ];
    write(dir, "mixed.jsonl", &lines)
}

#[test]
fn import_stats_count() {
    {
        let dir = tempfile::tempdir().unwrap();
        let input = mixed_file(dir.path());
        let db = dir.path().join("db");
        let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "-w", "2", input.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("Lines:                 9  (0 skipped by --offset, 1 empty)"), "{out}");
        assert!(out.contains("bad_json 1, not_object 1, no_text 1, too_short 1"), "{out}");
        assert!(out.contains("Duplicates:            1"), "{out}");
        assert!(out.contains("Indexed:               3"), "{out}");
        assert!(out.contains("Store: about 3 documents"), "{out}");
        assert_eq!(std::fs::read_to_string(store::marker_path(&db)).unwrap().trim(), store::ENGINE);

        let (code, out, _) = run(&["stats", "--db", db.to_str().unwrap()]);
        assert_eq!(code, 0);
        assert!(out.contains("| Scanned: 3 documents"), "{out}");
        assert!(out.contains("| - cli: 3"), "{out}");
        assert!(out.contains("| words"), "{out}");

        let (code, out, _) = run(&["count", "--db", db.to_str().unwrap()]);
        assert_eq!(code, 0);
        assert_eq!(out, "3 / 3 documents match (100.000%).\n");
        let (code, out, _) = run(&["count", "--db", db.to_str().unwrap(), "-t", "3", "words > 1000"]);
        assert_eq!(code, 0);
        assert_eq!(out, "0 / 3 documents match (0.000%).\n");
        let (_, out, _) = run(&["count", "--db", db.to_str().unwrap(), "source == \"cli\" && text.contains(\"[2]\")"]);
        assert_eq!(out, "1 / 3 documents match (33.333%).\n");

        // re-import is a no-op: everything is a duplicate
        let (code, out, _) = run(&["import", "--db", db.to_str().unwrap(), input.to_str().unwrap()]);
        assert_eq!(code, 0);
        assert!(out.contains("Duplicates:            4"), "{out}");
        assert!(out.contains("Indexed:               0"), "{out}");
        let (_, out, _) = run(&["count", "--db", db.to_str().unwrap()]);
        assert_eq!(out, "3 / 3 documents match (100.000%).\n");
    }
}

#[test]
fn extra_fields_source_key_and_expressions() {
    let dir = tempfile::tempdir().unwrap();
    let lines = vec![
        serde_json::json!({ "text": format!("A {PROSE}"), "source": "gut", "year": 1842, "title": "Old", "ignored": 1 }).to_string(),
        serde_json::json!({ "text": format!("B {PROSE}"), "year": "1901", "title": "New" }).to_string(),
        serde_json::json!({ "text": format!("C {PROSE}") }).to_string(),
    ];
    let input = write(dir.path(), "in.jsonl", &lines);
    let db = dir.path().join("db");
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "-e", "year,title", input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Indexed:               3"), "{out}");
    let db = db.to_str().unwrap();
    let count = |expr: &str| run(&["count", "--db", db, expr]).1;
    assert_eq!(count("extra.year < 1850"), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("num(extra.year) > 1900"), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("has(extra.title)"), "2 / 3 documents match (66.667%).\n");
    assert_eq!(count("source == \"gut\""), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("source == \"cli\""), "2 / 3 documents match (66.667%).\n");
    assert_eq!(count("source in [\"gut\", \"cli\"]"), "3 / 3 documents match (100.000%).\n");
    assert_eq!(count("extra.title.matches(\"^O\")"), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("has(extra.ignored)"), "0 / 3 documents match (0.000%).\n");
    assert_eq!(count("len(id) == 32"), "3 / 3 documents match (100.000%).\n");
    let (code, out, _) = run(&["stats", "--db", db, "extra.year < 1850"]);
    assert_eq!(code, 0);
    assert!(out.contains("| Matched: 1 (extra.year < 1850)"), "{out}");
    assert!(out.contains("| - gut: 1"), "{out}");
    let (code, out, _) = run(&["stats", "--db", db, "--limit", "2"]);
    assert_eq!(code, 0);
    assert!(out.contains("| Scanned: 2 documents"), "{out}");
}

#[test]
fn offset_limit_rejects_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let input = mixed_file(dir.path());
    let db = dir.path().join("db");
    let rejects = dir.path().join("rejects.jsonl");
    let (code, out, err) = run(&[
        "import",
        "--db",
        db.to_str().unwrap(),
        "--offset",
        "1",
        "--limit",
        "6",
        "--rejects",
        rejects.to_str().unwrap(),
        "--chunk-mb",
        "1",
        input.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Lines:                 7  (1 skipped by --offset, 1 empty)"), "{out}");
    assert!(out.contains("Stopped at --limit"), "{out}");
    assert!(out.contains("Indexed:               1"), "{out}");
    let rej = std::fs::read_to_string(&rejects).unwrap();
    let lines: Vec<serde_json::Value> = rej.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 4, "{rej}");
    assert_eq!(lines.iter().map(|l| l["line"].as_u64().unwrap()).collect::<Vec<_>>(), vec![4, 5, 6, 7]);
    assert_eq!(lines[0]["reason"], "bad_json");
    assert_eq!(lines[0]["raw"], "{not json");
    assert_eq!(lines[3]["reason"], "too_short");

    let db2 = dir.path().join("db2");
    let (code, _, err) = run(&["import", "--db", db2.to_str().unwrap(), "--on-error", "abort", input.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("import aborted") && err.contains("mixed.jsonl:4: bad_json"), "{err}");
    // A bad line early in a multi-chunk file: the reader stops once the writer aborts.
    let mut big = vec!["{bad".to_string()];
    big.extend((0..20_000).map(doc));
    let big = write(dir.path(), "big.jsonl", &big);
    let db3 = dir.path().join("db3");
    let (code, _, err) =
        run(&["import", "--db", db3.to_str().unwrap(), "--on-error", "abort", "--chunk-mb", "1", "-w", "1", big.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("big.jsonl:1: bad_json"), "{err}");
    // the store is still usable and keeps what was indexed before the abort
    let (code, _, _) = run(&["count", "--db", db2.to_str().unwrap()]);
    assert_eq!(code, 0);
}

#[test]
fn conflicts_replace_or_keep_by_score() {
    let dir = tempfile::tempdir().unwrap();
    // "quick-brown" is one word, "quick brown" two: same key, different score.
    let fewer = serde_json::json!({ "text": format!("{PROSE} and quick-brown ends here") }).to_string();
    let more = serde_json::json!({ "text": format!("{PROSE} and quick brown ends here") }).to_string();
    let a = write(dir.path(), "a.jsonl", std::slice::from_ref(&fewer));
    let b = write(dir.path(), "b.jsonl", &[more.clone(), more.clone(), fewer.clone()]);
    let db = dir.path().join("db");
    let (code, out, _) = run(&["import", "--db", db.to_str().unwrap(), a.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("Indexed:               1"), "{out}");
    let (code, out, _) = run(&["import", "--db", db.to_str().unwrap(), "-w", "1", b.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("Replaced:              1"), "{out}");
    assert!(out.contains("Duplicates:            2"), "{out}");
    assert!(out.contains("Indexed:               0"), "{out}");
    let (_, out, _) = run(&["count", "--db", db.to_str().unwrap(), "text.contains(\"quick brown ends\")"]);
    assert_eq!(out, "1 / 1 documents match (100.000%).\n");
    // the lower-scoring copy never replaces the better one
    let (code, out, _) = run(&["import", "--db", db.to_str().unwrap(), a.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("Duplicates:            1"), "{out}");
    // Both copies in one batch, lower score first: replaced before it is ever written.
    let c = write(dir.path(), "c.jsonl", &[fewer, more]);
    let db2 = dir.path().join("db2");
    let (code, out, _) = run(&["import", "--db", db2.to_str().unwrap(), "-w", "1", c.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("Replaced:              1") && out.contains("Indexed:               1"), "{out}");
    let (_, out, _) = run(&["count", "--db", db2.to_str().unwrap(), "text.contains(\"quick brown ends\")"]);
    assert_eq!(out, "1 / 1 documents match (100.000%).\n");
}

type Row = (String, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn dump(db: &Path) -> Vec<Row> {
    let store = store::open(db, false).unwrap();
    let mut rows = Vec::new();
    store
        .scan_meta(&[0; 16], None, &mut |k, m| {
            rows.push((pipeline::hash::key_to_hex(k), m.to_vec(), store.get_text(k).unwrap().unwrap(), store.get_extra(k).unwrap()));
            Ok(true)
        })
        .unwrap();
    rows
}

#[test]
fn worker_count_does_not_change_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines: Vec<String> = (0..300).map(doc).collect();
    for i in 0..30 {
        lines.push(serde_json::json!({ "text": format!("[{i}] {PROSE}").replace(' ', "  "), "k": i }).to_string());
    }
    let input = write(dir.path(), "in.jsonl", &lines);
    let db1 = dir.path().join("db1");
    let db8 = dir.path().join("db8");
    for (db, w) in [(&db1, "1"), (&db8, "8")] {
        let (code, _, err) = run(&[
            "import",
            "--db",
            db.to_str().unwrap(),
            "-w",
            w,
            "-e",
            "k",
            "--chunk-mb",
            "1",
            "--batch-size",
            "7",
            input.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
    }
    let (a, b) = (dump(&db1), dump(&db8));
    assert_eq!(a.len(), 300);
    assert_eq!(a, b);
}

#[test]
fn compressed_inputs_and_multiple_files() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let plain = write(dir.path(), "a.jsonl", &[doc(1), doc(2)]);
    let gz = dir.path().join("b.jsonl.gz");
    {
        let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::fast());
        enc.write_all((doc(3) + "\n").as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    let db = dir.path().join("db");
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), plain.to_str().unwrap(), gz.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Imported: ") && out.contains("b.jsonl.gz"), "{out}");
    assert!(out.contains("Totals: 3 lines, 3 indexed"), "{out}");
    // each file is announced on stderr as it is opened, in order
    let a = err.find("Importing: ").expect(&err);
    let b = err.rfind("Importing: ").expect(&err);
    assert!(a < b && err[a..b].contains("a.jsonl") && err[b..].contains("b.jsonl.gz"), "{err}");
    // a limit that ends inside the first file leaves the second unopened
    let db2 = dir.path().join("db2");
    let (code, out, err) = run(&["import", "--db", db2.to_str().unwrap(), "--limit", "1", plain.to_str().unwrap(), gz.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Imported: ") && out.contains("a.jsonl\n"), "{out}");
    assert!(out.contains("Not reached: ") && out.contains("b.jsonl.gz\n") && out.contains("Stopped at --limit"), "{out}");
    assert_eq!(err.matches("Importing: ").count(), 1, "{err}");
}

#[test]
fn recompute_refreshes_stale_features() {
    use pipeline::meta::Meta;
    use pipeline::store::Keyspace;
    let dir = tempfile::tempdir().unwrap();
    let input = write(dir.path(), "in.jsonl", &[doc(1), doc(2), doc(3)]);
    let db = dir.path().join("db");
    let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let dbs = db.to_str().unwrap();
    // make one record look like an older build wrote it
    let (key, stale) = {
        let store = store::open(&db, false).unwrap();
        let mut first = None;
        store
            .scan_meta(&[0; 16], None, &mut |k, v| {
                first = Some((*k, Meta::decode(v).unwrap()));
                Ok(false)
            })
            .unwrap();
        let (key, meta) = first.unwrap();
        let stale = Meta { words: 1, entropy: None, ..meta };
        store.put_raw(Keyspace::Meta, &key, Some(&stale.encode())).unwrap();
        store.persist().unwrap();
        (key, stale)
    };
    let (code, _, err) = run(&["check", "--db", dbs, "--deep"]);
    assert_eq!(code, 1, "the stale record is a deep-check problem: {err}");

    let (code, out, err) = run(&["recompute", "--db", dbs, "--dry-run", "-t", "2", "--batch-size", "1"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Recomputed 3 documents in"), "{out}");
    assert!(out.contains("Updated:               1  (would be rewritten)"), "{out}");
    assert!(out.contains("Unchanged:             2"), "{out}");
    assert!(out.contains("Dry run: nothing was written."), "{out}");
    {
        let store = store::open(&db, false).unwrap();
        assert_eq!(Meta::decode(&store.get_meta(&key).unwrap().unwrap()).unwrap(), stale, "a dry run changes nothing");
    }

    let (code, out, err) = run(&["recompute", "--db", dbs, "-t", "2", "--batch-size", "1"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Updated:               1  (features rewritten)"), "{out}");
    assert!(out.contains("run `compact` to merge the rewritten tables"), "{out}");
    let (code, _, err) = run(&["check", "--db", dbs, "--deep"]);
    assert_eq!(code, 0, "after recompute the deep check is clean: {err}");
    let (code, out, _) = run(&["recompute", "--db", dbs]);
    assert_eq!(code, 0);
    assert!(out.contains("Updated:               0") && out.contains("Unchanged:             3"), "{out}");
    assert!(!out.contains("run `compact`"), "{out}");
    let (code, out, _) = run(&["recompute", "--db", dbs, "--force"]);
    assert_eq!(code, 0);
    assert!(out.contains("Updated:               3") && out.contains("Unchanged:             0"), "{out}");
    let (_, out, _) = run(&["count", "--db", dbs, "words == 1"]);
    assert_eq!(out, "0 / 3 documents match (0.000%).\n");

    let (code, _, err) = run(&["recompute", "--db", dir.path().join("nope").to_str().unwrap()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not a pipeline store"), "{err}");
}

#[test]
fn dry_run_counts_without_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let input = mixed_file(dir.path());
    let db = dir.path().join("never-created");
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "--dry-run", input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Indexed:               3") && out.contains("Duplicates:            1"), "{out}");
    assert!(out.contains("Dry run: nothing was written") && !out.contains("Store:"), "{out}");
    assert!(!db.exists());
}

#[test]
fn nocow_flag_reports_what_happened() {
    // /tmp is tmpfs here (warning path); the cargo target dir is btrfs on the
    // target machine (success path). Both must report what they did.
    for dir in [tempfile::tempdir().unwrap(), tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()] {
        let input = write(dir.path(), "a.jsonl", &[doc(1)]);
        let db = dir.path().join("db");
        let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), "--nocow", input.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(err.contains("Set nodatacow on") || err.contains("could not set nodatacow"), "{err}");
    }
}

#[test]
fn progress_lines_appear_for_big_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let short = serde_json::json!({ "text": "x" }).to_string();
    let lines: Vec<String> = (0..100_001).map(|_| short.clone()).collect();
    let input = write(dir.path(), "many.jsonl", &lines);
    let db = dir.path().join("db");
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("100,001 lines, 0 indexed"), "{err}");
    assert!(out.contains("too_short 100,001"), "{out}");
}

#[test]
fn usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let dbs = db.to_str().unwrap();
    let missing = dir.path().join("missing.jsonl");
    let (code, _, err) = run(&["import", "--db", dbs, missing.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("is not a file"), "{err}");
    let input = write(dir.path(), "a.jsonl", &[doc(1)]);
    let (code, _, err) = run(&["import", "--db", dbs, "--max-len", "50", input.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("--max-len must be greater"), "{err}");
    let (code, _, err) = run(&["count", "--db", dbs]);
    assert_eq!(code, 2);
    assert!(err.contains("not a pipeline store"), "{err}");
    let (code, _, _) = run(&["import", "--db", dbs, input.to_str().unwrap()]);
    assert_eq!(code, 0);
    let (code, _, err) = run(&["count", "--db", dbs, "nope > 1"]);
    assert_eq!(code, 2);
    assert!(err.contains("unknown name `nope`"), "{err}");
    let (code, _, err) = run(&["stats", "--db", dbs, "words >"]);
    assert_eq!(code, 2);
    assert!(err.contains("expression error"), "{err}");
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("Usage: pipeline"), "{out}");
    let (code, _, err) = run(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("Usage"), "{err}");
    let (code, _, err) = run(&["import", "--db", dbs]);
    assert_eq!(code, 2);
    assert!(err.contains("required"), "{err}");
    // a rejects path that cannot be created
    let (code, _, err) =
        run(&["import", "--db", dbs, "--rejects", dir.path().join("no/such/dir.jsonl").to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(err.contains("io error"), "{err}");
    // a store directory that is a file
    let file_db = dir.path().join("file");
    std::fs::write(&file_db, b"x").unwrap();
    let (code, _, _) = run(&["import", "--db", file_db.to_str().unwrap(), input.to_str().unwrap()]);
    assert_ne!(code, 0);
}

#[test]
fn library_options_default_sensibly() {
    let opts = pipeline::import::ImportOptions::new(PathBuf::from("x"), vec![]);
    assert_eq!(opts.text_key, "text");
    assert!(opts.workers >= 1);
    let mut log = Vec::new();
    let err = pipeline::import::import(&opts, &mut log).unwrap_err();
    assert!(err.to_string().contains("at least one input"));

    let opts = pipeline::export::ExportOptions::new(PathBuf::from("x"), PathBuf::from("o.jsonl"));
    assert_eq!(opts.header_sep, "\n\n");
    assert_eq!(opts.progress_every, 100_000);
    assert!(opts.fields.is_empty() && !opts.verify && !opts.force);
}

#[test]
fn export_progress_lines_follow_the_interval() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<String> = (0..7).map(doc).collect();
    let input = write(dir.path(), "in.jsonl", &lines);
    let db = dir.path().join("db");
    let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let mut opts = pipeline::export::ExportOptions::new(db, PathBuf::from("-"));
    opts.fields = vec!["id".into()];
    opts.progress_every = 3;
    opts.chunk_docs = 2;
    opts.threads = 2;
    let (mut out, mut log) = (Vec::new(), Vec::new());
    let report = pipeline::export::export(&opts, &mut out, &mut log).unwrap();
    assert_eq!(report.written, 7);
    assert_eq!(String::from_utf8(log).unwrap(), "  exported 4 / 7 documents\n  exported 6 / 7 documents\n");
    assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 7);
}

fn books_store(dir: &Path) -> String {
    let lines = vec![
        serde_json::json!({ "text": format!("A {PROSE}"), "source": "gut", "year": 1818, "title": "Frankenstein", "author": "Mary Shelley" }).to_string(),
        serde_json::json!({ "text": format!("B {PROSE} {PROSE}"), "source": "gut", "year": 1900, "title": "Untitled" }).to_string(),
        serde_json::json!({ "text": format!("C {PROSE} {PROSE} {PROSE}"), "source": "web" }).to_string(),
        serde_json::json!({ "text": format!("D {PROSE}"), "year": "1850", "title": "Quote \"d\"", "words": 999 }).to_string(),
    ];
    let input = write(dir, "books.jsonl", &lines);
    let db = dir.join("db");
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "-e", "year,title,author,words", input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Indexed:               4"), "{out}");
    db.to_str().unwrap().to_string()
}

fn read_lines(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read(path).unwrap();
    let raw = if path.to_string_lossy().ends_with(".zst") { zstd::decode_all(&raw[..]).unwrap() } else { raw };
    String::from_utf8(raw).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn export_default_layout() {
    {
        let dir = tempfile::tempdir().unwrap();
        let db = books_store(dir.path());
        let out = dir.path().join("all.jsonl");
        let (code, report, err) = run(&["export", "--db", &db, "-o", out.to_str().unwrap(), "--verify", "-t", "3"]);
        assert_eq!(code, 0, "{err}");
        assert!(report.starts_with(&format!("Exported 4 documents to {}", out.display())), "{report}");
        assert!(report.contains("Scanned 4, matched 4\n"), "{report}");
        assert!(report.contains("Verified: 4 lines parse as JSON in 1 file"), "{report}");
        assert!(report.contains("Skipped 1 metadata keys"), "{report}");
        let docs = read_lines(&out);
        assert_eq!(docs.len(), 4);
        let frank = docs.iter().find(|d| d["title"] == "Frankenstein").unwrap();
        assert_eq!(frank["source"], "gut");
        assert_eq!(frank["year"], 1818);
        assert_eq!(frank["author"], "Mary Shelley");
        assert_eq!(frank["words"], 30);
        assert_eq!(frank["id"].as_str().unwrap().len(), 32);
        assert!(frank["text"].as_str().unwrap().starts_with("A The quick"));
        assert_eq!(frank["vowel"], 100.0);
        assert_eq!(frank["dictHit"], 96.67, "29 of 30 words: only \"vexingly\" is not in the vocabulary");
        let d = docs.iter().find(|d| d["title"] == "Quote \"d\"").unwrap();
        assert_eq!(d["words"], 30, "the feature wins over the imported `words` key");
        assert_eq!(d["year"], "1850");
        let web = docs.iter().find(|d| d["source"] == "web").unwrap();
        assert!(web.get("title").is_none());
        // ids are in key order
        let ids: Vec<&str> = docs.iter().map(|d| d["id"].as_str().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        // refuses to overwrite, unless forced
        let (code, _, err) = run(&["export", "--db", &db, "-o", out.to_str().unwrap()]);
        assert_eq!(code, 2);
        assert!(err.contains("exists; use --force"), "{err}");
        let (code, _, _) = run(&["export", "--db", &db, "-o", out.to_str().unwrap(), "--force", "-f", "id"]);
        assert_eq!(code, 0);
        assert_eq!(read_lines(&out)[0].as_object().unwrap().len(), 1);
        assert!(!dir.path().join("all.jsonl.partial").exists());
    }
}

#[test]
fn export_headers_fields_and_filters() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    let tpl = dir.path().join("header.txt");
    std::fs::write(&tpl, "---\nyear = {extra.year}\ntitle = {extra.title}\nauthor = {extra.author}\n---\n").unwrap();
    let out = dir.path().join("with-headers.jsonl");
    let (code, report, err) = run(&[
        "export",
        "--db",
        &db,
        "-o",
        out.to_str().unwrap(),
        "--header-file",
        tpl.to_str().unwrap(),
        "-f",
        "text,source",
        "source == \"gut\" || has(extra.year)",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(report.contains("matched 3 (source == \"gut\" || has(extra.year))"), "{report}");
    let docs = read_lines(&out);
    assert_eq!(docs.len(), 3);
    let texts: Vec<&str> = docs.iter().map(|d| d["text"].as_str().unwrap()).collect();
    assert!(
        texts.iter().any(|t| t.starts_with("---\nyear = 1818\ntitle = Frankenstein\nauthor = Mary Shelley\n---\n\nA The quick")),
        "{texts:?}"
    );
    assert!(texts.iter().any(|t| t.starts_with("---\nyear = 1900\ntitle = Untitled\n---\n\nB The quick")), "{texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("---\nyear = 1850\ntitle = Quote \"d\"\n---\n\nD The quick")), "{texts:?}");
    assert!(docs.iter().all(|d| d.as_object().unwrap().len() == 2));

    // inline template with escapes, custom separator, one-line header, no match for the web doc
    let out2 = dir.path().join("inline.jsonl");
    let (code, _, err) = run(&[
        "export",
        "--db",
        &db,
        "-o",
        out2.to_str().unwrap(),
        "--header",
        "Title: {extra.title}\\nWords: {words}",
        "--header-sep",
        "\\n",
        "-f",
        "text,extra.title,words",
    ]);
    assert_eq!(code, 0, "{err}");
    let docs = read_lines(&out2);
    let web = docs.iter().find(|d| d.get("title").is_none()).unwrap();
    assert!(web["text"].as_str().unwrap().starts_with("Words: 88\nC The quick"), "a line with a null placeholder is dropped");
    let frank = docs.iter().find(|d| d["title"] == "Frankenstein").unwrap();
    assert!(frank["text"].as_str().unwrap().starts_with("Title: Frankenstein\nWords: 30\nA The quick"));
    assert_eq!(frank["words"], 30);

    // sample, top, limit and stdout
    let (code, stdout, stderr) = run(&["export", "--db", &db, "-o", "-", "--top", "2", "-f", "id,words"]);
    assert_eq!(code, 0, "{stderr}");
    let docs: Vec<serde_json::Value> = stdout.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(docs.iter().map(|d| d["words"].as_u64().unwrap()).collect::<Vec<_>>(), vec![88, 59], "best first");
    assert!(stderr.starts_with("Exported 2 documents to stdout"), "{stderr}");
    assert!(stderr.contains("selected 2 (top 2)"), "{stderr}");
    let (code, stdout, _) = run(&["export", "--db", &db, "-o", "-", "--limit", "3", "-f", "id"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.lines().count(), 3);
    let (code, stdout, _) = run(&["export", "--db", &db, "-o", "-", "--top", "3", "--limit", "1", "-f", "words"]);
    assert_eq!(code, 0);
    assert_eq!(stdout, "{\"words\":88}\n");
    let (code, a, _) = run(&["export", "--db", &db, "-o", "-", "--sample", "0.5", "--seed", "3", "-f", "id"]);
    assert_eq!(code, 0);
    let (_, b, _) = run(&["export", "--db", &db, "-o", "-", "--sample", "0.5", "--seed", "3", "-f", "id"]);
    assert_eq!(a, b, "sampling is reproducible");
    let (_, all, _) = run(&["export", "--db", &db, "-o", "-", "--sample", "1", "-f", "id"]);
    assert_eq!(all.lines().count(), 4);
    let (code, stdout, stderr) = run(&["export", "--db", &db, "-o", "-", "-f", "id", "words > 100000"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.starts_with("Exported 0 documents"), "{stderr}");
}

#[test]
fn export_shards_and_zstd() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<String> = (0..25).map(doc).collect();
    let input = write(dir.path(), "in.jsonl", &lines);
    let db = dir.path().join("db");
    let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let db = db.to_str().unwrap();
    let out = dir.path().join("shards").join("v.jsonl");
    let (code, report, err) = run(&["export", "--db", db, "-o", out.to_str().unwrap(), "--shard", "10", "--zstd", "--verify", "-f", "id"]);
    assert_eq!(code, 0, "{err}");
    assert!(report.starts_with("Exported 25 documents to 3 files"), "{report}");
    assert!(report.contains("v-0001.jsonl.zst  10 docs"), "{report}");
    assert!(report.contains("v-0003.jsonl.zst  5 docs"), "{report}");
    assert!(report.contains("Verified: 25 lines parse as JSON in 3 files"), "{report}");
    let mut all = Vec::new();
    for n in 1..=3 {
        let p = dir.path().join("shards").join(format!("v-{n:04}.jsonl.zst"));
        all.extend(read_lines(&p));
    }
    assert_eq!(all.len(), 25);
    // a .zst output name turns compression on by itself; an empty match still produces a complete file
    let out = dir.path().join("none.jsonl.zst");
    let (code, report, err) = run(&["export", "--db", db, "-o", out.to_str().unwrap(), "words > 1000"]);
    assert_eq!(code, 0, "{err}");
    assert!(report.starts_with(&format!("Exported 0 documents to {}", out.display())), "{report}");
    assert!(read_lines(&out).is_empty());
    assert!(!dir.path().join("none.jsonl.zst.partial").exists());
}

#[test]
fn export_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    let out = dir.path().join("x.jsonl");
    let o = out.to_str().unwrap();
    let usage = |args: &[&str]| {
        let (code, _, err) = run(&[&["export", "--db", &db][..], args].concat());
        assert_eq!(code, 2, "{err}");
        err
    };
    assert!(usage(&["-o", o, "-f", "nope"]).contains("unknown field \"nope\""));
    assert!(usage(&["-o", o, "--header", "{bogus}"]).contains("placeholder {bogus}"));
    assert!(usage(&["-o", o, "--header-file", dir.path().join("missing").to_str().unwrap()]).contains("--header-file"));
    assert!(usage(&["-o", o, "--sample", "2"]).contains("--sample"));
    assert!(usage(&["-o", "-", "--shard", "5"]).contains("--shard cannot"));
    assert!(usage(&["-o", o, "words >"]).contains("expression"));
    assert!(usage(&["-o", o, "--zstd-level", "99"]).contains("--zstd-level"));
    let (code, _, err) = run(&["export", "--db", dir.path().join("nostore").to_str().unwrap(), "-o", o]);
    assert_eq!(code, 2, "{err}");
    assert!(!out.exists());
}

#[test]
fn info_show_compact_and_check() {
    {
        let dir = tempfile::tempdir().unwrap();
        let db = books_store(dir.path());

        let (code, out, err) = run(&["info", "--db", &db]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with(&format!("Store:      {db}\nDocuments:  about ")), "{out}");
        assert!(
            out.contains("Disk:       ")
                && out.contains("Sources:    3 (")
                && out.contains("gut")
                && out.contains("web")
                && out.contains("cli"),
            "{out}"
        );
        assert!(
            out.contains(
                "Features:   len, bytes, tokens, uniqChars, words, uniqWords, meanWordLen, stopWords, lines, sentences, vowel, alpha, dictHit, dup5gram, entropy, quality (schema 1)\n"
            ),
            "{out}"
        );
        assert!(out.contains("\nmeta:       ") && out.contains("\ntext:       "), "{out}");

        let (_, ids, _) = run(&["export", "--db", &db, "-o", "-", "-f", "id", "--limit", "1"]);
        let id: serde_json::Value = serde_json::from_str(ids.trim()).unwrap();
        let id = id["id"].as_str().unwrap().to_string();
        let (code, out, err) = run(&["show", "--db", &db, &id]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with(&format!("id:        {id}\nsource:    ")), "{out}");
        assert!(
            out.contains("\nfeatures:  len ") && out.contains("  vowel ") && out.contains("  alpha ") && out.contains("\n--- text ("),
            "{out}"
        );
        assert!(out.contains("The quick brown fox"), "{out}");
        let (code, out, _) = run(&["show", "--db", &db, &id[..16], "--head", "5"]);
        assert_eq!(code, 0);
        assert!(out.contains("--- text (") && out.contains("\n... [") && out.contains(" more chars]\n"), "{out}");
        let (code, out, _) = run(&["show", "--db", &db, "--json", &id, &id]);
        assert_eq!(code, 0);
        let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], id);
        assert!(lines[0]["text"].as_str().unwrap().contains("quick"));
        let (code, out, _) = run(&["show", "--db", &db, &id, &id]);
        assert_eq!(code, 0);
        assert_eq!(out.matches("\nid:        ").count(), 1, "two documents are separated by a blank line: {out}");
        let (code, _, err) = run(&["show", "--db", &db, "ffff"]);
        assert_eq!(code, 2, "{err}");
        assert!(err.contains("no document starts with it"), "{err}");

        let (code, out, err) = run(&["check", "--db", &db, "-t", "2"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Check: OK, 4 documents (structure only"), "{out}");
        assert!(out.contains("Keyspaces: meta 4, text 4, extra 3; 3 source labels\n"), "{out}");
        let (code, out, err) = run(&["check", "--db", &db, "--deep"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Check: OK, 4 documents (deep check"), "{out}");
        assert!(out.contains("stale features:                    0\n"), "{out}");

        let (code, out, err) = run(&["compact", "--db", &db]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Compacted store: ") && out.contains(" MB -> ") && out.contains(" MB in "), "{out}");
        let (_, out, _) = run(&["count", "--db", &db]);
        assert_eq!(out, "4 / 4 documents match (100.000%).\n");

        // damage the store through the library, then check must fail
        {
            let store = store::open(Path::new(&db), false).unwrap();
            store.put_raw(store::Keyspace::Text, &[0xee; 16], Some(b"orphan")).unwrap();
            store.put_raw(store::Keyspace::Meta, &[0xef; 16], Some(&pipeline::meta::Meta::default().encode())).unwrap();
            store.persist().unwrap();
        }
        let (code, out, err) = run(&["check", "--db", &db, "--examples", "1"]);
        assert_eq!(code, 1, "{err}");
        assert!(out.starts_with("Check: 2 problems in 5 documents"), "{out}");
        assert!(out.contains("missing text:                      1  e.g. efefefefefefefefefefefefefefefef\n"), "{out}");
        assert!(out.contains("orphan text:                       1  e.g. eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\n"), "{out}");
        assert_eq!(err, "Error: store error: check found 2 problems\n");
        let (code, _, err) = run(&["show", "--db", &db, &pipeline::hash::key_to_hex(&[0xef; 16])]);
        assert_eq!(code, 1, "{err}");
        assert!(err.contains("text missing"), "{err}");
    }
    let (code, _, err) = run(&["info", "--db", "/nonexistent/store"]);
    assert_eq!(code, 2, "{err}");
}

#[test]
fn replacement_drops_stale_extra() {
    {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let with_year = serde_json::json!({ "text": format!("X {PROSE}"), "year": 1800 }).to_string();
        let input = write(dir.path(), "in.jsonl", &[with_year, doc(1)]);
        let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), "-e", "year", input.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        let db = db.to_str().unwrap();
        let (_, out, _) = run(&["count", "--db", db, "has(extra.year)"]);
        assert_eq!(out, "1 / 2 documents match (50.000%).\n");
        // A replacement without metadata, written the way the import writer writes one.
        let rows = dump(Path::new(db));
        {
            let store = store::open(Path::new(db), false).unwrap();
            let row = rows.iter().find(|r| r.3.is_some()).unwrap();
            let key = pipeline::hash::key_from_hex(&row.0).unwrap();
            store.put_batch(&[store::Record { key, meta: row.1.clone(), text: row.2.clone(), extra: None, replace: true }]).unwrap();
            store.persist().unwrap();
        }
        let (_, out, _) = run(&["count", "--db", db, "has(extra.year)"]);
        assert_eq!(out, "0 / 2 documents match (0.000%).\n");
        // the meta still carries has_extra: exactly the inconsistency `check` exists for
        let (code, out, err) = run(&["check", "--db", db]);
        assert_eq!(code, 1, "{err}");
        assert!(out.contains("missing extra:                     1"), "{out}");
    }
}

fn count_all(db: &str) -> String {
    run(&["count", "--db", db]).1
}

#[test]
fn delete_dry_run_prompt_and_yes() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    // dry run: ids on stdout, summary on stderr, nothing changes
    let (code, out, err) = run(&["delete", "--db", &db, "--dry-run", "source == \"gut\""]);
    assert_eq!(code, 0, "{err}");
    let ids: Vec<&str> = out.lines().collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|i| i.len() == 32 && i.chars().all(|c| c.is_ascii_hexdigit())), "{out}");
    assert_eq!(err, "Dry run: 2 documents would be deleted (4 scanned); nothing changed\n");
    assert_eq!(count_all(&db), "4 / 4 documents match (100.000%).\n");
    // the prompt, declined (and with no input at all)
    for answer in ["no\n", "", "YES\n"] {
        let (code, out, err) = run_in(&["delete", "--db", &db, "source == \"gut\""], answer);
        assert_eq!(code, 1, "{err}");
        assert!(err.starts_with("About to delete 2 documents from ") && err.contains("/db (4 scanned). Type yes"), "{err}");
        assert!(err.contains("Type yes to continue: Error: cancelled: nothing deleted"), "{err}");
        assert_eq!(out, "");
        assert_eq!(count_all(&db), "4 / 4 documents match (100.000%).\n");
    }
    // the prompt, accepted
    let (code, out, err) = run_in(&["delete", "--db", &db, "source == \"gut\""], "yes\n");
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("Deleted 2 documents in "), "{out}");
    assert!(out.contains(" (4 scanned)\n  Store: about ") && out.contains("run `compact` to reclaim"), "{out}");
    assert!(!out.contains("Backup"), "{out}");
    assert_eq!(count_all(&db), "2 / 2 documents match (100.000%).\n");
    assert_eq!(run(&["count", "--db", &db, "source == \"gut\""]).1, "0 / 2 documents match (0.000%).\n");
    // no orphans left behind
    let (code, out, _) = run(&["check", "--db", &db, "--deep"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("meta 2, text 2, extra 1;"), "{out}");
    // nothing matches: no prompt, exit 0
    let (code, out, err) = run_in(&["delete", "--db", &db, "source == \"gut\""], "");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "Nothing to delete: 0 documents selected (2 scanned)\n");
    assert_eq!(err, "");
    // --yes skips the prompt even with input available
    let (code, out, _) = run_in(&["delete", "--db", &db, "-y", "source == \"web\""], "no\n");
    assert_eq!(code, 0);
    assert!(out.starts_with("Deleted 1 documents"), "{out}");
    assert_eq!(count_all(&db), "1 / 1 documents match (100.000%).\n");
    let (code, out, _) = run(&["compact", "--db", &db]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn delete_by_id_limit_and_backup() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    let (_, ids, _) = run(&["export", "--db", &db, "-o", "-", "-f", "id"]);
    let ids: Vec<String> =
        ids.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 4);
    // by full id and by prefix, the same document twice counts once
    let (code, out, err) = run(&["delete", "--db", &db, "-y", "--id", &ids[0], "--id", &ids[0][..16]]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("Deleted 1 documents in "), "{out}");
    assert!(out.contains("(0 scanned)"), "{out}");
    let (code, _, err) = run(&["show", "--db", &db, &ids[0]]);
    assert_eq!(code, 2, "{err}");
    // limit: the first N in key order
    let (code, out, _) = run(&["delete", "--db", &db, "-y", "--limit", "1", "words > 0"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("Deleted 1 documents"), "{out}");
    assert_eq!(count_all(&db), "2 / 2 documents match (100.000%).\n");
    // backup first, then delete; the backup holds the full records
    let backup = dir.path().join("deleted.jsonl");
    let (code, out, err) = run(&["delete", "--db", &db, "-y", "--backup", backup.to_str().unwrap(), "words > 0"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("Deleted 2 documents"), "{out}");
    assert!(out.contains(&format!("  Backup: {} (2 documents, ", backup.display())), "{out}");
    let docs = read_lines(&backup);
    assert_eq!(docs.len(), 2);
    assert!(docs.iter().all(|d| d["text"].as_str().unwrap().contains("quick") && d["id"].as_str().unwrap().len() == 32));
    assert_eq!(count_all(&db), "0 / 0 documents match (0.000%).\n");
    // an existing backup is never overwritten without --force
    std::fs::create_dir(dir.path().join("second")).unwrap();
    let db2 = books_store(&dir.path().join("second"));
    let (code, _, err) = run(&["delete", "--db", &db2, "-y", "--backup", backup.to_str().unwrap(), "words > 0"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("exists; use --force"), "{err}");
    assert_eq!(count_all(&db2), "4 / 4 documents match (100.000%).\n", "nothing deleted when the backup is refused");
    let (code, _, err) = run(&["delete", "--db", &db2, "-y", "--force", "--backup", backup.to_str().unwrap(), "words > 0"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(read_lines(&backup).len(), 4);
}

#[test]
fn delete_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    let usage = |args: &[&str]| {
        let (code, _, err) = run(&[&["delete", "--db", &db][..], args].concat());
        assert_eq!(code, 2, "{err}");
        err
    };
    assert!(usage(&[]).contains("expression or at least one --id"));
    assert!(usage(&["--id", "abcd", "words > 1"]).contains("not both"));
    assert!(usage(&["--limit", "0", "words > 1"]).contains("--limit"));
    assert!(usage(&["--id", "zzzz"]).contains("not hexadecimal"));
    assert!(usage(&["words >"]).contains("expression"));
    assert_eq!(count_all(&db), "4 / 4 documents match (100.000%).\n");
}

fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Ok((status, body))
}

#[test]
fn explore_serves_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _, err) = run(&["explore", "--db", dir.path().join("none").to_str().unwrap()]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not a pipeline store"), "{err}");
    std::fs::create_dir(dir.path().join("second")).unwrap();
    let other = books_store(&dir.path().join("second"));
    let (code, _, err) = run(&["explore", "--db", &other, "--bind", "999.999.999.999", "--port", "0"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("cannot listen on 999.999.999.999:0"), "{err}");

    let db = books_store(dir.path());
    // a port nobody holds right now; the server takes it a moment later
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let port_s = port.to_string();
    let args: Vec<String> = ["explore", "--db", &db, "--port", &port_s, "-t", "2"].iter().map(|s| s.to_string()).collect();
    // serves until the process ends; the thread is left behind on purpose
    std::thread::spawn(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&refs)
    });
    let mut reply = None;
    for _ in 0..100 {
        if let Ok((200, body)) = http_get(port, "/api/info") {
            reply = Some(body);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let body = reply.expect("the server came up");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["dir"], db);
    assert_eq!(v["sources"], serde_json::json!(["cli", "gut", "web"]));
    let (status, body) = http_get(port, "/api/random?expr=extra.title%20%3D%3D%20%22Frankenstein%22").unwrap();
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["extra"]["author"], "Mary Shelley");
    assert!(v["text"].as_str().unwrap().starts_with("A The quick"));
}

#[test]
fn query_prints_matches() {
    let dir = tempfile::tempdir().unwrap();
    let db = books_store(dir.path());
    let (code, out, err) = run(&["query", "--db", &db, "words > 40", "--sort", "words"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("=== Match 1 of 2  words 88 ===\nid:        "), "{out}");
    assert!(out.contains("--- text (") && out.contains("C The quick brown fox"), "{out}");
    assert!(out.contains("=== Match 2 of 2  words 59 ===\n"), "{out}");
    assert_eq!(err, "Showed 2 of 2 matches, highest words first; scanned 4 documents.\n");
    let (_, ids, _) = run(&["query", "--db", &db, "--sort", "words", "--asc", "-l", "1", "--ids"]);
    assert_eq!(ids.lines().count(), 1);
    let (_, shown, _) = run(&["show", "--db", &db, ids.trim(), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(v["words"], 30);
    let (_, json, err) = run(&["query", "--db", &db, "source == \"web\"", "--json"]);
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["source"], "web");
    assert_eq!(err, "Showed all 1 matches; scanned 4 documents.\n");
    let (_, out, err) = run(&["query", "--db", &db, "-l", "2", "--random", "--seed", "1", "--head", "5"]);
    assert_eq!(out.matches("=== Match").count(), 2);
    assert!(out.contains(" more chars]"), "{out}");
    let (_, out, _) = run(&["query", "--db", &db, "source == \"gut\"", "--head", "1"]);
    assert!(out.contains("=== Match 1 of 2, scanned ") && out.contains("=== Match 2 of 2, scanned "), "{out}");
    assert!(err.starts_with("Showed the first 2 matches in key order"), "{err}");
    let (_, all, _) = run(&["query", "--db", &db, "--full", "--random"]);
    assert_eq!(all.matches("=== Match").count(), 4);
    let (code, _, err) = run(&["query", "--db", &db, "--sort", "nope"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not a feature"), "{err}");
    let (code, _, err) = run(&["query", "--db", &db, "words >"]);
    assert_ne!(code, 0, "{err}");
}

#[test]
fn source_flag_overrides_the_record_and_reimport_relabels() {
    let dir = tempfile::tempdir().unwrap();
    let with_key = |n: usize, extra: Option<i64>| {
        let mut v = serde_json::json!({ "text": format!("[{n}] {PROSE}"), "source": "tcp_p4" });
        if let Some(y) = extra {
            v["year"] = y.into();
        }
        v.to_string()
    };
    let f = write(dir.path(), "a.jsonl", &[with_key(1, Some(1600)), doc(2)]);
    let db = dir.path().join("db");
    let dbs = db.to_str().unwrap();
    let count = |expr: &str| run(&["count", "--db", dbs, expr]).1;
    // No -s: the record's key labels it, the rest get "cli".
    let (code, _, err) = run(&["import", "--db", dbs, f.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(count("source == 'tcp_p4'").starts_with("1 / 2"));
    assert!(count("source == 'cli'").starts_with("1 / 2"));
    // Re-import with -s: same texts, every label replaced, nothing new indexed.
    let (code, out, err) = run(&["import", "--db", dbs, "-s", "EEBO", "-e", "year", f.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Updated:               2") && out.contains("Indexed:               0"), "{out}");
    assert!(out.contains("Unused sources removed: cli, tcp_p4\n"), "{out}");
    assert!(count("source == 'EEBO'").starts_with("2 / 2"), "{}", count("source == 'EEBO'"));
    assert!(run(&["info", "--db", dbs]).1.contains("Sources:    1 (EEBO)\n"));
    assert!(count("extra.year == 1600").starts_with("1 / 2"));
    // Same labels again: plain duplicates, nothing rewritten.
    let (_, out, _) = run(&["import", "--db", dbs, "-s", "EEBO", "-e", "year", f.to_str().unwrap()]);
    assert!(out.contains("Updated:               0") && out.contains("Duplicates:            2"), "{out}");
    // A relabel without -e keeps the stored extras.
    let (_, out, _) = run(&["import", "--db", dbs, "-s", "EVANS", f.to_str().unwrap()]);
    assert!(out.contains("Updated:               2"), "{out}");
    assert!(count("source == 'EVANS' && extra.year == 1600").starts_with("1 / 2"));
    // A new extra value on the same text and source is an update too.
    let g = write(dir.path(), "g.jsonl", &[with_key(1, Some(1601))]);
    let (_, out, _) = run(&["import", "--db", dbs, "-s", "EVANS", "-e", "year", g.to_str().unwrap()]);
    assert!(out.contains("Updated:               1"), "{out}");
    assert!(count("extra.year == 1601").starts_with("1 / 2"));
    let (code, out, _) = run(&["check", "--db", dbs, "--deep"]);
    assert_eq!(code, 0, "{out}");
    // A delete that empties a source drops its name too.
    let h = write(dir.path(), "h.jsonl", &[doc(9)]);
    run(&["import", "--db", dbs, "-s", "TMP", h.to_str().unwrap()]);
    assert!(run(&["info", "--db", dbs]).1.contains("Sources:    2 (EVANS, TMP)\n"));
    let (code, out, err) = run(&["delete", "--db", dbs, "-y", "source == 'TMP'"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Unused sources removed: TMP\n"), "{out}");
    assert!(run(&["info", "--db", dbs]).1.contains("Sources:    1 (EVANS)\n"));
}

#[test]
fn extra_renames_keys() {
    let dir = tempfile::tempdir().unwrap();
    let line = serde_json::json!({ "text": PROSE, "Publication": "Gazette", "titleCode": "sn123", "year": 1800 }).to_string();
    let f = write(dir.path(), "a.jsonl", &[line]);
    let db = dir.path().join("db");
    let dbs = db.to_str().unwrap();
    let (code, _, err) = run(&["import", "--db", dbs, "-e", "Publication=title,titleCode=pubID,year", f.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let (_, out, _) = run(&["count", "--db", dbs, "extra.title == 'Gazette' && extra.pubID == 'sn123' && extra.year == 1800"]);
    assert!(out.starts_with("1 / 1"), "{out}");
    let (_, out, _) = run(&["count", "--db", dbs, "has(extra.Publication) || has(extra.titleCode)"]);
    assert!(out.starts_with("0 / 1"), "{out}");
    let (code, _, err) = run(&["import", "--db", dbs, "-e", "title,Publication=title", f.to_str().unwrap()]);
    assert_ne!(code, 0);
    assert!(err.contains("two keys are stored as \"title\""), "{err}");
}

fn tok(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).display().to_string()
}

/// The `tokens` of every document, by text prefix `[n]`.
fn token_counts(db: &str) -> Vec<(String, Option<u64>)> {
    let (code, out, _) = run(&["export", "--db", db, "-o", "-", "-f", "text,tokens"]);
    assert_eq!(code, 0);
    let mut v: Vec<(String, Option<u64>)> = out
        .lines()
        .map(|l| {
            let j: serde_json::Value = serde_json::from_str(l).unwrap();
            (j["text"].as_str().unwrap()[..3].to_string(), j["tokens"].as_u64())
        })
        .collect();
    v.sort();
    v
}

#[test]
fn tokenizer_is_set_once_and_mismatches_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = write(dir.path(), "first.jsonl", &[doc(1), doc(2)]);
    let second = write(dir.path(), "second.jsonl", &[doc(3)]);
    let db_path = dir.path().join("db");
    let db = db_path.to_str().unwrap();
    let (a, b) = (tok("tok-a"), tok("tok-b"));

    // a bad path fails before any store is created
    let (code, _, err) = run(&["import", "--db", db, "--tokenizer", &format!("{a}-missing"), first.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("tok-a-missing does not exist; pass a folder that contains a tokenizer.json"), "{err}");
    assert!(!db_path.exists());

    // the first import sets the store's tokenizer; HF counts "[n] PROSE" as 141 with tok-a
    let (code, out, err) = run(&["import", "--db", db, "--tokenizer", &a, first.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("Counting tokens with tok-a (blake3 "), "{err}");
    assert!(out.contains("Tokens: counted with tok-a (blake3 ") && out.contains(", now the store's tokenizer\n"), "{out}");
    assert_eq!(token_counts(db), vec![("[1]".into(), Some(141)), ("[2]".into(), Some(141))]);
    let (_, out, _) = run(&["info", "--db", db]);
    assert!(out.contains("\nTokenizer:  tok-a (blake3 "), "{out}");

    // a different tokenizer is refused with both names and the way out; nothing is written
    let (code, _, err) = run(&["import", "--db", db, "--tokenizer", &b, second.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.starts_with(&format!("Error: tokenizer mismatch: the store {db} counts tokens with tok-a (blake3 ")), "{err}");
    assert!(err.contains(&format!("and --tokenizer {b} is tok-b (blake3 ")), "{err}");
    assert!(err.contains("Either leave out --tokenizer to count with the store's tok-a"), "{err}");
    assert!(err.contains(&format!("pipeline recompute --db {db} --tokenizer {b}")), "{err}");
    assert_eq!(token_counts(db).len(), 2);

    // leaving the flag out counts with the store's own copy
    let (code, out, _) = run(&["import", "--db", db, second.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains(", the store's tokenizer\n"), "{out}");
    assert_eq!(token_counts(db)[2], ("[3]".into(), Some(141)));

    // tokens is a feature like any other
    let (_, out, _) = run(&["count", "--db", db, "tokens == 141 && bytes / tokens > 1"]);
    assert_eq!(out, "3 / 3 documents match (100.000%).\n");
    let (_, out, _) = run(&["stats", "--db", db]);
    assert!(out.contains("| tokens "), "{out}");
    assert!(out.contains("| - tokens:    423\n"), "{out}");
    let (code, _, _) = run(&["check", "--db", db, "--deep"]);
    assert_eq!(code, 0);
}

#[test]
fn recompute_switches_the_tokenizer() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(dir.path(), "in.jsonl", &[doc(1), doc(2)]);
    let db_path = dir.path().join("db");
    let db = db_path.to_str().unwrap();
    let (a, b) = (tok("tok-a"), tok("tok-b"));
    run(&["import", "--db", db, "--tokenizer", &a, input.to_str().unwrap()]);

    // a dry run changes nothing
    let (code, out, _) = run(&["recompute", "--db", db, "--tokenizer", &b, "--dry-run"]);
    assert_eq!(code, 0);
    assert!(out.contains("Tokenizer: would switch from tok-a (blake3 ") && out.contains("Dry run: nothing was written."), "{out}");
    assert_eq!(token_counts(db)[0].1, Some(141));

    // the switch recounts with tok-b (131 by HF) and makes it the store's
    let (code, out, err) = run(&["recompute", "--db", db, "--tokenizer", &b]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("Switching the tokenizer from tok-a (blake3 "), "{err}");
    assert!(out.contains("Tokenizer: switched from tok-a (blake3 ") && out.contains("to tok-b (blake3 "), "{out}");
    assert_eq!(token_counts(db), vec![("[1]".into(), Some(131)), ("[2]".into(), Some(131))]);
    let (_, out, _) = run(&["info", "--db", db]);
    assert!(out.contains("\nTokenizer:  tok-b (blake3 "), "{out}");
    let (code, out, _) = run(&["check", "--db", db, "--deep"]);
    assert_eq!(code, 0, "{out}");

    // and now tok-a is the one refused
    let (code, _, err) = run(&["import", "--db", db, "--tokenizer", &a, input.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains("the store") && err.contains("counts tokens with tok-b"), "{err}");
}

#[test]
fn stores_without_a_tokenizer_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let input = write(dir.path(), "in.jsonl", &[doc(1), doc(2), doc(3)]);
    let more = write(dir.path(), "more.jsonl", &[doc(4)]);
    let db_path = dir.path().join("db");
    let db = db_path.to_str().unwrap();
    let a = tok("tok-a");

    let (code, out, _) = run(&["import", "--db", db, input.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(!out.contains("Tokens:"), "{out}");
    assert!(token_counts(db).iter().all(|(_, t)| t.is_none()));
    let (_, out, _) = run(&["info", "--db", db]);
    assert!(out.contains("\nTokenizer:  none\n"), "{out}");
    let (_, out, _) = run(&["stats", "--db", db]);
    assert!(!out.contains("- tokens:"), "no tokens total without counts: {out}");

    // adding a tokenizer to a store that has documents is a recompute, not an import
    let (code, _, err) = run(&["import", "--db", db, "--tokenizer", &a, more.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(err.contains(&format!("the store {db} already holds 3 documents without token counts")), "{err}");
    assert!(err.contains(&format!("pipeline recompute --db {db} --tokenizer {a}\nthen run this import again.")), "{err}");
    assert_eq!(token_counts(db).len(), 3, "nothing imported");

    let (code, out, _) = run(&["recompute", "--db", db, "--tokenizer", &a]);
    assert_eq!(code, 0);
    assert!(out.contains("Tokenizer: switched from no tokenizer to tok-a"), "{out}");
    let (code, _, _) = run(&["import", "--db", db, more.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(token_counts(db).iter().all(|(_, t)| *t == Some(141)));

    // a dry run counts with the given tokenizer without a store
    let (code, out, _) = run(&["import", "--db", "unused", "--dry-run", "--tokenizer", &a, input.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("Tokens: counted with tok-a (blake3 ") && !out.contains("store's tokenizer"), "{out}");
}
