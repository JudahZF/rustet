//! Frame timing for the root view: the interval between frames and the CPU time from the start
//! of `render` to the end of `paint` (layout and paint included; GPU work excluded).
//!
//! Call [`FrameStats::begin`] first in the root `render` and add [`FrameStats::probe`] as the
//! root's last child.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{IntoElement, Styled, canvas};

#[derive(Debug, Default)]
struct Inner {
    started: Option<Instant>,
    last_begin: Option<Instant>,
    intervals: Vec<Duration>,
    work: Vec<Duration>,
}

#[derive(Debug, Default, Clone)]
pub struct FrameStats(Rc<RefCell<Inner>>);

/// Percentiles of a sample, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Summary {
    pub fn of(samples: &[Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.).collect();
        ms.sort_by(f64::total_cmp);
        let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
        Some(Self {
            count: ms.len(),
            p50: at(0.5),
            p95: at(0.95),
            p99: at(0.99),
            max: ms[ms.len() - 1],
        })
    }
}

impl FrameStats {
    pub fn begin(&self) {
        let now = Instant::now();
        let mut inner = self.0.borrow_mut();
        if let Some(last) = inner.last_begin.replace(now) {
            inner.intervals.push(now - last);
        }
        inner.started = Some(now);
    }

    /// An empty element whose paint marks the end of the frame's CPU work.
    pub fn probe(&self) -> impl IntoElement {
        let stats = self.clone();
        canvas(
            |_, _, _| (),
            move |_, _, _, _| {
                let mut inner = stats.0.borrow_mut();
                if let Some(start) = inner.started.take() {
                    inner.work.push(start.elapsed());
                }
            },
        )
        .absolute()
        .size_0()
    }

    /// Clears the samples; the next frame starts a new interval series.
    pub fn reset(&self) {
        *self.0.borrow_mut() = Inner::default();
    }

    pub fn intervals(&self) -> Option<Summary> {
        Summary::of(&self.0.borrow().intervals)
    }

    pub fn work(&self) -> Option<Summary> {
        Summary::of(&self.0.borrow().work)
    }

    /// Frames whose interval exceeded `budget` (dropped frames at that refresh rate).
    pub fn late(&self, budget: Duration) -> usize {
        self.0
            .borrow()
            .intervals
            .iter()
            .filter(|d| **d > budget)
            .count()
    }
}
