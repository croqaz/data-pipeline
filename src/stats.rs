//! `pipeline stats`: a parallel scan of the `meta` keyspace into per-feature
//! histograms, plus the shared scan machinery used by `count`.

use std::collections::HashMap;

use hdrhistogram::Histogram;

use crate::error::{Error, Result};
use crate::expr::{Compiled, RecordRow};
use crate::hash::Key;
use crate::meta::{FEATURES, Meta};
use crate::store::{Sources, Store};
use crate::util::{fmt_int, fmt_num};

pub struct ScanConfig<'a> {
    pub filter: Option<&'a Compiled>,
    pub threads: usize,
    pub limit: Option<u64>,
}

/// Key range for thread `t` of `n`, splitting on the first key byte.
pub fn range_for(t: usize, n: usize) -> Option<(Key, Option<Key>)> {
    let lo = t * 256 / n;
    let hi = (t + 1) * 256 / n;
    if lo >= hi {
        return None;
    }
    let mut lo_key = [0u8; 16];
    lo_key[0] = lo as u8;
    let hi_key = if hi >= 256 {
        None
    } else {
        let mut k = [0u8; 16];
        k[0] = hi as u8;
        Some(k)
    };
    Some((lo_key, hi_key))
}

/// Does `filter` accept this record? `text` and `extra` are read only when
/// the expression refers to them.
pub fn matches(store: &dyn Store, sources: &Sources, filter: &Compiled, key: &Key, meta: &Meta) -> Result<bool> {
    let text = if filter.needs_text { store.get_text(key)? } else { None };
    let text_str = match &text {
        Some(t) => Some(std::str::from_utf8(t).map_err(|_| Error::Store("stored text is not UTF-8".into()))?),
        None => None,
    };
    let extra = if filter.needs_extra { store.get_extra(key)? } else { None };
    let row = RecordRow { meta, key, source: sources.name(meta.source), text: text_str, extra: extra.as_deref() };
    Ok(filter.matches(&row))
}

fn scan_range<A>(
    store: &dyn Store,
    sources: &Sources,
    cfg: &ScanConfig<'_>,
    lo: &Key,
    hi: Option<&Key>,
    acc: &mut A,
    step: &(impl Fn(&mut A, &Key, &Meta) -> bool + Sync),
) -> Result<(u64, u64)> {
    let mut scanned = 0u64;
    let mut matched = 0u64;
    store.scan_meta(lo, hi, &mut |key, bytes| {
        scanned += 1;
        let meta = Meta::decode(bytes)?;
        let keep = match cfg.filter {
            None => true,
            Some(f) => matches(store, sources, f, key, &meta)?,
        };
        if keep {
            matched += 1;
            if !step(acc, key, &meta) {
                return Ok(false);
            }
        }
        Ok(cfg.limit.is_none_or(|l| scanned < l))
    })?;
    Ok((scanned, matched))
}

