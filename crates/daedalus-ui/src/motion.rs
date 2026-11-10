use std::time::{Duration, Instant};

use gpui::{Animation, AnimationExt, AnyElement, ElementId, IntoElement};

/// CSS-style cubic-bezier easing, solved for t by Newton iteration with a bisection fallback.
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> impl Fn(f32) -> f32 + Clone {
    move |x| {
        let bez = |a: f32, b: f32, t: f32| {
            3. * a * (1. - t).powi(2) * t + 3. * b * (1. - t) * t * t + t.powi(3)
        };
        let mut t = x;
        let mut converged = false;
        for _ in 0..8 {
            let err = bez(x1, x2, t) - x;
            if err.abs() < 1e-6 {
                converged = true;
                break;
            }
            let slope =
                3. * (1. - t).powi(2) * x1 + 6. * (1. - t) * t * (x2 - x1) + 3. * t * t * (1. - x2);
            if slope.abs() < 1e-6 {
                break;
            }
            t -= err / slope;
            if !(0. ..=1.).contains(&t) {
                break;
            }
        }
        if !converged {
            let (mut lo, mut hi) = (0., 1.);
            for _ in 0..32 {
                t = (lo + hi) / 2.;
                if bez(x1, x2, t) < x {
                    lo = t;
                } else {
                    hi = t;
                }
            }
        }
        bez(y1, y2, t.clamp(0., 1.))
    }
}

/// Milliseconds between spinner steps.
pub const SPIN_TICK_MS: u128 = 125;

/// Steps in one full turn of the spinner.
pub const SPIN_STEPS: u128 = 8;

pub(crate) const EASE_STANDARD: [f32; 4] = [0.2, 0., 0., 1.];
pub(crate) const EASE_ENTER: [f32; 4] = [0., 0., 0., 1.];
pub(crate) const EASE_EXIT: [f32; 4] = [0.3, 0., 1., 1.];

pub fn ease_standard() -> impl Fn(f32) -> f32 + Clone {
    let [x1, y1, x2, y2] = EASE_STANDARD;
    cubic_bezier(x1, y1, x2, y2)
}

pub fn ease_enter() -> impl Fn(f32) -> f32 + Clone {
    let [x1, y1, x2, y2] = EASE_ENTER;
    cubic_bezier(x1, y1, x2, y2)
}

pub fn ease_exit() -> impl Fn(f32) -> f32 + Clone {
    let [x1, y1, x2, y2] = EASE_EXIT;
    cubic_bezier(x1, y1, x2, y2)
}

/// Underdamped spring step response over [0, 1] that overshoots a few percent before settling.
pub fn ease_spring() -> impl Fn(f32) -> f32 + Clone {
    const ZETA: f32 = 0.72;
    const A: f32 = 5.0;
    let root = (1. - ZETA * ZETA).sqrt();
    let b = A / ZETA * root;
    let k = ZETA / root;
    move |t| {
        if t <= 0. {
            0.
        } else if t >= 1. {
            1.
        } else {
            1. - (-A * t).exp() * ((b * t).cos() + k * (b * t).sin())
        }
    }
}

/// A value easing toward a target; retargeting mid-flight continues from the value shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glide {
    from: f32,
    to: f32,
    since: Instant,
    duration: Duration,
    pub generation: u64,
}

impl Glide {
    pub fn settled(at: f32, duration: Duration) -> Self {
        Self {
            from: at,
            to: at,
            since: Instant::now(),
            duration,
            generation: 0,
        }
    }

    pub fn retarget(&mut self, to: f32) {
        self.retarget_at(to, Instant::now());
    }

    fn retarget_at(&mut self, to: f32, now: Instant) {
        if to == self.to {
            return;
        }
        self.from = self.value_at(now);
        self.to = to;
        self.since = now;
        self.generation += 1;
    }

    pub fn value(&self) -> f32 {
        self.value_at(Instant::now())
    }

    fn value_at(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.since);
        let p = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).min(1.);
        self.from + (self.to - self.from) * ease_standard()(p)
    }

    pub fn running(&self) -> bool {
        self.from != self.to && self.since.elapsed() < self.duration
    }
}

/// Runs `animator` as an animation, or renders its final frame directly when motion is reduced.
pub fn animate_if<E: IntoElement + AnimationExt + 'static>(
    reduced: bool,
    element: E,
    id: impl Into<ElementId>,
    animation: Animation,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    if reduced {
        animator(element, 1.).into_any_element()
    } else {
        element
            .with_animation(id, animation, animator)
            .into_any_element()
    }
}

/// Marks an element that is animating out; `generation` keys the oneshot so a reopen replays it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closing {
    pub since: Instant,
    pub generation: u64,
}

impl Closing {
    pub fn new(generation: u64) -> Self {
        Self {
            since: Instant::now(),
            generation,
        }
    }
}

/// Marks when an element started animating in, for [`animate_enter`]'s `fresh`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opening {
    pub since: Instant,
}

impl Opening {
    pub fn now() -> Self {
        Self {
            since: Instant::now(),
        }
    }

    pub fn running(&self, duration: Duration) -> bool {
        self.since.elapsed() < duration
    }
}

/// Enter animation that plays only while `fresh`, so a parent whose id changes (a project switch)
/// cannot replay it; the wrapper stays either way, keeping its children's element state.
pub fn animate_enter<E: IntoElement + AnimationExt + 'static>(
    reduced: bool,
    fresh: bool,
    element: E,
    id: impl Into<ElementId>,
    animation: Animation,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    animate_if(reduced, element, id, animation, move |el, d| {
        if fresh { animator(el, d) } else { el }
    })
}

