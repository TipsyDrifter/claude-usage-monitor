//! M5 統計核心 — pure math, no DB, no IO, unit-tested (D64).
//!
//! Everything here is generic over the interval type so the same bootstrap /
//! permutation machinery serves the display rate (Σusd/Σδ), the 7d cross-check
//! and the fixed-basket shrink index. Randomness is a seeded xorshift so a
//! given ledger always draws the same confidence band (規格書 §5 工程紅線：
//! 幾何可反推——同一份資料必須畫出同一條帶).

/// Model families the price table knows. Index into `FamIv::by_family`.
pub const FAMILIES: [&str; 5] = ["fable", "opus", "sonnet-5", "sonnet", "haiku"];

pub fn family_index(model: &str) -> Option<usize> {
    let m = model.to_lowercase();
    if m.contains("fable") {
        Some(0)
    } else if m.contains("opus") {
        Some(1)
    } else if m.contains("sonnet-5") || m.contains("sonnet5") {
        Some(2)
    } else if m.contains("sonnet") {
        Some(3)
    } else if m.contains("haiku") {
        Some(4)
    } else {
        None
    }
}

/// One paired truth interval: Δ% of one limit over a short span, and the
/// API-priced local usage that fell inside it (split by model family).
#[derive(Clone, Debug)]
pub struct FamIv {
    pub delta: f64,
    pub usd: f64,
    pub by_family: [f64; 5],
}

/// Anything that can be scored as (delta, usd).
pub trait Pair {
    fn delta(&self) -> f64;
    fn usd(&self) -> f64;
}
impl Pair for (f64, f64) {
    fn delta(&self) -> f64 {
        self.0
    }
    fn usd(&self) -> f64 {
        self.1
    }
}
impl Pair for FamIv {
    fn delta(&self) -> f64 {
        self.delta
    }
    fn usd(&self) -> f64 {
        self.usd
    }
}

// ---------------------------------------------------------------------------
// RNG — xorshift64*, deterministic. Not for security; for reproducible bands.
// ---------------------------------------------------------------------------
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform integer in [0, n).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------
// Estimators
// ---------------------------------------------------------------------------

/// Linear-interpolated quantile of an ascending slice. q in [0,1].
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Δ-weighted ratio estimator Σusd / Σδ. None when Σδ ≤ 0.
pub fn ratio<T: Pair>(iv: &[T]) -> Option<f64> {
    let (d, u) = iv.iter().fold((0.0, 0.0), |(d, u), x| (d + x.delta(), u + x.usd()));
    (d > 0.0).then(|| u / d)
}

/// Asymmetric trim on per-interval rates: keep intervals whose usd/δ lies in
/// [quantile(lo), quantile(hi)]. Below 8 intervals nothing is trimmed (a 25%
/// cut of 6 points is just noise-on-noise). Lower tail is the polluted one —
/// Δ% burned by usage the local JSONL never saw (web, other machines) pulls
/// rates toward 0 — so lo=0.25, hi=0.95 is the house cut (F5 / D64-1).
pub fn trim<T: Pair>(iv: &[T], lo: f64, hi: f64) -> Vec<&T> {
    if iv.len() < 8 {
        return iv.iter().collect();
    }
    let mut rates: Vec<f64> = iv
        .iter()
        .filter(|x| x.delta() > 0.0)
        .map(|x| x.usd() / x.delta())
        .collect();
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (qlo, qhi) = (quantile(&rates, lo), quantile(&rates, hi));
    iv.iter()
        .filter(|x| {
            if x.delta() <= 0.0 {
                return false;
            }
            let r = x.usd() / x.delta();
            r >= qlo && r <= qhi
        })
        .collect()
}

pub const TRIM_LO: f64 = 0.25;
pub const TRIM_HI: f64 = 0.95;

/// House estimator: trimmed Δ-weighted ratio.
pub fn trimmed_ratio<T: Pair>(iv: &[T]) -> Option<f64> {
    let kept = trim(iv, TRIM_LO, TRIM_HI);
    let (d, u) = kept.iter().fold((0.0, 0.0), |(d, u), x| (d + x.delta(), u + x.usd()));
    (d > 0.0).then(|| u / d)
}

