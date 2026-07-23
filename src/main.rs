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
const ABS_MAX_RADIUS: f32 = 16.0; // hard upper size cap (column units)
const MIN_RADIUS: f32 = 2.5; // never shrink a blob below this
const BUOY: f32 = 16.0; // buoyancy acceleration scale
const DAMP_V: f32 = 1.4; // vertical velocity damping (1/s)
const DAMP_H: f32 = 1.3; // horizontal velocity damping (1/s)
const WOBBLE: f32 = 7.0; // horizontal wander acceleration
const HEAT_RATE: f32 = 1.6; // how fast the end zones re-temperature a blob
const NOISE: f32 = 2.2; // tiny random jitter to break symmetry

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
    r: f32,
    temp: f32, // 0 = cold, 1 = hot
    phase: f32, // personal wobble phase
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

struct World {
    w: usize,
    h: usize,
    max_r: f32,
    target: usize,
    blobs: Vec<Blob>,
    pools: Vec<Pool>,
    rng: Rng,
    t: f32,
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
            rng: Rng::new(seed),
            t: 0.0,
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
        self.max_r = (metric * 0.16).clamp(MIN_RADIUS, ABS_MAX_RADIUS);
        let area = (self.w * self.h) as f32;
        self.target = ((area / 950.0) as usize).clamp(3, 9);
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
                speed: self.rng.range(0.7, 1.3),
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
                speed: self.rng.range(0.7, 1.3),
            });
        }
    }

    fn spawn_initial(&mut self) {
        self.blobs.clear();
        for _ in 0..self.target {
            let y = self.rng.range(self.max_r, self.h as f32 - self.max_r);
            let temp = 1.0 - y / self.h as f32; // hot near the bottom
            let r = self.rng.range(self.max_r * 0.5, self.max_r * 0.9);
            self.blobs.push(Blob {
                x: self.rng.range(self.max_r, self.w as f32 - self.max_r),
                y,
                vx: self.rng.range(-2.0, 2.0),
                vy: self.rng.range(-2.0, 2.0),
                r,
                temp: temp.clamp(0.0, 1.0),
                phase: self.rng.range(0.0, 6.28),
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
        while self.blobs.len() < self.target {
            self.split_largest();
        }
    }

    fn update(&mut self, dt: f32) {
        let dt = dt.min(0.05); // clamp so a stall can't explode the sim
        self.t += dt;
        let (w, h) = (self.w as f32, self.h as f32);

        for b in &mut self.blobs {
            let ny = b.y / h; // 0 top .. 1 bottom
            // Heat exchange only bites hard in the end zones; the middle is
            // near-adiabatic so momentum carries a blob through without
            // equilibrating to a stuck, neutral-buoyancy temperature.
            if ny > 0.80 {
                b.temp += (1.0 - b.temp) * HEAT_RATE * dt;
            } else if ny < 0.20 {
                b.temp += (0.0 - b.temp) * HEAT_RATE * dt;
            } else {
                b.temp += (0.5 - b.temp) * 0.04 * dt;
            }
            b.temp = b.temp.clamp(0.0, 1.0);

            // Buoyancy: hotter than neutral rises (screen-up is -y).
            let buoy = (b.temp - 0.5) * BUOY;
            b.vy -= buoy * dt;
            // Gentle horizontal wander + jitter.
            b.vx += (self.t * 1.3 + b.phase).sin() * WOBBLE * dt;
            b.vy += (self.rng.f32() - 0.5) * NOISE * dt;
            b.vx += (self.rng.f32() - 0.5) * NOISE * dt;
            // Damping.
            b.vx *= (1.0 - DAMP_H * dt).max(0.0);
            b.vy *= (1.0 - DAMP_V * dt).max(0.0);
            // Integrate.
            b.x += b.vx * dt;
            b.y += b.vy * dt;

            // Side walls bounce.
            let m = b.r * 0.4;
            if b.x < m {
                b.x = m;
                b.vx = b.vx.abs() * 0.4;
            } else if b.x > w - m {
                b.x = w - m;
                b.vx = -b.vx.abs() * 0.4;
            }
            // Soft floor/ceiling so blobs sink into the pools without escaping.
            let top = b.r / ASPECT * 0.4;
            let bot = h - b.r / ASPECT * 0.4;
            if b.y < top {
                b.y = top;
                b.vy = b.vy.abs() * 0.3;
            } else if b.y > bot {
                b.y = bot;
                b.vy = -b.vy.abs() * 0.3;
            }
        }

        self.merge_pass();
        self.split_pass();
        self.enforce_count();
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
                if dist < (a.r + b.r) * 0.5 && combined_r <= self.max_r * 1.02 {
                    let (aa, ba) = (a.r * a.r, b.r * b.r);
                    let sum = aa + ba;
                    let nb = Blob {
                        x: (a.x * aa + b.x * ba) / sum,
                        y: (a.y * aa + b.y * ba) / sum,
                        vx: (a.vx * aa + b.vx * ba) / sum,
                        vy: (a.vy * aa + b.vy * ba) / sum,
                        r: combined_r,
                        temp: (a.temp * aa + b.temp * ba) / sum,
                        phase: a.phase,
                    };
                    self.blobs[i] = nb;
                    self.blobs.swap_remove(j);
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

    /// Occasionally a fat blob pinches into two, pushed gently apart. They
    /// start close so the metaball bridge thins out for a goopy split.
    fn split_pass(&mut self) {
        if self.blobs.len() >= self.target + 3 {
            return;
        }
        let n = self.blobs.len();
        for i in 0..n {
            let b = self.blobs[i].clone();
            if b.r > self.max_r * 0.82 && self.rng.f32() < 0.010 {
                self.split_at(i, &b);
            }
        }
    }

    fn split_largest(&mut self) {
        if let Some((i, _)) = self
            .blobs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.r.partial_cmp(&b.1.r).unwrap())
        {
            let b = self.blobs[i].clone();
            self.split_at(i, &b);
        }
    }

    fn split_at(&mut self, i: usize, b: &Blob) {
        let nr = (b.r / std::f32::consts::SQRT_2).max(MIN_RADIUS);
        let push = self.rng.range(1.5, 3.0);
        let ang = self.rng.range(0.0, 6.28);
        let (ox, oy) = (ang.cos() * nr * 0.5, ang.sin() * nr * 0.5 / ASPECT);
        self.blobs[i] = Blob {
            x: b.x - ox,
            y: b.y - oy,
            vx: b.vx - push,
            vy: b.vy - oy.signum() * 0.5,
            r: nr,
            temp: b.temp,
            phase: self.rng.range(0.0, 6.28),
        };
        self.blobs.push(Blob {
            x: b.x + ox,
            y: b.y + oy,
            vx: b.vx + push,
            vy: b.vy + oy.signum() * 0.5,
            r: nr,
            temp: b.temp,
            phase: self.rng.range(0.0, 6.28),
        });
    }

    fn enforce_count(&mut self) {
        while self.blobs.len() < self.target.saturating_sub(1).max(2) {
            self.split_largest();
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

/// Map temperature (0 cold .. 1 hot) to an RGB colour along a lava gradient.
fn temp_color(t: f32) -> (u8, u8, u8) {
    // stops: (pos, r, g, b)
    const STOPS: [(f32, f32, f32, f32); 5] = [
        (0.00, 28.0, 42.0, 130.0),   // deep blue
        (0.35, 120.0, 40.0, 170.0),  // purple
        (0.60, 215.0, 45.0, 110.0),  // magenta
        (0.82, 245.0, 110.0, 30.0),  // orange
        (1.00, 255.0, 205.0, 70.0),  // hot yellow
    ];
    let t = t.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < STOPS.len() && t > STOPS[i + 1].0 {
        i += 1;
    }
    let (p0, r0, g0, b0) = STOPS[i];
    let (p1, r1, g1, b1) = STOPS[(i + 1).min(STOPS.len() - 1)];
    let f = if p1 > p0 { (t - p0) / (p1 - p0) } else { 0.0 };
    (
        (r0 + (r1 - r0) * f) as u8,
        (g0 + (g1 - g0) * f) as u8,
        (b0 + (b1 - b0) * f) as u8,
    )
}

/// Pick a glyph and a brightness scale from the field magnitude. Softer edges
/// use lighter shade blocks so the surface looks molten rather than hard-edged.
fn glyph(field: f32) -> Option<(char, f32)> {
    if field >= 0.80 {
        Some(('█', 1.0))
    } else if field >= 0.52 {
        Some(('▓', 0.9))
    } else if field >= 0.34 {
        Some(('▒', 0.72))
    } else if field >= 0.20 {
        Some(('░', 0.5)) // outer glow
    } else {
        None
    }
}

/// Render the world into an ANSI truecolor frame (cursor is homed by caller).
fn render_ansi(world: &World, buf: &mut String) {
    buf.clear();
    buf.push_str("\x1b[H"); // cursor home
    let mut last: Option<(u8, u8, u8)> = None;
    for row in 0..world.h {
        for col in 0..world.w {
            let (field, temp) = world.sample(col as f32 + 0.5, row as f32 + 0.5);
            match glyph(field) {
                Some((ch, bright)) => {
                    let (r, g, b) = temp_color(temp);
                    let c = (
                        (r as f32 * bright) as u8,
                        (g as f32 * bright) as u8,
                        (b as f32 * bright) as u8,
                    );
                    if last != Some(c) {
                        buf.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                        last = Some(c);
                    }
                    buf.push(ch);
                }
                None => {
                    // background
                    if last != Some((0, 0, 0)) {
                        buf.push_str("\x1b[38;2;8;8;16m");
                        last = Some((0, 0, 0));
                    }
                    buf.push(' ');
                }
            }
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
            match glyph(field) {
                Some((ch, _)) => s.push(ch),
                None => s.push(' '),
            }
        }
        s.push('\n');
    }
    s
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
    // Solid dark background, cleared once.
    out.write_all(b"\x1b[48;2;8;8;16m\x1b[2J")?;
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
    if let Err(e) = run_interactive() {
        let _ = terminal::disable_raw_mode();
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