/// Exit counterpart of [`animate_if`]: eases out, and renders the final (gone) frame when motion is reduced.
pub fn animate_exit<E: IntoElement + AnimationExt + 'static>(
    reduced: bool,
    element: E,
    id: impl Into<ElementId>,
    duration: Duration,
    animator: impl Fn(E, f32) -> E + 'static,
) -> AnyElement {
    animate_if(
        reduced,
        element,
        id,
        Animation::new(duration).with_easing(ease_exit()),
        animator,
    )
}

/// How long to keep a closing element mounted before removing it.
pub fn exit_delay(reduced: bool, duration: Duration) -> Duration {
    if reduced { Duration::ZERO } else { duration }
}

/// Reads macOS "Reduce motion" (System Settings, Accessibility, Display).
pub fn system_reduce_motion() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_fixed() {
        for ease in [cubic_bezier(0.2, 0., 0., 1.), cubic_bezier(0.3, 0., 1., 1.)] {
            assert!(ease(0.).abs() < 1e-4);
            assert!((ease(1.) - 1.).abs() < 1e-4);
        }
    }

    #[test]
    fn standard_is_front_loaded() {
        let ease = ease_standard();
        assert!(ease(0.5) > 0.8);
    }

    #[test]
    fn exit_delay_is_zero_when_motion_is_reduced() {
        let d = Duration::from_millis(120);
        assert_eq!(exit_delay(true, d), Duration::ZERO);
        assert_eq!(exit_delay(false, d), d);
    }

    #[test]
    fn opening_stops_running_after_its_duration() {
        let opening = Opening::now();
        assert!(opening.running(Duration::from_secs(60)));
        assert!(!opening.running(Duration::ZERO));
    }

    #[test]
    fn exit_is_back_loaded() {
        let ease = ease_exit();
        assert!(ease(0.5) < 0.5);
    }

    #[test]
    fn spring_hits_endpoints() {
        let f = ease_spring();
        assert_eq!(f(0.), 0.);
        assert_eq!(f(1.), 1.);
    }

    #[test]
    fn spring_overshoots_a_little() {
        let f = ease_spring();
        let max = (0..=1000).map(|i| f(i as f32 / 1000.)).fold(0., f32::max);
        assert!(max > 1.0 && max < 1.06, "{max}");
    }

    #[test]
    fn spring_settles_near_the_end() {
        let f = ease_spring();
        for i in 900..=1000 {
            let t = i as f32 / 1000.;
            assert!((f(t) - 1.).abs() < 0.02, "{t}");
        }
    }

    #[test]
    fn glide_settled_holds_its_value() {
        let g = Glide::settled(3., Duration::from_millis(200));
        assert_eq!(g.value(), 3.);
        assert!(!g.running());
    }

    #[test]
    fn glide_retarget_starts_from_the_shown_value() {
        let t0 = Instant::now();
        let mut g = Glide::settled(0., Duration::from_millis(200));
        g.retarget_at(10., t0);
        let now = t0 + Duration::from_millis(50);
        let shown = g.value_at(now);
        assert!(shown > 0. && shown < 10.);
        g.retarget_at(20., now);
        assert_eq!(g.from, shown);
        assert_eq!(g.generation, 2);
    }

    #[test]
    fn glide_same_target_is_a_noop() {
        let mut g = Glide::settled(5., Duration::from_millis(200));
        let before = g;
        g.retarget(5.);
        assert_eq!(g, before);
    }

    #[test]
    fn glide_zero_duration_jumps() {
        let mut g = Glide::settled(0., Duration::ZERO);
        g.retarget(7.);
        assert_eq!(g.value(), 7.);
    }

    type Ease = Box<dyn Fn(f32) -> f32>;

    fn eases() -> [(&'static str, Ease); 3] {
        [
            ("standard", Box::new(ease_standard())),
            ("enter", Box::new(ease_enter())),
            ("exit", Box::new(ease_exit())),
        ]
    }

    #[test]
    fn eases_hit_both_endpoints() {
        for (name, ease) in eases() {
            assert_eq!(ease(0.), 0., "{name}");
            assert!((ease(1.) - 1.).abs() < 1e-6, "{name}");
        }
    }

    #[test]
    fn eases_are_monotonic_finite_and_in_unit_range() {
        const N: usize = 10_000;
        for (name, ease) in eases() {
            let mut prev = 0.;
            for i in 0..=N {
                let y = ease(i as f32 / N as f32);
                assert!(
                    y.is_finite() && (0. ..=1.).contains(&y),
                    "{name} at {i}: {y}"
                );
                assert!(y >= prev - 1e-6, "{name} decreased at {i}: {prev} -> {y}");
                prev = y;
            }
        }
    }

    #[test]
    fn enter_does_not_overshoot_near_zero() {
        assert!(ease_enter()(0.005) < 0.2);
    }

    #[test]
    fn eases_match_bisection_reference() {
        let bez = |a: f64, b: f64, t: f64| {
            3. * a * (1. - t).powi(2) * t + 3. * b * (1. - t) * t * t + t.powi(3)
        };
        for ((name, ease), params) in
            eases()
                .into_iter()
                .zip([EASE_STANDARD, EASE_ENTER, EASE_EXIT])
        {
            let [x1, y1, x2, y2] = params.map(f64::from);
            for i in 0..=1000 {
                let x = i as f64 / 1000.;
                let (mut lo, mut hi) = (0., 1.);
                for _ in 0..60 {
                    let mid = (lo + hi) / 2.;
                    if bez(x1, x2, mid) < x {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let expected = bez(y1, y2, (lo + hi) / 2.);
                assert!(
                    (f64::from(ease(x as f32)) - expected).abs() < 1e-4,
                    "{name} at x = {x}"
                );
            }
        }
    }
}
