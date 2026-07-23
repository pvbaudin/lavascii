//! A full-color ASCII lava lamp for the terminal.
//!
//! Rendering is metaball-based: every wax body (free blobs + the top/bottom
//! pools) contributes a smooth field, and the fields simply *add*. Where two
//! bodies approach, their fields fuse into a goopy neck — so merges and splits
//! come for free with an organic look. Temperature is carried as a per-body
//! value and rendered as a field-weighted *blend*, never an overwrite, so two
//! blobs that touch mix their heat instead of erasing each other (which is what
//! would otherwise let a blob get stranded, neither hot nor cold, in the
//! middle).

use std::io::{self, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal, ExecutableCommand,
};

/// Terminal cells are roughly twice as tall as they are wide. We scale
/// vertical distances by this so blobs read as round instead of squashed, and
/// so radii (kept in column-units) map sensibly onto rows.
const ASPECT: f32 = 2.0;

// ---- tunables -------------------------------------------------------------
const ABS_MAX_RADIUS: f32 = 26.0; // hard upper size cap (column units)
const MIN_RADIUS: f32 = 2.5; // never shrink a blob below this
const BUOY: f32 = 9.0; // buoyancy acceleration scale
const DAMP_V: f32 = 1.6; // vertical velocity damping (1/s)
const DAMP_H: f32 = 1.5; // horizontal velocity damping (1/s)
const WOBBLE: f32 = 3.5; // horizontal wander acceleration
const HOT: f32 = 0.92; // temperature of a rising (hot) blob
const COLD: f32 = 0.08; // temperature of a sinking (cold) blob
const EASE: f32 = 2.5; // radius easing rate (1/s) — smooths merges/splits
const COHESION: f32 = 6.0; // like-temp attraction that draws blobs together
const VISC: f32 = 2.5; // relative-velocity damping between close blobs

/// A tiny, dependency-free xorshift PRNG. Good enough for jitter and phases.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform in [0, 1).
    fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Uniform in [lo, hi).
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }
}

/// A free-floating wax blob.
#[derive(Clone)]
struct Blob {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    r: f32,  // current (rendered) radius, eases toward rt
    rt: f32, // target radius
    temp: f32, // 0 = cold, 1 = hot
    phase: f32, // personal wobble phase
    dying: bool, // being absorbed into a pool: melt to nothing, then remove
}

/// A slab of the top or bottom pool — a big metaball whose surface bobs.
#[derive(Clone)]
struct Pool {
    x: f32,
    base_y: f32,
    r: f32,
    temp: f32,
    phase: f32,
    amp: f32,
    speed: f32,
}

impl Pool {
    /// Current bobbing centre.
    fn y(&self, t: f32) -> f32 {
        self.base_y + self.amp * (t * self.speed + self.phase).sin()
    }
}

/// A tiny, short-lived speck of wax flung off during a merge or split. It
/// shrinks over its lifetime and contributes to the field like everything
/// else, so it reads as a stringy filament of goo rather than a hard particle.
#[derive(Clone)]
struct Droplet {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    r0: f32,
    temp: f32,
    age: f32,
    ttl: f32,
}

struct World {
    w: usize,
    h: usize,
    max_r: f32,
    target: usize,
    blobs: Vec<Blob>,
    pools: Vec<Pool>,
    droplets: Vec<Droplet>,
    rng: Rng,
    t: f32,
    cycles: u32, // total pool absorptions — a liveness counter for --test
    merges: u32, // diagnostics
    splits: u32,
}

impl World {
    fn new(w: usize, h: usize, seed: u64) -> Self {
        let mut world = World {
            w: w.max(1),
            h: h.max(1),
            max_r: MIN_RADIUS,
            target: 3,
            blobs: Vec::new(),
            pools: Vec::new(),
            droplets: Vec::new(),
            rng: Rng::new(seed),
            t: 0.0,
            cycles: 0,
            merges: 0,
            splits: 0,
        };
        world.recompute_layout();
        world.spawn_initial();
        world
    }

