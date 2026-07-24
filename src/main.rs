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

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
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
const EASE: f32 = 1.6; // radius easing rate (1/s) — smooths merges/melts/births
const COHESION: f32 = 6.0; // like-temp attraction that draws blobs together
const VISC: f32 = 2.5; // relative-velocity damping between close blobs
const MELT_PULL: f32 = 3.0; // how fast a dying blob flows toward its sink point
const SCROLL_ROWS_PER_SEC: f32 = 0.7; // default text scroll speed in file mode (rows/s)

// ---- configuration (CLI) --------------------------------------------------

/// A colour palette. Each entry maps a normalised intensity `i` (0 = faint
/// edge, 1 = dense core) to an RGB colour, keeping the whole scene in one
/// coherent family.
#[derive(Clone, Copy, PartialEq)]
enum Scheme {
    Magenta,
    Ember,
    Green,
    Ocean,
    Sunset,
    Ice,
    Rainbow,
    Mono,
}

impl Scheme {
    const ALL: [Scheme; 8] = [
        Scheme::Magenta,
        Scheme::Ember,
        Scheme::Green,
        Scheme::Ocean,
        Scheme::Sunset,
        Scheme::Ice,
        Scheme::Rainbow,
        Scheme::Mono,
    ];

    fn name(self) -> &'static str {
        match self {
            Scheme::Magenta => "magenta",
            Scheme::Ember => "ember",
            Scheme::Green => "green",
            Scheme::Ocean => "ocean",
            Scheme::Sunset => "sunset",
            Scheme::Ice => "ice",
            Scheme::Rainbow => "rainbow",
            Scheme::Mono => "mono",
        }
    }

    fn parse(s: &str) -> Option<Scheme> {
        Scheme::ALL.into_iter().find(|sc| sc.name() == s.to_lowercase())
    }

    fn next(self) -> Scheme {
        let i = Scheme::ALL.iter().position(|&s| s == self).unwrap_or(0);
        Scheme::ALL[(i + 1) % Scheme::ALL.len()]
    }

    /// Colour for a normalised intensity in [0, 1].
    fn color(self, i: f32) -> (u8, u8, u8) {
        let i = i.clamp(0.0, 1.0);
        match self {
            Scheme::Magenta => lerp_stops(
                &[
                    (0.00, 55.0, 15.0, 80.0),
                    (0.35, 120.0, 25.0, 150.0),
                    (0.65, 195.0, 35.0, 165.0),
                    (0.85, 240.0, 60.0, 155.0),
                    (1.00, 255.0, 150.0, 205.0),
                ],
                i,
            ),
            Scheme::Ember => lerp_stops(
                &[
                    (0.00, 40.0, 12.0, 6.0),
                    (0.35, 140.0, 35.0, 10.0),
                    (0.65, 220.0, 80.0, 15.0),
                    (0.85, 250.0, 150.0, 30.0),
                    (1.00, 255.0, 235.0, 130.0),
                ],
                i,
            ),
            Scheme::Green => lerp_stops(
                &[
                    (0.00, 0.0, 25.0, 8.0),
                    (0.40, 12.0, 90.0, 25.0),
                    (0.70, 35.0, 170.0, 55.0),
                    (0.90, 100.0, 220.0, 95.0),
                    (1.00, 205.0, 255.0, 185.0),
                ],
                i,
            ),
            Scheme::Ocean => lerp_stops(
                &[
                    (0.00, 8.0, 22.0, 65.0),
                    (0.40, 15.0, 70.0, 150.0),
                    (0.70, 20.0, 140.0, 190.0),
                    (0.90, 60.0, 200.0, 210.0),
                    (1.00, 185.0, 245.0, 240.0),
                ],
                i,
            ),
            Scheme::Sunset => lerp_stops(
                &[
                    (0.00, 45.0, 18.0, 75.0),
                    (0.35, 150.0, 40.0, 95.0),
                    (0.60, 220.0, 70.0, 60.0),
                    (0.85, 245.0, 140.0, 40.0),
                    (1.00, 255.0, 220.0, 120.0),
                ],
                i,
            ),
            Scheme::Ice => lerp_stops(
                &[
                    (0.00, 30.0, 40.0, 70.0),
                    (0.40, 60.0, 90.0, 150.0),
                    (0.70, 120.0, 170.0, 220.0),
                    (0.90, 190.0, 220.0, 245.0),
                    (1.00, 240.0, 250.0, 255.0),
                ],
                i,
            ),
            // Hue sweeps with intensity: cool faint edges to warm bright cores.
            Scheme::Rainbow => hsv(270.0 * (1.0 - i), 0.85, 0.35 + 0.6 * i),
            Scheme::Mono => {
                let v = (35.0 + 220.0 * i) as u8;
                (v, v, v)
            }
        }
    }
}

