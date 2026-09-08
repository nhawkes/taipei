//! Lanes — wire traffic as two-edge pulses.
//!
//! A request on screen is a two-edge pulse. The **front** edge is the request itself and runs
//! on the engine's clock — one network leg long, invisible at ×1 by design. The **back** edge
//! is afterglow: user-facing persistence, [`AFTERGLOW_MS`] of wall clock at every speed, never
//! a position — it only ever trails where the request really was. The head is a hard cut, so
//! the cliff *is* the request; everything behind it is memory.
//!
//! All sends departing one wire, one direction, one colour, in one frame merge into **one
//! pulse**; opacity carries the count. A frame was always the floor of temporal resolution, so
//! requests indistinguishable in time share a pulse rather than pretending to be separable —
//! which is also what bounds the work: pulses scale with wire count × frame rate, never QPS.
//!
//! The **carrier wave** is rate rendered as texture, never requests — each wire's crest
//! frequency is locked to that wire's own smoothed rate and its depth fades out as the wire
//! goes quiet, handing over to individual pulses exactly as they become sparse enough to
//! read. Out and home waves counter-scroll half a wavelength apart; direction survives in hue.
//!
//! Mechanically a tier holds the traffic and nothing else: [`frame`](LaneTier::frame) seats
//! the frame's departures and ages what is already flying, and [`paint`](LaneTier::paint)
//! writes what that comes to as [`Shape`]s along each wire's own bow — where a pulse is on its
//! wire is the curve's parameter, so a bow runs its traffic faster through the shallow middle
//! than through the turns. Nothing here is an element, so nothing here is retained: a card
//! whose loop stops stops drawing, which is why the loop runs on while anything is
//! [`airborne`](LaneTier::airborne).

use std::collections::HashMap;

use idyll::{Curve, Shape};

use crate::atoms::stage::{Paint, Stage};
use crate::atoms::wires::Bow;

/// The back edge's length: presentation time, the same at every sim speed (clamped to never
/// outrun the front once slow motion makes fronts long).
pub const AFTERGLOW_MS: f64 = 250.0;

/// How many wires a tier can show carrying at once. A lattice's *active* set, not its size —
/// bounded by rate × afterglow, ~150 at the cards' loaded settings — and a frame with more
/// active wires than seats drops the excess wires' pulses (never their counts or readings).
pub const WIRES: usize = 128;

/// Pulse slots per lane. A wire hot enough to want more is a wire whose carrier wave already
/// carries the texture, so recycling the oldest afterglow there costs nothing readable.
const SLOTS: usize = 4;

/// The carrier wave: wavelength in px, where in one the crest stands, one crest per this many
/// requests, and the rate (in *virtual* req/s) at which its depth saturates.
const WAVE_L: f64 = 160.0;
const CREST_AT: f64 = 60.0;
const CREST_PER_REQ: f64 = 1.0 / 90.0;
const FULL_WAVE_AT: f64 = 15.0;

/// The wave at full depth, and the depth it has faded to by the time it hands over: under it
/// a wire is drawn by its pulses alone. A crest's speed is the wire's rate too, so a wave this
/// far faded is not only invisible — it is a blotch that would take half a minute to cross.
const WAVE_DEEP: f64 = 0.35;
const WAVE_FAINT: f64 = 0.15;

/// How often a wire's rate is re-smoothed, wall ms.
const STAT_MS: f64 = 500.0;

/// The shortest tail a pulse ever has, as a fraction of its wire: slow motion stretches a front
/// until it is as long as the afterglow behind it, and a pulse whose edges met would be a point.
const TAIL: f64 = 0.04;

/// Stroke widths: the traffic reads over the wire it runs on, the wave under it.
const PULSE_W: f64 = 3.0;
const WAVE_W: f64 = 2.0;

/// A wire's identity across frames: the endpoint indices its card draws it between.
pub type Key = (usize, usize);

/// What a pulse carries: a send, an answer, or a refusal riding home.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ink {
    Sent,
    Answer,
    Refusal,
}

impl Ink {
    fn paint(self) -> Paint {
        match self {
            Ink::Sent => Stage::teal.value(),
            Ink::Answer => Stage::green.value(),
            Ink::Refusal => Stage::amber.value(),
        }
    }
}

