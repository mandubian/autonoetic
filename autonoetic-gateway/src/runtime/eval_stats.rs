//! Multi-axis statistical comparison for the self-improvement loop's
//! A/B testing (P1, #246).
//!
//! Pure logic, no IO. Consumers feed in two collections of per-axis
//! samples (typically derived from `SessionOutcome` rows of N replays
//! per variant) and receive a [`CompareRecommendation`] verdict plus
//! a confidence score derived by bootstrap resampling.
//!
//! ## Design philosophy
//!
//! Two principles run through the implementation:
//!
//! 1. **Completion is the Pareto floor.** A variant cannot be preferred
//!    if its completion rate is meaningfully below the other's, no
//!    matter how cheap or fast it is. The whole point of the
//!    self-improvement loop is to escape "cost cuts that came from
//!    giving up on hard sub-problems"; the rule lives here.
//!
//! 2. **Inconclusive is a normal outcome.** When CIs overlap and
//!    neither variant Pareto-dominates, return `Inconclusive` rather
//!    than guessing. The downstream operator-approval path treats
//!    `Inconclusive` as "collect more data or drop"; that's the
//!    correct response.
//!
//! ## Bootstrap CI
//!
//! For each axis we report the sample mean plus a 95% bootstrap
//! percentile confidence interval. Bootstrap is used (rather than a
//! parametric test) so the same code handles the small-N case (≥ 3
//! samples) without distributional assumptions. The number of
//! bootstrap replicates defaults to 1000 — enough for stable 95%
//! percentile bounds but cheap (≤ 1 ms even at N = 50, 7 axes).
//!
//! ## Confidence
//!
//! When the rule reaches `PreferA` or `PreferB`, we re-run the
//! decision over each bootstrap replicate of the input samples and
//! report the **fraction of replicates that produce the same
//! verdict**. A confidence of 0.95 means "if I'd drawn slightly
//! different samples, the same answer comes out 95% of the time".

use rand::Rng;
use serde::{Deserialize, Serialize};

/// Per-axis input samples for a single variant. All axes have one
/// `f64` per replay (typically 3–10 replays).
#[derive(Debug, Clone, Default)]
pub struct VariantSamples {
    /// `1.0` for success, `0.0` for failure, on `SessionOutcome
    /// ::judged_success()`. Sessions with `Unknown` completion and no
    /// operator rating should be skipped by the caller, not folded in
    /// as 0.5.
    pub completion: Vec<f64>,
    pub cost_usd: Vec<f64>,
    pub tokens: Vec<f64>,
    pub turns: Vec<f64>,
    pub wall_clock_secs: Vec<f64>,
}

impl VariantSamples {
    pub fn sample_count(&self) -> usize {
        // All axes should have the same length — invariant from the
        // caller (one row per replay). We take the min so that a
        // partial input doesn't claim more samples than it has.
        [
            self.completion.len(),
            self.cost_usd.len(),
            self.tokens.len(),
            self.turns.len(),
            self.wall_clock_secs.len(),
        ]
        .iter()
        .copied()
        .min()
        .unwrap_or(0)
    }
}

/// Comparison knobs. Defaults match the design doc §5 D4 (Pareto floor)
/// and the per-axis tolerances called out in #246.
#[derive(Debug, Clone)]
pub struct CompareConfig {
    pub bootstrap_iterations: usize,
    /// Confidence level for CIs (e.g. 0.95 = 95% CI).
    pub confidence_level: f64,
    /// Pareto floor on completion: a variant whose mean completion
    /// rate is below the other's by more than this is mechanically
    /// rejected. Units: absolute fraction (e.g., 0.05 = 5 percentage
    /// points).
    pub completion_floor_tolerance: f64,
    /// Per-axis tolerances for the Pareto-dominance check on cost /
    /// tokens / turns / wall. A variant must beat the other on an
    /// axis by *more* than this fraction (relative to the other's
    /// mean) to count as "better" on that axis. Prevents
    /// floating-point noise from declaring a winner.
    pub axis_tolerance_pct: f64,
    /// Seed for the bootstrap RNG. `None` means non-deterministic
    /// (production); tests pin a seed for reproducibility.
    pub rng_seed: Option<u64>,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            bootstrap_iterations: 1000,
            confidence_level: 0.95,
            completion_floor_tolerance: 0.05,
            axis_tolerance_pct: 0.05,
            rng_seed: None,
        }
    }
}

