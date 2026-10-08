//! The optional tokenizer a store counts `tokens` with.
//!
//! A store has at most one, kept as a single store-wide setting: the name
//! of the folder it came from, the BLAKE3 hash of its `tokenizer.json`, and
//! the file itself, so the store never depends on the original path.
//!
//! - `import --tokenizer DIR` sets it, on a store with no documents yet.
//! - Every later `import` and `recompute` counts with the stored copy, so all
//!   documents of a store are counted by the same tokenizer. Passing a
//!   different `--tokenizer` to `import` is an error that names both.
//! - `recompute --tokenizer DIR` switches it. The new one is written as
//!   "pending" first, every document is recounted, then it replaces the old
//!   one. A switch that stops halfway leaves "pending" behind: `import`
//!   refuses to run, `check` reports it, and a plain `recompute` finishes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use minicbor::{Decode, Encode};

use crate::error::{Error, Result};
use crate::store::Store;

/// The setting holding the store's tokenizer.
pub const CURRENT: &str = "tokenizer";
/// The setting holding the target of an unfinished switch.
pub const PENDING: &str = "tokenizer.pending";
/// The file a tokenizer folder must contain.
pub const FILE: &str = "tokenizer.json";

/// One tokenizer, as stored: where it came from and the file itself.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct TokenizerSpec {
    /// The folder name, `t-v5` for `../tokenizers/t-v5/`.
    #[n(0)]
    pub name: String,
    /// BLAKE3 of `tokenizer.json`, 64 hex characters. Two specs are the same
    /// tokenizer exactly when these match.
    #[n(1)]
    pub blake3: String,
    /// The whole `tokenizer.json`.
    #[n(2)]
    pub json: String,
}

/// A tokenizer ready to count with.
#[derive(Clone)]
pub struct Loaded {
    pub spec: TokenizerSpec,
    pub tokenizer: Arc<tokie::Tokenizer>,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").field("spec", &self.spec.label()).finish()
    }
}

impl TokenizerSpec {
    /// Read `--tokenizer PATH`: a folder holding `tokenizer.json` (the usual
    /// case, named after the folder) or the file itself (named after the
    /// folder it is in).
    pub fn from_path(path: &Path) -> Result<Loaded> {
        let file = if path.is_dir() { path.join(FILE) } else { path.to_path_buf() };
        if !file.is_file() {
            let what =
                if path.is_dir() { format!("{} has no {FILE}", path.display()) } else { format!("{} does not exist", path.display()) };
            return Err(Error::Usage(format!("--tokenizer {}: {what}; pass a folder that contains a {FILE}", path.display())));
        }
        let bytes = std::fs::read(&file)?;
        let json = String::from_utf8(bytes)
            .map_err(|_| Error::Usage(format!("--tokenizer {}: {} is not UTF-8 text", path.display(), file.display())))?;
        let folder: PathBuf = std::fs::canonicalize(&file)?.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "tokenizer".to_string());
        let spec = TokenizerSpec { name, blake3: blake3::hash(json.as_bytes()).to_hex().to_string(), json };
        let tokenizer = parse(&spec.json).map_err(|e| {
            Error::Usage(format!("--tokenizer {}: {} is not a tokenizer this build can read: {e}", path.display(), file.display()))
        })?;
        Ok(Loaded { spec, tokenizer })
    }

    /// The first 12 hex characters of the hash, enough to tell two apart.
    pub fn short_hash(&self) -> &str {
        &self.blake3[..12.min(self.blake3.len())]
    }

    /// `t-v5 (blake3 1a2b3c4d5e6f)`.
    pub fn label(&self) -> String {
        format!("{} (blake3 {})", self.name, self.short_hash())
    }

    pub fn same_as(&self, other: &TokenizerSpec) -> bool {
        self.blake3 == other.blake3
    }

    /// Parse the stored file; it was valid when it was stored.
    pub fn load(&self) -> Result<Loaded> {
        let tokenizer =
            parse(&self.json).map_err(|e| Error::Store(format!("the store's tokenizer {} no longer loads: {e}", self.label())))?;
        Ok(Loaded { spec: self.clone(), tokenizer })
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("encoding into a Vec cannot fail")
    }

    pub fn decode(bytes: &[u8], setting: &str) -> Result<TokenizerSpec> {
        minicbor::decode(bytes).map_err(|e| Error::Store(format!("the store's {setting:?} setting does not decode: {e}")))
    }
}

