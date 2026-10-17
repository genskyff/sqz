use anyhow::Result;

pub enum Fit {
    /// 不超过上限的最高质量结果
    Found { quality: u8, data: Vec<u8> },
    /// 质量降到下限仍超出上限；附带试过的最小结果
    Unreachable { quality: u8, data: Vec<u8> },
}

pub struct SearchResult {
    pub fit: Fit,
    pub tries: u32,
}

/// 第一轮怎么试，以及之后怎么在剩下的区间里找。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// 先试上限，失败后在下面二分。适合体积随质量大致单调的编码，如 JPEG
    Ceiling,
    /// 上限走另一条编码路径（PNG 的 q100 是无损），它超限说明不了有损区间：上限失败后
    /// 先试下限，下限也超限就收工。`parallel` 时上限、次上限、下限在第一轮一起并行试
    SplitCeiling { parallel: bool },
    /// 先试下限，超限就收工；可行时从小往大倍增，再在缺口里二分
    Floor,
}

/// 在 [lo, hi] 中找输出不超过 `cap` 字节的最高质量。
///
/// 第一轮的候选见 [`Order`]，交给 `encode` 一起编码。体积随质量大致单调，偶有回落
/// 也不会选中超限的结果：只记录真正满足上限的最高质量。
pub fn fit_size(
    lo: u8,
    hi: u8,
    cap: u64,
    order: Order,
    mut encode: impl FnMut(&[u8]) -> Result<Vec<Vec<u8>>>,
) -> Result<SearchResult> {
    let lo = lo.min(hi);
    // 候选从高到低排列，hi 与 lo 相邻或相等时 dedup 去掉重复
    let mut first = match order {
        Order::Floor => vec![lo],
        Order::SplitCeiling { parallel: true } => vec![hi, hi.saturating_sub(1).max(lo), lo],
        Order::Ceiling | Order::SplitCeiling { parallel: false } => vec![hi],
    };
    first.dedup();
    let batch = encode(&first)?;

    let mut encode_one = |q: u8| -> Result<Vec<u8>> {
        let mut batch = encode(&[q])?;
        Ok(batch.pop().expect("单个质量只应有一条结果"))
    };
    let mut search = Search::new(&mut encode_one, cap);
    search.tries = first.len() as u32;
    for (q, data) in first.into_iter().zip(batch) {
        search.record(q, data);
    }

    match order {
        Order::Floor => {
            // 下限都超限就到此为止，不去试更贵的高质量
            if search.best.is_some() {
                search.gallop(hi)?;
                search.bisect(lo, hi)?;
            }
        }
        Order::SplitCeiling { parallel: false } => {
            let floor_fails = search.best.is_none() && lo < hi && !search.probe(lo)?;
            if !floor_fails {
                search.bisect(lo, hi)?;
            }
        }
        Order::Ceiling | Order::SplitCeiling { parallel: true } => search.bisect(lo, hi)?,
    }
    Ok(search.finish())
}

/// 从已知不超限的 `quality` 往上找，返回 (质量, 数据, 编码次数)。
///
/// 搜索按快速档位估算体积，定稿体积更小，估算时超限的更高质量也可能放得下。
/// `encode` 应按定稿档位编码；先倍增步长，越界后在缺口里二分，最多补测到 `hi`。
pub fn climb(
    quality: u8,
    data: Vec<u8>,
    hi: u8,
    cap: u64,
    mut encode: impl FnMut(u8) -> Result<Vec<u8>>,
) -> Result<(u8, Vec<u8>, u32)> {
    let mut search = Search::new(&mut encode, cap);
    search.record(quality, data);
    search.gallop(hi)?;
    search.bisect(quality, hi)?;
    let tries = search.tries;
    match search.finish().fit {
        Fit::Found { quality, data } => Ok((quality, data, tries)),
        Fit::Unreachable { .. } => unreachable!("起点已经不超限"),
    }
}

type Sample = (u8, Vec<u8>);

/// 搜索过程中已知的两条边界。
struct Search<'a> {
    encode: &'a mut dyn FnMut(u8) -> Result<Vec<u8>>,
    cap: u64,
    tries: u32,
    /// 不超限的最高质量
    best: Option<Sample>,
    /// 超限的最低质量
    fail: Option<Sample>,
}