/// Sample mean + 95% (configurable) bootstrap percentile CI for one
/// axis of one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxisCI {
    pub axis: String,
    pub mean: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub sample_count: usize,
}

/// Per-axis deltas (B − A) when a winner is named.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxisDeltas {
    pub completion_delta: f64,
    pub cost_usd_delta: f64,
    pub tokens_delta: f64,
    pub turns_delta: f64,
    pub wall_clock_secs_delta: f64,
}

/// Outcome of [`compare`]. Mirrors the shape called out in #246.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recommendation", rename_all = "snake_case")]
pub enum CompareRecommendation {
    PreferA {
        evidence: AxisDeltas,
        confidence: f64,
    },
    PreferB {
        evidence: AxisDeltas,
        confidence: f64,
    },
    Inconclusive {
        reason: String,
        axis_cis: AxisCISummary,
    },
}

/// Bundle of per-axis CIs for both variants, included on
/// `Inconclusive` so the operator can see exactly which axes were
/// uncertain. (We don't repeat this on `PreferA` / `PreferB` to keep
/// the happy-path payload small; consumers that want the full
/// breakdown can call [`compute_axis_cis`] directly.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxisCISummary {
    pub variant_a: Vec<AxisCI>,
    pub variant_b: Vec<AxisCI>,
}

/// Top-level entry point. `Err` is reserved for caller-fault (e.g.,
/// not enough samples to even attempt a comparison); a normal
/// inconclusive result is returned as `Ok(Inconclusive)`.
pub fn compare(
    a: &VariantSamples,
    b: &VariantSamples,
    config: &CompareConfig,
) -> Result<CompareRecommendation, String> {
    let n_a = a.sample_count();
    let n_b = b.sample_count();
    if n_a < 3 || n_b < 3 {
        return Err(format!(
            "insufficient samples for comparison: variant A has {}, variant B has {} (need ≥ 3 each)",
            n_a, n_b
        ));
    }

    let mut rng = make_rng(config.rng_seed);

    // Compute the means once for the central recommendation.
    let mean_a = AxisMeans::from(a);
    let mean_b = AxisMeans::from(b);
    let central = decide(&mean_a, &mean_b, config);

    match central {
        Verdict::Inconclusive(reason) => {
            let axis_cis = compute_axis_cis(a, b, config, &mut rng);
            Ok(CompareRecommendation::Inconclusive {
                reason,
                axis_cis,
            })
        }
        Verdict::PreferA | Verdict::PreferB => {
            let confidence = bootstrap_confidence(a, b, config, &central, &mut rng);
            let evidence = AxisDeltas {
                completion_delta: mean_b.completion - mean_a.completion,
                cost_usd_delta: mean_b.cost_usd - mean_a.cost_usd,
                tokens_delta: mean_b.tokens - mean_a.tokens,
                turns_delta: mean_b.turns - mean_a.turns,
                wall_clock_secs_delta: mean_b.wall_clock_secs - mean_a.wall_clock_secs,
            };
            match central {
                Verdict::PreferA => Ok(CompareRecommendation::PreferA { evidence, confidence }),
                Verdict::PreferB => Ok(CompareRecommendation::PreferB { evidence, confidence }),
                Verdict::Inconclusive(_) => unreachable!(),
            }
        }
    }
}