/// Piecewise-linear colour ramp over `(pos, r, g, b)` stops.
fn lerp_stops(stops: &[(f32, f32, f32, f32)], i: f32) -> (u8, u8, u8) {
    let mut k = 0;
    while k + 1 < stops.len() && i > stops[k + 1].0 {
        k += 1;
    }
    let (p0, r0, g0, b0) = stops[k];
    let (p1, r1, g1, b1) = stops[(k + 1).min(stops.len() - 1)];
    let f = if p1 > p0 { (i - p0) / (p1 - p0) } else { 0.0 };
    (
        (r0 + (r1 - r0) * f) as u8,
        (g0 + (g1 - g0) * f) as u8,
        (b0 + (b1 - b0) * f) as u8,
    )
}

/// HSV (h in degrees, s/v in [0,1]) to RGB.
fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = (h.rem_euclid(360.0)) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// Named glyph ramps (light -> heavy).
const RAMP_ASCII: [char; 10] = ['.', ':', '-', '=', '+', '*', 'o', '%', '#', '@'];
const RAMP_BLOCKS: [char; 4] = ['░', '▒', '▓', '█'];
const RAMP_DOTS: [char; 6] = ['·', ':', 'o', 'O', '0', '@'];
const RAMP_SHADE: [char; 8] = [',', ';', 'c', 'l', 'x', 'k', 'X', 'W'];

fn ramp_by_name(s: &str) -> Option<&'static [char]> {
    match s.to_lowercase().as_str() {
        "ascii" => Some(&RAMP_ASCII),
        "blocks" => Some(&RAMP_BLOCKS),
        "dots" => Some(&RAMP_DOTS),
        "shade" => Some(&RAMP_SHADE),
        _ => None,
    }
}

/// A bounded ring of the most-recently-seen words, fed live from stdin. Old
/// words are dropped once the total length exceeds `TEXT_CAP`, so a long or
/// endless stream (`tail -f …`) stays bounded. `version` bumps on every change
/// so the renderer knows when to rebuild.
const TEXT_CAP: usize = 8192; // ~chars retained
struct TextBuffer {
    words: std::collections::VecDeque<String>,
    total: usize,
    version: u64,
}

impl TextBuffer {
    fn new() -> Self {
        TextBuffer {
            words: std::collections::VecDeque::new(),
            total: 0,
            version: 0,
        }
    }
    fn push(&mut self, word: &str) {
        self.words.push_back(word.to_string());
        self.total += word.len() + 1;
        while self.total > TEXT_CAP && self.words.len() > 1 {
            if let Some(old) = self.words.pop_front() {
                self.total -= old.len() + 1;
            }
        }
        self.version += 1;
    }
    fn snapshot(&self) -> String {
        let mut s = String::with_capacity(self.total);
        for (i, w) in self.words.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(w);
        }
        s
    }
}

/// Spawn a thread that reads `reader` line-by-line into the shared ring buffer.
/// Works for both a one-shot pipe (`echo … | lavalamp`, read to EOF) and a live
/// feed (`tail -f log | lavalamp`).
fn spawn_stdin_reader(buf: Arc<Mutex<TextBuffer>>, reader: Box<dyn io::Read + Send>) {
    std::thread::spawn(move || {
        let mut r = io::BufReader::new(reader);
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            // Read raw bytes, not a UTF-8 String: a single non-UTF-8 byte (a
            // Latin-1 quote/dash, a subtitle-encoding artifact, …) must not
            // abort the whole feed. Decode lossily, like the file path does.
            match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break, // EOF or error: stop, keep what we have
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    if let Ok(mut b) = buf.lock() {
                        for w in text.split_whitespace() {
                            b.push(w);
                        }
                    }
                }
            }
        }
    });
}

/// If stdin is a pipe, hand back a reader for the piped data. On Unix this also
/// transplants the controlling terminal (`/dev/tty`) onto fd 0 first, so that
/// crossterm sees stdin as a tty and takes its normal, well-tested input path —
/// otherwise crossterm tries to open /dev/tty itself and fails to initialize
/// its event reader (notably on macOS). Returns None when stdin is a terminal
/// (nothing piped) so normal interactive use is untouched.
#[cfg(unix)]
fn take_piped_stdin() -> Option<Box<dyn io::Read + Send>> {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            return None; // stdin is the terminal: nothing to stream
        }
        let pipe = libc::dup(libc::STDIN_FILENO); // save the piped input
        if pipe < 0 {
            return None;
        }
        // Point stdin at the real terminal so crossterm can read keys/resize.
        // Prefer duplicating an already-open tty fd (stdout/stderr) — that's the
        // actual terminal *device*, which kqueue can poll. Opening "/dev/tty"
        // instead yields the controlling-terminal alias, which mio/kqueue can't
        // register on macOS (crossterm then fails to init its input reader).
        let tty_src = if libc::isatty(libc::STDOUT_FILENO) == 1 {
            libc::STDOUT_FILENO
        } else if libc::isatty(libc::STDERR_FILENO) == 1 {
            libc::STDERR_FILENO
        } else {
            -1
        };
        if tty_src >= 0 {
            libc::dup2(tty_src, libc::STDIN_FILENO);
        } else if let Ok(tty) =
            std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")
        {
            let tfd = tty.into_raw_fd();
            libc::dup2(tfd, libc::STDIN_FILENO);
            libc::close(tfd);
        }
        Some(Box::new(std::fs::File::from_raw_fd(pipe)))
    }
}

