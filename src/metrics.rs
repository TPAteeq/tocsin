use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Confusion {
    pub tp: u64,
    pub fp: u64,
    #[serde(rename = "fn")]
    pub fn_: u64,
    pub tn: u64,
}

impl Confusion {
    pub fn precision(&self) -> f64 {
        ratio(self.tp, self.tp + self.fp)
    }

    pub fn recall(&self) -> f64 {
        ratio(self.tp, self.tp + self.fn_)
    }

    pub fn f1(&self) -> f64 {
        ratio(2 * self.tp, 2 * self.tp + self.fp + self.fn_)
    }
}

pub fn confusion(scores: &[f32], labels: &[bool], threshold: f32) -> Confusion {
    scores
        .iter()
        .zip(labels)
        .fold(Confusion::default(), |mut c, (&s, &y)| {
            match (s >= threshold, y) {
                (true, true) => c.tp += 1,
                (true, false) => c.fp += 1,
                (false, true) => c.fn_ += 1,
                (false, false) => c.tn += 1,
            }
            c
        })
}

#[derive(Debug, Clone, Serialize)]
pub struct Ranking {
    pub average_precision: f64,
    pub roc_auc: f64,
    pub best_f1: f64,
    pub best_threshold: f32,
}

pub fn ranking(scores: &[f32], labels: &[bool]) -> Ranking {
    let positives = labels.iter().filter(|&&y| y).count() as f64;
    let negatives = labels.len() as f64 - positives;
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_unstable_by(|&a, &b| scores[b].total_cmp(&scores[a]));

    let (mut tp, mut fp, mut i) = (0.0, 0.0, 0);
    let (mut ap, mut auc, mut prev_tp, mut prev_fp) = (0.0, 0.0, 0.0, 0.0);
    let (mut best_f1, mut best_threshold) = (0.0, f32::INFINITY);
    while i < order.len() {
        let threshold = scores[order[i]];
        while i < order.len() && same(scores[order[i]], threshold) {
            if labels[order[i]] {
                tp += 1.0
            } else {
                fp += 1.0
            }
            i += 1;
        }
        if positives > 0.0 {
            ap += (tp - prev_tp) / positives * tp / (tp + fp);
        }
        if positives > 0.0 && negatives > 0.0 {
            auc += (fp - prev_fp) / negatives * (tp + prev_tp) / (2.0 * positives);
        }
        let f1 = 2.0 * tp / (2.0 * tp + fp + (positives - tp));
        if f1 > best_f1 {
            best_f1 = f1;
            best_threshold = threshold;
        }
        (prev_tp, prev_fp) = (tp, fp);
    }
    Ranking {
        average_precision: ap,
        roc_auc: auc,
        best_f1,
        best_threshold,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Bin {
    pub lo: f32,
    pub hi: f32,
    pub count: u64,
    pub mean_predicted: f64,
    pub observed_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Calibration {
    pub brier: f64,
    pub ece: f64,
    pub bins: Vec<Bin>,
}

pub fn calibration(probs: &[f32], targets: &[f32], bins: usize) -> Calibration {
    let mut sums = vec![(0u64, 0.0f64, 0.0f64); bins];
    let mut brier = 0.0;
    for (&p, &y) in probs.iter().zip(targets) {
        let b = ((p.clamp(0.0, 1.0) * bins as f32) as usize).min(bins - 1);
        sums[b].0 += 1;
        sums[b].1 += p as f64;
        sums[b].2 += y as f64;
        brier += (p as f64 - y as f64).powi(2);
    }
    let n = probs.len().max(1) as f64;
    let bins: Vec<Bin> = sums
        .into_iter()
        .enumerate()
        .map(|(i, (count, p, y))| Bin {
            lo: i as f32 / bins as f32,
            hi: (i + 1) as f32 / bins as f32,
            count,
            mean_predicted: p / count.max(1) as f64,
            observed_rate: y / count.max(1) as f64,
        })
        .collect();
    let ece = bins
        .iter()
        .map(|b| b.count as f64 / n * (b.mean_predicted - b.observed_rate).abs())
        .sum();
    Calibration {
        brier: brier / n,
        ece,
        bins,
    }
}

fn same(a: f32, b: f32) -> bool {
    a == b || a.to_bits() == b.to_bits()
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_ranking() {
        let r = ranking(&[0.9, 0.8, 0.2, 0.1], &[true, true, false, false]);
        assert_eq!((r.average_precision, r.roc_auc, r.best_f1), (1.0, 1.0, 1.0));
        assert_eq!(r.best_threshold, 0.8);
    }

    #[test]
    fn ranking_matches_reference_values() {
        let scores = [0.1, 0.4, 0.35, 0.8];
        let labels = [false, false, true, true];
        let r = ranking(&scores, &labels);
        assert!((r.average_precision - 0.8333333).abs() < 1e-6);
        assert!((r.roc_auc - 0.75).abs() < 1e-9);
    }

    #[test]
    fn tied_scores_are_one_threshold() {
        let r = ranking(&[0.5, 0.5, 0.5, 0.5], &[true, false, true, false]);
        assert!((r.average_precision - 0.5).abs() < 1e-9);
        assert!((r.roc_auc - 0.5).abs() < 1e-9);
    }

    #[test]
    fn signed_zeros_tie() {
        let r = ranking(&[0.0, -0.0], &[true, false]);
        assert_eq!((r.average_precision, r.roc_auc), (0.5, 0.5));
    }

    #[test]
    fn non_finite_scores_terminate() {
        let r = ranking(&[f32::NAN, 0.9, f32::NAN], &[true, false, true]);
        assert!(r.roc_auc.is_finite());
    }

    #[test]
    fn confusion_counts() {
        let c = confusion(&[0.9, 0.6, 0.4, 0.1], &[true, false, true, false], 0.5);
        assert_eq!(
            c,
            Confusion {
                tp: 1,
                fp: 1,
                fn_: 1,
                tn: 1
            }
        );
        assert_eq!((c.precision(), c.recall(), c.f1()), (0.5, 0.5, 0.5));
    }

    #[test]
    fn calibrated_predictions_have_low_ece() {
        let probs = [0.25f32; 8];
        let targets = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let c = calibration(&probs, &targets, 10);
        assert!(c.ece < 1e-6);
        assert!((c.brier - 0.1875).abs() < 1e-6);
        assert_eq!(c.bins[2].count, 8);
    }
}