    /// Derive blob size and count from *both* dimensions. `min(w, h*ASPECT)`
    /// means a wide-but-short window is limited by its height, so we never grow
    /// giant blobs that don't fit vertically.
    fn recompute_layout(&mut self) {
        let metric = (self.w as f32).min(self.h as f32 * ASPECT);
        self.max_r = (metric * 0.23).clamp(MIN_RADIUS, ABS_MAX_RADIUS);
        let area = (self.w * self.h) as f32;
        self.target = ((area / 520.0) as usize).clamp(5, 16);
        self.rebuild_pools();
    }

    fn rebuild_pools(&mut self) {
        self.pools.clear();
        let pool_r = self.max_r * 1.35;
        let spacing = (self.max_r * 1.05).max(2.0);
        let n = (self.w as f32 / spacing).ceil() as i32 + 2;
        // Off-screen centres so only the wobbling surface shows as a band.
        let bottom_c = self.h as f32 + pool_r / ASPECT * 0.35;
        let top_c = -(pool_r / ASPECT * 0.35);
        let amp = (self.max_r / ASPECT * 0.30).max(0.6);
        for i in 0..n {
            let x = i as f32 * spacing - spacing * 0.5;
            let phase = self.rng.range(0.0, 6.28);
            // bottom = hot
            self.pools.push(Pool {
                x,
                base_y: bottom_c,
                r: pool_r,
                temp: 1.0,
                phase,
                amp,
                speed: self.rng.range(0.4, 0.8),
            });
            let phase2 = self.rng.range(0.0, 6.28);
            // top = cold
            self.pools.push(Pool {
                x,
                base_y: top_c,
                r: pool_r,
                temp: 0.0,
                phase: phase2,
                amp,
                speed: self.rng.range(0.4, 0.8),
            });
        }
    }

    fn spawn_initial(&mut self) {
        self.blobs.clear();
        let (w, h) = (self.w as f32, self.h as f32);
        let tb = self.band();
        let m = self.max_r.min(w * 0.4);
        // Half hot (already climbing), half cold (already sinking), spread over
        // the whole column so the lamp is mid-cycle from the first frame.
        for i in 0..self.target {
            let hot = i % 2 == 0;
            let r = self.rng.range(self.max_r * 0.6, self.max_r * 0.95);
            self.blobs.push(Blob {
                x: self.rng.range(m, (w - m).max(m + 0.1)),
                y: self.rng.range(tb + 1.0, (h - tb - 1.0).max(tb + 1.1)),
                vx: self.rng.range(-1.5, 1.5),
                vy: if hot { -1.0 } else { 1.0 },
                r,
                rt: r,
                temp: if hot { HOT } else { COLD },
                phase: self.rng.range(0.0, 6.28),
                dying: false,
            });
        }
    }

    fn resize(&mut self, w: usize, h: usize) {
        let (ow, oh) = (self.w as f32, self.h as f32);
        self.w = w.max(1);
        self.h = h.max(1);
        let (sx, sy) = (self.w as f32 / ow, self.h as f32 / oh);
        self.recompute_layout();
        // Rescale existing blobs so the scene stretches with the window.
        for b in &mut self.blobs {
            b.x = (b.x * sx).clamp(0.0, self.w as f32);
            b.y = (b.y * sy).clamp(0.0, self.h as f32);
            b.r = b.r.min(self.max_r).max(MIN_RADIUS);
        }
        // Keep the blob population sensible for the new size.
        self.enforce_count();
    }