#[cfg(not(unix))]
fn take_piped_stdin() -> Option<Box<dyn io::Read + Send>> {
    use crossterm::tty::IsTty;
    if io::stdin().is_tty() {
        None
    } else {
        Some(Box::new(io::stdin()))
    }
}

/// Runtime configuration assembled from CLI arguments.
#[derive(Clone)]
struct Config {
    speed: f32,      // simulation time multiplier
    scheme: Scheme,  // colour palette
    density: f32,    // blob-count multiplier
    size: f32,       // blob-size multiplier
    fps: u32,        // target frame rate
    ramp: &'static [char], // glyph density ramp
    color: bool,     // truecolor vs. plain glyphs
    paused: bool,    // interactive pause
    seed: u64,       // PRNG seed
    text_speed: f32, // file-mode scroll speed in rows/sec (0 = static)
    text: Option<String>, // static text (file/PDF) the wax reveals instead of glyphs
    stdin_text: bool,     // read the revealed text live from stdin (`--text -` or a pipe)
    text_stream: Option<Arc<Mutex<TextBuffer>>>, // live text ring, when streaming
}

impl Default for Config {
    fn default() -> Self {
        Config {
            speed: 1.0,
            scheme: Scheme::Magenta,
            density: 1.0,
            size: 1.0,
            fps: 30,
            ramp: &RAMP_ASCII,
            color: true,
            paused: false,
            seed: 0x1234_5678,
            text_speed: SCROLL_ROWS_PER_SEC,
            text: None,
            stdin_text: false,
            text_stream: None,
        }
    }
}

const HELP: &str = "\
lavalamp — a full-color ASCII lava lamp for your terminal

USAGE:
    lavalamp [OPTIONS]

OPTIONS:
    -s, --speed <F>       Simulation speed multiplier (0.1–5)     [default: 1.0]
    -c, --scheme <NAME>   Color scheme (see below)                [default: magenta]
    -d, --density <F>     Blob-count multiplier (0.3–4)           [default: 1.0]
    -z, --size <F>        Blob-size multiplier (0.3–2)            [default: 1.0]
    -f, --fps <N>         Target frame rate (5–120)               [default: 30]
    -r, --ramp <NAME>     Glyph ramp: ascii, blocks, dots, shade  [default: ascii]
    -t, --text <FILE>     Reveal words from a text file or PDF in the wax
                          (use `-` to read live from stdin; piping in also works)
        --text-speed <F>  Scroll speed through a --text file, rows/sec (0 = static)
                          [default: 0.7]
        --mono            Monochrome (no color, just glyphs)
        --seed <N>        Fixed PRNG seed (reproducible)
    -h, --help            Print this help
    -V, --version         Print version

COLOR SCHEMES:
    magenta  ember  green  ocean  sunset  ice  rainbow  mono

INTERACTIVE KEYS:
    q / Esc     quit                 space   pause / resume
    + / -       faster / slower      c       cycle color scheme
    r           cycle glyph ramp     m       toggle monochrome

EXAMPLES:
    lavalamp --scheme ocean --speed 0.6
    lavalamp -c ember -d 1.5 -f 60
    lavalamp --scheme rainbow --ramp blocks
    lavalamp --text poem.txt --scheme ember
    tail -f app.log | lavalamp -c green        # live log in the wax
";