/// Percentile bootstrap CI (2.5 / 97.5) of `est` over resampled intervals.
/// None when the estimator fails on the full sample or on >20% of replicates.
pub fn bootstrap_ci<T: Clone, F: Fn(&[T]) -> Option<f64>>(
    items: &[T],
    est: F,
    reps: usize,
    seed: u64,
) -> Option<(f64, f64)> {
    if items.is_empty() {
        return None;
    }
    est(items)?;
    let mut rng = Rng::new(seed);
    let mut vals = Vec::with_capacity(reps);
    let mut buf: Vec<T> = Vec::with_capacity(items.len());
    for _ in 0..reps {
        buf.clear();
        for _ in 0..items.len() {
            buf.push(items[rng.below(items.len())].clone());
        }
        if let Some(v) = est(&buf) {
            vals.push(v);
        }
    }
    if vals.len() < reps * 4 / 5 {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some((quantile(&vals, 0.025), quantile(&vals, 0.975)))
}

#[derive(Clone, Debug)]
pub struct PermResult {
    /// est(b) − est(a) on the observed labelling.
    pub diff: f64,
    /// Two-sided permutation p-value, (k+1)/(reps+1).
    pub p: f64,
}

/// Two-sample permutation test: is est(b) − est(a) beyond label noise?
/// Labels are shuffled `reps` times over the pooled intervals. Caveat carried
/// to the UI (D64-4): intervals are time-adjacent and correlated, so this p is
/// optimistic — it is a noise gate, not a proof.
pub fn perm_test<T: Clone, F: Fn(&[T]) -> Option<f64>>(
    a: &[T],
    b: &[T],
    est: F,
    reps: usize,
    seed: u64,
) -> Option<PermResult> {
    let obs = est(b)? - est(a)?;
    let mut pool: Vec<T> = a.iter().chain(b.iter()).cloned().collect();
    let na = a.len();
    let mut rng = Rng::new(seed);
    let mut k = 0usize;
    let mut valid = 0usize;
    for _ in 0..reps {
        rng.shuffle(&mut pool);
        let (pa, pb) = pool.split_at(na);
        if let (Some(ea), Some(eb)) = (est(pa), est(pb)) {
            valid += 1;
            if (eb - ea).abs() >= obs.abs() {
                k += 1;
            }
        }
    }
    if valid < reps * 4 / 5 {
        return None;
    }
    Some(PermResult {
        diff: obs,
        p: (k as f64 + 1.0) / (valid as f64 + 1.0),
    })
}

// ---------------------------------------------------------------------------
// Fixed basket (縮水指數) and per-family rates (模型倍率)
// ---------------------------------------------------------------------------

pub const PURE_SHARE: f64 = 0.85;
pub const PURE_MIN_DELTA: f64 = 2.0;
pub const FAMILY_MIN_N: usize = 5;
pub const BASKET_MIN_COVERAGE: f64 = 0.8;

/// Which family dominates this interval (≥ PURE_SHARE of its usd), if any.
pub fn pure_family(iv: &FamIv) -> Option<usize> {
    if iv.usd <= 0.0 || iv.delta < PURE_MIN_DELTA {
        return None;
    }
    let (i, top) = iv
        .by_family
        .iter()
        .enumerate()
        .fold((0, 0.0), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
    (top / iv.usd >= PURE_SHARE).then_some(i)
}

/// Per-family trimmed rate from pure intervals: (rate, n) or None below FAMILY_MIN_N.
pub fn family_rates(ivs: &[FamIv]) -> [Option<(f64, usize)>; 5] {
    let mut buckets: [Vec<(f64, f64)>; 5] = Default::default();
    for iv in ivs {
        if let Some(f) = pure_family(iv) {
            buckets[f].push((iv.delta, iv.usd));
        }
    }
    let mut out: [Option<(f64, usize)>; 5] = Default::default();
    for (f, b) in buckets.iter().enumerate() {
        if b.len() >= FAMILY_MIN_N {
            out[f] = trimmed_ratio(b).map(|r| (r, b.len()));
        }
    }
    out
}

/// API-$ share of each family over a set of intervals (the basket weights).
pub fn basket_shares(ivs: &[FamIv]) -> [f64; 5] {
    let mut s = [0.0; 5];
    for iv in ivs {
        for (acc, v) in s.iter_mut().zip(iv.by_family.iter()) {
            *acc += v;
        }
    }
    let tot: f64 = s.iter().sum();
    if tot > 0.0 {
        for v in s.iter_mut() {
            *v /= tot;
        }
    }
    s
}

#[derive(Clone, Debug)]
pub struct BasketIndex {
    /// USD per 1% for the frozen basket.
    pub index: f64,
    /// Fraction of basket weight backed by a family rate this period.
    pub coverage: f64,
}

/// Laspeyres-style fixed-basket rate: 1 / Σ (w_m / r_m) over families with a
/// rate this period, weights renormalised over the covered part. None when
/// coverage < BASKET_MIN_COVERAGE.
pub fn basket_index(ivs: &[FamIv], basket: &[f64; 5]) -> Option<BasketIndex> {
    let rates = family_rates(ivs);
    let mut covered = 0.0;
    let mut inv = 0.0;
    for f in 0..5 {
        if let Some((r, _)) = rates[f] {
            if r > 0.0 && basket[f] > 0.0 {
                covered += basket[f];
                inv += basket[f] / r;
            }
        }
    }
    if covered < BASKET_MIN_COVERAGE || inv <= 0.0 {
        return None;
    }
    Some(BasketIndex {
        index: covered / inv,
        coverage: covered,
    })
}

// ---------------------------------------------------------------------------
// v1.6（D80）— 分模型倍率：加權非負最小平方（NNLS）回歸
//
// 每段區間：Δ% ≈ Σ_k β_k · usd_k，β_k ≥ 0（模型 k 每 API $1 扣幾個百分點）。
// 純區間法只挑單一家族 ≥85% 的段，混用的段全丟；回歸把全部段都用上，
// Sonnet 這種永遠跟別人混著用的模型才算得出來。
// 業界查證：docs/research/2026-09-18-v1.6-regression-attribution-industry.md——
// NNLS 是「成分成本」歸因的標準工具（scipy 文件原句 component costs）；
// 零截距是質量平衡慣例；w=1/x 的 WLS 過原點解＝比值估計量，所以單一模型
// 時本估計量退化成 house 的 Σusd/Σδ；係數卡在 0 邊界時 bootstrap 分位數
// 區間不可信——這裡把那種係數標成「校準中」而不是給一個看似精確的區間。
// 原型與真帳本敏感度：prototypes/multiplier-regression.py。
// ---------------------------------------------------------------------------

/// 一段區間在設計矩陣裡的一列：各模型 usd（對齊 `Design::models`）、Δ%、權重。
#[derive(Clone, Debug)]
pub struct DesignRow {
    pub x: Vec<f64>,
    pub y: f64,
    pub w: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Design {
    pub models: Vec<String>,
    pub rows: Vec<DesignRow>,
}

impl Design {
    /// 從稀疏的 (模型 id, usd) 列建密集矩陣。權重 w＝1/usd（比值估計量的多元
    /// 推廣，見上）；usd ≤ 0 的段沒有東西可歸因，略過。
    pub fn from_sparse<'a, I>(rows: I) -> Self
    where
        I: IntoIterator<Item = (&'a [(String, f64)], f64)>,
    {
        let rows: Vec<(&[(String, f64)], f64)> = rows.into_iter().collect();
        let mut models: Vec<String> = rows
            .iter()
            .flat_map(|(bm, _)| bm.iter().map(|(m, _)| m.clone()))
            .collect();
        models.sort();
        models.dedup();
        let dense = rows
            .iter()
            .filter_map(|(bm, delta)| {
                let usd: f64 = bm.iter().map(|(_, v)| v).sum();
                if usd <= 0.0 || *delta <= 0.0 {
                    return None;
                }
                let mut x = vec![0.0; models.len()];
                for (m, v) in bm.iter() {
                    if let Ok(k) = models.binary_search(m) {
                        x[k] += v;
                    }
                }
                Some(DesignRow { x, y: *delta, w: 1.0 / usd })
            })
            .collect();
        Design { models, rows: dense }
    }

    /// 把模型欄合併成家族欄（四家族，主人 2026-09-18 拍板：Sonnet 5 與 4.x 同一家）。
    /// 家族的「綜合費率」就是這個設計矩陣跑回歸的結果；回傳的 models 是 GROUPS 名。
    pub fn collapse_to_groups(&self) -> Design {
        let map: Vec<Option<usize>> = self.models.iter().map(|m| group_index(m)).collect();
        let rows = self
            .rows
            .iter()
            .map(|r| {
                let mut x = vec![0.0; GROUPS.len()];
                for (k, v) in r.x.iter().enumerate() {
                    if let Some(g) = map[k] {
                        x[g] += v;
                    }
                }
                DesignRow { x, y: r.y, w: r.w }
            })
            .collect();
        Design { models: GROUPS.iter().map(|s| s.to_string()).collect(), rows }
    }
}

/// 倍率表的家族分組（顯示用，四家族）。`FAMILIES` 仍是價目表／固定籃的五分法
/// （sonnet-5 與 sonnet 4.x 價格不同），兩者只差在 Sonnet 併不併。
pub const GROUPS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

pub fn group_of_family(f: usize) -> usize {
    match f {
        0 => 0,
        1 => 1,
        2 | 3 => 2,
        _ => 3,
    }
}

pub fn group_index(model: &str) -> Option<usize> {
    family_index(model).map(group_of_family)
}

/// 純區間法在四家族層級的匯率（交叉驗證用）：單一家族佔 ≥PURE_SHARE 且 Δ≥2，
/// 家族 n≥FAMILY_MIN_N 才給。跟 `family_rates` 同規則，只是 Sonnet 併成一家。
pub fn group_rates(ivs: &[FamIv]) -> [Option<(f64, usize)>; 4] {
    let mut buckets: [Vec<(f64, f64)>; 4] = Default::default();
    for iv in ivs {
        if iv.usd <= 0.0 || iv.delta < PURE_MIN_DELTA {
            continue;
        }
        let mut by_group = [0.0f64; 4];
        for (f, v) in iv.by_family.iter().enumerate() {
            by_group[group_of_family(f)] += v;
        }
        let (g, top) = by_group
            .iter()
            .enumerate()
            .fold((0, 0.0), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
        if top / iv.usd >= PURE_SHARE {
            buckets[g].push((iv.delta, iv.usd));
        }
    }
    let mut out: [Option<(f64, usize)>; 4] = Default::default();
    for (g, b) in buckets.iter().enumerate() {
        if b.len() >= FAMILY_MIN_N {
            out[g] = trimmed_ratio(b).map(|r| (r, b.len()));
        }
    }
    out
}

/// Lawson–Hanson 主動集 NNLS：argmin ‖√W(Xβ − y)‖² s.t. β ≥ 0。
/// K 個模型（≤ 十來個）、幾百段——用正規方程 XᵀWX 做，每次迭代解一個 |P|×|P|
/// 的小系統。回傳 β（長度 K）。空資料回全 0。
pub fn nnls(rows: &[DesignRow], k: usize) -> Vec<f64> {
    if k == 0 || rows.is_empty() {
        return vec![0.0; k];
    }
    // 正規方程
    let mut ata = vec![vec![0.0f64; k]; k];
    let mut aty = vec![0.0f64; k];
    for r in rows {
        for i in 0..k {
            let xi = r.x[i];
            if xi == 0.0 {
                continue;
            }
            aty[i] += r.w * xi * r.y;
            for (a, xj) in ata[i].iter_mut().zip(&r.x) {
                *a += r.w * xi * xj;
            }
        }
    }
    let scale = aty.iter().fold(0.0f64, |m, v| m.max(v.abs())).max(1e-12);
    let tol = 1e-10 * scale;
    let mut x = vec![0.0f64; k];
    let mut passive = vec![false; k];
    let mut outer = 0;
    loop {
        outer += 1;
        if outer > 3 * k + 10 {
            break;
        }
        // w = Aᵀy − AᵀA x
        let grad: Vec<f64> = (0..k)
            .map(|i| aty[i] - (0..k).map(|j| ata[i][j] * x[j]).sum::<f64>())
            .collect();
        let mut best: Option<(usize, f64)> = None;
        for i in 0..k {
            if !passive[i] && grad[i] > tol && best.is_none_or(|(_, g)| grad[i] > g) {
                best = Some((i, grad[i]));
            }
        }
        let Some((j, _)) = best else { break };
        passive[j] = true;
        // 內迴圈：在 passive 集上解無約束最小平方，負的就退回邊界
        let mut inner = 0;
        loop {
            inner += 1;
            if inner > 3 * k + 10 {
                break;
            }
            let idx: Vec<usize> = (0..k).filter(|&i| passive[i]).collect();
            let z = match solve_subsystem(&ata, &aty, &idx) {
                Some(z) => z,
                None => {
                    // 奇異（完全共線）：把剛加進來的那個退回去，放棄這一步
                    passive[j] = false;
                    break;
                }
            };
            if z.iter().all(|&v| v > 0.0) {
                for (pos, &i) in idx.iter().enumerate() {
                    x[i] = z[pos];
                }
                break;
            }
            // 沿 x→z 走到第一個碰到 0 的座標
            let mut alpha = f64::INFINITY;
            for (pos, &i) in idx.iter().enumerate() {
                if z[pos] <= 0.0 {
                    let denom = x[i] - z[pos];
                    if denom > 0.0 {
                        alpha = alpha.min(x[i] / denom);
                    }
                }
            }
            if !alpha.is_finite() {
                alpha = 0.0;
            }
            for (pos, &i) in idx.iter().enumerate() {
                x[i] += alpha * (z[pos] - x[i]);
                if x[i] <= tol {
                    x[i] = 0.0;
                    passive[i] = false;
                }
            }
            if !passive[j] {
                break;
            }
        }
    }
    x
}

/// 高斯消去（部分樞軸）解 AᵀA[idx,idx] z = Aᵀy[idx]。奇異回 None。
fn solve_subsystem(ata: &[Vec<f64>], aty: &[f64], idx: &[usize]) -> Option<Vec<f64>> {
    let n = idx.len();
    if n == 0 {
        return Some(vec![]);
    }
    let mut m: Vec<Vec<f64>> = idx
        .iter()
        .map(|&i| {
            let mut row: Vec<f64> = idx.iter().map(|&j| ata[i][j]).collect();
            row.push(aty[i]);
            row
        })
        .collect();
    for col in 0..n {
        let (piv, pv) = (col..n).map(|r| (r, m[r][col].abs())).fold((col, -1.0), |a, b| if b.1 > a.1 { b } else { a });
        if pv <= 1e-14 * (1.0 + m[col][col].abs()) || pv == 0.0 {
            return None;
        }
        m.swap(col, piv);
        let pivot_row = m[col].clone();
        for (r, row) in m.iter_mut().enumerate() {
            if r == col {
                continue;
            }
            let f = row[col] / pivot_row[col];
            if f == 0.0 {
                continue;
            }
            for (cell, pv) in row.iter_mut().zip(&pivot_row).skip(col) {
                *cell -= f * pv;
            }
        }
    }
    Some((0..n).map(|r| m[r][n] / m[r][r]).collect())
}

/// 一列回歸結果（一個模型 id）。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegRow {
    pub model: String,
    pub family: Option<String>,
    /// β：每 API $1 扣的百分點。
    pub beta: f64,
    /// $/1%（＝1/β）；β＝0 時 None。
    pub rate: Option<f64>,
    /// rate 的 bootstrap 95% 區間；上界碰到 β→0 時 None（無上界）。
    pub ci: Option<(f64, Option<f64>)>,
    /// bootstrap 裡 β 被壓在 0 的比例——邊界訊號（Andrews 2000）。
    pub zero_share: f64,
    /// 這個模型有花錢的段數。
    pub n_seg: usize,
    /// "ok" | "calibrating" | "collinear"
    pub status: &'static str,
    pub collinear_with: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Regression {
    pub n: usize,
    /// 無截距的 uncentered R²（1 − Σ(y−ŷ)²／Σy²）。
    pub r2: Option<f64>,
    pub rows: Vec<RegRow>,
}

/// 一個係數要「站得住」的門檻：有花錢的段數、bootstrap 卡 0 的比例、區間寬度。
pub const REG_MIN_SEG: usize = 10;
pub const REG_ZERO_SHARE_MAX: f64 = 0.05;
pub const REG_MAX_CI_RATIO: f64 = 5.0;
/// 兩個模型欄的相關係數超過這個數就標「共線」——係數只能一起看。
pub const REG_COLLINEAR_R: f64 = 0.8;

/// 加權 NNLS ＋ case-resampling bootstrap。
pub fn regress(design: &Design, reps: usize, seed: u64) -> Regression {
    let k = design.models.len();
    let n = design.rows.len();
    let beta = nnls(&design.rows, k);
    let mut ss_res = 0.0;
    let mut ss_tot = 0.0;
    for r in &design.rows {
        let yhat: f64 = r.x.iter().zip(&beta).map(|(x, b)| x * b).sum();
        ss_res += (r.y - yhat).powi(2);
        ss_tot += r.y.powi(2);
    }
    let r2 = (ss_tot > 0.0 && n > 0).then(|| 1.0 - ss_res / ss_tot);

    // bootstrap
    let mut boots: Vec<Vec<f64>> = vec![Vec::with_capacity(reps); k];
    if n > 0 {
        let mut rng = Rng::new(seed);
        let mut buf: Vec<DesignRow> = Vec::with_capacity(n);
        for _ in 0..reps {
            buf.clear();
            for _ in 0..n {
                buf.push(design.rows[rng.below(n)].clone());
            }
            let b = nnls(&buf, k);
            for (i, v) in b.into_iter().enumerate() {
                boots[i].push(v);
            }
        }
    }

    // 共線：欄與欄的 Pearson r
    let mut collinear: Vec<Option<usize>> = vec![None; k];
    for i in 0..k {
        for j in (i + 1)..k {
            let r = column_corr(&design.rows, i, j);
            if r.abs() > REG_COLLINEAR_R {
                collinear[i].get_or_insert(j);
                collinear[j].get_or_insert(i);
            }
        }
    }

    let rows = (0..k)
        .map(|i| {
            let n_seg = design.rows.iter().filter(|r| r.x[i] > 0.0).count();
            let mut bs = boots[i].clone();
            bs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let zero_share = if bs.is_empty() { 1.0 } else { bs.iter().filter(|v| **v <= 1e-9).count() as f64 / bs.len() as f64 };
            let (b_lo, b_hi) = if bs.len() >= 20 { (quantile(&bs, 0.025), quantile(&bs, 0.975)) } else { (0.0, 0.0) };
            let rate = (beta[i] > 1e-9).then(|| 1.0 / beta[i]);
            let ci = (b_hi > 1e-9 && bs.len() >= 20).then(|| (1.0 / b_hi, (b_lo > 1e-9).then(|| 1.0 / b_lo)));
            let ci_ratio = ci.and_then(|(lo, hi)| hi.map(|h| h / lo)).unwrap_or(f64::INFINITY);
            let status = if collinear[i].is_some() {
                "collinear"
            } else if rate.is_none() || n_seg < REG_MIN_SEG || zero_share > REG_ZERO_SHARE_MAX || ci_ratio > REG_MAX_CI_RATIO {
                "calibrating"
            } else {
                "ok"
            };
            RegRow {
                model: design.models[i].clone(),
                family: group_index(&design.models[i]).map(|g| GROUPS[g].to_string()),
                beta: beta[i],
                rate,
                ci,
                zero_share,
                n_seg,
                status,
                collinear_with: collinear[i].map(|j| design.models[j].clone()),
            }
        })
        .collect();
    Regression { n, r2, rows }
}

fn column_corr(rows: &[DesignRow], i: usize, j: usize) -> f64 {
    let n = rows.len() as f64;
    if n < 3.0 {
        return 0.0;
    }
    let (mi, mj) = (rows.iter().map(|r| r.x[i]).sum::<f64>() / n, rows.iter().map(|r| r.x[j]).sum::<f64>() / n);
    let (mut sij, mut sii, mut sjj) = (0.0, 0.0, 0.0);
    for r in rows {
        let (a, b) = (r.x[i] - mi, r.x[j] - mj);
        sij += a * b;
        sii += a * a;
        sjj += b * b;
    }
    if sii <= 0.0 || sjj <= 0.0 {
        return 0.0;
    }
    sij / (sii * sjj).sqrt()
}

/// 「Fable 5.1」「Opus 4.8」「Haiku 4.5」——從模型 id 拆出人看的名字。
pub fn model_label(model: &str) -> String {
    let body = model.strip_prefix("claude-").unwrap_or(model);
    let mut parts = body.split('-');
    let Some(fam) = parts.next() else { return model.to_string() };
    let mut fam_c = fam.chars();
    let fam_label = match fam_c.next() {
        Some(c) => c.to_uppercase().collect::<String>() + fam_c.as_str(),
        None => String::new(),
    };
    let ver: Vec<&str> = parts.filter(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())).collect();
    if ver.is_empty() {
        fam_label
    } else {
        format!("{fam_label} {}", ver.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(delta: f64, usd: f64) -> (f64, f64) {
        (delta, usd)
    }

    #[test]
    fn ratio_is_delta_weighted() {
        let v = vec![iv(1.0, 1.0), iv(9.0, 27.0)];
        assert!((ratio(&v).unwrap() - 2.8).abs() < 1e-9);
        assert!(ratio(&Vec::<(f64, f64)>::new()).is_none());
    }

    #[test]
    fn quantile_interpolates() {
        let s = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&s, 0.0), 1.0);
        assert_eq!(quantile(&s, 1.0), 4.0);
        assert!((quantile(&s, 0.5) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn trim_drops_zero_usd_tail() {
        // 30 clean intervals at $3/1% + 6 polluted zeros: trimmed ratio
        // recovers ~3, raw ratio is dragged down.
        let mut v: Vec<(f64, f64)> = (0..30).map(|i| iv(2.0, 6.0 + (i % 3) as f64 * 0.1)).collect();
        v.extend((0..6).map(|_| iv(2.0, 0.0)));
        let raw = ratio(&v).unwrap();
        let t = trimmed_ratio(&v).unwrap();
        assert!(raw < 2.7, "raw {raw}");
        assert!((t - 3.05).abs() < 0.1, "trimmed {t}");
        // tiny samples are left alone
        let small = vec![iv(1.0, 0.0), iv(1.0, 5.0), iv(1.0, 5.0)];
        assert_eq!(trim(&small, 0.25, 0.95).len(), 3);
    }

    #[test]
    fn bootstrap_ci_brackets_estimate_and_is_deterministic() {
        let v: Vec<(f64, f64)> = (0..50).map(|i| iv(2.0, 5.0 + (i % 5) as f64)).collect();
        let est = trimmed_ratio(&v).unwrap();
        let ci = bootstrap_ci(&v, trimmed_ratio, 500, 42).unwrap();
        assert!(ci.0 <= est && est <= ci.1, "{ci:?} vs {est}");
        assert!(ci.1 - ci.0 < 1.0);
        assert_eq!(ci, bootstrap_ci(&v, trimmed_ratio, 500, 42).unwrap());
    }

    #[test]
    fn perm_test_separates_shifted_samples_and_not_identical_ones() {
        let a: Vec<(f64, f64)> = (0..60).map(|i| iv(2.0, 5.0 + (i % 5) as f64 * 0.2)).collect();
        let b: Vec<(f64, f64)> = a.iter().map(|x| iv(x.0, x.1 * 1.6)).collect();
        let r = perm_test(&a, &b, trimmed_ratio, 500, 7).unwrap();
        assert!(r.diff > 0.0);
        assert!(r.p < 0.01, "p {}", r.p);
        let r2 = perm_test(&a, &a, trimmed_ratio, 500, 7).unwrap();
        assert!(r2.p > 0.5, "p {}", r2.p);
    }

    fn fam(delta: f64, f: usize, usd: f64) -> FamIv {
        let mut by = [0.0; 5];
        by[f] = usd;
        FamIv { delta, usd, by_family: by }
    }

    #[test]
    fn basket_index_equals_family_rate_for_single_family() {
        let ivs: Vec<FamIv> = (0..12).map(|_| fam(4.0, 0, 10.0)).collect();
        let basket = basket_shares(&ivs);
        assert!((basket[0] - 1.0).abs() < 1e-9);
        let bi = basket_index(&ivs, &basket).unwrap();
        assert!((bi.index - 2.5).abs() < 1e-9);
        assert!((bi.coverage - 1.0).abs() < 1e-9);
    }

    #[test]
    fn basket_index_is_harmonic_and_refuses_thin_coverage() {
        // fable $2/1%, opus $4/1%, basket 50/50 → 1/(0.5/2+0.5/4) = 2.667
        let mut ivs: Vec<FamIv> = (0..10).map(|_| fam(5.0, 0, 10.0)).collect();
        ivs.extend((0..10).map(|_| fam(2.5, 1, 10.0)));
        let bi = basket_index(&ivs, &[0.5, 0.5, 0.0, 0.0, 0.0]).unwrap();
        assert!((bi.index - 8.0 / 3.0).abs() < 1e-9, "{}", bi.index);
        // a period with only fable data cannot price a 50/50 basket
        let only_fable: Vec<FamIv> = (0..10).map(|_| fam(5.0, 0, 10.0)).collect();
        assert!(basket_index(&only_fable, &[0.5, 0.5, 0.0, 0.0, 0.0]).is_none());
        // ...but can price a 90/10 one (coverage 0.9)
        let bi = basket_index(&only_fable, &[0.9, 0.1, 0.0, 0.0, 0.0]).unwrap();
        assert!((bi.coverage - 0.9).abs() < 1e-9);
        assert!((bi.index - 2.0).abs() < 1e-9);
    }

    // ---- v1.6（D80）回歸 ----
    fn sparse(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(m, v)| (m.to_string(), *v)).collect()
    }

    #[test]
    fn nnls_recovers_known_coefficients_and_stays_nonnegative() {
        // 真 β：fable 0.3 %/$、opus 0.15 %/$、sonnet 0.25 %/$；混用的段照樣能拆
        let mut rows: Vec<(Vec<(String, f64)>, f64)> = Vec::new();
        for i in 0..60 {
            let f = 5.0 + (i % 7) as f64;
            let o = 3.0 + (i % 5) as f64 * 2.0;
            let s = if i % 3 == 0 { 4.0 + (i % 4) as f64 } else { 0.0 };
            let y = 0.3 * f + 0.15 * o + 0.25 * s + ((i % 2) as f64 - 0.5) * 0.2;
            rows.push((sparse(&[("claude-fable-5", f), ("claude-opus-5", o), ("claude-sonnet-5", s)]), y));
        }
        let d = Design::from_sparse(rows.iter().map(|(bm, y)| (bm.as_slice(), *y)));
        assert_eq!(d.models, vec!["claude-fable-5", "claude-opus-5", "claude-sonnet-5"]);
        let b = nnls(&d.rows, 3);
        assert!((b[0] - 0.3).abs() < 0.03, "{b:?}");
        assert!((b[1] - 0.15).abs() < 0.03, "{b:?}");
        assert!((b[2] - 0.25).abs() < 0.05, "{b:?}");
        // 一個對 y 毫無貢獻、還跟 y 反向的欄：無約束會給負數，NNLS 要壓到 0
        let mut rows2 = rows.clone();
        for (i, (bm, y)) in rows2.iter_mut().enumerate() {
            bm.push(("claude-haiku-4-5-20251001".into(), 20.0 - (i % 10) as f64));
            *y += (i % 10) as f64 * 0.01;
        }
        let d2 = Design::from_sparse(rows2.iter().map(|(bm, y)| (bm.as_slice(), *y)));
        let b2 = nnls(&d2.rows, 4);
        assert!(b2.iter().all(|v| *v >= 0.0), "{b2:?}");
        assert_eq!(b2[1], 0.0, "haiku 欄（排序後第 2 個）該被壓在 0：{b2:?}");
    }

    #[test]
    fn single_model_regression_is_the_house_ratio() {
        // 只有一個模型時，w=1/usd 的過原點 WLS ＝ Σδ/Σusd（查證報告 §4.3b）
        let rows: Vec<(Vec<(String, f64)>, f64)> =
            (0..20).map(|i| (sparse(&[("claude-fable-5", 4.0 + (i % 5) as f64)]), 1.0 + (i % 3) as f64)).collect();
        let d = Design::from_sparse(rows.iter().map(|(bm, y)| (bm.as_slice(), *y)));
        let b = nnls(&d.rows, 1);
        let (sd, su) = rows.iter().fold((0.0, 0.0), |(sd, su), (bm, y)| (sd + y, su + bm[0].1));
        assert!((b[0] - sd / su).abs() < 1e-9, "{} vs {}", b[0], sd / su);
    }

    #[test]
    fn regress_flags_thin_and_collinear_columns_and_is_deterministic() {
        let mut rows: Vec<(Vec<(String, f64)>, f64)> = Vec::new();
        for i in 0..80 {
            let f = 5.0 + (i % 7) as f64;
            // opus-4-6 永遠是 fable 的 1/10：完全共線
            let mut bm = sparse(&[("claude-fable-5", f), ("claude-opus-4-6", f / 10.0)]);
            // haiku 只出現 4 段
            if i < 4 {
                bm.push(("claude-haiku-4-5-20251001".into(), 1.0));
            }
            rows.push((bm, 0.3 * f + 0.02 * f + ((i % 2) as f64 - 0.5) * 0.1));
        }
        let d = Design::from_sparse(rows.iter().map(|(bm, y)| (bm.as_slice(), *y)));
        let r = regress(&d, 200, 7);
        assert_eq!(r.n, 80);
        let by = |m: &str| r.rows.iter().find(|x| x.model == m).unwrap().clone();
        assert_eq!(by("claude-fable-5").status, "collinear");
        assert_eq!(by("claude-opus-4-6").collinear_with.as_deref(), Some("claude-fable-5"));
        assert_eq!(by("claude-haiku-4-5-20251001").status, "calibrating");
        assert_eq!(by("claude-haiku-4-5-20251001").n_seg, 4);
        assert!(r.r2.unwrap() > 0.9);
        let r2 = regress(&d, 200, 7);
        assert_eq!(r.rows[0].ci, r2.rows[0].ci, "同種子同區間");
    }

    #[test]
    fn sonnet_versions_share_one_group() {
        assert_eq!(group_index("claude-sonnet-5"), group_index("claude-sonnet-4-6"));
        assert_eq!(GROUPS[group_index("claude-sonnet-5").unwrap()], "sonnet");
        assert_ne!(group_index("claude-fable-5-1"), group_index("claude-opus-5"));
        let rows: Vec<(Vec<(String, f64)>, f64)> = (0..12)
            .map(|i| (sparse(&[("claude-sonnet-5", 2.0 + i as f64), ("claude-sonnet-4-6", 3.0)]), 1.0 + i as f64 * 0.5))
            .collect();
        let d = Design::from_sparse(rows.iter().map(|(bm, y)| (bm.as_slice(), *y)));
        let g = d.collapse_to_groups();
        assert_eq!(g.models, GROUPS.to_vec());
        assert!((g.rows[0].x[2] - 5.0).abs() < 1e-9, "sonnet 兩版合併到同一欄：{:?}", g.rows[0].x);
        assert_eq!(g.rows[0].x[0], 0.0);
        // 純區間法的四家族版：全是 Sonnet 的段要算成 sonnet 家的純區間
        let ivs: Vec<FamIv> = (0..6)
            .map(|_| FamIv { delta: 4.0, usd: 10.0, by_family: [0.0, 0.0, 6.0, 4.0, 0.0] })
            .collect();
        let r = group_rates(&ivs);
        assert!(r[2].is_some(), "{r:?}");
        assert!(family_rates(&ivs)[2].is_none() && family_rates(&ivs)[3].is_none(), "五分法分開就都不純");
    }

    #[test]
    fn model_labels_read_like_product_names() {
        assert_eq!(model_label("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(model_label("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_label("claude-opus-5"), "Opus 5");
    }

    #[test]
    fn pure_family_needs_share_and_delta() {
        let mut mixed = fam(4.0, 0, 8.0);
        mixed.by_family[1] = 3.0;
        mixed.usd = 11.0;
        assert_eq!(pure_family(&mixed), None);
        assert_eq!(pure_family(&fam(4.0, 1, 8.0)), Some(1));
        assert_eq!(pure_family(&fam(1.0, 1, 8.0)), None);
    }
}
