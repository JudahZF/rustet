//! Bandwidth scheduling between playback and exports.
//!
//! Playback owns the bandwidth budget; exports are opportunistic background consumers. While
//! playback streams, an export's network stream is throttled by an AIMD controller (additive
//! increase, multiplicative decrease, the shape of TCP and LEDBAT-style "scavenger" transports)
//! driven by how fast playback is actually fetching. The export never pauses: every finite limit
//! is clamped to the floor. A floor rather than a hard pause also keeps the CDN body flowing:
//! the stall detector aborts a download whose body delivers < 256 B in 30 s, which any sane
//! floor exceeds.
//!
//! Backpressure reaches the network because the pipeline joins its stages with bounded pipes
//! and sources read segments sequentially: slowing the export's final writer slows its reads.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use rustet_core::pipeline::Sink;
use rustet_playback::ForegroundStats;
use tokio::io::AsyncWrite;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

/// The export never goes below this (bytes/s) while playback streams.
pub(crate) const DEFAULT_FLOOR_BPS: f64 = (128 << 10) as f64;
/// The conservative rate when playback appears.
const DEFAULT_START_BPS: f64 = (1 << 20) as f64;
/// Additive increase per tick.
const DEFAULT_STEP_BPS: f64 = (256 << 10) as f64;
/// Limiter burst; writes wait for at most this many bytes at a time.
pub(crate) const THROTTLE_BURST: usize = 64 << 10;
const THROTTLE_TICK: Duration = Duration::from_millis(500);

/// Hysteresis band on measured foreground throughput, as multiples of the foreground need:
/// below `DECREASE_AT` back off, above `INCREASE_AT` grow, in between hold. The gap keeps the
/// limit from oscillating every tick.
const DECREASE_AT: f64 = 3.0;
const INCREASE_AT: f64 = 4.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── Token bucket ─────────────────────────────────────────────────────────────

/// A token bucket shared by every throttled writer. `None` is no limit.
pub(crate) struct Limiter(Mutex<Bucket>);

struct Bucket {
    limit: Option<f64>,
    tokens: f64,
    last: Instant,
}

impl Limiter {
    /// A limiter with a full bucket.
    pub(crate) fn new(limit: Option<f64>) -> Self {
        Self(Mutex::new(Bucket {
            limit,
            tokens: THROTTLE_BURST as f64,
            last: Instant::now(),
        }))
    }

    #[cfg(test)]
    pub(crate) fn limit(&self) -> Option<f64> {
        lock(&self.0).limit
    }

    pub(crate) fn set_limit(&self, limit: Option<f64>) {
        let mut b = lock(&self.0);
        b.advance(Instant::now());
        b.limit = limit;
    }

    /// Takes `n` tokens (at most the burst) and returns how long to wait before using them.
    fn reserve(&self, n: usize) -> Duration {
        let mut b = lock(&self.0);
        let Some(limit) = b.limit else {
            return Duration::ZERO;
        };
        b.advance(Instant::now());
        b.tokens -= n as f64;
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / limit)
        }
    }
}

impl Bucket {
    fn advance(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = match self.limit {
            // Unlimited: a later finite limit starts with a full bucket.
            None => THROTTLE_BURST as f64,
            Some(limit) => (self.tokens + elapsed * limit).min(THROTTLE_BURST as f64),
        };
    }
}

/// A sink whose writes wait on the shared limiter, in chunks no larger than the burst.
/// Cancellation needs nothing here: the pipeline run that writes into it stops on its token.
pub(crate) struct Throttled<W> {
    inner: W,
    limiter: Arc<Limiter>,
    /// Bytes already paid for.
    granted: usize,
    /// A reservation being waited out: its size and its timer.
    waiting: Option<(usize, Pin<Box<Sleep>>)>,
}

impl<W> Throttled<W> {
    pub(crate) fn new(inner: W, limiter: Arc<Limiter>) -> Self {
        Self {
            inner,
            limiter,
            granted: 0,
            waiting: None,
        }
    }