    fn update(&mut self, dt: f32) {
        let dt = dt.min(0.05); // clamp so a stall can't explode the sim
        self.t += dt;
        let (w, h) = (self.w as f32, self.h as f32);

        // Like-temperature blobs gently draw together and share momentum, so
        // they slide into a merge instead of snapping across a gap.
        self.cohesion(dt);

        for b in &mut self.blobs {
            // Radius glides toward its target so merges grow, splits shrink,
            // and absorbed blobs melt away smoothly rather than popping.
            b.r += (b.rt - b.r) * (EASE * dt).min(1.0);
            // A dying blob is dissolving into a pool: no forces, just melt.
            if b.dying {
                continue;
            }
            // Temperature is fixed while a blob is in flight — it's set when the
            // blob is born from a pool and only ever HOT or COLD. So a blob is
            // always decisively buoyant or heavy and can never equilibrate to a
            // neutral, stuck-in-the-middle state.
            let buoy = (b.temp - 0.5) * BUOY;
            b.vy -= buoy * dt;
            // Smooth, low-frequency horizontal wander. No per-frame random
            // jitter — that was what made touching blobs shimmer.
            b.vx += (self.t * 0.8 + b.phase).sin() * WOBBLE * dt;
            b.vx *= (1.0 - DAMP_H * dt).max(0.0);
            b.vy *= (1.0 - DAMP_V * dt).max(0.0);
            b.x += b.vx * dt;
            b.y += b.vy * dt;

            // Side walls: gentle bounce.
            let m = b.r * 0.3;
            if b.x < m {
                b.x = m;
                b.vx = b.vx.abs() * 0.5;
            } else if b.x > w - m {
                b.x = w - m;
                b.vx = -b.vx.abs() * 0.5;
            }
            // Never leave the screen; the pools turn blobs around by absorbing
            // them (below), so there's no hard vertical wall to bounce off.
            b.y = b.y.clamp(0.5, h - 0.5);
        }
        // Reap fully-melted blobs.
        self.blobs.retain(|b| !(b.dying && b.r < 0.5));

        self.pool_exchange();

        // Advance goo droplets: drift, gently follow buoyancy, shrink, expire.
        for d in &mut self.droplets {
            d.age += dt;
            let buoy = (d.temp - 0.5) * BUOY * 0.6;
            d.vy -= buoy * dt;
            d.vx *= (1.0 - DAMP_H * dt).max(0.0);
            d.vy *= (1.0 - DAMP_V * dt).max(0.0);
            d.x += d.vx * dt;
            d.y += d.vy * dt;
        }
        self.droplets.retain(|d| d.age < d.ttl);

        self.merge_pass();
        self.enforce_count();
    }

    fn spawn_goo(&mut self, x: f32, y: f32, temp: f32, base_r: f32, n: usize, spread: f32) {
        for _ in 0..n {
            let ang = self.rng.range(0.0, 6.28);
            let spd = self.rng.range(0.3, 1.0) * spread;
            self.droplets.push(Droplet {
                x,
                y,
                vx: ang.cos() * spd,
                vy: ang.sin() * spd / ASPECT,
                r0: (base_r * self.rng.range(0.35, 0.6)).clamp(1.2, self.max_r * 0.5),
                temp,
                age: 0.0,
                ttl: self.rng.range(0.5, 1.1),
            });
        }
    }