/// Load the words for `--text` from a file. Handles PDFs (extracted with a
/// pure-Rust parser) as well as plain-text files in any encoding — non-UTF-8
/// text is read lossily rather than rejected. PDF detection is by extension or
/// the `%PDF-` magic bytes, and a malformed PDF is reported instead of crashing.
fn load_text(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read '{path}': {e}"))?;
    let is_pdf = path.to_lowercase().ends_with(".pdf") || bytes.starts_with(b"%PDF-");
    if is_pdf {
        // pdf-extract can panic on malformed input, so isolate it.
        let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes))
            .map_err(|_| format!("could not parse PDF '{path}' (malformed or unsupported)"))?;
        let text = extracted
            .map_err(|e| format!("could not extract text from PDF '{path}': {e}"))?;
        if text.split_whitespace().next().is_none() {
            return Err(format!(
                "no selectable text in PDF '{path}' (it may be scanned/image-only)"
            ));
        }
        Ok(text)
    } else {
        // Any other file: treat as text, tolerating non-UTF-8 bytes.
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Fetch an option's value, supporting both `--key val` and `--key=val`.
fn take_val(
    args: &[String],
    i: &mut usize,
    inline: &Option<String>,
    name: &str,
) -> Result<String, String> {
    if let Some(v) = inline {
        Ok(v.clone())
    } else {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{name} requires a value"))
    }
}

/// Parse CLI args into a Config. Returns Err(message) on bad input, or
/// Ok(None) if a help/version message was printed and we should exit.
fn parse_args() -> Result<Option<Config>, String> {
    let mut cfg = Config { seed: seed(), ..Config::default() };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (a.clone(), None),
        };
        match key.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("lavalamp {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-s" | "--speed" => {
                cfg.speed = take_val(&args, &mut i, &inline, "--speed")?
                    .parse::<f32>()
                    .map_err(|_| "--speed must be a number")?
                    .clamp(0.1, 5.0);
            }
            "-c" | "--scheme" | "--colors" | "--palette" => {
                let v = take_val(&args, &mut i, &inline, "--scheme")?;
                cfg.scheme = Scheme::parse(&v)
                    .ok_or_else(|| format!("unknown scheme '{v}' (try --help)"))?;
            }
            "-d" | "--density" => {
                cfg.density = take_val(&args, &mut i, &inline, "--density")?
                    .parse::<f32>()
                    .map_err(|_| "--density must be a number")?
                    .clamp(0.3, 4.0);
            }
            "-z" | "--size" => {
                cfg.size = take_val(&args, &mut i, &inline, "--size")?
                    .parse::<f32>()
                    .map_err(|_| "--size must be a number")?
                    .clamp(0.3, 2.0);
            }
            "-f" | "--fps" => {
                cfg.fps = take_val(&args, &mut i, &inline, "--fps")?
                    .parse::<u32>()
                    .map_err(|_| "--fps must be an integer")?
                    .clamp(5, 120);
            }
            "-r" | "--ramp" | "--chars" => {
                let v = take_val(&args, &mut i, &inline, "--ramp")?;
                cfg.ramp = ramp_by_name(&v)
                    .ok_or_else(|| format!("unknown ramp '{v}' (ascii|blocks|dots|shade)"))?;
            }
            "-t" | "--text" => {
                let path = take_val(&args, &mut i, &inline, "--text")?;
                if path == "-" {
                    cfg.stdin_text = true; // read the text live from stdin
                } else {
                    let body = load_text(&path)?;
                    if body.split_whitespace().next().is_none() {
                        return Err(format!("'{path}' has no extractable words"));
                    }
                    cfg.text = Some(body);
                }
            }
            "--text-speed" => {
                cfg.text_speed = take_val(&args, &mut i, &inline, "--text-speed")?
                    .parse::<f32>()
                    .map_err(|_| "--text-speed must be a number")?
                    .clamp(0.0, 20.0);
            }
            "--mono" => cfg.color = false,
            "--seed" => {
                cfg.seed = take_val(&args, &mut i, &inline, "--seed")?
                    .parse::<u64>()
                    .map_err(|_| "--seed must be an integer")?;
            }
            // Internal test/diagnostic flags handled in main().
            "--test" | "--diag" => {}
            other => return Err(format!("unknown option '{other}' (try --help)")),
        }
        i += 1;
    }
    Ok(Some(cfg))
}

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

/// Remove ANSI escape sequences and control characters from a line of revealed
/// text. Live inputs (voice-to-text, progress meters, coloured logs) rewrite
/// their output with `\r`, cursor moves and CSI colour codes; if those bytes
/// reached our frame they'd corrupt the terminal (jitter) or show as gibberish
/// fragments like `[2K`. We keep only printable characters.
fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Escape sequence: skip `ESC [ ... final`, `ESC ] ... BEL/ST`, or a
            // lone `ESC <char>`.
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) {
                            break; // CSI final byte
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break; // BEL terminates OSC
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if !c.is_control() {
            out.push(c);
        }
        // other control chars are dropped
    }
    out
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
    dying: bool, // melting into a pool or a bigger blob, then remove
    sx: f32, // sink target — while dying the blob flows toward here as it melts
    sy: f32,
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
    cfg: Config,
    // Revealed text is laid out as a tall canvas of wrapped rows; the visible
    // window scrolls slowly down through it (with occasional random jumps) so
    // file mode flows dynamically instead of showing a static beginning.
    text_canvas: Vec<Vec<char>>, // wrapped rows of the source (empty = glyph ramp)
    text_version: u64,           // version of the streamed text last laid out
    auto_scroll: bool,           // scroll through the canvas (file mode)
    scroll: f32,                 // current top row into the canvas
    cycles: u32, // total pool absorptions — a liveness counter for --test
    merges: u32, // diagnostics
    splits: u32,
}

