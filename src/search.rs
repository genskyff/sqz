use std::ops::RangeInclusive;

use anyhow::Result;

pub enum Fit {
    Found {
        quality: u8,
        data: Vec<u8>,
    },
    /// 带着试过的最小结果
    Unreachable {
        quality: u8,
        data: Vec<u8>,
    },
}

pub struct SearchResult {
    pub fit: Fit,
    pub tries: u32,
    /// (质量, 估算体积)，按试探先后
    pub probes: Vec<(u8, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// 先试上限，失败后往下二分
    Ceiling,
    /// 上限走另一条编码路径，超限说明不了有损区间：再试下限，也超限就收工。
    /// `parallel` 时上限、次上限、下限一起试
    SplitCeiling { parallel: bool },
    /// 先试下限，超限就收工；下限体积离上限不到 `GALLOP_RATIO` 倍就倍增，否则二分
    Floor,
}

/// 上限不到下限体积的这么多倍时，答案多半贴着下限，倍增比二分省。经验值
const GALLOP_RATIO: f64 = 3.0;

/// 折算后的估算在上限的这么多倍以内才值得复测。经验值
const RETEST_MARGIN: f64 = 1.3;

/// 在 [lo, hi] 中找输出不超过 `cap` 字节的最高质量；体积偶有回落也只认真正不超限的。
/// `lookahead` 时把接下来可能要试的质量一并交给 `encode` 并行编码，试探顺序和结果不变。
pub fn fit_size(
    lo: u8,
    hi: u8,
    cap: u64,
    order: Order,
    lookahead: bool,
    mut encode: impl FnMut(&[u8]) -> Result<Vec<Vec<u8>>>,
) -> Result<SearchResult> {
    let lo = lo.min(hi);
    // hi 与 lo 相邻或相等时会重复
    let mut first = match order {
        Order::Floor => vec![lo],
        Order::SplitCeiling { parallel: true } => vec![hi, hi.saturating_sub(1).max(lo), lo],
        Order::Ceiling | Order::SplitCeiling { parallel: false } => vec![hi],
    };
    first.dedup();
    let batch = encode(&first)?;

    let mut search = Search::new(&mut encode, cap, lookahead);
    search.tries = first.len() as u32;
    for (q, data) in first.into_iter().zip(batch) {
        search.record(q, data);
    }

    match order {
        Order::Floor => {
            // 下限都超限就到此为止
            if let Some((_, data)) = &search.best {
                if cap as f64 <= data.len() as f64 * GALLOP_RATIO {
                    search.gallop(hi)?;
                }
                search.bisect(lo, hi)?;
            }
        }
        Order::SplitCeiling { parallel: false } => {
            let floor_fails = search.best.is_none() && lo < hi && !search.probe(lo, [None; 2])?;
            if !floor_fails {
                search.bisect(lo, hi)?;
            }
        }
        Order::Ceiling | Order::SplitCeiling { parallel: true } => search.bisect(lo, hi)?,
    }
    Ok(search.finish())
}

/// 从不超限的 `quality` 往上找，返回 (质量, 数据, 编码次数)：定稿比估算小，估算时超限的
/// 更高质量也可能放得下。先倍增，越界后在缺口里二分；`encode` 按定稿档位编码。
pub fn climb(
    quality: u8,
    data: Vec<u8>,
    hi: u8,
    cap: u64,
    lookahead: bool,
    mut encode: impl FnMut(&[u8]) -> Result<Vec<Vec<u8>>>,
) -> Result<(u8, Vec<u8>, u32)> {
    let mut search = Search::new(&mut encode, cap, lookahead);
    search.record(quality, data);
    search.gallop(hi)?;
    search.bisect(quality, hi)?;
    let tries = search.tries;
    match search.finish().fit {
        Fit::Found { quality, data } => Ok((quality, data, tries)),
        Fit::Unreachable { .. } => unreachable!("起点已经不超限"),
    }
}

/// [`climb`] 头两步会试的质量，供调用方提前编码
pub fn climb_start(quality: u8, hi: u8) -> Vec<u8> {
    let mut start = Vec::new();
    if quality < hi {
        start.push(quality + 1);
        if quality + 1 < hi {
            start.push(quality.saturating_add(3).min(hi));
        }
    }
    start
}

/// 体积不一定单调，估算又偏大，最高的质量可能被误判超限，而 [`climb`] 爬不过中间的高峰：
/// 从高到低复测估算够近的质量，没试探过的借用最近更高质量的估算。
/// `gap` 是这张图上估算体积与定稿体积之比。
pub fn retest_top(
    candidates: RangeInclusive<u8>,
    above: Option<u8>,
    cap: u64,
    gap: f64,
    probes: &[(u8, usize)],
    mut encode: impl FnMut(u8) -> Result<Vec<u8>>,
) -> Result<Option<(u8, Vec<u8>)>> {
    for q in candidates.rev() {
        if above.is_some_and(|a| q <= a) {
            continue;
        }
        let estimate = probes
            .iter()
            .rev()
            .filter(|(p, _)| *p >= q)
            .min_by_key(|(p, _)| *p)
            .map(|&(_, size)| size);
        if estimate.is_none_or(|size| size as f64 / gap > cap as f64 * RETEST_MARGIN) {
            continue;
        }
        let data = encode(q)?;
        if data.len() as u64 <= cap {
            return Ok(Some((q, data)));
        }
    }
    Ok(None)
}

type Sample = (u8, Vec<u8>);
/// 结果与质量一一对应
type Encode<'a> = &'a mut dyn FnMut(&[u8]) -> Result<Vec<Vec<u8>>>;

struct Search<'a> {
    encode: Encode<'a>,
    cap: u64,
    lookahead: bool,
    /// 预读编码好、还没轮到的
    ahead: Vec<Sample>,
    tries: u32,
    probes: Vec<(u8, usize)>,
    /// 不超限的最高质量
    best: Option<Sample>,
    /// 超限的最低质量
    fail: Option<Sample>,
}