    /// Gentle cohesion between like-temperature blobs that are close enough to
    /// eventually merge: a soft mutual attraction plus relative-velocity
    /// damping (viscosity). This makes them ooze together and settle nearly
    /// concentric before the discrete merge fires, so the merge barely moves
    /// the centre. Opposite-temperature blobs are ignored (they must not clump)
    /// and pairs too big to merge are skipped (so they don't rattle at the cap).
    fn cohesion(&mut self, dt: f32) {
        let n = self.blobs.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let (a, b) = (self.blobs[i].clone(), self.blobs[j].clone());
                if a.dying || b.dying {
                    continue;
                }
                if (a.temp - b.temp).abs() >= 0.4 {
                    continue;
                }
                if (a.r * a.r + b.r * b.r).sqrt() > self.max_r * 1.02 {
                    continue;
                }
                let (dsx, dsy) = (b.x - a.x, b.y - a.y);
                let dscreen = (dsx * dsx + dsy * dsy).sqrt().max(0.001);
                let dcorr = (dsx * dsx + dsy * dsy * ASPECT * ASPECT).sqrt();
                let reach = (a.r + b.r) * 1.15;
                if dcorr > reach {
                    continue;
                }
                let k = 1.0 - dcorr / reach; // 0 at the edge, 1 when concentric
                let (ux, uy) = (dsx / dscreen, dsy / dscreen);
                let acc = COHESION * k * dt;
                self.blobs[i].vx += ux * acc;
                self.blobs[i].vy += uy * acc;
                self.blobs[j].vx -= ux * acc;
                self.blobs[j].vy -= uy * acc;
                let vk = VISC * k * dt * 0.5;
                let (rvx, rvy) = (b.vx - a.vx, b.vy - a.vy);
                self.blobs[i].vx += rvx * vk;
                self.blobs[i].vy += rvy * vk;
                self.blobs[j].vx -= rvx * vk;
                self.blobs[j].vy -= rvy * vk;
            }
        }
    }

    /// Vertical reach of a pool's wobbling surface, in rows. Absorption happens
    /// once a blob has sunk this far into the top or bottom band.
    fn band(&self) -> f32 {
        (self.max_r / ASPECT * 0.6).max(2.5)
    }

    /// The lava-lamp cycle. A hot riser that reaches the cold top pool is
    /// absorbed (goopy splash) and the top pool sheds a cold drip in its place;
    /// a cold sinker that reaches the hot bottom pool is absorbed and the bottom
    /// pool sheds a hot riser. Mass circulates and the count stays balanced, so
    /// nothing accumulates in the middle.
    fn pool_exchange(&mut self) {
        let h = self.h as f32;
        let tb = self.band();
        let (mut emit_hot, mut emit_cold) = (0u32, 0u32);
        for b in &mut self.blobs {
            if b.dying {
                continue; // already melting into a pool
            }
            // A blob that has reached the far pool starts dissolving into it
            // (melt its radius to 0) and the opposite pool sheds a replacement.
            if b.temp > 0.5 && b.y <= tb {
                b.dying = true;
                b.rt = 0.0;
                emit_cold += 1;
            } else if b.temp < 0.5 && b.y >= h - tb {
                b.dying = true;
                b.rt = 0.0;
                emit_hot += 1;
            }
        }
        self.cycles += emit_hot + emit_cold;
        for _ in 0..emit_cold {
            self.emit(false);
        }
        for _ in 0..emit_hot {
            self.emit(true);
        }
    }

    /// Birth a blob from a pool: a hot riser from the bottom, or a cold drip
    /// from the top. It starts just inside the pool surface with a nudge in its
    /// travel direction, plus a little goo to sell the pinch-off.
    fn emit(&mut self, hot: bool) {
        let tb = self.band();
        let (w, h) = (self.w as f32, self.h as f32);
        let m = self.max_r.min(w * 0.4);
        let x = self.rng.range(m, (w - m).max(m + 0.1));
        let r = self.rng.range(self.max_r * 0.55, self.max_r * 0.95);
        let (y, vy, temp) = if hot {
            (h - tb - 1.0, self.rng.range(-1.5, -0.5), HOT)
        } else {
            (tb + 1.0, self.rng.range(0.5, 1.5), COLD)
        };
        self.blobs.push(Blob {
            x,
            y,
            vx: self.rng.range(-1.2, 1.2),
            vy,
            r: MIN_RADIUS, // grows out of the pool
            rt: r,
            temp,
            phase: self.rng.range(0.0, 6.28),
            dying: false,
        });
        self.spawn_goo(x, y, temp, r, 2, 1.4);
    }

    /// Merge overlapping blobs, conserving area (and blending temperature by
    /// area) — the metaball neck between them has already been drawn goopy.
    fn merge_pass(&mut self) {
        let mut i = 0;
        while i < self.blobs.len() {
            let mut j = i + 1;
            let mut merged = false;
            while j < self.blobs.len() {
                let (a, b) = (&self.blobs[i], &self.blobs[j]);
                let dx = a.x - b.x;
                let dy = (a.y - b.y) * ASPECT;
                let dist = (dx * dx + dy * dy).sqrt();
                let combined_r = (a.r * a.r + b.r * b.r).sqrt();
                // Only fuse like-temperature wax (both rising or both sinking).
                // A hot and a cold blob must NOT average to neutral buoyancy —
                // that's what stranded them in the middle. They pass through
                // each other goopily via the field instead.
                let alike = (a.temp - b.temp).abs() < 0.4 && !a.dying && !b.dying;
                if alike && dist < (a.r + b.r) * 0.45 && combined_r <= self.max_r * 1.02 {
                    let (aa, ba) = (a.r * a.r, b.r * b.r);
                    let sum = aa + ba;
                    let nb = Blob {
                        x: (a.x * aa + b.x * ba) / sum,
                        y: (a.y * aa + b.y * ba) / sum,
                        vx: (a.vx * aa + b.vx * ba) / sum,
                        vy: (a.vy * aa + b.vy * ba) / sum,
                        r: a.r.max(b.r), // start at the larger, grow into...
                        rt: combined_r,  // ...the combined size
                        temp: (a.temp * aa + b.temp * ba) / sum,
                        phase: a.phase,
                        dying: false,
                    };
                    // A little splash of goo squeezed out at the contact point.
                    let (cx, cy, ct, cr) = (nb.x, nb.y, nb.temp, nb.r);
                    self.blobs[i] = nb;
                    self.blobs.swap_remove(j);
                    self.spawn_goo(cx, cy, ct, cr, 2, 2.0);
                    self.merges += 1;
                    merged = true;
                } else {
                    j += 1;
                }
            }
            if !merged {
                i += 1;
            }
        }
    }

    fn enforce_count(&mut self) {
        // Top up from the pools, alternating hot risers and cold drips.
        let mut hot = self.blobs.len() % 2 == 0;
        while self.blobs.len() < self.target {
            self.emit(hot);
            hot = !hot;
        }
        // Guard against runaway growth from splits.
        while self.blobs.len() > self.target + 4 {
            self.blobs.pop();
        }
    }

    /// Sample the total field and blended temperature at a grid cell.
    /// Returns (field, temp).
    ///
    /// Uses a finite-support kernel `(1 - (d/R)^2)^2` (peak 1 at the centre,
    /// exactly 0 beyond R). Because it's bounded and compact, summing many
    /// bodies stays well-behaved — distant wax contributes nothing, so the
    /// background clears cleanly and overlaps read as smooth goopy necks.
    fn sample(&self, col: f32, row: f32) -> (f32, f32) {
        let mut field = 0.0f32;
        let mut twsum = 0.0f32;
        for b in &self.blobs {
            let dx = col - b.x;
            let dy = (row - b.y) * ASPECT;
            let w = kernel(dx * dx + dy * dy, b.r);
            field += w;
            twsum += w * b.temp;
        }
        for p in &self.pools {
            let dx = col - p.x;
            let dy = (row - p.y(self.t)) * ASPECT;
            let w = kernel(dx * dx + dy * dy, p.r);
            field += w;
            twsum += w * p.temp;
        }
        for d in &self.droplets {
            let r = d.r0 * (1.0 - d.age / d.ttl); // shrinks as it ages
            if r <= 0.1 {
                continue;
            }
            let dx = col - d.x;
            let dy = (row - d.y) * ASPECT;
            let w = kernel(dx * dx + dy * dy, r);
            field += w;
            twsum += w * d.temp;
        }
        let temp = if field > 1e-4 { twsum / field } else { 0.5 };
        (field, temp)
    }
}

