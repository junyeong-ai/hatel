//! Drop accounting shared by the export queue, the tool buffer, and the unresolved-egress
//! counter: surface the first drop immediately, then once per `every` drops thereafter — so an
//! operator sees a problem before many records are already lost, without per-drop spam.

/// Whether advancing a cumulative drop total from `before` by `added` should emit a log line:
/// the very first drop, and every time the total crosses a multiple of `every`. The division
/// form (not `total % every == 1`) stays correct when `added` > 1 — a whole batch dropped at
/// once — which the tool buffer does.
pub fn should_log(before: u64, added: u64, every: u64) -> bool {
    before == 0 || before / every != (before + added) / every
}

#[cfg(test)]
mod tests {
    use super::should_log;

    #[test]
    fn logs_first_then_every_n() {
        assert!(should_log(0, 1, 100), "first drop logs");
        assert!(!should_log(1, 1, 100));
        assert!(!should_log(98, 1, 100));
        assert!(should_log(99, 1, 100), "crossing 100 logs");
        assert!(!should_log(100, 1, 100));
    }

    #[test]
    fn a_batch_crossing_the_boundary_logs_even_when_added_gt_one() {
        // 50 → 150 jumps past 100 in one step; `% == 1` would miss it, the division form does not.
        assert!(should_log(50, 100, 100));
        // 101 → 150 stays within the same bucket — no log.
        assert!(!should_log(101, 49, 100));
    }
}