impl World {
    fn new(w: usize, h: usize, cfg: Config) -> Self {
        let mut world = World {
            w: w.max(1),
            h: h.max(1),
            max_r: MIN_RADIUS,
            target: 3,
            blobs: Vec::new(),
            pools: Vec::new(),
            rng: Rng::new(cfg.seed),
            t: 0.0,
            // File text scrolls; a live stdin stream flows on its own.
            auto_scroll: cfg.text.is_some(),
            cfg,
            text_canvas: Vec::new(),
            text_version: u64::MAX,
            scroll: 0.0,
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
        let size = self.cfg.size;
        self.max_r = (metric * 0.23 * size).clamp(MIN_RADIUS, ABS_MAX_RADIUS * size.max(1.0));
        let area = (self.w * self.h) as f32;
        // Bigger blobs cover more area, so scale the count down as size grows.
        let base = (area / 520.0) * self.cfg.density / (size * size);
        self.target = (base as usize).clamp(2, 40);
        self.rebuild_pools();
        self.rebuild_text_canvas();
    }

    /// Lay the source text out as a tall canvas of rows wrapped to the current
    /// width, preserving the file's own line breaks (blank lines become blank
    /// rows, so paragraphs stay separated and readable). The visible window
    /// scrolls down through this canvas; see `cell_glyph`. Text comes from a
    /// live stdin stream if present, else the static file.
    fn rebuild_text_canvas(&mut self) {
        self.text_canvas.clear();
        let source = if let Some(stream) = &self.cfg.text_stream {
            let b = stream.lock().unwrap();
            self.text_version = b.version;
            b.snapshot()
        } else if let Some(t) = &self.cfg.text {
            t.clone()
        } else {
            return;
        };
        let w = self.w.max(1);
        let mut canvas: Vec<Vec<char>> = Vec::new();
        for raw in source.split('\n') {
            // Strip ANSI escapes / control chars first: live inputs (e.g. a
            // voice-to-text feed) rewrite their line with escape sequences, and
            // emitting those into our frame would corrupt the terminal.
            let clean = sanitize_line(raw);
            let mut row: Vec<char> = Vec::new();
            for word in clean.split_whitespace() {
                let wl = word.chars().count();
                if !row.is_empty() && row.len() + 1 + wl > w {
                    canvas.push(std::mem::take(&mut row));
                }
                if wl > w {
                    // Word wider than the screen: hard-break it across rows.
                    for ch in word.chars() {
                        if row.len() >= w {
                            canvas.push(std::mem::take(&mut row));
                        }
                        row.push(ch);
                    }
                } else {
                    if !row.is_empty() {
                        row.push(' ');
                    }
                    row.extend(word.chars());
                }
            }
            canvas.push(row); // may be empty -> a blank separator row
        }
        // Drop a trailing empty row so the wrap-around isn't a long gap.
        if canvas.last().map(|r| r.is_empty()).unwrap_or(false) {
            canvas.pop();
        }
        if canvas.iter().all(|r| r.is_empty()) {
            return;
        }
        self.text_canvas = canvas;
        if self.scroll >= self.text_canvas.len() as f32 {
            self.scroll = 0.0;
        }
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
                sx: 0.0,
                sy: 0.0,
            });
        }
    }

    fn resize(&mut self, w: usize, h: usize) {
        self.w = w.max(1);
        self.h = h.max(1);
        self.recompute_layout();
        // Blobs keep their absolute cell positions — the scene does NOT stretch
        // with the window. Widening just reveals more room; narrowing clamps
        // blobs back inside. Radius is only ever clamped *down* to the new cap
        // (which shrinks for small windows); blobs never get stretched wider.
        let (w, h) = (self.w as f32, self.h as f32);
        for b in &mut self.blobs {
            b.rt = b.rt.min(self.max_r).max(MIN_RADIUS);
            b.r = b.r.min(self.max_r).max(MIN_RADIUS);
            b.x = b.x.clamp(b.r * 0.3, (w - b.r * 0.3).max(b.r * 0.3));
            b.y = b.y.clamp(0.5, h - 0.5);
        }
        // Keep the blob population sensible for the new size.
        self.enforce_count();
    }

    /// Advance the simulation by `real_dt` seconds of wall-clock time, scaled by
    /// the configured speed. Large or fast steps are split into sub-steps of at
    /// most ~20ms so the physics stays stable at any speed.
    fn update(&mut self, real_dt: f32) {
        // Rebuild the revealed text if the live stream has advanced (cheap; only
        // when new stdin data has arrived). Done even while paused so freshly
        // piped text still appears.
        if let Some(stream) = &self.cfg.text_stream {
            let v = stream.lock().unwrap().version;
            if v != self.text_version {
                self.rebuild_text_canvas();
            }
        }
        if self.cfg.paused {
            return;
        }
        let total = (real_dt * self.cfg.speed).min(0.25);
        let steps = (total / 0.02).ceil().max(1.0) as usize;
        let h = total / steps as f32;
        for _ in 0..steps {
            self.step(h);
        }
    }

    fn step(&mut self, dt: f32) {
        self.t += dt;
        let (w, h) = (self.w as f32, self.h as f32);

        // Scroll the revealed text slowly down through the canvas so file mode
        // flows, with occasional jumps to a random passage for variety. Paused
        // and speed settings carry through (this runs inside the stepped loop).
        if self.auto_scroll && self.cfg.text_speed > 0.0 && !self.text_canvas.is_empty() {
            let n = self.text_canvas.len() as f32;
            self.scroll += dt * self.cfg.text_speed;
            if self.scroll >= n {
                self.scroll -= n; // reached the end -> loop back to the beginning
            }
        }

        // Like-temperature blobs gently draw together and share momentum, so
        // they slide into a merge instead of snapping across a gap.
        self.cohesion(dt);

        for b in &mut self.blobs {
            // Radius glides toward its target so merges grow, splits shrink,
            // and absorbed blobs melt away smoothly rather than popping.
            b.r += (b.rt - b.r) * (EASE * dt).min(1.0);
            // A dying blob is melting into its sink target (a pool edge, or the
            // blob it's merging with): flow toward that point as it shrinks, so
            // it pours in rather than vanishing at its own position.
            if b.dying {
                let k = (MELT_PULL * dt).min(1.0);
                b.x += (b.sx - b.x) * k;
                b.y += (b.sy - b.y) * k;
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
        self.merge_pass();
        self.enforce_count();
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
    /// once a blob has sunk this far into the top or bottom band, and new blobs
    /// are born deep inside this band so their emergence is hidden until they
    /// rise/fall clear of the pool.
    fn band(&self) -> f32 {
        (self.max_r / ASPECT * 0.8).max(3.0)
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
                (b.sx, b.sy) = (b.x, -1.0); // flow up into the cold pool
                emit_cold += 1;
            } else if b.temp < 0.5 && b.y >= h - tb {
                b.dying = true;
                b.rt = 0.0;
                (b.sx, b.sy) = (b.x, h + 1.0); // flow down into the hot pool
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
    /// from the top. The blob is born *deep inside* the pool body — right at
    /// the screen edge, where the pool's own field masks it — and starts small,
    /// then grows and drifts out. So it isn't a speck that pops into view at the
    /// border; it oozes up (or down) out of the pool as the neck to it thins.
    /// No goo splash: the emergence itself is the effect.
    fn emit(&mut self, hot: bool) {
        let (w, h) = (self.w as f32, self.h as f32);
        let m = self.max_r.min(w * 0.4);
        let x = self.rng.range(m, (w - m).max(m + 0.1));
        let r = self.rng.range(self.max_r * 0.55, self.max_r * 0.95);
        let (y, vy, temp) = if hot {
            (h - 0.5, self.rng.range(-0.6, -0.2), HOT) // deep in the hot pool
        } else {
            (0.5, self.rng.range(0.2, 0.6), COLD) // deep in the cold pool
        };
        self.blobs.push(Blob {
            x,
            y,
            vx: self.rng.range(-0.8, 0.8),
            vy,
            // Starts small enough to stay hidden inside the pool, grows into
            // full size as it emerges.
            r: (r * 0.35).min(self.band() * ASPECT * 0.5),
            rt: r,
            temp,
            phase: self.rng.range(0.0, 6.28),
            dying: false,
            sx: 0.0,
            sy: 0.0,
        });
    }

    /// Merge overlapping like-temperature blobs. The *smaller* blob doesn't
    /// vanish at its own spot — it's marked dying with its sink point set to the
    /// larger blob's centre, so it flows into the larger one (which grows to the
    /// combined, area-conserving size) and pours in. Opposite-temperature blobs
    /// never fuse (they'd average to neutral buoyancy and stall mid-lamp); they
    /// just pass through each other goopily via the field.
    fn merge_pass(&mut self) {
        let n = self.blobs.len();
        for i in 0..n {
            for j in (i + 1)..n {
                if self.blobs[i].dying || self.blobs[j].dying {
                    continue;
                }
                let (a, b) = (&self.blobs[i], &self.blobs[j]);
                if (a.temp - b.temp).abs() >= 0.4 {
                    continue;
                }
                let dx = a.x - b.x;
                let dy = (a.y - b.y) * ASPECT;
                let dist = (dx * dx + dy * dy).sqrt();
                let combined = (a.r * a.r + b.r * b.r).sqrt();
                if dist >= (a.r + b.r) * 0.45 || combined > self.max_r * 1.02 {
                    continue;
                }
                // The larger absorbs the smaller.
                let (bi, si) = if a.r >= b.r { (i, j) } else { (j, i) };
                let big_a = self.blobs[bi].r * self.blobs[bi].r;
                let small = self.blobs[si].clone();
                let small_a = small.r * small.r;
                let sum = big_a + small_a;
                let (bx, by) = (self.blobs[bi].x, self.blobs[bi].y);
                {
                    let big = &mut self.blobs[bi];
                    big.temp = (big.temp * big_a + small.temp * small_a) / sum;
                    big.vx = (big.vx * big_a + small.vx * small_a) / sum;
                    big.vy = (big.vy * big_a + small.vy * small_a) / sum;
                    big.rt = combined; // grow to hold both, over the ease time
                }
                let sm = &mut self.blobs[si];
                sm.dying = true;
                sm.rt = 0.0;
                (sm.sx, sm.sy) = (bx, by); // flow into the larger blob
                self.merges += 1;
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

#[inline]
fn intensity(field: f32) -> f32 {
    ((field - FIELD_LO) / (FIELD_HI - FIELD_LO)).clamp(0.0, 1.0)
}

/// Pick a ramp glyph for a normalised intensity from the chosen glyph set.
fn ramp_char(ramp: &[char], i: f32) -> char {
    let idx = (i * (ramp.len() - 1) as f32).round() as usize;
    ramp[idx.min(ramp.len() - 1)]
}

/// The glyph for a filled cell: a letter from the revealed text if one sits
/// here, otherwise the density-ramp glyph. The text canvas scrolls, so the
/// visible row maps to `floor(scroll) + row` (wrapping around the canvas).
/// Word gaps inside a blob fall back to the ramp so the wax body stays solid.
fn cell_glyph(world: &World, col: usize, row: usize, dens: f32) -> char {
    let n = world.text_canvas.len();
    if n > 0 {
        let cr = (world.scroll as usize + row) % n;
        if let Some(&c) = world.text_canvas[cr].get(col) {
            // Never emit a space (gap) or a control char into the frame.
            if c != ' ' && !c.is_control() {
                return c;
            }
        }
    }
    ramp_char(world.cfg.ramp, dens)
}

/// Render the world into an ANSI frame (cursor is homed by caller), honouring
/// the configured colour scheme, glyph ramp and mono flag.
fn render_ansi(world: &World, buf: &mut String) {
    let cfg = &world.cfg;
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
            let ch = cell_glyph(world, col, row, dens);
            if cfg.color {
                // Density drives the glyph; colour follows density but drifts a
                // little with temperature to keep the wax alive within one hue.
                let ci = (dens + (temp - 0.5) * 0.30).clamp(0.0, 1.0);
                let c = cfg.scheme.color(ci);
                if last != Some(c) {
                    buf.push_str(&format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2));
                    last = Some(c);
                }
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
    String::from_iter(glyph_grid(world).chunks(world.w.max(1)).flat_map(|row| {
        row.iter().copied().chain(std::iter::once('\n'))
    }))
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
                cell_glyph(world, col, row, intensity(field))
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
    let mut world = World::new(w, h, Config { seed: 0x1234_5678, ..Config::default() });
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

/// Micro-benchmark the two hot paths (physics `update` + `render_ansi`) at a
/// range of terminal sizes, so we can see the CPU cost per frame and how it
/// scales. Prints implied CPU load at 30 and 60 fps.
fn run_bench() {
    // (cols, rows) — from a small pane up to a very large full-screen terminal.
    let sizes = [(80, 24), (120, 40), (200, 50), (300, 80), (400, 100)];
    println!(
        "{:>9} {:>6} {:>6} {:>9} {:>8} {:>9} {:>9} {:>8} {:>7} {:>7}",
        "size", "cells", "bodies", "update_ms", "mono_ms", "render_ms", "total_ms", "max_fps", "@30fps", "@60fps"
    );
    for (w, h) in sizes {
        let mut world = World::new(w, h, Config { seed: 0x1234_5678, ..Config::default() });
        for _ in 0..90 {
            world.update(1.0 / 30.0);
        }
        let bodies = world.blobs.len() + world.pools.len();
        let iters = 200;

        let t0 = Instant::now();
        for _ in 0..iters {
            world.update(1.0 / 30.0);
        }
        let update_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        let mut buf = String::with_capacity(w * h * 8);
        let t1 = Instant::now();
        for _ in 0..iters {
            render_ansi(&world, &mut buf);
        }
        let render_ms = t1.elapsed().as_secs_f64() * 1000.0 / iters as f64;

        // Time again with colour off to attribute cost: mono skips the per-cell
        // colour escape, so (render - mono) is the colour-formatting overhead
        // and mono is the field-sampling floor.
        world.cfg.color = false;
        let t2 = Instant::now();
        for _ in 0..iters {
            render_ansi(&world, &mut buf);
        }
        let mono_ms = t2.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        world.cfg.color = true;

        let total = update_ms + render_ms;
        let max_fps = if total > 0.0 { 1000.0 / total } else { 0.0 };
        println!(
            "{:>9} {:>6} {:>6} {:>9.3} {:>8.3} {:>9.3} {:>9.3} {:>8.0} {:>6.1}% {:>6.1}%",
            format!("{w}x{h}"),
            w * h,
            bodies,
            update_ms,
            mono_ms,
            render_ms,
            total,
            max_fps,
            total / (1000.0 / 30.0) * 100.0,
            total / (1000.0 / 60.0) * 100.0,
        );
    }
    println!(
        "\nCPU% is the fraction of one core used by the render loop at that fps\n\
         (the loop sleeps the rest of each frame). Frame buffer memory is ~w*h bytes."
    );
}

fn run_test() {
    let (w, h) = (80usize, 40usize);
    // Honour CLI options (scheme/text/etc.) so --test doubles as a headless
    // preview, but pin the seed for deterministic diagnostics.
    let mut cfg = parse_args().ok().flatten().unwrap_or_default();
    cfg.seed = 0x1234_5678;
    attach_text_stream(&mut cfg); // allow piping text in for a headless preview
    if cfg.text_stream.is_some() {
        std::thread::sleep(Duration::from_millis(150)); // let the reader drain stdin
    }
    let mut world = World::new(w, h, cfg);
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
    let mut wide = World::new(200, 12, Config { seed: 0x9, ..Config::default() });
    for _ in 0..100 {
        wide.update(0.033);
    }
    println!(
        "== wide-short 200x12: max_r {:.1} (should be small) ==",
        wide.max_r
    );

    // Resize check: widening must NOT stretch blobs or the arrangement.
    let mut rz = World::new(80, 40, Config { seed: 0x55, ..Config::default() });
    for _ in 0..200 {
        rz.update(0.033);
    }
    let before: Vec<(f32, f32, f32)> = rz.blobs.iter().map(|b| (b.x, b.y, b.r)).collect();
    let mr_before = rz.max_r;
    rz.resize(160, 40); // double the width, same height
    let stretched = rz
        .blobs
        .iter()
        .zip(&before)
        .any(|(b, &(ox, _, or))| (b.x - ox).abs() > 0.01 || (b.r - or).abs() > 0.01);
    println!(
        "== resize 80x40 -> 160x40: max_r {:.1} -> {:.1} (unchanged), positions/radii stretched: {} (should be false) ==",
        mr_before, rz.max_r, stretched
    );
}

fn run_interactive(cfg: Config) -> io::Result<()> {
    let mut out = io::stdout();
    terminal::enable_raw_mode()?;
    out.execute(terminal::EnterAlternateScreen)?;
    out.execute(cursor::Hide)?;
    // Clear the alternate screen using the terminal's own default background.
    out.write_all(b"\x1b[2J")?;
    out.flush()?;

    let (mut w, mut h) = terminal::size().map(|(c, r)| (c as usize, r as usize))?;
    let mut world = World::new(w, h, cfg);
    let mut buf = String::with_capacity(w * h * 4);
    let mut last = Instant::now();

    let result = (|| -> io::Result<()> {
        loop {
            // Drain all pending input/resize events without blocking.
            while event::poll(Duration::from_millis(0))? {
                match event::read()? {
                    Event::Key(KeyEvent { code, modifiers, .. }) => {
                        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
                        match code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('c') if ctrl => return Ok(()),
                            // Live tuning.
                            KeyCode::Char(' ') => world.cfg.paused = !world.cfg.paused,
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                world.cfg.speed = (world.cfg.speed * 1.25).min(5.0);
                            }
                            KeyCode::Char('-') | KeyCode::Char('_') => {
                                world.cfg.speed = (world.cfg.speed / 1.25).max(0.1);
                            }
                            KeyCode::Char('c') => world.cfg.scheme = world.cfg.scheme.next(),
                            KeyCode::Char('m') => world.cfg.color = !world.cfg.color,
                            KeyCode::Char('r') => {
                                world.cfg.ramp = next_ramp(world.cfg.ramp);
                                out.write_all(b"\x1b[2J")?; // ramps differ in width
                            }
                            _ => {}
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

            let frame = Duration::from_micros(1_000_000 / world.cfg.fps.max(1) as u64);
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

/// Stream the revealed text from stdin when asked (`--text -`) or when stdin is
/// a pipe and no static text was given. Keyboard input is unaffected because
/// crossterm reads it from /dev/tty when stdin isn't a tty.
fn attach_text_stream(cfg: &mut Config) {
    let _ = cfg.stdin_text;
    // Whenever stdin is a pipe, move the real terminal onto fd 0 so crossterm's
    // key/resize input works (see take_piped_stdin). This must happen even when
    // a static --text file/PDF is given (e.g. `foo | lavalamp --text bar.txt`).
    if let Some(reader) = take_piped_stdin() {
        // Stream the piped text only if no static source takes precedence.
        if cfg.text.is_none() {
            let buf = Arc::new(Mutex::new(TextBuffer::new()));
            spawn_stdin_reader(Arc::clone(&buf), reader);
            cfg.text_stream = Some(buf);
        }
        // else: static --text wins; drop the pipe reader (closes the saved fd).
    }
}

/// Cycle through the named glyph ramps (for the interactive `r` key).
fn next_ramp(cur: &'static [char]) -> &'static [char] {
    let ramps: [&'static [char]; 4] = [&RAMP_ASCII, &RAMP_BLOCKS, &RAMP_DOTS, &RAMP_SHADE];
    let i = ramps.iter().position(|r| std::ptr::eq(*r, cur)).unwrap_or(0);
    ramps[(i + 1) % ramps.len()]
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.iter().any(|a| a == "--test") {
        run_test();
        return;
    }
    if raw.iter().any(|a| a == "--diag") {
        run_diag();
        return;
    }
    if raw.iter().any(|a| a == "--bench") {
        run_bench();
        return;
    }
    match parse_args() {
        Ok(Some(mut cfg)) => {
            attach_text_stream(&mut cfg);
            if let Err(e) = run_interactive(cfg) {
                let _ = terminal::disable_raw_mode();
                eprintln!("lavalamp: error: {e}");
                std::process::exit(1);
            }
        }
        Ok(None) => {} // --help / --version already printed
        Err(msg) => {
            eprintln!("lavalamp: {msg}");
            std::process::exit(2);
        }
    }
}