/// Finite-support metaball kernel: 1 at the centre, 0 at and beyond radius `r`.
#[inline]
fn kernel(d2: f32, r: f32) -> f32 {
    let t = 1.0 - d2 / (r * r);
    if t > 0.0 {
        t * t
    } else {
        0.0
    }
}

// Field magnitude below FIELD_LO is empty space; at/above FIELD_HI is the
// dense molten core. Everything renders as a normalised intensity in between.
const FIELD_LO: f32 = 0.20;
const FIELD_HI: f32 = 1.05;

/// Light-to-heavy ASCII density ramp — sparse at a blob's faint edge, solid at
/// its core. This is what gives the hand-drawn ASCII-art look.
const RAMP: [char; 10] = ['.', ':', '-', '=', '+', '*', 'o', '%', '#', '@'];

#[inline]
fn intensity(field: f32) -> f32 {
    ((field - FIELD_LO) / (FIELD_HI - FIELD_LO)).clamp(0.0, 1.0)
}

/// Pick the ramp glyph for a given normalised intensity.
fn ramp_char(i: f32) -> char {
    let idx = (i * (RAMP.len() - 1) as f32).round() as usize;
    RAMP[idx.min(RAMP.len() - 1)]
}

/// Unified "magenta neon" palette: dim purple at the faint edges, ramping
/// through magenta to a hot-pink core. Colour is driven by density (`i`) so
/// the whole scene stays one coherent hue family; temperature only nudges
/// within it (warmer wax leans pink, cooler wax leans purple).
fn lava_color(i: f32) -> (u8, u8, u8) {
    // stops: (pos, r, g, b)
    const STOPS: [(f32, f32, f32, f32); 5] = [
        (0.00, 55.0, 15.0, 80.0),    // dim purple edge
        (0.35, 120.0, 25.0, 150.0),  // purple
        (0.65, 195.0, 35.0, 165.0),  // magenta
        (0.85, 240.0, 60.0, 155.0),  // hot pink
        (1.00, 255.0, 150.0, 205.0), // pale pink core
    ];
    let i = i.clamp(0.0, 1.0);
    let mut k = 0;
    while k + 1 < STOPS.len() && i > STOPS[k + 1].0 {
        k += 1;
    }
    let (p0, r0, g0, b0) = STOPS[k];
    let (p1, r1, g1, b1) = STOPS[(k + 1).min(STOPS.len() - 1)];
    let f = if p1 > p0 { (i - p0) / (p1 - p0) } else { 0.0 };
    (
        (r0 + (r1 - r0) * f) as u8,
        (g0 + (g1 - g0) * f) as u8,
        (b0 + (b1 - b0) * f) as u8,
    )
}