/// The hue a direction wears: what a send is drawn in going out, what an answer is drawn in
/// coming home — so the carrier wave says which way it is running in the same words a pulse does.
fn hue(homeward: bool) -> Paint {
    match homeward {
        false => Ink::Sent,
        true => Ink::Answer,
    }
    .paint()
}

/// A merged pulse's opacity: brightness is how many rode it, log₂ and saturating — fifteen
/// requests in one frame read as "full", not as fifteen times one.
fn intensity(count: usize) -> f64 {
    (0.55 + 0.45 * ((1 + count) as f64).log2() / 4.0).min(1.0)
}

/// The slot for `key`: its current seat, or the first empty one, claimed. `None` when the
/// tier is saturated.
fn seat(seats: &mut [Option<Key>], key: Key) -> Option<usize> {
    match seats.iter().position(|k| *k == Some(key)) {
        Some(at) => Some(at),
        None => {
            let at = seats.iter().position(|k| k.is_none())?;
            seats[at] = Some(key);
            Some(at)
        }
    }
}

/// One launched pulse: what it carries, how bright the merge made it, when it left, and its
/// two edges' lengths in wall ms — the front one network leg at the sim's clock, the back the
/// afterglow.
struct Slot {
    ink: Ink,
    alpha: f64,
    at: f64,
    front_ms: f64,
    back_ms: f64,
}

impl Slot {
    /// Wall ms at which the back edge clears the far end and the slot is free again.
    fn ends_at(&self) -> f64 {
        self.at + self.back_ms * (1.0 + TAIL)
    }

    /// Where the two edges stand at `wall`, in the wire's own parameter, and the opacity at
    /// each — the head a hard cut at the request's real position, the tail fading out behind
    /// it. A homeward pulse runs the same wire backwards.
    fn edges(&self, wall: f64, homeward: bool) -> ((f64, f64), (f64, f64)) {
        let since = wall - self.at;
        let (head, tail) = (since / self.front_ms, since / self.back_ms - TAIL);
        match homeward {
            false => ((tail, head), (0.0, self.alpha)),
            true => ((1.0 - head, 1.0 - tail), (self.alpha, 0.0)),
        }
    }
}

/// One wire's traffic: the pulses each way, and the carrier wave's phase and the smoothed rate
/// that drives it.
struct SeatState {
    out: Vec<Option<Slot>>,
    home: Vec<Option<Slot>>,
    wave: f64,
    rate: f64,
    sends: f64,
}

impl SeatState {
    fn lane(&mut self, homeward: bool) -> &mut Vec<Option<Slot>> {
        match homeward {
            false => &mut self.out,
            true => &mut self.home,
        }
    }

    fn carrying(&self, wall: f64) -> bool {
        [&self.out, &self.home]
            .into_iter()
            .flatten()
            .flatten()
            .any(|slot| slot.ends_at() > wall)
    }
}

/// One tier of lanes: every wire of one leg family, seated as traffic arrives.
pub struct LaneTier {
    /// The front edge's length — this tier's network leg, virtual ms.
    leg_ms: f64,
    seats: Vec<Option<Key>>,
    state: Vec<SeatState>,
    wall: f64,
    since_stat: f64,
    speed: f64,
}

impl LaneTier {
    pub fn new(wires: usize, leg_ms: f64) -> LaneTier {
        LaneTier {
            leg_ms,
            seats: vec![None; wires],
            state: (0..wires)
                .map(|_| SeatState {
                    out: (0..SLOTS).map(|_| None).collect(),
                    home: (0..SLOTS).map(|_| None).collect(),
                    wave: 0.0,
                    rate: 0.0,
                    sends: 0.0,
                })
                .collect(),
            wall: 0.0,
            since_stat: 0.0,
            speed: 1.0,
        }
    }