/// Scan every record, in parallel key ranges, feeding matched records to
/// `step`, which returns `false` to stop its range early. Returns (scanned,
/// matched, one accumulator per thread, in key order).
pub fn scan<A: Send>(
    store: &dyn Store,
    sources: &Sources,
    cfg: &ScanConfig<'_>,
    init: impl Fn() -> A + Sync,
    step: impl Fn(&mut A, &Key, &Meta) -> bool + Sync,
) -> Result<(u64, u64, Vec<A>)> {
    let threads = if cfg.limit.is_some() { 1 } else { cfg.threads.clamp(1, 256) };
    let step = &step;
    let results: Vec<Result<(u64, u64, A)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .filter_map(|t| range_for(t, threads))
            .map(|(lo, hi)| {
                let init = &init;
                s.spawn(move || {
                    let mut acc = init();
                    let (scanned, matched) = scan_range(store, sources, cfg, &lo, hi.as_ref(), &mut acc, step)?;
                    Ok((scanned, matched, acc))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("scan thread panicked")).collect()
    });
    let mut scanned = 0;
    let mut matched = 0;
    let mut accs = Vec::new();
    for r in results {
        let (s, m, a) = r?;
        scanned += s;
        matched += m;
        accs.push(a);
    }
    Ok((scanned, matched, accs))
}

pub struct FeatureAcc {
    pub present: u64,
    pub sum: f64,
    pub min: u64,
    pub max: u64,
    pub hist: Histogram<u64>,
}

impl FeatureAcc {
    fn new() -> Self {
        Self { present: 0, sum: 0.0, min: u64::MAX, max: 0, hist: Histogram::new(3).expect("valid sigfig") }
    }

    fn add(&mut self, v: u64) {
        self.present += 1;
        self.sum += v as f64;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.hist.record(v).expect("auto-resizing histogram accepts any u64");
    }

    fn merge(&mut self, o: &FeatureAcc) {
        self.present += o.present;
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.hist.add(&o.hist).expect("histograms share a configuration");
    }

    pub fn mean(&self) -> f64 {
        if self.present == 0 { 0.0 } else { self.sum / self.present as f64 }
    }

    /// The histogram answers with the top of a value's bucket (3 significant
    /// digits), which can pass the largest value seen: clamp to it, so a P95
    /// never reads above the Max column.
    pub fn percentile(&self, q: f64) -> u64 {
        if self.present == 0 { 0 } else { self.hist.value_at_quantile(q).min(self.max) }
    }
}

pub struct StatsAcc {
    pub features: Vec<FeatureAcc>,
    pub sources: HashMap<u16, u64>,
}

impl StatsAcc {
    pub fn new() -> Self {
        Self { features: FEATURES.iter().map(|_| FeatureAcc::new()).collect(), sources: HashMap::new() }
    }

    pub fn add(&mut self, meta: &Meta) {
        *self.sources.entry(meta.source).or_insert(0) += 1;
        for (acc, def) in self.features.iter_mut().zip(FEATURES) {
            if let Some(v) = (def.raw)(meta) {
                acc.add(v);
            }
        }
    }

    pub fn merge(&mut self, o: &StatsAcc) {
        for (a, b) in self.features.iter_mut().zip(&o.features) {
            a.merge(b);
        }
        for (k, v) in &o.sources {
            *self.sources.entry(*k).or_insert(0) += v;
        }
    }
}

impl Default for StatsAcc {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StatsReport {
    pub scanned: u64,
    pub matched: u64,
    pub acc: StatsAcc,
}

pub fn collect(store: &dyn Store, sources: &Sources, cfg: &ScanConfig<'_>) -> Result<StatsReport> {
    let (scanned, matched, accs) = scan(store, sources, cfg, StatsAcc::new, |acc, _, meta| {
        acc.add(meta);
        true
    })?;
    let mut acc = StatsAcc::new();
    for a in &accs {
        acc.merge(a);
    }
    Ok(StatsReport { scanned, matched, acc })
}

pub fn render(rep: &StatsReport, sources: &Sources, expr: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(&format!("| Scanned: {} documents\n", fmt_int(rep.scanned)));
    if let Some(e) = expr {
        s.push_str(&format!("| Matched: {} ({e})\n", fmt_int(rep.matched)));
    }
    let mut src: Vec<(u16, u64)> = rep.acc.sources.iter().map(|(k, v)| (*k, *v)).collect();
    src.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if !src.is_empty() {
        s.push_str(&format!("| Sources ({}):\n", src.len()));
        for (id, n) in src {
            s.push_str(&format!("| - {}: {}\n", sources.name(id), fmt_int(n)));
        }
    }
    s.push_str("|\n");
    let headers = ["Feature", "Present", "Mean", "Min", "P5", "P50", "P95", "Max"];
    let rows: Vec<Vec<String>> = FEATURES
        .iter()
        .zip(&rep.acc.features)
        .map(|(def, acc)| {
            let d = def.decimals();
            let cell = |v: f64| fmt_num(def.present(v), d);
            vec![
                def.name.to_string(),
                fmt_int(acc.present),
                cell(acc.mean()),
                cell(if acc.present == 0 { 0.0 } else { acc.min as f64 }),
                cell(acc.percentile(0.05) as f64),
                cell(acc.percentile(0.50) as f64),
                cell(acc.percentile(0.95) as f64),
                cell(acc.max as f64),
            ]
        })
        .collect();
    let widths: Vec<usize> =
        (0..headers.len()).map(|i| rows.iter().map(|r| r[i].len()).chain(std::iter::once(headers[i].len())).max().unwrap_or(0)).collect();
    let fmt_row = |cells: &[String]| {
        let parts: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| if i == 0 { format!("{:<w$}", c, w = widths[i]) } else { format!("{:>w$}", c, w = widths[i]) })
            .collect();
        format!("| {} |\n", parts.join(" | "))
    };
    s.push_str(&fmt_row(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>()));
    s.push_str(&format!("|{}|\n", widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("|")));
    for r in &rows {
        s.push_str(&fmt_row(r));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_the_key_space_once() {
        for n in [1usize, 2, 3, 16, 255, 256, 300] {
            let mut next = 0usize;
            let mut last_open = false;
            for t in 0..n {
                if let Some((lo, hi)) = range_for(t, n) {
                    assert_eq!(lo[0] as usize, next, "n={n} t={t}");
                    assert!(!last_open);
                    match hi {
                        Some(h) => next = h[0] as usize,
                        None => last_open = true,
                    }
                }
            }
            assert!(last_open, "n={n}");
        }
        assert!(range_for(300, 300).is_none());
    }

    #[test]
    fn feature_acc_math() {
        let mut a = FeatureAcc::new();
        assert_eq!(a.mean(), 0.0);
        assert_eq!(a.percentile(0.5), 0);
        for v in [10, 20, 30] {
            a.add(v);
        }
        let mut b = FeatureAcc::new();
        b.add(40);
        a.merge(&b);
        assert_eq!(a.present, 4);
        assert_eq!(a.mean(), 25.0);
        assert_eq!(a.min, 10);
        assert_eq!(a.max, 40);
        assert!(a.percentile(0.5) >= 20 && a.percentile(0.5) <= 30);
        assert_eq!(a.percentile(0.95), 40);
        // 10000 shares a bucket with 10007: the percentile stops at the max
        let mut full = FeatureAcc::new();
        for _ in 0..20 {
            full.add(10_000);
        }
        assert_eq!(full.percentile(0.95), 10_000);
    }

    #[test]
    fn stats_acc_counts_sources_and_optional_features() {
        let mut acc = StatsAcc::default();
        acc.add(&Meta { source: 1, words: 5, ..Default::default() });
        acc.add(&Meta { source: 1, words: 7, dict_hit: Some(100), ..Default::default() });
        let mut other = StatsAcc::new();
        other.add(&Meta { source: 2, words: 1, ..Default::default() });
        acc.merge(&other);
        assert_eq!(acc.sources[&1], 2);
        assert_eq!(acc.sources[&2], 1);
        let words = &acc.features[crate::meta::feature_index("words").unwrap()];
        assert_eq!(words.present, 3);
        let dict = &acc.features[crate::meta::feature_index("dictHit").unwrap()];
        assert_eq!(dict.present, 1);
    }

    #[test]
    fn render_table_shape() {
        let mut acc = StatsAcc::new();
        acc.add(&Meta { source: 0, words: 50, vowel: 9950, ..Default::default() });
        let mut sources = Sources::default();
        let fake = crate::store::NullStore::default();
        sources.intern("cli", &fake).unwrap();
        let rep = StatsReport { scanned: 1, matched: 1, acc };
        let s = render(&rep, &sources, Some("words > 1"));
        assert!(s.contains("| Scanned: 1 documents"));
        assert!(s.contains("| Matched: 1 (words > 1)"));
        assert!(s.contains("| - cli: 1"));
        assert!(s.contains("| vowel"));
        assert!(s.contains("99.50"));
        let empty = render(&StatsReport { scanned: 0, matched: 0, acc: StatsAcc::new() }, &sources, None);
        assert!(!empty.contains("Matched"));
        assert!(!empty.contains("Sources"));
    }
}