fn parse(json: &str) -> std::result::Result<Arc<tokie::Tokenizer>, String> {
    tokie::hf::from_json_str(json).map(Arc::new).map_err(|e| e.to_string())
}

/// Count the tokens of `text`: the text's own tokens, with no BOS, EOS or
/// other template tokens added.
pub fn count(tokenizer: &tokie::Tokenizer, text: &str) -> u32 {
    u32::try_from(tokenizer.count_tokens(text)).unwrap_or(u32::MAX)
}

/// What a store says about its tokenizer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub current: Option<TokenizerSpec>,
    /// The target of a switch that has not finished.
    pub pending: Option<TokenizerSpec>,
}

impl State {
    pub fn read(store: &dyn Store) -> Result<State> {
        let get = |name: &str| store.setting(name)?.map(|b| TokenizerSpec::decode(&b, name)).transpose();
        Ok(State { current: get(CURRENT)?, pending: get(PENDING)? })
    }

    /// One line for `info`.
    pub fn describe(&self) -> String {
        let cur = self.current.as_ref().map_or("none".to_string(), TokenizerSpec::label);
        match &self.pending {
            None => cur,
            Some(p) => format!("{cur}; unfinished switch to {}, run `recompute` to finish it", p.label()),
        }
    }
}

fn store_label(db: &Path) -> String {
    db.display().to_string()
}

/// The error for a store whose tokenizer switch did not finish.
pub fn unfinished_switch(db: &Path, state: &State) -> Error {
    let pending = state.pending.as_ref().expect("called with a pending switch");
    let from = state.current.as_ref().map_or("no tokenizer".to_string(), TokenizerSpec::label);
    Error::Usage(format!(
        "the store {} is switching its tokenizer from {from} to {}, and that switch did not finish: \
         some documents are counted with each.\n\
         Finish the switch first (every document is recounted with {}):\n  pipeline recompute --db {}",
        store_label(db),
        pending.label(),
        pending.name,
        store_label(db)
    ))
}

/// Which tokenizer an import counts with. `docs` is how many documents the
/// store already holds. Returns the tokenizer and whether this import sets
/// it on the store (the caller writes it before indexing anything).
///
/// `given` is `--tokenizer`, already read with [`TokenizerSpec::from_path`]
/// so a bad path fails before a store is created.
pub fn for_import(db: &Path, state: &State, given: Option<(&Path, Loaded)>, docs: u64) -> Result<Option<(Loaded, bool)>> {
    if state.pending.is_some() {
        return Err(unfinished_switch(db, state));
    }
    match (&state.current, given) {
        (None, None) => Ok(None),
        (Some(cur), None) => Ok(Some((cur.load()?, false))),
        (Some(cur), Some((_, new))) if cur.same_as(&new.spec) => Ok(Some((new, false))),
        (Some(cur), Some((path, new))) => Err(mismatch(db, cur, &new.spec, path)),
        (None, Some((_, new))) if docs == 0 => Ok(Some((new, true))),
        (None, Some((path, new))) => Err(Error::Usage(format!(
            "the store {} already holds {} documents without token counts, and --tokenizer {} would count \
             only the new ones.\n\
             Count the existing documents first, which also sets the store's tokenizer to {}:\n  \
             pipeline recompute --db {} --tokenizer {}\n\
             then run this import again.",
            store_label(db),
            crate::util::fmt_int(docs),
            path.display(),
            new.spec.label(),
            store_label(db),
            path.display()
        ))),
    }
}

