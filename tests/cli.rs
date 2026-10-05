//! End-to-end tests: every subcommand and error path, in-process, on both
//! backends.

use std::path::{Path, PathBuf};

use pipeline::store::{self, Backend};

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

fn backends() -> Vec<&'static str> {
    vec!["fjall", "rocksdb"]
}

#[test]
fn import_stats_count_on_both_backends() {
    for backend in backends() {
        let dir = tempfile::tempdir().unwrap();
        let input = mixed_file(dir.path());
        let db = dir.path().join("db");
        let (code, out, err) = run(&["import", "--db", db.to_str().unwrap(), "--backend", backend, "-w", "2", input.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("Lines:                 9  (0 skipped by --offset, 1 empty)"), "{out}");
        assert!(out.contains("bad_json 1, not_object 1, no_text 1, too_short 1"), "{out}");
        assert!(out.contains("Duplicates:            1"), "{out}");
        assert!(out.contains("Indexed:               3"), "{out}");
        assert!(out.contains(&format!("Store: {backend} backend")), "{out}");
        assert_eq!(std::fs::read_to_string(store::marker_path(&db)).unwrap().trim(), backend);

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

        // backend mismatch is a usage error
        let other = if backend == "fjall" { "rocksdb" } else { "fjall" };
        let (code, _, err) = run(&["count", "--db", db.to_str().unwrap(), "--backend", other]);
        assert_eq!(code, 2);
        assert!(err.contains("does not match"), "{err}");

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
    let store = store::open(db, None, false).unwrap();
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
    assert!(Backend::Fjall.available());
}