impl<'a> Search<'a> {
    fn new(encode: &'a mut dyn FnMut(u8) -> Result<Vec<u8>>, cap: u64) -> Self {
        Self {
            encode,
            cap,
            tries: 0,
            best: None,
            fail: None,
        }
    }

    fn record(&mut self, q: u8, data: Vec<u8>) -> bool {
        let fits = data.len() as u64 <= self.cap;
        if fits {
            if self.best.as_ref().is_none_or(|(b, _)| q > *b) {
                self.best = Some((q, data));
            }
        } else if self.fail.as_ref().is_none_or(|(f, _)| q < *f) {
            self.fail = Some((q, data));
        }
        fits
    }

    fn probe(&mut self, q: u8) -> Result<bool> {
        let data = (self.encode)(q)?;
        self.tries += 1;
        Ok(self.record(q, data))
    }

    /// 还没测过的质量区间：已知可行之上、已知超限之下，再夹在 [floor, ceiling] 里。
    fn untested(&self, floor: u8, ceiling: u8) -> (i32, i32) {
        let low = self
            .best
            .as_ref()
            .map_or(i32::from(floor), |(b, _)| i32::from(*b) + 1);
        let high = self
            .fail
            .as_ref()
            .map_or(i32::from(ceiling), |(f, _)| i32::from(*f) - 1);
        (low, high)
    }

    fn bisect(&mut self, floor: u8, ceiling: u8) -> Result<()> {
        let (mut l, mut h) = self.untested(floor, ceiling);
        while l <= h {
            let mid = l + (h - l) / 2;
            if self.probe(mid as u8)? {
                l = mid + 1;
            } else {
                h = mid - 1;
            }
        }
        Ok(())
    }

    /// 从已知可行的质量往上倍增步长，直到超限或到达 `ceiling`。
    fn gallop(&mut self, ceiling: u8) -> Result<()> {
        let mut step = 1;
        while let Some((best, _)) = &self.best {
            let (_, high) = self.untested(*best, ceiling);
            let best = i32::from(*best);
            if best >= high {
                break;
            }
            if !self.probe((best + step).min(high) as u8)? {
                break;
            }
            step *= 2;
        }
        Ok(())
    }

    fn finish(self) -> SearchResult {
        let fit = match (self.best, self.fail) {
            (Some((quality, data)), _) => Fit::Found { quality, data },
            (None, Some((quality, data))) => Fit::Unreachable { quality, data },
            (None, None) => unreachable!("至少编码过一次"),
        };
        SearchResult {
            fit,
            tries: self.tries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARALLEL: Order = Order::SplitCeiling { parallel: true };
    const SEQUENTIAL: Order = Order::SplitCeiling { parallel: false };

    /// 模拟编码器：体积 = 质量 × 10 字节
    fn linear(q: u8) -> Vec<u8> {
        vec![0; usize::from(q) * 10]
    }

    fn single(qs: &[u8]) -> Result<Vec<Vec<u8>>> {
        Ok(qs.iter().copied().map(linear).collect())
    }

    fn found(r: &SearchResult) -> Option<u8> {
        match r.fit {
            Fit::Found { quality, .. } => Some(quality),
            Fit::Unreachable { .. } => None,
        }
    }

    fn size_of(r: &SearchResult) -> usize {
        match &r.fit {
            Fit::Found { data, .. } | Fit::Unreachable { data, .. } => data.len(),
        }
    }

    #[test]
    fn ceiling_fits_in_one_try() {
        let r = fit_size(1, 80, 10_000, Order::Ceiling, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(80), 1));
    }

    #[test]
    fn ceiling_unreachable_bisects_down_to_floor() {
        // JPEG 不提前试下限：常见的可达场景少一次编码，代价是达不到时要二分到底
        let r = fit_size(1, 95, 5, Order::Ceiling, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 7, 10));
    }