/// Render the world into an ANSI truecolor frame (cursor is homed by caller).
fn render_ansi(world: &World, buf: &mut String) {
    buf.clear();
    buf.push_str("\x1b[H"); // cursor home
    let mut last: Option<(u8, u8, u8)> = None;
    for row in 0..world.h {
        for col in 0..world.w {
            let (field, temp) = world.sample(col as f32 + 0.5, row as f32 + 0.5);
            if field < FIELD_LO {
                // Empty space: a plain blank keeps the terminal's own
                // background — we never paint or colour it. (A space shows no
                // foreground ink, so leaving the last fg colour set is fine.)
                buf.push(' ');
                continue;
            }
            let dens = intensity(field);
            let ch = ramp_char(dens);
            // Density drives the glyph; colour follows density but drifts a
            // little with temperature to keep the wax alive within one hue.
            let ci = (dens + (temp - 0.5) * 0.30).clamp(0.0, 1.0);
            let c = lava_color(ci);
            if last != Some(c) {
                buf.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                last = Some(c);
            }
            buf.push(ch);
        }
        if row + 1 < world.h {
            buf.push_str("\r\n");
        }
    }
}

/// Plain (colourless) render for `--test`, so the shape can be eyeballed
/// without a live terminal.
fn render_plain(world: &World) -> String {
    let mut s = String::new();
    for row in 0..world.h {
        for col in 0..world.w {
            let (field, _t) = world.sample(col as f32 + 0.5, row as f32 + 0.5);
            if field < FIELD_LO {
                s.push(' ');
            } else {
                s.push(ramp_char(intensity(field)));
            }
        }
        s.push('\n');
    }
    s
}