impl<'a> Search<'a> {
    fn new(encode: Encode<'a>, cap: u64, lookahead: bool) -> Self {
        Self {
            encode,
            cap,
            lookahead,
            ahead: Vec::new(),
            tries: 0,
            probes: Vec::new(),
            best: None,
            fail: None,
        }
    }

    fn record(&mut self, q: u8, data: Vec<u8>) -> bool {
        self.probes.push((q, data.len()));
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

    /// `next` 是 `q` 不超限、超限时各自接着要试的质量
    fn probe(&mut self, q: u8, next: [Option<u8>; 2]) -> Result<bool> {
        let data = match self.ahead.iter().position(|(a, _)| *a == q) {
            Some(i) => self.ahead.swap_remove(i).1,
            None => {
                let mut batch = vec![q];
                if self.lookahead {
                    batch.extend(next.into_iter().flatten());
                }
                let mut encoded = (self.encode)(&batch)?.into_iter();
                let data = encoded.next().expect("每个质量都应有结果");
                self.ahead = batch.into_iter().skip(1).zip(encoded).collect();
                data
            }
        };
        self.tries += 1;
        Ok(self.record(q, data))
    }

    /// 已知可行之上、已知超限之下，再夹在 [floor, ceiling] 里
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
        while let Some(mid) = midpoint(l, h) {
            let m = i32::from(mid);
            if self.probe(mid, [midpoint(m + 1, h), midpoint(l, m - 1)])? {
                l = m + 1;
            } else {
                h = m - 1;
            }
        }
        Ok(())
    }

    fn gallop(&mut self, ceiling: u8) -> Result<()> {
        let mut step = 1;
        while let Some((best, _)) = &self.best {
            let (_, high) = self.untested(*best, ceiling);
            let best = i32::from(*best);
            if best >= high {
                break;
            }
            let q = (best + step).min(high);
            // 不超限接着倍增；超限时调用方会在 (best, q) 之间二分
            let next = [
                (q < high).then(|| (q + step * 2).min(high) as u8),
                midpoint(best + 1, q - 1),
            ];
            if !self.probe(q as u8, next)? {
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
            probes: self.probes,
        }
    }
}

fn midpoint(l: i32, h: i32) -> Option<u8> {
    (l <= h).then(|| (l + (h - l) / 2) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARALLEL: Order = Order::SplitCeiling { parallel: true };
    const SEQUENTIAL: Order = Order::SplitCeiling { parallel: false };

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
        let r = fit_size(1, 80, 10_000, Order::Ceiling, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(80), 1));
    }