    /// One frame: launch the departures' pulses, advance the waves, retire the quiet.
    ///
    /// `launches` are this frame's [`WireEvent`](crate::multi::WireEvent)s mapped onto this
    /// tier's wires — the tier knows which wires carry, never where they run. `speed` is the
    /// sim's clock dilation: fronts stretch by it, afterglow does not. `dt_wall_ms` is the
    /// reader's own time, so a paused sim still lands what it has in the air.
    pub fn frame(
        &mut self,
        launches: impl IntoIterator<Item = (Key, bool, Ink)>,
        dt_wall_ms: f64,
        speed: f64,
    ) {
        self.wall += dt_wall_ms;
        self.speed = speed;
        // The merging rule: one pulse per wire, per direction, per colour, per frame.
        let mut merged: HashMap<(Key, bool, Ink), usize> = HashMap::new();
        for (key, homeward, ink) in launches {
            *merged.entry((key, homeward, ink)).or_default() += 1;
        }
        let mut buckets: Vec<((Key, bool, Ink), usize)> = merged.into_iter().collect();
        buckets.sort_unstable_by_key(|&(k, _)| k);

        let front_ms = self.leg_ms / speed.max(1e-9);
        let back_ms = AFTERGLOW_MS.max(front_ms);
        for ((key, homeward, ink), count) in buckets {
            let Some(at) = seat(&mut self.seats, key) else { continue };
            let state = &mut self.state[at];
            state.sends += count as f64;
            let wall = self.wall;
            let slots = state.lane(homeward);
            let free = slots.iter().position(|s| s.as_ref().is_none_or(|s| s.ends_at() <= wall));
            let slot = free.unwrap_or_else(|| oldest(slots));
            slots[slot] =
                Some(Slot { ink, alpha: intensity(count), at: wall, front_ms, back_ms });
        }

        self.since_stat += dt_wall_ms;
        let cut = self.since_stat >= STAT_MS;
        for (key, state) in self.seats.iter_mut().zip(&mut self.state) {
            if key.is_none() {
                continue;
            }
            if cut {
                state.rate += (state.sends / (self.since_stat / 1000.0) - state.rate) * 0.5;
                if state.rate < 0.01 {
                    state.rate = 0.0;
                }
                state.sends = 0.0;
            }
            // The wire went quiet and its last afterglow decayed: the seat frees, and the next
            // wire to carry takes it.
            if state.rate == 0.0 && !state.carrying(self.wall) {
                *key = None;
                continue;
            }
            let moved = state.rate * CREST_PER_REQ * WAVE_L * (dt_wall_ms / 1000.0);
            state.wave = (state.wave + moved) % WAVE_L;
        }
        if cut {
            self.since_stat = 0.0;
        }
    }

    /// The tier's traffic as it stands, drawn along the bows its card places its wires on. A
    /// wire whose ends the layout has not measured yet draws nothing — its pulses are still
    /// flying, and land wherever the measurement puts them.
    pub fn paint(&self, bow: impl Fn(Key) -> Option<Bow>, out: &mut Vec<Shape>) {
        for (key, state) in self.seats.iter().zip(&self.state) {
            let Some(bow) = key.and_then(&bow) else { continue };
            let curve = Curve::from(bow);
            let depth = WAVE_DEEP * ((state.rate / self.speed.max(1e-9)) / FULL_WAVE_AT).min(1.0);
            let len = bow.length();
            for (homeward, slots) in [(false, &state.out), (true, &state.home)] {
                if depth >= WAVE_FAINT {
                    crests(&curve, len, state.wave, homeward, depth, out);
                }
                for slot in slots.iter().flatten().filter(|slot| slot.ends_at() > self.wall) {
                    let (span, alpha) = slot.edges(self.wall, homeward);
                    lit(&curve, span, alpha, &slot.ink.paint().to_string(), PULSE_W, out);
                }
            }
        }
    }

    /// Whether anything is still in the air. A pause stops departures, never travel already
    /// launched — and on a canvas that means the card's frame loop runs until this is false.
    pub fn airborne(&self) -> bool {
        self.state.iter().any(|state| state.carrying(self.wall))
    }
}

/// The slot whose afterglow is furthest gone — what a lane recycles when every one of its
/// slots is still lit.
fn oldest(slots: &[Option<Slot>]) -> usize {
    let ends_at = |slot: &Option<Slot>| slot.as_ref().map_or(f64::MIN, Slot::ends_at);
    let mut at = 0;
    for (i, slot) in slots.iter().enumerate() {
        if ends_at(slot) < ends_at(&slots[at]) {
            at = i;
        }
    }
    at
}