/// Flat row-major glyph grid, for frame-to-frame churn diagnostics.
fn glyph_grid(world: &World) -> Vec<char> {
    let mut g = Vec::with_capacity(world.w * world.h);
    for row in 0..world.h {
        for col in 0..world.w {
            let (field, _t) = world.sample(col as f32 + 0.5, row as f32 + 0.5);
            g.push(if field < FIELD_LO {
                ' '
            } else {
                ramp_char(intensity(field))
            });
        }
    }
    g
}

fn grid_str(g: &[char], w: usize, h: usize) -> String {
    let mut s = String::with_capacity(g.len() + h);
    for row in 0..h {
        s.extend(&g[row * w..(row + 1) * w]);
        s.push('\n');
    }
    s
}

/// Simulate headlessly and report how "spastic" the motion is: per-frame churn
/// (cells changing glyph), correlated with merge/split/absorb events, plus a
/// dump of the frames around the worst spike so the culprit is visible.
fn run_diag() {
    let (w, h) = (80usize, 40usize);
    let mut world = World::new(w, h, 0x1234_5678);
    for _ in 0..150 {
        world.update(1.0 / 30.0);
    }
    let n = 300usize;
    let total = (w * h) as f32;
    let mut frames: Vec<Vec<char>> = Vec::with_capacity(n);
    let mut churn = vec![0usize; n];
    let mut ev = vec![(0u32, 0u32, 0u32, 0usize); n];
    let (mut pm, mut ps, mut pc) = (world.merges, world.splits, world.cycles);
    let mut prev = glyph_grid(&world);
    for f in 0..n {
        world.update(1.0 / 30.0);
        let g = glyph_grid(&world);
        churn[f] = (0..g.len()).filter(|&k| g[k] != prev[k]).count();
        ev[f] = (
            world.merges - pm,
            world.splits - ps,
            world.cycles - pc,
            world.blobs.len(),
        );
        pm = world.merges;
        ps = world.splits;
        pc = world.cycles;
        prev = g.clone();
        frames.push(g);
    }
    let mean = churn.iter().sum::<usize>() as f32 / n as f32;
    let (peak_f, &peak_c) = churn.iter().enumerate().max_by_key(|(_, &c)| c).unwrap();
    println!("== diag {}x{}, {} frames @30fps ==", w, h, n);
    println!(
        "totals: merges {} splits {} absorptions {}",
        world.merges, world.splits, world.cycles
    );
    println!(
        "churn: mean {:.0} cells/frame ({:.1}%), peak {} ({:.1}%) at frame {}",
        mean,
        100.0 * mean / total,
        peak_c,
        100.0 * peak_c as f32 / total,
        peak_f
    );
    // Timeline: flag frames that spike or carry an event.
    println!("-- notable frames (spike >2.5x mean, or merge/split/absorb) --");
    for f in 0..n {
        let (m, s, c, cnt) = ev[f];
        let spike = churn[f] as f32 > 2.5 * mean;
        if spike || m > 0 || s > 0 || c > 0 {
            println!(
                "  f{:>3} churn {:>4} ({:>2.0}%) {}{}{} blobs={}",
                f,
                churn[f],
                100.0 * churn[f] as f32 / total,
                if m > 0 { format!("MERGE×{m} ") } else { String::new() },
                if s > 0 { format!("SPLIT×{s} ") } else { String::new() },
                if c > 0 { format!("ABSORB×{c} ") } else { String::new() },
                cnt,
            );
        }
    }
    // Show the frames straddling the worst spike.
    println!("-- frames around peak churn (f{}) --", peak_f);
    for f in peak_f.saturating_sub(1)..=(peak_f + 1).min(n - 1) {
        println!("--- frame {} (churn {}) ---", f, churn[f]);
        print!("{}", grid_str(&frames[f], w, h));
    }
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15)
}

