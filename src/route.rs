use clap::ValueEnum;
use serde::Serialize;

use crate::judge::Verdict;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    Page,
    Ticket,
    Log,
}

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub page: f32,
    pub ticket: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            page: 0.75,
            ticket: 0.5,
        }
    }
}

impl Thresholds {
    pub fn route(&self, verdict: &Verdict) -> Route {
        match verdict.attention() {
            a if a >= self.page => Route::Page,
            a if a >= self.ticket => Route::Ticket,
            _ => Route::Log,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(pageable: f32, detail: f32) -> Verdict {
        Verdict {
            pageable,
            detail,
            severity: 2.0,
            area: "other".into(),
        }
    }

    #[test]
    fn routes_on_attention() {
        let thresholds = Thresholds::default();
        assert_eq!(thresholds.route(&verdict(0.97, 0.05)), Route::Page);
        assert_eq!(thresholds.route(&verdict(0.97, 0.3)), Route::Ticket);
        assert_eq!(thresholds.route(&verdict(0.95, 0.9)), Route::Log);
        assert_eq!(thresholds.route(&verdict(0.2, 0.0)), Route::Log);
    }
}
