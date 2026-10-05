//! `pipeline count`: how many documents match an expression.

use crate::error::Result;
use crate::stats::{ScanConfig, scan};
use crate::store::{Sources, Store};
use crate::util::fmt_int;

pub struct CountReport {
    pub scanned: u64,
    pub matched: u64,
}

impl CountReport {
    pub fn pct(&self) -> f64 {
        if self.scanned == 0 { 0.0 } else { self.matched as f64 * 100.0 / self.scanned as f64 }
    }

    pub fn render(&self) -> String {
        format!("{} / {} documents match ({:.3}%).\n", fmt_int(self.matched), fmt_int(self.scanned), self.pct())
    }
}

pub fn count(store: &dyn Store, sources: &Sources, cfg: &ScanConfig<'_>) -> Result<CountReport> {
    let (scanned, matched, _) = scan(store, sources, cfg, || (), |(), _, _| true)?;
    Ok(CountReport { scanned, matched })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_math() {
        let r = CountReport { scanned: 0, matched: 0 };
        assert_eq!(r.pct(), 0.0);
        assert_eq!(r.render(), "0 / 0 documents match (0.000%).\n");
        let r = CountReport { scanned: 3000, matched: 1000 };
        assert!((r.pct() - 33.333).abs() < 0.001);
        assert!(r.render().starts_with("1,000 / 3,000"));
    }
}