    #[test]
    fn parallel_first_round_tries_three_candidates() {
        let r = fit_size(1, 80, 10_000, PARALLEL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(80), 3));
    }

    #[test]
    fn equal_bounds_encode_once() {
        let r = fit_size(50, 50, 10_000, PARALLEL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(50), 1));
        let r = fit_size(50, 51, 10_000, PARALLEL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(51), 2));
        let r = fit_size(50, 50, 5, SEQUENTIAL, single).unwrap();
        assert_eq!((found(&r), r.tries), (None, 1));
    }

    #[test]
    fn sequential_split_stops_when_floor_fails() {
        let r = fit_size(1, 100, 5, SEQUENTIAL, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 2, 10));
    }

    #[test]
    fn sequential_split_reachable_costs_one_extra_try() {
        let r = fit_size(1, 100, 555, SEQUENTIAL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(55), 9));
        let r = fit_size(1, 100, 10_000, SEQUENTIAL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(100), 1));
    }

    #[test]
    fn floor_fails_in_one_try() {
        // 连最低质量都超限，最贵的上限不该被试
        let r = fit_size(1, 95, 5, Order::Floor, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 1, 10));
    }

    #[test]
    fn floor_gallops_when_cap_is_near_floor_size() {
        // 倍增 2、4，再二分 3
        let r = fit_size(1, 95, 25, Order::Floor, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(2), 4));
    }

    #[test]
    fn parallel_unreachable_stops_after_first_round() {
        let r = fit_size(1, 95, 5, PARALLEL, single).unwrap();
        assert_eq!((found(&r), r.tries), (None, 3));
    }

    #[test]
    fn finds_highest_quality_under_cap() {
        for order in [Order::Ceiling, PARALLEL, SEQUENTIAL, Order::Floor] {
            let r = fit_size(1, 95, 555, order, single).unwrap();
            assert_eq!(found(&r), Some(55), "{order:?}");
            assert!(r.tries <= 12, "{order:?}: {} 次", r.tries);
        }
    }

    #[test]
    fn exact_lower_bound() {
        let r = fit_size(10, 95, 100, Order::Ceiling, single).unwrap();
        assert_eq!(found(&r), Some(10));
    }

    #[test]
    fn parallel_first_round_tries_the_second_ceiling() {
        // 体积 = 质量 × 10：上限 100 超限（1000 > 990），次上限 99 正好可行
        let r = fit_size(1, 100, 990, PARALLEL, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(99), 3));
        let r = fit_size(1, 100, 990, SEQUENTIAL, single).unwrap();
        assert_eq!(found(&r), Some(99));
    }

    #[test]
    fn unreachable_returns_smallest() {
        for order in [Order::Ceiling, PARALLEL, SEQUENTIAL] {
            let r = fit_size(10, 95, 50, order, single).unwrap();
            match r.fit {
                Fit::Unreachable { quality, data } => {
                    assert_eq!((quality, data.len()), (10, 100), "{order:?}")
                }
                Fit::Found { .. } => panic!("{order:?} 不应满足"),
            }
        }
    }

    #[test]
    fn zero_lower_bound_does_not_underflow() {
        let r = fit_size(0, 0, 1, Order::Ceiling, |qs| {
            Ok(qs.iter().map(|_| vec![0; 5]).collect())
        })
        .unwrap();
        assert!(matches!(r.fit, Fit::Unreachable { quality: 0, .. }));
        assert_eq!(r.tries, 1);
        let r = fit_size(0, 100, 0, Order::Ceiling, single).unwrap();
        assert_eq!(found(&r), Some(0));
    }

    #[test]
    fn never_returns_oversized_on_non_monotonic_sizes() {
        // 质量 60 处体积回落，但结果必须真的不超上限
        let bumpy = |qs: &[u8]| {
            Ok(qs
                .iter()
                .map(|&q| vec![0; if q == 60 { 100 } else { usize::from(q) * 10 }])
                .collect())
        };
        let r = fit_size(1, 95, 450, Order::Ceiling, bumpy).unwrap();
        match r.fit {
            Fit::Found { data, .. } => assert!(data.len() <= 450),
            Fit::Unreachable { .. } => panic!("应能满足"),
        }
    }

    #[test]
    fn climb_finds_higher_quality_that_fits() {
        let (q, data, tries) = climb(40, linear(40), 95, 555, |q| Ok(linear(q))).unwrap();
        assert_eq!((q, data.len()), (55, 550));
        // 倍增 41、43、47、55、71，再在 56..=70 二分 4 次
        assert_eq!(tries, 9);
    }

    #[test]
    fn climb_stops_at_ceiling() {
        let (q, _, tries) = climb(90, linear(90), 95, 10_000, |q| Ok(linear(q))).unwrap();
        assert_eq!(q, 95);
        assert!(tries <= 3);
        let (q, _, tries) = climb(95, linear(95), 95, 10_000, |q| Ok(linear(q))).unwrap();
        assert_eq!((q, tries), (95, 0));
    }
}