/// One direction's carrier wave: crests riding the wire a wavelength apart, each a triangle
/// that swells to [`CREST_AT`] and falls away again. Symmetric on purpose — a crest is not a
/// traveller, and the hard cut at a head is what says one is.
fn crests(curve: &Curve, len: f64, wave: f64, homeward: bool, depth: f64, out: &mut Vec<Shape>) {
    if len <= 0.0 {
        return;
    }
    let phase = match homeward {
        false => wave,
        true => -(wave + WAVE_L / 2.0),
    };
    let ink = hue(homeward).to_string();
    let mut px = phase.rem_euclid(WAVE_L) - WAVE_L;
    while px < len {
        lit(curve, (px / len, (px + CREST_AT) / len), (0.0, depth), &ink, WAVE_W, out);
        lit(curve, ((px + CREST_AT) / len, (px + WAVE_L) / len), (depth, 0.0), &ink, WAVE_W, out);
        px += WAVE_L;
    }
}

/// One lit stretch of a wire, clipped to the wire itself: a stretch reaching past either end
/// keeps the opacity it really has where it crosses, so a pulse leaving its wire thins out
/// instead of being cut off bright.
fn lit(
    curve: &Curve,
    span: (f64, f64),
    alpha: (f64, f64),
    ink: &str,
    width: f64,
    out: &mut Vec<Shape>,
) {
    let (from, to) = (span.0.max(0.0), span.1.min(1.0));
    if to <= from || span.1 <= span.0 {
        return;
    }
    let at = |t: f64| alpha.0 + (alpha.1 - alpha.0) * (t - span.0) / (span.1 - span.0);
    out.push(Shape {
        curve: *curve,
        span: (from, to),
        ink: ink.to_string(),
        width,
        alpha: (at(from), at(to)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bow every test draws its traffic on: a straight run of ten, so a position along it
    /// is the number it reads as.
    fn wire() -> Bow {
        Bow { from: (0.0, 0.0), c1: (0.0, 0.0), c2: (10.0, 0.0), to: (10.0, 0.0) }
    }

    fn tier() -> LaneTier {
        LaneTier::new(4, 6.0)
    }

    fn painted(tier: &LaneTier) -> Vec<Shape> {
        let mut shapes = Vec::new();
        tier.paint(|_| Some(wire()), &mut shapes);
        shapes
    }

    /// A wire keeps its seat while it carries, and a saturated tier refuses new wires
    /// without unseating live ones.
    #[test]
    fn seats_are_sticky_and_bounded() {
        let mut seats: Vec<Option<Key>> = vec![None; 3];
        let a = seat(&mut seats, (0, 1)).expect("an empty tier seats");
        let b = seat(&mut seats, (2, 3)).expect("a second wire seats elsewhere");
        assert_ne!(a, b);
        assert_eq!(seat(&mut seats, (0, 1)), Some(a), "the same wire keeps its seat");
        seat(&mut seats, (4, 5)).expect("the tier fills");
        assert_eq!(seat(&mut seats, (6, 7)), None, "a full tier refuses");
        assert_eq!(seat(&mut seats, (2, 3)), Some(b), "without unseating anyone");
        seats[a] = None;
        assert_eq!(seat(&mut seats, (6, 7)), Some(a), "a freed seat is taken over");
    }

    /// Brightness is multiplicity: more riders never darken a pulse, and the scale
    /// saturates rather than blowing out.
    #[test]
    fn intensity_is_monotone_and_saturating() {
        assert!(intensity(1) > 0.5);
        for n in 1..100 {
            assert!(intensity(n + 1) >= intensity(n));
        }
        assert_eq!(intensity(40), 1.0);
    }

    /// The front edge is the request: half a leg after it departs, its head is half way along
    /// the wire — and the stretch behind it is the afterglow, fading to nothing.
    #[test]
    fn a_front_edge_stands_where_the_leg_has_carried_it() {
        let mut tier = tier();
        tier.frame([((0, 1), false, Ink::Sent)], 0.0, 1.0);
        tier.frame([], 3.0, 1.0);

        let picture = painted(&tier);
        let [pulse] = &picture[..] else { panic!("one send is one pulse") };
        assert!((pulse.span.1 - 0.5).abs() < 1e-9, "half a 6ms leg is half the wire");
        assert_eq!(pulse.span.0, 0.0, "and the afterglow is still leaving the near end");
        assert!(pulse.alpha.0 < 0.05, "all but transparent at the tail");
        assert!(pulse.alpha.1 > 0.5, "solid at the head");
    }

    /// A pulse home runs the same wire the other way: the head is what reaches the near end,
    /// and the memory of it trails back toward the far one.
    #[test]
    fn a_homeward_pulse_runs_its_wire_backwards() {
        let mut tier = tier();
        tier.frame([((0, 1), true, Ink::Answer)], 0.0, 1.0);
        tier.frame([], 3.0, 1.0);

        let picture = painted(&tier);
        let [pulse] = &picture[..] else { panic!("one answer is one pulse") };
        assert!((pulse.span.0 - 0.5).abs() < 1e-9, "the head has come half way home");
        assert_eq!(pulse.span.1, 1.0);
        assert!(pulse.alpha.0 > 0.5, "solid at the head");
        assert!(pulse.alpha.1 < 0.05, "all but transparent at the tail");
    }

    /// The merging rule: everything one wire sent one way in one frame is one pulse, brighter
    /// for the crowd on it.
    #[test]
    fn a_frame_of_sends_on_one_wire_is_one_pulse() {
        let mut tier = tier();
        tier.frame(
            [((0, 1), false, Ink::Sent), ((0, 1), false, Ink::Sent), ((0, 1), true, Ink::Answer)],
            0.0,
            1.0,
        );
        tier.frame([], 1.0, 1.0);

        let shapes = painted(&tier);
        assert_eq!(shapes.len(), 2, "one out, one home: {shapes:#?}");
        assert!(shapes[0].alpha.1 > intensity(1), "two riders are brighter than one");
    }

    /// A pause never freezes a request: nothing departs, and what is in the air keeps flying
    /// on the reader's clock until the last afterglow has decayed.
    #[test]
    fn travel_lands_after_the_departures_stop() {
        let mut tier = tier();
        tier.frame([((0, 1), false, Ink::Sent)], 0.0, 1.0);
        assert!(tier.airborne());

        tier.frame([], AFTERGLOW_MS / 2.0, 1.0);
        assert!(tier.airborne(), "still in the air with the sim stopped");
        assert_eq!(painted(&tier).len(), 1);

        tier.frame([], AFTERGLOW_MS, 1.0);
        assert!(!tier.airborne(), "and the loop is free to stop");
        assert!(painted(&tier).is_empty());
    }

    /// Slow motion stretches the front until it is as long as the afterglow behind it. The
    /// pulse stays a pulse: the tail never overtakes the head.
    #[test]
    fn slow_motion_never_lets_the_afterglow_outrun_the_front() {
        let mut tier = tier();
        tier.frame([((0, 1), false, Ink::Sent)], 0.0, 0.001);
        for _ in 0..40 {
            tier.frame([], 16.0, 0.001);
            for pulse in painted(&tier) {
                assert!(pulse.span.0 < pulse.span.1, "{pulse:#?}");
            }
        }
    }

    /// A quiet wire carries no wave: the crests are rate as texture, and a wire nobody is
    /// using has no rate to draw.
    #[test]
    fn a_quiet_wire_draws_no_crests() {
        let mut tier = tier();
        tier.frame([((0, 1), false, Ink::Sent)], 0.0, 1.0);
        tier.frame([], 1.0, 1.0);
        assert_eq!(painted(&tier).len(), 1, "the pulse, and nothing under it");
    }

    /// A busy wire does, both ways, and never past the ends of the wire it rides.
    #[test]
    fn a_busy_wire_carries_a_wave_each_way() {
        let mut tier = LaneTier::new(4, 6.0);
        for _ in 0..STAT_MS as usize {
            tier.frame((0..20).map(|_| ((0, 1), false, Ink::Sent)), 1.0, 1.0);
        }
        let waves: Vec<Shape> = painted(&tier)
            .into_iter()
            .filter(|shape| shape.width == WAVE_W)
            .collect();
        assert!(!waves.is_empty(), "a wire at 20 000 req/s is textured");
        for shape in &waves {
            assert!(shape.span.0 >= 0.0 && shape.span.1 <= 1.0, "{shape:#?}");
        }
        assert!(
            waves.iter().any(|s| s.ink == Ink::Sent.paint().to_string())
                && waves.iter().any(|s| s.ink == Ink::Answer.paint().to_string()),
            "direction survives in hue",
        );
    }
}
