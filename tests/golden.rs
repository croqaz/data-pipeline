//! The Rust features must match tests/reference.py byte for byte.

use std::collections::BTreeMap;

use pipeline::features::{self, Scratch};

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    text: String,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    drop: Option<String>,
    len: Option<u32>,
    bytes: Option<u32>,
    words: Option<u32>,
    #[serde(rename = "uniqWords")]
    uniq_words: Option<u32>,
    lines: Option<u32>,
    sentences: Option<u32>,
    #[serde(rename = "uniqChars")]
    uniq_chars: Option<u32>,
    vowel: Option<u16>,
    alpha: Option<u16>,
    entropy: Option<u16>,
    #[serde(rename = "meanWordLen")]
    mean_word_len: Option<u16>,
    #[serde(rename = "stopWords")]
    stop_words: Option<u16>,
    dup5gram: Option<u16>,
    quality: Option<u16>,
}

#[test]
fn golden() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let cases = std::fs::read_to_string(format!("{dir}/golden.jsonl")).unwrap();
    let expected: BTreeMap<String, Expected> =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/golden_expected.json")).unwrap()).unwrap();
    let mut scratch = Scratch::default();
    let mut checked = 0;
    for line in cases.lines() {
        let case: Case = serde_json::from_str(line).unwrap();
        let want = &expected[&case.name];
        match features::compute(&case.text, features::DEFAULT_MAX_LEN, &mut scratch) {
            Err(reason) => assert_eq!(Some(reason.as_str().to_string()), want.drop, "{}", case.name),
            Ok(f) => {
                assert!(want.drop.is_none(), "{} should have been dropped", case.name);
                let meta = f.to_meta(0, false);
                let got = Expected {
                    drop: None,
                    len: Some(meta.len),
                    bytes: Some(meta.bytes),
                    words: Some(meta.words),
                    uniq_words: Some(meta.uniq_words),
                    lines: meta.lines,
                    sentences: Some(meta.sentences),
                    uniq_chars: Some(meta.uniq_chars),
                    vowel: Some(meta.vowel),
                    alpha: Some(meta.alpha),
                    entropy: meta.entropy,
                    mean_word_len: meta.mean_word_len,
                    stop_words: meta.stop_words,
                    dup5gram: meta.dup5gram,
                    quality: meta.quality,
                };
                assert_eq!(&got, want, "{}", case.name);
            }
        }
        checked += 1;
    }
    assert_eq!(checked, expected.len());
}
