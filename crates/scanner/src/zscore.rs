//! Stateful cross-venue spread mean-reversion signal.
//!
//! This module contains no exchange IO and never places orders. A live feed, paper broker and
//! live executor can therefore share exactly the same entry and exit decisions.

use std::collections::VecDeque;

use arb_core::Decimal;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct Settings {
    pub window: usize,
    pub warmup: usize,
    pub entry_z: f64,
    pub max_entry_z: f64,
    pub exit_z: f64,
    pub stop_z: f64,
    pub confirm_ms: i64,
    pub min_hold_ms: i64,
    pub max_hold_ms: i64,
    pub max_book_skew_ms: i64,
    pub min_projected_net_pnl: Decimal,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window: 300,
            warmup: 300,
            entry_z: 2.5,
            max_entry_z: 8.0,
            exit_z: 0.5,
            stop_z: 4.0,
            confirm_ms: 3_000,
            min_hold_ms: 5_000,
            max_hold_ms: 2 * 60 * 60 * 1_000,
            max_book_skew_ms: 500,
            min_projected_net_pnl: Decimal::new(10, 2),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub at_ms: i64,
    /// ln(venue_a_mid / venue_b_mid) * 10_000.
    pub spread_bps: f64,
    pub book_skew_ms: i64,
    /// Projected convergence PnL after entry/exit depth, four fees and safety costs.
    pub projected_net_pnl: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    LongAShortB,
    LongBShortA,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    BookSkew,
    Warmup,
    ZBelowEntry,
    ZAboveMaximum,
    AwaitingConfirmation,
    ProjectedNetPnl,
    Open(Direction),
    Hold,
    CloseConvergence,
    CloseStop,
    CloseTimeout,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evaluation {
    pub decision: Decision,
    pub samples: usize,
    pub mean_bps: f64,
    pub standard_deviation_bps: f64,
    pub z_score: f64,
    pub signal_age_ms: i64,
    pub projected_net_pnl: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct PendingSignal {
    direction: Direction,
    since_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct Position {
    direction: Direction,
    opened_at_ms: i64,
}

pub struct Engine {
    settings: Settings,
    values: VecDeque<f64>,
    pending: Option<PendingSignal>,
    position: Option<Position>,
}

impl Engine {
    pub fn new(settings: Settings) -> Result<Self, String> {
        if settings.window < 2 || settings.warmup < 2 || settings.warmup > settings.window {
            return Err("z-score window/warmup must satisfy window >= warmup >= 2".into());
        }
        if settings.entry_z <= 0.0
            || settings.max_entry_z < settings.entry_z
            || settings.exit_z < 0.0
            || settings.stop_z <= settings.entry_z
        {
            return Err("invalid z-score thresholds".into());
        }
        Ok(Self {
            settings,
            values: VecDeque::new(),
            pending: None,
            position: None,
        })
    }

    pub fn on_tick(&mut self, tick: Tick) -> Evaluation {
        if tick.book_skew_ms.abs() > self.settings.max_book_skew_ms {
            self.pending = None;
            return self.evaluation(Decision::BookSkew, tick, 0.0, 0.0, 0.0, 0);
        }

        self.values.push_back(tick.spread_bps);
        if self.values.len() > self.settings.window {
            self.values.pop_front();
        }
        let (mean, std_dev, z_score) = statistics(&self.values, tick.spread_bps);

        if let Some(position) = self.position {
            let held_ms = tick.at_ms.saturating_sub(position.opened_at_ms);
            let decision = if z_score.abs() >= self.settings.stop_z {
                Some(Decision::CloseStop)
            } else if held_ms >= self.settings.max_hold_ms {
                Some(Decision::CloseTimeout)
            } else if held_ms >= self.settings.min_hold_ms && z_score.abs() <= self.settings.exit_z {
                Some(Decision::CloseConvergence)
            } else {
                None
            };
            if let Some(decision) = decision {
                self.position = None;
                return self.evaluation(decision, tick, mean, std_dev, z_score, 0);
            }
            return self.evaluation(Decision::Hold, tick, mean, std_dev, z_score, 0);
        }

        if self.values.len() < self.settings.warmup {
            self.pending = None;
            return self.evaluation(Decision::Warmup, tick, mean, std_dev, z_score, 0);
        }
        if z_score.abs() < self.settings.entry_z {
            self.pending = None;
            return self.evaluation(Decision::ZBelowEntry, tick, mean, std_dev, z_score, 0);
        }
        if z_score.abs() > self.settings.max_entry_z {
            self.pending = None;
            return self.evaluation(Decision::ZAboveMaximum, tick, mean, std_dev, z_score, 0);
        }

        let direction = if z_score < 0.0 {
            Direction::LongAShortB
        } else {
            Direction::LongBShortA
        };
        let pending = match self.pending {
            Some(signal) if signal.direction == direction => signal,
            _ => {
                let signal = PendingSignal { direction, since_ms: tick.at_ms };
                self.pending = Some(signal);
                signal
            }
        };
        let signal_age_ms = tick.at_ms.saturating_sub(pending.since_ms);
        if signal_age_ms < self.settings.confirm_ms {
            return self.evaluation(
                Decision::AwaitingConfirmation,
                tick,
                mean,
                std_dev,
                z_score,
                signal_age_ms,
            );
        }
        if tick.projected_net_pnl < self.settings.min_projected_net_pnl {
            return self.evaluation(
                Decision::ProjectedNetPnl,
                tick,
                mean,
                std_dev,
                z_score,
                signal_age_ms,
            );
        }

        self.position = Some(Position { direction, opened_at_ms: tick.at_ms });
        self.pending = None;
        self.evaluation(
            Decision::Open(direction),
            tick,
            mean,
            std_dev,
            z_score,
            signal_age_ms,
        )
    }

    fn evaluation(
        &self,
        decision: Decision,
        tick: Tick,
        mean_bps: f64,
        standard_deviation_bps: f64,
        z_score: f64,
        signal_age_ms: i64,
    ) -> Evaluation {
        Evaluation {
            decision,
            samples: self.values.len(),
            mean_bps,
            standard_deviation_bps,
            z_score,
            signal_age_ms,
            projected_net_pnl: tick.projected_net_pnl,
        }
    }
}

fn statistics(values: &VecDeque<f64>, current: f64) -> (f64, f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / count;
    let standard_deviation = variance.sqrt();
    let z_score = if standard_deviation > f64::EPSILON {
        (current - mean) / standard_deviation
    } else {
        0.0
    };
    (mean, standard_deviation, z_score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tick(at_ms: i64, spread_bps: f64, projected: Decimal) -> Tick {
        Tick { at_ms, spread_bps, book_skew_ms: 10, projected_net_pnl: projected }
    }

    fn settings() -> Settings {
        Settings {
            window: 3,
            warmup: 3,
            entry_z: 1.0,
            max_entry_z: 8.0,
            exit_z: 0.8,
            stop_z: 5.0,
            confirm_ms: 2_000,
            min_hold_ms: 5_000,
            max_hold_ms: 60_000,
            max_book_skew_ms: 500,
            min_projected_net_pnl: dec!(0.10),
        }
    }

    #[test]
    fn waits_for_confirmation_and_projected_profit() {
        let mut engine = Engine::new(settings()).unwrap();
        assert_eq!(engine.on_tick(tick(0, 0.0, dec!(1))).decision, Decision::Warmup);
        assert_eq!(engine.on_tick(tick(1_000, 0.0, dec!(1))).decision, Decision::Warmup);
        assert_eq!(engine.on_tick(tick(2_000, 10.0, dec!(1))).decision, Decision::AwaitingConfirmation);
        assert_eq!(engine.on_tick(tick(3_000, 20.0, dec!(1))).decision, Decision::AwaitingConfirmation);
        assert_eq!(engine.on_tick(tick(4_000, 30.0, dec!(0.05))).decision, Decision::ProjectedNetPnl);
        assert!(matches!(engine.on_tick(tick(5_000, 40.0, dec!(0.20))).decision, Decision::Open(_)));
    }

    #[test]
    fn rejects_skew_without_polluting_the_window() {
        let mut engine = Engine::new(settings()).unwrap();
        let mut value = tick(0, 100.0, dec!(1));
        value.book_skew_ms = 501;
        let result = engine.on_tick(value);
        assert_eq!(result.decision, Decision::BookSkew);
        assert_eq!(result.samples, 0);
    }

    #[test]
    fn convergence_respects_minimum_hold() {
        let mut config = settings();
        config.confirm_ms = 0;
        let mut engine = Engine::new(config).unwrap();
        engine.on_tick(tick(0, 0.0, dec!(1)));
        engine.on_tick(tick(1_000, 0.0, dec!(1)));
        assert!(matches!(engine.on_tick(tick(2_000, 10.0, dec!(1))).decision, Decision::Open(_)));
        assert_eq!(engine.on_tick(tick(3_000, 10.0, dec!(1))).decision, Decision::Hold);
        assert_eq!(engine.on_tick(tick(7_000, 10.0, dec!(1))).decision, Decision::CloseConvergence);
    }
}