    pub(crate) fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Throttled<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if buf.is_empty() {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        loop {
            if this.granted > 0 {
                let n = buf.len().min(this.granted);
                let written = ready!(Pin::new(&mut this.inner).poll_write(cx, &buf[..n]))?;
                this.granted -= written;
                return Poll::Ready(Ok(written));
            }
            if let Some((n, sleep)) = &mut this.waiting {
                ready!(sleep.as_mut().poll(cx));
                this.granted = *n;
                this.waiting = None;
                continue;
            }
            let n = buf.len().min(THROTTLE_BURST);
            let wait = this.limiter.reserve(n);
            if wait.is_zero() {
                this.granted = n;
            } else {
                this.waiting = Some((n, Box::pin(tokio::time::sleep(wait))));
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<W: AsyncWrite + Send + Unpin> Sink for Throttled<W> {}

// ── Controller ───────────────────────────────────────────────────────────────

/// The foreground counters of the playback manager.
pub(crate) type StatsFn = Arc<dyn Fn() -> ForegroundStats + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No playback: no limit.
    Unlimited,
    /// Playback just appeared: start rate, baseline taken, no evaluation yet.
    Starting,
    /// AIMD with hysteresis.
    Adaptive,
}

/// Adapts the shared export limiter to foreground playback.
///
/// The foreground need is a protection threshold, not an estimate of available bandwidth:
/// measured playback throughput is observed consumption, not link capacity. Do not turn this into
/// "spare = link − playback" arithmetic.
pub(crate) struct BwController {
    limiter: Arc<Limiter>,
    stats: Option<StatsFn>,
    floor_bps: f64,
    start_bps: f64,
    ctl: Mutex<Ctl>,
}

struct Ctl {
    state: State,
    /// Meaningful when `state` is not `Unlimited`.
    limit_bps: f64,
    /// The last value handed to the limiter.
    applied: Option<f64>,
    last_fg_bytes: u64,
    last_time: Instant,
    /// The running ticker: its generation and its stop token.
    running: Option<(u64, CancellationToken)>,
    generation: u64,
    /// How many times the limiter was changed, so tests can see redundant updates.
    #[cfg(test)]
    applies: usize,
}

impl BwController {
    /// `floor_bps` 0 selects the default floor.
    pub(crate) fn new(stats: Option<StatsFn>, floor_bps: f64) -> Self {
        let floor_bps = if floor_bps > 0.0 {
            floor_bps
        } else {
            DEFAULT_FLOOR_BPS
        };
        Self {
            limiter: Arc::new(Limiter::new(None)),
            stats,
            floor_bps,
            start_bps: DEFAULT_START_BPS.max(floor_bps),
            ctl: Mutex::new(Ctl {
                state: State::Unlimited,
                limit_bps: 0.0,
                applied: None,
                last_fg_bytes: 0,
                last_time: Instant::now(),
                running: None,
                generation: 0,
                #[cfg(test)]
                applies: 0,
            }),
        }
    }

    /// Wraps `w` so every byte waits on the shared export limiter.
    pub(crate) fn writer<W>(&self, w: W) -> Throttled<W> {
        Throttled::new(w, Arc::clone(&self.limiter))
    }

    /// The finite limit in force, or `None` when unlimited.
    pub(crate) fn current_limit(&self) -> Option<i64> {
        let ctl = lock(&self.ctl);
        (ctl.state != State::Unlimited).then_some(ctl.limit_bps as i64)
    }

    /// Starts adapting for the duration of one export's network stream; dropping the guard stops
    /// and resets to unlimited. The first step runs at once, so an export that starts during
    /// playback is throttled from its first byte. A second start while running does nothing.
    pub(crate) fn start(self: &Arc<Self>) -> ThrottleGuard {
        if self.stats.is_none() {
            return ThrottleGuard(None);
        }
        let (generation, stop) = {
            let mut ctl = lock(&self.ctl);
            if ctl.running.is_some() {
                return ThrottleGuard(None);
            }
            ctl.generation += 1;
            let stop = CancellationToken::new();
            ctl.running = Some((ctl.generation, stop.clone()));
            (ctl.generation, stop)
        };
        self.step(Instant::now(), Some(generation));
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(Instant::now() + THROTTLE_TICK, THROTTLE_TICK);
            while stop.run_until_cancelled(tick.tick()).await.is_some() {
                if !this.step(Instant::now(), Some(generation)) {
                    return;
                }
            }
        });
        ThrottleGuard(Some((Arc::clone(self), generation)))
    }

    fn stop(&self, generation: u64) {
        let mut ctl = lock(&self.ctl);
        if ctl.running.as_ref().is_none_or(|(g, _)| *g != generation) {
            return;
        }
        if let Some((_, stop)) = ctl.running.take() {
            stop.cancel();
        }
        ctl.state = State::Unlimited;
        self.apply(&mut ctl, None);
    }

    /// One controller step at `now`. With a generation, only while that run is current, so a
    /// late tick never applies a limit after stop. Throughput is measured per interval (delta
    /// bytes / delta time), never cumulative bytes over total time.
    fn step(&self, now: Instant, generation: Option<u64>) -> bool {
        let Some(stats) = &self.stats else {
            return false;
        };
        let fg = stats();
        let mut ctl = lock(&self.ctl);
        if let Some(g) = generation
            && ctl.running.as_ref().is_none_or(|(cur, _)| *cur != g)
        {
            return false;
        }

        if fg.active == 0 {
            if ctl.state != State::Unlimited {
                ctl.state = State::Unlimited;
                self.apply(&mut ctl, None);
            }
            // Always re-baseline, so a later playback never measures across the idle gap.
            ctl.last_fg_bytes = fg.bytes;
            ctl.last_time = now;
            return true;
        }

        match ctl.state {
            State::Unlimited => {
                // Playback appeared: drop to a conservative rate at once and take a fresh
                // baseline. The interval so far is incomplete, so do not evaluate it.
                ctl.state = State::Starting;
                self.set_limit(&mut ctl, self.start_bps);
                ctl.last_fg_bytes = fg.bytes;
                ctl.last_time = now;
                return true;
            }
            State::Starting => ctl.state = State::Adaptive, // first complete interval
            State::Adaptive => {}
        }

        let dt = now.saturating_duration_since(ctl.last_time).as_secs_f64();
        let delta = fg.bytes.saturating_sub(ctl.last_fg_bytes);
        ctl.last_fg_bytes = fg.bytes;
        ctl.last_time = now;
        if dt <= 0.0 {
            return true;
        }
        let fg_bps = delta as f64 / dt;
        let need = fg.need_bps as f64;
        let mut next = ctl.limit_bps;
        if need <= 0.0 || fg_bps < DECREASE_AT * need {
            // Without a threshold there is no evidence of headroom: never grow.
            next *= 0.5;
        } else if fg_bps > INCREASE_AT * need {
            next += DEFAULT_STEP_BPS;
        }
        self.set_limit(&mut ctl, next);
        true
    }

    /// Clamps to the floor and applies only real changes.
    fn set_limit(&self, ctl: &mut Ctl, bps: f64) {
        ctl.limit_bps = bps.max(self.floor_bps);
        self.apply(ctl, Some(ctl.limit_bps));
    }

    fn apply(&self, ctl: &mut Ctl, limit: Option<f64>) {
        if ctl.applied == limit {
            return;
        }
        ctl.applied = limit;
        self.limiter.set_limit(limit);
        #[cfg(test)]
        {
            ctl.applies += 1;
        }
    }
}

/// Ends a [`BwController::start`] run when dropped.
pub(crate) struct ThrottleGuard(Option<(Arc<BwController>, u64)>);

impl Drop for ThrottleGuard {
    fn drop(&mut self) {
        if let Some((c, generation)) = self.0.take() {
            c.stop(generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[derive(Default)]
    struct FakeFg(Mutex<ForegroundStats>);

    impl FakeFg {
        fn set(&self, active: usize, need_bps: u64) {
            let mut s = lock(&self.0);
            s.active = active;
            s.need_bps = need_bps;
        }
        fn add(&self, n: u64) {
            lock(&self.0).bytes += n;
        }
    }

    fn stats_of(fg: &Arc<FakeFg>) -> StatsFn {
        let fg = Arc::clone(fg);
        Arc::new(move || *lock(&fg.0))
    }

    struct Harness {
        c: BwController,
        fg: Arc<FakeFg>,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self {
            let fg = Arc::new(FakeFg::default());
            Self {
                c: BwController::new(Some(stats_of(&fg)), 0.0),
                fg,
                now: Instant::now(),
            }
        }

        /// Advances one tick after playback fetched `fg_bps` over it, then steps.
        fn step(&mut self, fg_bps: f64) {
            self.fg.add((fg_bps * THROTTLE_TICK.as_secs_f64()) as u64);
            self.now += THROTTLE_TICK;
            self.c.step(self.now, None);
        }

        fn limit(&self) -> Option<f64> {
            self.c.current_limit().map(|l| l as f64)
        }

        fn applies(&self) -> usize {
            lock(&self.c.ctl).applies
        }

        fn state(&self) -> State {
            lock(&self.c.ctl).state
        }
    }

    const NEED: u64 = 200_000;

    #[test]
    fn unlimited_without_playback() {
        let mut h = Harness::new();
        for _ in 0..3 {
            h.step(0.0);
        }
        assert_eq!(h.limit(), None);
        assert_eq!(h.applies(), 0, "limiter changed while idle");
    }

    #[test]
    fn start_then_aimd() {
        let mut h = Harness::new();
        h.fg.set(1, NEED);
        let need = NEED as f64;

        h.step(0.0); // playback appears: start rate, no evaluation
        assert_eq!(h.limit(), Some(DEFAULT_START_BPS));
        assert_eq!(h.state(), State::Starting);

        h.step(need); // under pressure: halve
        assert_eq!(h.limit(), Some(DEFAULT_START_BPS / 2.0));
        assert_eq!(h.state(), State::Adaptive);

        h.step(5.0 * need); // headroom: + step
        let grown = DEFAULT_START_BPS / 2.0 + DEFAULT_STEP_BPS;
        assert_eq!(h.limit(), Some(grown));

        let n = h.applies();
        h.step(3.5 * need); // hysteresis band: hold, no redundant update
        assert_eq!(h.limit(), Some(grown));
        assert_eq!(h.applies(), n);

        h.fg.set(0, 0); // playback stops
        h.step(0.0);
        assert_eq!(h.limit(), None);
        assert_eq!(h.c.limiter.limit(), None);
    }

    #[test]
    fn floor_clamp() {
        let mut h = Harness::new();
        h.fg.set(1, NEED);
        h.step(0.0);
        for _ in 0..20 {
            h.step(0.0); // sustained pressure
            assert!(h.limit().unwrap() >= DEFAULT_FLOOR_BPS);
        }
        assert_eq!(h.limit(), Some(DEFAULT_FLOOR_BPS));
        let n = h.applies();
        h.step(0.0);
        assert_eq!(h.applies(), n, "re-applied an unchanged floor");
    }

    // An idle interval of active playback is no evidence of headroom: be conservative.
    #[test]
    fn zero_byte_interval_decreases() {
        let mut h = Harness::new();
        h.fg.set(1, NEED);
        h.step(0.0);
        let before = h.limit().unwrap();
        h.step(0.0);
        assert_eq!(h.limit(), Some(before / 2.0));
    }

    #[test]
    fn non_positive_need_never_increases() {
        let mut h = Harness::new();
        h.fg.set(1, 0);
        h.step(0.0);
        let mut prev = h.limit().unwrap();
        for _ in 0..5 {
            h.step((100 << 20) as f64);
            let got = h.limit().unwrap();
            assert!(got <= prev && got >= DEFAULT_FLOOR_BPS, "{prev} -> {got}");
            prev = got;
        }
    }

    // Throughput is per interval: a burst before playback went idle must not count once
    // playback returns.
    #[test]
    fn fresh_baseline_after_idle() {
        let mut h = Harness::new();
        let need = NEED as f64;
        h.fg.set(1, NEED);
        h.step(0.0);
        h.step(50.0 * need);

        h.fg.set(0, 0);
        h.fg.add(100 << 20); // written while idle: must not leak into the next interval
        h.step(0.0);

        h.fg.set(1, NEED);
        h.step(0.0);
        assert_eq!(h.limit(), Some(DEFAULT_START_BPS));
        h.step(need);
        assert_eq!(
            h.limit(),
            Some(DEFAULT_START_BPS / 2.0),
            "stale bytes leaked"
        );
    }

    #[test]
    fn custom_floor_above_start_rate() {
        let fg = Arc::new(FakeFg::default());
        fg.set(1, NEED);
        let c = BwController::new(Some(stats_of(&fg)), (2 << 20) as f64);
        c.step(Instant::now(), None);
        assert!(c.current_limit().unwrap() >= 2 << 20);
    }

    #[tokio::test]
    async fn start_stop_cycles() {
        let fg = Arc::new(FakeFg::default());
        fg.set(1, NEED);
        let c = Arc::new(BwController::new(Some(stats_of(&fg)), 0.0));
        for _ in 0..50 {
            let first = c.start();
            assert!(
                c.limiter.limit().is_some(),
                "not throttled from the first byte"
            );
            fg.add(1000);
            drop(c.start()); // idempotent while running
            assert!(c.current_limit().is_some());
            drop(first);
            assert_eq!(c.current_limit(), None, "limit survived stop");
            assert_eq!(c.limiter.limit(), None, "limiter not reset after stop");
        }
    }

    #[tokio::test]
    async fn writes_are_chunked_at_the_burst() {
        let limiter = Arc::new(Limiter::new(None));
        let mut w = Throttled::new(Vec::new(), limiter);
        let data = vec![7u8; 150 << 10];
        let n = w.write(&data).await.unwrap();
        assert_eq!(n, THROTTLE_BURST);
        w.write_all(&data[n..]).await.unwrap();
        assert_eq!(w.into_inner(), data);
    }

    // A finite limit actually paces writes: burst + 16 KiB at 64 KiB/s takes about 250 ms.
    #[tokio::test]
    async fn real_limiter_paces() {
        let limiter = Arc::new(Limiter::new(Some((64 << 10) as f64)));
        let mut w = Throttled::new(tokio::io::sink(), limiter);
        let start = std::time::Instant::now();
        w.write_all(&vec![0u8; (64 << 10) + (16 << 10)])
            .await
            .unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "finite limit did not pace writes ({:?})",
            start.elapsed()
        );
    }
}