fn mismatch(db: &Path, cur: &TokenizerSpec, new: &TokenizerSpec, path: &Path) -> Error {
    let path = path.display();
    let what = if cur.name == new.name {
        format!(
            "--tokenizer {path} is also called {} but is a different file (blake3 {}): it changed after the store was set up",
            new.name,
            new.short_hash()
        )
    } else {
        format!("--tokenizer {path} is {}", new.label())
    };
    Error::Usage(format!(
        "tokenizer mismatch: the store {} counts tokens with {}, and {what}.\n\
         Every document in a store is counted with the same tokenizer. Either leave out --tokenizer \
         to count with the store's {}, or switch the whole store to the new one first \
         (every document is recounted):\n  pipeline recompute --db {} --tokenizer {path}",
        store_label(db),
        cur.label(),
        cur.name,
        store_label(db)
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::NullStore;

    pub const PROSE: &str = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! \
How vexingly quick daft zebras jump. The five boxing wizards jump quickly.";

    /// `tests/fixtures/tok-a` or `tok-b`: tiny byte-level BPE tokenizers made
    /// by `tests/make_tokenizers.py`.
    pub fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    pub fn loaded(name: &str) -> Loaded {
        TokenizerSpec::from_path(&fixture(name)).unwrap()
    }

    fn usage(e: Error) -> String {
        match e {
            Error::Usage(m) => m,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn counts_match_hugging_face() {
        // Reference counts from HF `tokenizers` 0.22 in Python, no special tokens added.
        let (a, b) = (loaded("tok-a"), loaded("tok-b"));
        assert_eq!((count(&a.tokenizer, PROSE), count(&b.tokenizer, PROSE)), (137, 127));
        assert_eq!((count(&a.tokenizer, &format!("[1] {PROSE}")), count(&b.tokenizer, &format!("[1] {PROSE}"))), (141, 131));
        assert_eq!(count(&a.tokenizer, "<|bos|>Hello<|eos|>"), 7, "added tokens in the text count once each");
        assert_eq!(count(&a.tokenizer, ""), 0);
    }

    #[test]
    fn a_folder_or_its_file_is_the_same_tokenizer() {
        let by_dir = loaded("tok-a");
        let by_file = TokenizerSpec::from_path(&fixture("tok-a").join(FILE)).unwrap();
        assert_eq!(by_dir.spec, by_file.spec);
        assert_eq!(by_dir.spec.name, "tok-a");
        let bytes = std::fs::read(fixture("tok-a").join(FILE)).unwrap();
        assert_eq!(by_dir.spec.blake3, blake3::hash(&bytes).to_hex().to_string());
        assert_eq!(by_dir.spec.short_hash().len(), 12);
        assert_eq!(by_dir.spec.label(), format!("tok-a (blake3 {})", by_dir.spec.short_hash()));
        assert!(!by_dir.spec.same_as(&loaded("tok-b").spec));
        assert!(format!("{by_dir:?}").contains("tok-a"));
    }

    #[test]
    fn bad_paths_say_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let e = usage(TokenizerSpec::from_path(dir.path()).unwrap_err());
        assert!(e.contains("has no tokenizer.json") && e.contains("pass a folder that contains a tokenizer.json"), "{e}");
        let e = usage(TokenizerSpec::from_path(&dir.path().join("nope")).unwrap_err());
        assert!(e.contains("nope does not exist"), "{e}");
        std::fs::write(dir.path().join(FILE), "{\"not\": \"a tokenizer\"}").unwrap();
        let e = usage(TokenizerSpec::from_path(dir.path()).unwrap_err());
        assert!(e.contains("is not a tokenizer this build can read"), "{e}");
        std::fs::write(dir.path().join(FILE), b"\xff\xfe").unwrap();
        let e = usage(TokenizerSpec::from_path(dir.path()).unwrap_err());
        assert!(e.contains("is not UTF-8 text"), "{e}");
    }

    #[test]
    fn spec_round_trips_and_state_reads_both_settings() {
        let store = NullStore::default();
        assert_eq!(State::read(&store).unwrap(), State::default());
        assert_eq!(State::default().describe(), "none");
        let (a, b) = (loaded("tok-a").spec, loaded("tok-b").spec);
        assert_eq!(TokenizerSpec::decode(&a.encode(), CURRENT).unwrap(), a);
        store.put_setting(CURRENT, Some(&a.encode())).unwrap();
        store.put_setting(PENDING, Some(&b.encode())).unwrap();
        let state = State::read(&store).unwrap();
        assert_eq!((state.current.as_ref(), state.pending.as_ref()), (Some(&a), Some(&b)));
        assert_eq!(state.describe(), format!("{}; unfinished switch to {}, run `recompute` to finish it", a.label(), b.label()));
        store.put_setting(PENDING, Some(b"\xff")).unwrap();
        assert!(matches!(State::read(&store), Err(Error::Store(m)) if m.contains("tokenizer.pending")));
        let broken = TokenizerSpec { json: "{}".into(), ..a };
        assert!(matches!(broken.load(), Err(Error::Store(m)) if m.contains("no longer loads")));
    }

    #[test]
    fn import_rules() {
        let db = Path::new("/data/corpus");
        let (a, b) = (loaded("tok-a"), loaded("tok-b"));
        let pa = fixture("tok-a");
        let pb = fixture("tok-b");
        let with = |cur: Option<&Loaded>| State { current: cur.map(|l| l.spec.clone()), pending: None };

        // no tokenizer anywhere: no tokens, as before
        assert!(for_import(db, &State::default(), None, 5).unwrap().is_none());
        // a new store takes the given one
        let (t, set) = for_import(db, &State::default(), Some((&pa, a.clone())), 0).unwrap().unwrap();
        assert_eq!((t.spec.name.as_str(), set), ("tok-a", true));
        // later imports count with the store's own, with or without the flag
        let (t, set) = for_import(db, &with(Some(&a)), None, 5).unwrap().unwrap();
        assert_eq!((t.spec.name.as_str(), set), ("tok-a", false));
        let (t, set) = for_import(db, &with(Some(&a)), Some((&pa, a.clone())), 5).unwrap().unwrap();
        assert_eq!((t.spec.name.as_str(), set), ("tok-a", false));

        // a different tokenizer: refused, naming both and the way out
        let e = usage(for_import(db, &with(Some(&a)), Some((&pb, b.clone())), 5).unwrap_err());
        assert!(
            e.starts_with(&format!("tokenizer mismatch: the store /data/corpus counts tokens with {}, and --tokenizer ", a.spec.label())),
            "{e}"
        );
        assert!(e.contains(&format!("tok-b is {}.", b.spec.label())), "{e}");
        assert!(e.contains("Either leave out --tokenizer to count with the store's tok-a"), "{e}");
        assert!(e.ends_with(&format!("pipeline recompute --db /data/corpus --tokenizer {}", pb.display())), "{e}");
        // the same folder name with other content: the file changed
        let edited = Loaded { spec: TokenizerSpec { name: "tok-a".into(), ..b.spec.clone() }, tokenizer: b.tokenizer.clone() };
        let e = usage(for_import(db, &with(Some(&a)), Some((&pa, edited)), 5).unwrap_err());
        assert!(e.contains(&format!("is also called tok-a but is a different file (blake3 {})", b.spec.short_hash())), "{e}");
        assert!(e.contains("it changed after the store was set up"), "{e}");

        // a store that already has documents without counts
        let e = usage(for_import(db, &State::default(), Some((&pa, a.clone())), 2_000).unwrap_err());
        assert!(e.starts_with("the store /data/corpus already holds 2,000 documents without token counts"), "{e}");
        assert!(
            e.contains(&format!("pipeline recompute --db /data/corpus --tokenizer {}\nthen run this import again.", pa.display())),
            "{e}"
        );

        // an unfinished switch blocks every import, flag or not
        let half = State { current: Some(a.spec.clone()), pending: Some(b.spec.clone()) };
        for given in [None, Some((pa.as_path(), a.clone())), Some((pb.as_path(), b.clone()))] {
            let e = usage(for_import(db, &half, given, 5).unwrap_err());
            assert!(e.starts_with(&format!(
                "the store /data/corpus is switching its tokenizer from {} to {}, and that switch did not finish",
                a.spec.label(),
                b.spec.label()
            )));
            assert!(e.ends_with("pipeline recompute --db /data/corpus"), "{e}");
        }
        let from_none = State { current: None, pending: Some(b.spec.clone()) };
        let e = usage(for_import(db, &from_none, None, 5).unwrap_err());
        assert!(e.contains("from no tokenizer to tok-b"), "{e}");
    }
}
