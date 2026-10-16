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

/// 在 [lo, hi] 中找输出不超过 `cap` 字节的最高质量。
///
/// 先试 hi，满足就直接返回；否则对整数质量二分。体积随质量大致单调，
/// 偶有回落也不会选中超限的结果：只记录真正满足上限的最高质量。
pub fn fit_size(
    lo: u8,
    hi: u8,
    cap: u64,
    mut encode: impl FnMut(u8) -> Result<Vec<u8>>,
) -> Result<SearchResult> {
    let lo = lo.min(hi);
    let fits = |data: &[u8]| data.len() as u64 <= cap;

    let first = encode(hi)?;
    let mut tries = 1;
    if fits(&first) {
        return Ok(SearchResult {
            fit: Fit::Found {
                quality: hi,
                data: first,
            },
            tries,
        });
    }

    let mut smallest = (hi, first);
    let mut best: Option<(u8, Vec<u8>)> = None;
    let (mut l, mut h) = (i32::from(lo), i32::from(hi) - 1);
    while l <= h {
        let mid = l + (h - l) / 2;
        let q = mid as u8;
        let data = encode(q)?;
        tries += 1;
        if fits(&data) {
            best = Some((q, data));
            l = mid + 1;
        } else {
            if data.len() < smallest.1.len() {
                smallest = (q, data);
            }
            h = mid - 1;
        }
    }

    let fit = match best {
        Some((quality, data)) => Fit::Found { quality, data },
        None => Fit::Unreachable {
            quality: smallest.0,
            data: smallest.1,
        },
    };
    Ok(SearchResult { fit, tries })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟编码器：体积 = 质量 × 10 字节
    fn linear(q: u8) -> Result<Vec<u8>> {
        Ok(vec![0; usize::from(q) * 10])
    }

    fn found(r: &SearchResult) -> Option<u8> {
        match r.fit {
            Fit::Found { quality, .. } => Some(quality),
            Fit::Unreachable { .. } => None,
        }
    }

    #[test]
    fn ceiling_fits_in_one_try() {
        let r = fit_size(1, 80, 10_000, linear).unwrap();
        assert_eq!((found(&r), r.tries), (Some(80), 1));
    }

    #[test]
    fn finds_highest_quality_under_cap() {
        let r = fit_size(1, 95, 555, linear).unwrap();
        assert_eq!(found(&r), Some(55));
        assert!(r.tries <= 8);
    }

    #[test]
    fn exact_lower_bound() {
        let r = fit_size(10, 95, 100, linear).unwrap();
        assert_eq!(found(&r), Some(10));
    }

    #[test]
    fn unreachable_returns_smallest() {
        let r = fit_size(10, 95, 50, linear).unwrap();
        match r.fit {
            Fit::Unreachable { quality, data } => assert_eq!((quality, data.len()), (10, 100)),
            Fit::Found { .. } => panic!("不应满足"),
        }
    }

    #[test]
    fn zero_lower_bound_does_not_underflow() {
        let r = fit_size(0, 0, 1, |_| Ok(vec![0; 5])).unwrap();
        assert!(matches!(r.fit, Fit::Unreachable { quality: 0, .. }));
        let r = fit_size(0, 100, 0, linear).unwrap();
        assert_eq!(found(&r), Some(0));
    }

    #[test]
    fn never_returns_oversized_on_non_monotonic_sizes() {
        // 质量 60 处体积回落，但结果必须真的不超上限
        let bumpy = |q: u8| Ok(vec![0; if q == 60 { 100 } else { usize::from(q) * 10 }]);
        let r = fit_size(1, 95, 450, bumpy).unwrap();
        match r.fit {
            Fit::Found { data, .. } => assert!(data.len() <= 450),
            Fit::Unreachable { .. } => panic!("应能满足"),
        }
    }
}
