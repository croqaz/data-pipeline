//! End-to-end tests: every subcommand and error path, in-process.

use std::path::{Path, PathBuf};

use pipeline::store;

const PROSE: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

fn run(args: &[&str]) -> (u8, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let full: Vec<&str> = std::iter::once("pipeline").chain(args.iter().copied()).collect();
    let code = pipeline::run_with(full, &mut out, &mut err);
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
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "-s", "default", "-e", "year,title", input.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Indexed:               3"), "{out}");
    let db = db.to_str().unwrap();
    let count = |expr: &str| run(&["count", "--db", db, expr]).1;
    assert_eq!(count("extra.year < 1850"), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("num(extra.year) > 1900"), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("has(extra.title)"), "2 / 3 documents match (66.667%).\n");
    assert_eq!(count("source == \"gut\""), "1 / 3 documents match (33.333%).\n");
    assert_eq!(count("source == \"default\""), "2 / 3 documents match (66.667%).\n");
    assert_eq!(count("source in [\"gut\", \"default\"]"), "3 / 3 documents match (100.000%).\n");
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
        "import", "--db", db.to_str().unwrap(), "--offset", "1", "--limit", "6", "--rejects", rejects.to_str().unwrap(), "--chunk-mb", "1", input.to_str().unwrap(),
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
    let (code, _, err) = run(&["import", "--db", db3.to_str().unwrap(), "--on-error", "abort", "--chunk-mb", "1", "-w", "1", big.to_str().unwrap()]);
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
        let (code, _, err) = run(&["import", "--db", db.to_str().unwrap(), "-w", w, "-e", "k", "--chunk-mb", "1", "--batch-size", "7", input.to_str().unwrap()]);
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
    assert!(out.contains("File: ") && out.contains("b.jsonl.gz"), "{out}");
    assert!(out.contains("Totals: 3 lines, 3 indexed"), "{out}");
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
    let (code, _, err) = run(&["import", "--db", dbs, "--rejects", dir.path().join("no/such/dir.jsonl").to_str().unwrap(), input.to_str().unwrap()]);
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
    let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "-s", "cli", "-e", "year,title,author,words", input.to_str().unwrap()]);
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
        assert!(frank.get("dictHit").is_none());
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
        "export", "--db", &db, "-o", out.to_str().unwrap(), "--header-file", tpl.to_str().unwrap(), "-f", "text,source", "source == \"gut\" || has(extra.year)",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(report.contains("matched 3 (source == \"gut\" || has(extra.year))"), "{report}");
    let docs = read_lines(&out);
    assert_eq!(docs.len(), 3);
    let texts: Vec<&str> = docs.iter().map(|d| d["text"].as_str().unwrap()).collect();
    assert!(texts.iter().any(|t| t.starts_with("---\nyear = 1818\ntitle = Frankenstein\nauthor = Mary Shelley\n---\n\nA The quick")), "{texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("---\nyear = 1900\ntitle = Untitled\n---\n\nB The quick")), "{texts:?}");
    assert!(texts.iter().any(|t| t.starts_with("---\nyear = 1850\ntitle = Quote \"d\"\n---\n\nD The quick")), "{texts:?}");
    assert!(docs.iter().all(|d| d.as_object().unwrap().len() == 2));

    // inline template with escapes, custom separator, one-line header, no match for the web doc
    let out2 = dir.path().join("inline.jsonl");
    let (code, _, err) = run(&[
        "export", "--db", &db, "-o", out2.to_str().unwrap(), "--header", "Title: {extra.title}\\nWords: {words}", "--header-sep", "\\n", "-f", "text,extra.title,words",
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
        assert!(out.contains("Disk:       ") && out.contains("Sources:    3 (") && out.contains("gut") && out.contains("web") && out.contains("cli"), "{out}");
        assert!(out.contains("Features:   len, bytes, words, uniqWords, sentences, uniqChars, vowel, alpha, entropy, compress, dictHit (schema 1)\n"), "{out}");
        assert!(out.contains("\nmeta:       ") && out.contains("\ntext:       "), "{out}");

        let (_, ids, _) = run(&["export", "--db", &db, "-o", "-", "-f", "id", "--limit", "1"]);
        let id: serde_json::Value = serde_json::from_str(ids.trim()).unwrap();
        let id = id["id"].as_str().unwrap().to_string();
        let (code, out, err) = run(&["show", "--db", &db, &id]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with(&format!("id:        {id}\nsource:    ")), "{out}");
        assert!(out.contains("\nfeatures:  len ") && out.contains("  vowel ") && out.contains("  alpha ") && out.contains("\n--- text ("), "{out}");
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