fn run_test() {
    let (w, h) = (80usize, 40usize);
    let mut world = World::new(w, h, 0x1234_5678);
    // Warm up so blobs are mid-cycle.
    for _ in 0..400 {
        world.update(0.033);
    }
    println!(
        "== frame after warmup ({}x{}, {} blobs, max_r {:.1}) ==",
        w,
        h,
        world.blobs.len(),
        world.max_r
    );
    print!("{}", render_plain(&world));
    // Advance and print again to confirm motion.
    for _ in 0..120 {
        world.update(0.033);
    }
    println!("== frame +4s ({} blobs) ==", world.blobs.len());
    print!("{}", render_plain(&world));

    // Liveness check: over a long run, wax should keep circulating (many pool
    // absorptions) and not pile up in the middle third of the lamp.
    let c0 = world.cycles;
    let (mut top, mut mid, mut bot) = (0u64, 0u64, 0u64);
    let steps = 1800; // ~60s at 30fps
    for _ in 0..steps {
        world.update(0.033);
        for b in &world.blobs {
            let ny = b.y / world.h as f32;
            if ny < 0.33 {
                top += 1;
            } else if ny > 0.66 {
                bot += 1;
            } else {
                mid += 1;
            }
        }
    }
    let total = (top + mid + bot).max(1) as f32;
    println!(
        "== liveness over {}s: {} absorptions | occupancy top {:.0}% mid {:.0}% bot {:.0}% ==",
        (steps as f32 * 0.033) as u32,
        world.cycles - c0,
        100.0 * top as f32 / total,
        100.0 * mid as f32 / total,
        100.0 * bot as f32 / total,
    );

    // Sanity on a wide-short window: blobs must stay small.
    let mut wide = World::new(200, 12, 0x9);
    for _ in 0..100 {
        wide.update(0.033);
    }
    println!(
        "== wide-short 200x12: max_r {:.1} (should be small) ==",
        wide.max_r
    );
}

fn run_interactive() -> io::Result<()> {
    let mut out = io::stdout();
    terminal::enable_raw_mode()?;
    out.execute(terminal::EnterAlternateScreen)?;
    out.execute(cursor::Hide)?;
    // Clear the alternate screen using the terminal's own default background.
    out.write_all(b"\x1b[2J")?;
    out.flush()?;

    let (mut w, mut h) = terminal::size().map(|(c, r)| (c as usize, r as usize))?;
    let mut world = World::new(w, h, seed());
    let mut buf = String::with_capacity(w * h * 4);
    let mut last = Instant::now();
    let frame = Duration::from_millis(33);

    let result = (|| -> io::Result<()> {
        loop {
            // Drain all pending input/resize events without blocking.
            while event::poll(Duration::from_millis(0))? {
                match event::read()? {
                    Event::Key(KeyEvent { code, modifiers, .. }) => {
                        let quit = matches!(code, KeyCode::Char('q') | KeyCode::Esc)
                            || (modifiers.contains(KeyModifiers::CONTROL)
                                && code == KeyCode::Char('c'));
                        if quit {
                            return Ok(());
                        }
                    }
                    Event::Resize(nc, nr) => {
                        w = nc as usize;
                        h = nr as usize;
                        world.resize(w, h);
                        out.write_all(b"\x1b[2J")?;
                    }
                    _ => {}
                }
            }

            let now = Instant::now();
            let dt = (now - last).as_secs_f32();
            last = now;
            world.update(dt);

            render_ansi(&world, &mut buf);
            out.write_all(buf.as_bytes())?;
            out.flush()?;

            let elapsed = now.elapsed();
            if elapsed < frame {
                std::thread::sleep(frame - elapsed);
            }
        }
    })();

    // Always restore the terminal.
    let _ = out.write_all(b"\x1b[0m");
    let _ = out.execute(cursor::Show);
    let _ = out.execute(terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    result
}

fn main() {
    if std::env::args().any(|a| a == "--test") {
        run_test();
        return;
    }
    if std::env::args().any(|a| a == "--diag") {
        run_diag();
        return;
    }
    if let Err(e) = run_interactive() {
        let _ = terminal::disable_raw_mode();
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