    #[test]
    fn ceiling_unreachable_bisects_down_to_floor() {
        // 不提前试下限：可达时少一次编码，达不到时要二分到底
        let r = fit_size(1, 95, 5, Order::Ceiling, false, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 7, 10));
    }

    #[test]
    fn parallel_first_round_tries_three_candidates() {
        let r = fit_size(1, 80, 10_000, PARALLEL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(80), 3));
    }

    #[test]
    fn equal_bounds_encode_once() {
        let r = fit_size(50, 50, 10_000, PARALLEL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(50), 1));
        let r = fit_size(50, 51, 10_000, PARALLEL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(51), 2));
        let r = fit_size(50, 50, 5, SEQUENTIAL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (None, 1));
    }

    #[test]
    fn sequential_split_stops_when_floor_fails() {
        let r = fit_size(1, 100, 5, SEQUENTIAL, false, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 2, 10));
    }

    #[test]
    fn sequential_split_reachable_costs_one_extra_try() {
        let r = fit_size(1, 100, 555, SEQUENTIAL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(55), 9));
        let r = fit_size(1, 100, 10_000, SEQUENTIAL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(100), 1));
    }

    #[test]
    fn floor_fails_in_one_try() {
        let r = fit_size(1, 95, 5, Order::Floor, false, single).unwrap();
        assert_eq!((found(&r), r.tries, size_of(&r)), (None, 1, 10));
    }

    #[test]
    fn floor_gallops_when_cap_is_near_floor_size() {
        // 下限 10 字节，上限 25 字节：比值 2.5，倍增 2、4，再二分 3
        let r = fit_size(1, 95, 25, Order::Floor, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(2), 4));
    }

    #[test]
    fn floor_bisects_when_cap_is_far_from_floor_size() {
        // 比值 55.5：直接二分，倍增要 12 次
        let r = fit_size(1, 95, 555, Order::Floor, false, single).unwrap();
        assert_eq!(found(&r), Some(55));
        assert!(r.tries <= 8, "{} 次", r.tries);
    }

    #[test]
    fn parallel_unreachable_stops_after_first_round() {
        let r = fit_size(1, 95, 5, PARALLEL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (None, 3));
    }

    #[test]
    fn finds_highest_quality_under_cap() {
        for order in [Order::Ceiling, PARALLEL, SEQUENTIAL, Order::Floor] {
            let r = fit_size(1, 95, 555, order, false, single).unwrap();
            assert_eq!(found(&r), Some(55), "{order:?}");
            assert!(r.tries <= 10, "{order:?}: {} 次", r.tries);
        }
    }

    #[test]
    fn exact_lower_bound() {
        let r = fit_size(10, 95, 100, Order::Ceiling, false, single).unwrap();
        assert_eq!(found(&r), Some(10));
    }

    #[test]
    fn parallel_first_round_tries_the_second_ceiling() {
        // 上限 100 是 1000 字节，超限；次上限 99 正好可行
        let r = fit_size(1, 100, 990, PARALLEL, false, single).unwrap();
        assert_eq!((found(&r), r.tries), (Some(99), 3));
        let r = fit_size(1, 100, 990, SEQUENTIAL, false, single).unwrap();
        assert_eq!(found(&r), Some(99));
    }

    #[test]
    fn unreachable_returns_smallest() {
        for order in [Order::Ceiling, PARALLEL, SEQUENTIAL] {
            let r = fit_size(10, 95, 50, order, false, single).unwrap();
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
        let r = fit_size(0, 0, 1, Order::Ceiling, false, |qs| {
            Ok(qs.iter().map(|_| vec![0; 5]).collect())
        })
        .unwrap();
        assert!(matches!(r.fit, Fit::Unreachable { quality: 0, .. }));
        assert_eq!(r.tries, 1);
        let r = fit_size(0, 100, 0, Order::Ceiling, false, single).unwrap();
        assert_eq!(found(&r), Some(0));
    }

    #[test]
    fn never_returns_oversized_on_non_monotonic_sizes() {
        // 质量 60 处体积回落
        let bumpy = |qs: &[u8]| {
            Ok(qs
                .iter()
                .map(|&q| vec![0; if q == 60 { 100 } else { usize::from(q) * 10 }])
                .collect())
        };
        let r = fit_size(1, 95, 450, Order::Ceiling, false, bumpy).unwrap();
        match r.fit {
            Fit::Found { data, .. } => assert!(data.len() <= 450),
            Fit::Unreachable { .. } => panic!("应能满足"),
        }
    }

    #[test]
    fn probes_record_every_try() {
        let r = fit_size(1, 95, 555, Order::Ceiling, false, single).unwrap();
        assert_eq!(r.probes.len() as u32, r.tries);
        assert_eq!(r.probes[0], (95, 950));
    }

    #[test]
    fn climb_finds_higher_quality_that_fits() {
        let (q, data, tries) = climb(40, linear(40), 95, 555, false, single).unwrap();
        assert_eq!((q, data.len()), (55, 550));
        // 倍增 41、43、47、55、71，再在 56..=70 二分 4 次
        assert_eq!(tries, 9);
    }

    #[test]
    fn climb_stops_at_ceiling() {
        let (q, _, tries) = climb(90, linear(90), 95, 10_000, false, single).unwrap();
        assert_eq!(q, 95);
        assert!(tries <= 3);
        let (q, _, tries) = climb(95, linear(95), 95, 10_000, false, single).unwrap();
        assert_eq!((q, tries), (95, 0));
    }

    /// 仿调色板截图的定稿体积：低质量颜色少、高质量颜色全，中间反而最大
    fn u_final(q: u8) -> Vec<u8> {
        let size = match q {
            0..=3 => 6_788,
            4..=97 => 7_400,
            98..=99 => 7_160,
            _ => 7_172,
        };
        vec![0; size]
    }

    fn u_finals(qs: &[u8]) -> Result<Vec<Vec<u8>>> {
        Ok(qs.iter().copied().map(u_final).collect())
    }

    fn u_estimate(qs: &[u8]) -> Result<Vec<Vec<u8>>> {
        Ok(qs
            .iter()
            .map(|&q| vec![0; u_final(q).len() * 6 / 5])
            .collect())
    }

    #[test]
    fn retest_reaches_top_candidate_misjudged_by_estimate() {
        // 估算全部超限；定稿后 q1 放得下，但往上爬会撞上中间的高峰，必须靠复测拿到 q99
        let cap = 7_168;
        let r = fit_size(1, 100, cap, PARALLEL, false, u_estimate).unwrap();
        let Fit::Unreachable { quality, .. } = r.fit else {
            panic!("按估算应判为达不到");
        };
        assert_eq!(quality, 1);
        let (q, _, _) = climb(1, u_final(1), 99, cap, false, u_finals).unwrap();
        assert_eq!(q, 3, "倍增在 q4 撞上高峰就停");

        let mut tries = 0;
        let hit = retest_top(99..=100, Some(q), cap, 1.0, &r.probes, |q| {
            tries += 1;
            Ok(u_final(q))
        })
        .unwrap();
        assert_eq!(hit.map(|(q, d)| (q, d.len())), Some((99, 7_160)));
        assert_eq!(tries, 2, "先测 q100（7172 超限）再测 q99");
    }

    #[test]
    fn retest_prefers_lossless_when_it_fits() {
        let probes = [(100, 8_606), (99, 8_592)];
        let hit = retest_top(99..=100, Some(99), 7_680, 1.0, &probes, |q| Ok(u_final(q))).unwrap();
        assert_eq!(hit.map(|(q, _)| q), Some(100));
    }

    #[test]
    fn retest_skips_estimates_far_above_cap() {
        let probes = [(100, 50_000), (99, 20_000)];
        let hit = retest_top(
            99..=100,
            None,
            7_168,
            1.0,
            &probes,
            |_| -> Result<Vec<u8>> { panic!("估算远超上限，不该编码") },
        )
        .unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn retest_scales_estimates_by_measured_gap() {
        // 估算比定稿大 50%，超出了 RETEST_MARGIN：不知道这个比例时不值得测，知道了就该测
        let probes = [(100, 10_758), (99, 10_740)];
        let hit = retest_top(
            99..=100,
            None,
            7_168,
            1.0,
            &probes,
            |_| -> Result<Vec<u8>> { panic!("估算远超上限，不该编码") },
        )
        .unwrap();
        assert!(hit.is_none());
        let hit = retest_top(99..=100, None, 7_168, 1.5, &probes, |q| Ok(u_final(q))).unwrap();
        assert_eq!(hit.map(|(q, _)| q), Some(99));
    }

    #[test]
    fn retest_borrows_estimate_for_untested_quality() {
        let probes = [(100, 8_606), (1, 8_146)];
        let mut tested = Vec::new();
        let hit = retest_top(99..=100, None, 7_168, 1.0, &probes, |q| {
            tested.push(q);
            Ok(u_final(q))
        })
        .unwrap();
        assert_eq!(tested, [100, 99]);
        assert_eq!(hit.map(|(q, _)| q), Some(99));
    }

    #[test]
    fn retest_respects_candidates_and_above() {
        let probes = [(100, 7_000)];
        let hit = retest_top(100..=100, None, 7_168, 1.0, &probes, |q| Ok(u_final(q))).unwrap();
        assert!(hit.is_none(), "只给了 q100 就不测 q99，q100 本身 7172 超限");
        let hit = retest_top(
            99..=100,
            Some(100),
            10_000,
            1.0,
            &probes,
            |_| -> Result<Vec<u8>> { panic!("已经是最高质量，不该复测") },
        )
        .unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn lookahead_keeps_results_and_tries() {
        fn check(encode: impl Fn(&[u8]) -> Result<Vec<Vec<u8>>>) {
            for cap in [5, 25, 95, 333, 555, 949, 10_000] {
                for order in [Order::Ceiling, PARALLEL, SEQUENTIAL, Order::Floor] {
                    let [plain, ahead] = [false, true].map(|lookahead| {
                        let r = fit_size(1, 95, cap, order, lookahead, &encode).unwrap();
                        (found(&r), size_of(&r), r.tries, r.probes)
                    });
                    assert_eq!(plain, ahead, "{order:?} {cap}");
                }
                for start in [1, 9, 33] {
                    let data = encode(&[start]).unwrap().remove(0);
                    if data.len() as u64 > cap {
                        continue;
                    }
                    let [plain, ahead] = [false, true].map(|lookahead| {
                        climb(start, data.clone(), 95, cap, lookahead, &encode).unwrap()
                    });
                    assert_eq!(plain, ahead, "从 {start} 往上爬，上限 {cap}");
                }
            }
        }
        check(single);
        // 体积不单调时也一样
        check(|qs| {
            let size = |q: u8| {
                if q.is_multiple_of(7) {
                    100
                } else {
                    usize::from(q) * 10
                }
            };
            Ok(qs.iter().map(|&q| vec![0; size(q)]).collect())
        });
    }

    #[test]
    fn lookahead_encodes_upcoming_qualities_together() {
        let mut rounds = Vec::new();
        let r = fit_size(1, 95, 555, Order::Ceiling, true, |qs| {
            rounds.push(qs.to_vec());
            single(qs)
        })
        .unwrap();
        assert_eq!((found(&r), r.tries), (Some(55), 8));
        // 每轮带上不超限、超限两种情况下的下一个质量，一轮定两步
        assert_eq!(
            rounds,
            [
                vec![95],
                vec![47, 71, 23],
                vec![59, 65, 53],
                vec![56, 57, 54],
                vec![55]
            ]
        );
    }

    #[test]
    fn climb_start_matches_the_first_round() {
        for (quality, hi) in [(40, 95), (93, 95), (94, 95), (95, 95)] {
            let mut rounds = Vec::new();
            climb(quality, linear(quality), hi, 10_000, true, |qs| {
                rounds.push(qs.to_vec());
                single(qs)
            })
            .unwrap();
            let first = rounds.first().cloned().unwrap_or_default();
            assert_eq!(first, climb_start(quality, hi), "从 {quality} 起");
        }
    }
}
