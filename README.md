# lavalamp

A full-color ASCII lava lamp for your terminal, in Rust — in the spirit of
`asciiquarium` and `cmatrix`. It resizes freely with the window.

![it goops](https://example.invalid/placeholder)

## Install

Build and install the `lavalamp` binary onto your PATH:

```sh
cargo install --path .
```

Then run it from anywhere:

```sh
lavalamp
```

Or grab a standalone binary without installing:

```sh
cargo build --release   # -> target/release/lavalamp
```

## Run

```sh
lavalamp
```

Press `q`, `Esc`, or `Ctrl-C` to quit. Resize the window any time — the scene
reflows and rescales live.

Preview the shape without a live terminal:

```sh
cargo run --release -- --test
```

## How it works

- **Metaballs.** Every wax body (free blobs + the top/bottom pools + transient
  goo droplets) contributes a smooth, finite-support field. Fields add, so
  bodies fuse into goopy necks as they approach — merges and splits look
  organic for free.
- **Temperature is blended, never overwritten.** Each body carries a
  temperature; a cell's color is the field-weighted average of nearby bodies.
  Two blobs that touch *mix* their heat instead of erasing each other, so a
  blob can't get stranded neutral-buoyant in the middle.
- **The cycle.** A hot pool sits at the bottom, a cold pool hangs from the top.
  Heat exchange bites only in those end zones; the middle is near-adiabatic, so
  momentum carries a blob through without equilibrating. Hot wax rises, cools
  at the top, sinks, reheats at the bottom.
- **Sizing.** Maximum blob radius derives from `min(width, height·2)`, so a
  wide-but-short window stays limited by its height (no giant blobs), with a
  hard upper cap and free shrinking for small windows.
- **Goo.** Merges squeeze out a little splash; splits leave a stringy filament.
  Both are shrinking droplets fed back into the same field.

Full RGB truecolor output; a 24-bit-color terminal is recommended.