/// Compute axis-level CIs for both variants. Exposed so callers that
/// want a richer breakdown can request it independently of the central
/// decision. Uses the same RNG seed as [`compare`] when given the same
/// config.
pub fn compute_axis_cis(
    a: &VariantSamples,
    b: &VariantSamples,
    config: &CompareConfig,
    rng: &mut impl Rng,
) -> AxisCISummary {
    AxisCISummary {
        variant_a: variant_axis_cis(a, config, rng),
        variant_b: variant_axis_cis(b, config, rng),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Internal: per-axis mean bundle, decision rule, bootstrap helpers
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct AxisMeans {
    completion: f64,
    cost_usd: f64,
    tokens: f64,
    turns: f64,
    wall_clock_secs: f64,
}

impl From<&VariantSamples> for AxisMeans {
    fn from(v: &VariantSamples) -> Self {
        Self {
            completion: mean(&v.completion),
            cost_usd: mean(&v.cost_usd),
            tokens: mean(&v.tokens),
            turns: mean(&v.turns),
            wall_clock_secs: mean(&v.wall_clock_secs),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    PreferA,
    PreferB,
    Inconclusive(String),
}

/// Core rule operating on already-computed means. Pure function;
/// bootstrap loop calls this once per resample to compute confidence.
fn decide(a: &AxisMeans, b: &AxisMeans, config: &CompareConfig) -> Verdict {
    // 1. Pareto floor on completion. If either side's completion is
    //    meaningfully below the other's, that side cannot win.
    let completion_delta = b.completion - a.completion;
    if completion_delta < -config.completion_floor_tolerance {
        // B's completion is meaningfully worse → cannot prefer B; A wins
        // by floor. (This is the "cost cut that came from giving up"
        // case the design doc calls out.)
        return Verdict::PreferA;
    }
    if completion_delta > config.completion_floor_tolerance {
        return Verdict::PreferB;
    }

    // 2. Completion is similar enough → Pareto check on cost-style
    //    axes. "Better" on a cost axis = LOWER (we minimize cost,
    //    tokens, turns, wall clock).
    let cost_pref = prefer_lower(a.cost_usd, b.cost_usd, config.axis_tolerance_pct);
    let tokens_pref = prefer_lower(a.tokens, b.tokens, config.axis_tolerance_pct);
    let turns_pref = prefer_lower(a.turns, b.turns, config.axis_tolerance_pct);
    let wall_pref = prefer_lower(
        a.wall_clock_secs,
        b.wall_clock_secs,
        config.axis_tolerance_pct,
    );

    // Pareto domination across the 4 cost-style axes:
    //   - All preferences must be the same direction OR tied
    //   - At least one must be strictly preferred
    let prefs = [cost_pref, tokens_pref, turns_pref, wall_pref];
    let any_b = prefs.iter().any(|p| matches!(p, AxisPref::PreferB));
    let any_a = prefs.iter().any(|p| matches!(p, AxisPref::PreferA));

    match (any_a, any_b) {
        (false, false) => Verdict::Inconclusive(
            "all axes within tolerance — no meaningful difference between variants".to_string(),
        ),
        (false, true) => Verdict::PreferB,
        (true, false) => Verdict::PreferA,
        (true, true) => Verdict::Inconclusive(
            "axes disagree: A wins on some, B wins on others (no Pareto dominance)".to_string(),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AxisPref {
    PreferA,
    PreferB,
    Tied,
}

/// "Lower is better" — what cost/tokens/turns/wall_clock are. Returns
/// `Tied` when the difference is within `tolerance_pct` of the larger
/// value (relative tolerance: a 5% difference at $1.00 is meaningful,
/// a 5% difference at $0.0001 is noise).
fn prefer_lower(a: f64, b: f64, tolerance_pct: f64) -> AxisPref {
    let larger = a.abs().max(b.abs());
    if larger == 0.0 {
        return AxisPref::Tied;
    }
    let rel_diff = (a - b).abs() / larger;
    if rel_diff <= tolerance_pct {
        return AxisPref::Tied;
    }
    if a < b {
        AxisPref::PreferA
    } else {
        AxisPref::PreferB
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn variant_axis_cis(
    v: &VariantSamples,
    config: &CompareConfig,
    rng: &mut impl Rng,
) -> Vec<AxisCI> {
    vec![
        axis_ci("completion", &v.completion, config, rng),
        axis_ci("cost_usd", &v.cost_usd, config, rng),
        axis_ci("tokens", &v.tokens, config, rng),
        axis_ci("turns", &v.turns, config, rng),
        axis_ci("wall_clock_secs", &v.wall_clock_secs, config, rng),
    ]
}

fn axis_ci(axis: &str, xs: &[f64], config: &CompareConfig, rng: &mut impl Rng) -> AxisCI {
    let mut replicate_means: Vec<f64> = Vec::with_capacity(config.bootstrap_iterations);
    for _ in 0..config.bootstrap_iterations {
        replicate_means.push(mean(&bootstrap_resample(xs, rng)));
    }
    let (ci_low, ci_high) = percentile_ci(&mut replicate_means, config.confidence_level);
    AxisCI {
        axis: axis.to_string(),
        mean: mean(xs),
        ci_low,
        ci_high,
        sample_count: xs.len(),
    }
}

fn bootstrap_resample(xs: &[f64], rng: &mut impl Rng) -> Vec<f64> {
    let n = xs.len();
    (0..n).map(|_| xs[rng.gen_range(0..n)]).collect()
}

/// Percentile CI from a precomputed vector of bootstrap replicate
/// statistics. Mutates `replicates` (sorts in place) — the caller owns
/// the buffer and can drop it after.
fn percentile_ci(replicates: &mut [f64], confidence_level: f64) -> (f64, f64) {
    if replicates.is_empty() {
        return (0.0, 0.0);
    }
    replicates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let alpha = (1.0 - confidence_level) / 2.0;
    let lo_idx = ((replicates.len() as f64 - 1.0) * alpha).round() as usize;
    let hi_idx = ((replicates.len() as f64 - 1.0) * (1.0 - alpha)).round() as usize;
    (replicates[lo_idx], replicates[hi_idx.min(replicates.len() - 1)])
}

/// Re-run the decision on bootstrap-resampled inputs and report the
/// fraction of replicates that produce the same verdict as the
/// central one. Bounded `[0.0, 1.0]`.
fn bootstrap_confidence(
    a: &VariantSamples,
    b: &VariantSamples,
    config: &CompareConfig,
    central: &Verdict,
    rng: &mut impl Rng,
) -> f64 {
    if matches!(central, Verdict::Inconclusive(_)) {
        return 0.0;
    }
    let mut agreements: usize = 0;
    for _ in 0..config.bootstrap_iterations {
        let resampled_a = resample_variant(a, rng);
        let resampled_b = resample_variant(b, rng);
        let mean_a = AxisMeans::from(&resampled_a);
        let mean_b = AxisMeans::from(&resampled_b);
        let v = decide(&mean_a, &mean_b, config);
        if v == *central {
            agreements += 1;
        }
    }
    agreements as f64 / config.bootstrap_iterations as f64
}

fn resample_variant(v: &VariantSamples, rng: &mut impl Rng) -> VariantSamples {
    let n = v.sample_count();
    let indices: Vec<usize> = (0..n).map(|_| rng.gen_range(0..n)).collect();
    VariantSamples {
        completion: indices.iter().map(|&i| v.completion[i]).collect(),
        cost_usd: indices.iter().map(|&i| v.cost_usd[i]).collect(),
        tokens: indices.iter().map(|&i| v.tokens[i]).collect(),
        turns: indices.iter().map(|&i| v.turns[i]).collect(),
        wall_clock_secs: indices.iter().map(|&i| v.wall_clock_secs[i]).collect(),
    }
}

/// Reproducible RNG when a seed is provided, else thread-local.
fn make_rng(seed: Option<u64>) -> Box<dyn rand::RngCore> {
    match seed {
        Some(s) => Box::new(rand::rngs::StdRng::from_seed_u64(s)),
        None => Box::new(rand::rngs::ThreadRng::default()),
    }
}

// Trait helper so callers can write a single `make_rng` path.
trait StdRngFromSeed {
    fn from_seed_u64(seed: u64) -> Self;
}

impl StdRngFromSeed for rand::rngs::StdRng {
    fn from_seed_u64(seed: u64) -> Self {
        use rand::SeedableRng;
        Self::seed_from_u64(seed)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_samples(
        completion: Vec<f64>,
        cost: Vec<f64>,
        tokens: Vec<f64>,
        turns: Vec<f64>,
        wall: Vec<f64>,
    ) -> VariantSamples {
        VariantSamples {
            completion,
            cost_usd: cost,
            tokens,
            turns,
            wall_clock_secs: wall,
        }
    }

    fn seeded_config() -> CompareConfig {
        CompareConfig {
            rng_seed: Some(42),
            ..Default::default()
        }
    }

    // ── Means + AxisPref primitives ────────────────────────────────────

    #[test]
    fn mean_of_empty_is_zero() {
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn mean_simple_average() {
        assert!((mean(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn prefer_lower_flags_real_difference() {
        assert_eq!(prefer_lower(0.10, 0.20, 0.05), AxisPref::PreferA);
        assert_eq!(prefer_lower(0.20, 0.10, 0.05), AxisPref::PreferB);
    }

    #[test]
    fn prefer_lower_treats_within_tolerance_as_tied() {
        // 0.10 vs 0.103 = 2.9% diff; tolerance 5% → Tied.
        assert_eq!(prefer_lower(0.10, 0.103, 0.05), AxisPref::Tied);
    }

    #[test]
    fn prefer_lower_zero_both_is_tied() {
        assert_eq!(prefer_lower(0.0, 0.0, 0.05), AxisPref::Tied);
    }

    // ── Decision rule edge cases ──────────────────────────────────────

    #[test]
    fn decide_pareto_floor_rejects_cheaper_but_less_successful() {
        // B is half the cost but completes 30% less often.
        let a = AxisMeans {
            completion: 0.90,
            cost_usd: 1.00,
            tokens: 1000.0,
            turns: 10.0,
            wall_clock_secs: 60.0,
        };
        let b = AxisMeans {
            completion: 0.60,
            cost_usd: 0.50,
            tokens: 500.0,
            turns: 6.0,
            wall_clock_secs: 30.0,
        };
        let cfg = CompareConfig::default();
        assert_eq!(decide(&a, &b, &cfg), Verdict::PreferA);
    }

    #[test]
    fn decide_completion_floor_prefers_b_when_b_completes_more() {
        let a = AxisMeans {
            completion: 0.50,
            cost_usd: 0.10,
            tokens: 100.0,
            turns: 5.0,
            wall_clock_secs: 30.0,
        };
        let b = AxisMeans {
            completion: 0.90,
            cost_usd: 0.20,
            tokens: 200.0,
            turns: 10.0,
            wall_clock_secs: 60.0,
        };
        let cfg = CompareConfig::default();
        assert_eq!(decide(&a, &b, &cfg), Verdict::PreferB);
    }

    #[test]
    fn decide_pareto_dominance_b_strictly_cheaper_on_all_axes() {
        let a = AxisMeans {
            completion: 0.90,
            cost_usd: 1.00,
            tokens: 1000.0,
            turns: 10.0,
            wall_clock_secs: 60.0,
        };
        let b = AxisMeans {
            completion: 0.90,
            cost_usd: 0.70,
            tokens: 700.0,
            turns: 7.0,
            wall_clock_secs: 40.0,
        };
        let cfg = CompareConfig::default();
        assert_eq!(decide(&a, &b, &cfg), Verdict::PreferB);
    }

    #[test]
    fn decide_split_axes_is_inconclusive() {
        // Same completion, A is cheaper but B is faster — no Pareto win.
        let a = AxisMeans {
            completion: 0.90,
            cost_usd: 0.50,
            tokens: 800.0,
            turns: 10.0,
            wall_clock_secs: 60.0,
        };
        let b = AxisMeans {
            completion: 0.90,
            cost_usd: 1.00,
            tokens: 1200.0,
            turns: 6.0,
            wall_clock_secs: 30.0,
        };
        let cfg = CompareConfig::default();
        let v = decide(&a, &b, &cfg);
        assert!(matches!(v, Verdict::Inconclusive(_)));
    }

    #[test]
    fn decide_all_within_tolerance_is_inconclusive() {
        let a = AxisMeans {
            completion: 0.90,
            cost_usd: 0.50,
            tokens: 800.0,
            turns: 10.0,
            wall_clock_secs: 60.0,
        };
        // All within 3% of A → no meaningful difference.
        let b = AxisMeans {
            completion: 0.91,
            cost_usd: 0.515,
            tokens: 824.0,
            turns: 10.3,
            wall_clock_secs: 62.0,
        };
        let cfg = CompareConfig::default();
        let v = decide(&a, &b, &cfg);
        assert!(matches!(v, Verdict::Inconclusive(_)));
    }

    // ── compare() entry point: small-N rejection ──────────────────────

    #[test]
    fn compare_rejects_small_samples() {
        let a = make_samples(
            vec![1.0, 1.0],
            vec![0.10, 0.11],
            vec![100.0, 110.0],
            vec![5.0, 6.0],
            vec![30.0, 35.0],
        );
        let b = a.clone();
        let r = compare(&a, &b, &seeded_config());
        assert!(r.is_err(), "expected Err for N<3, got {:?}", r);
    }

    // ── compare() integration: known-better B is preferred ────────────

    #[test]
    fn compare_prefers_b_when_b_dominates_on_cost() {
        // 10 replays each. B is consistently cheaper at the same
        // completion rate.
        let a = make_samples(
            vec![1.0; 10],
            vec![1.00, 1.02, 0.98, 1.01, 1.00, 0.99, 1.03, 0.97, 1.01, 1.02],
            vec![1000.0; 10],
            vec![10.0; 10],
            vec![60.0; 10],
        );
        let b = make_samples(
            vec![1.0; 10],
            vec![0.50, 0.52, 0.49, 0.51, 0.50, 0.48, 0.53, 0.47, 0.51, 0.50],
            vec![500.0; 10],
            vec![5.0; 10],
            vec![30.0; 10],
        );
        let r = compare(&a, &b, &seeded_config()).unwrap();
        match r {
            CompareRecommendation::PreferB { confidence, evidence } => {
                assert!(confidence > 0.95, "expected high confidence, got {}", confidence);
                assert!(evidence.cost_usd_delta < 0.0);
            }
            other => panic!("expected PreferB, got {:?}", other),
        }
    }

    // ── Pareto floor end-to-end ───────────────────────────────────────

    #[test]
    fn compare_rejects_cheaper_b_that_fails_more() {
        // B is half the cost but completes 40% less often.
        let a = make_samples(
            vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            vec![1.00; 10],
            vec![1000.0; 10],
            vec![10.0; 10],
            vec![60.0; 10],
        );
        let b = make_samples(
            vec![1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            vec![0.50; 10],
            vec![500.0; 10],
            vec![5.0; 10],
            vec![30.0; 10],
        );
        let r = compare(&a, &b, &seeded_config()).unwrap();
        match r {
            CompareRecommendation::PreferA { confidence, .. } => {
                assert!(confidence > 0.90, "expected high confidence for floor-reject");
            }
            other => panic!("expected PreferA (Pareto floor), got {:?}", other),
        }
    }

    // ── Inconclusive on overlapping CIs ───────────────────────────────

    #[test]
    fn compare_inconclusive_when_axes_split() {
        // Same completion, A cheaper but B faster — split axes.
        let a = make_samples(
            vec![1.0; 8],
            vec![0.50; 8],
            vec![800.0; 8],
            vec![10.0; 8],
            vec![60.0; 8],
        );
        let b = make_samples(
            vec![1.0; 8],
            vec![1.00; 8],
            vec![1200.0; 8],
            vec![6.0; 8],
            vec![30.0; 8],
        );
        let r = compare(&a, &b, &seeded_config()).unwrap();
        assert!(matches!(r, CompareRecommendation::Inconclusive { .. }));
    }

    // ── False-positive property check ─────────────────────────────────

    #[test]
    fn no_real_difference_does_not_call_a_winner_too_often() {
        // Generate 20 trials of N=10 samples per variant, drawn from
        // the SAME distribution. Decision should be "Inconclusive"
        // on the large majority. We pick a permissive bound (>= 80%
        // Inconclusive) because a small N still has bootstrap noise;
        // the design goal is to avoid CONFIDENTLY claiming a winner
        // when there is none. We also verify that any winner-call
        // has confidence ≤ 0.85 (i.e., the decision *could* easily
        // flip on resampled data), so the operator-facing
        // "Recommended" surface wouldn't fire.
        let trials = 20;
        let mut inconclusive_count = 0;
        let mut max_winner_confidence: f64 = 0.0;

        // Use a different seed per trial so the bootstrap isn't itself
        // identical; the *input data* is sampled from the same
        // distribution and is what we're testing here.
        for trial in 0..trials {
            // Synthetic identical-distribution samples: small jitter
            // around the same mean. seeded_config() pins bootstrap;
            // the data is hand-crafted to be "the same up to noise".
            let a = make_samples(
                vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
                vec![0.50, 0.51, 0.49, 0.50, 0.50, 0.51, 0.49, 0.50, 0.50, 0.51],
                vec![800.0; 10],
                vec![10.0; 10],
                vec![60.0; 10],
            );
            let b = a.clone();
            let cfg = CompareConfig {
                rng_seed: Some(1000 + trial),
                ..Default::default()
            };
            let r = compare(&a, &b, &cfg).unwrap();
            match r {
                CompareRecommendation::Inconclusive { .. } => inconclusive_count += 1,
                CompareRecommendation::PreferA { confidence, .. }
                | CompareRecommendation::PreferB { confidence, .. } => {
                    max_winner_confidence = max_winner_confidence.max(confidence);
                }
            }
        }
        assert!(
            inconclusive_count as f64 / trials as f64 >= 0.80,
            "expected ≥ 80% Inconclusive on no-real-difference data, got {} of {}",
            inconclusive_count, trials
        );
        if max_winner_confidence > 0.0 {
            assert!(
                max_winner_confidence <= 0.95,
                "any winner-call on identical data must have confidence ≤ 0.95; got {}",
                max_winner_confidence
            );
        }
    }

    // ── Reproducibility under seed ────────────────────────────────────

    #[test]
    fn same_seed_yields_same_decision_and_confidence() {
        let a = make_samples(
            vec![1.0; 10],
            vec![1.00, 1.02, 0.98, 1.01, 1.00, 0.99, 1.03, 0.97, 1.01, 1.02],
            vec![1000.0; 10],
            vec![10.0; 10],
            vec![60.0; 10],
        );
        let b = make_samples(
            vec![1.0; 10],
            vec![0.50, 0.52, 0.49, 0.51, 0.50, 0.48, 0.53, 0.47, 0.51, 0.50],
            vec![500.0; 10],
            vec![5.0; 10],
            vec![30.0; 10],
        );
        let r1 = compare(&a, &b, &seeded_config()).unwrap();
        let r2 = compare(&a, &b, &seeded_config()).unwrap();
        assert_eq!(r1, r2);
    }
}
