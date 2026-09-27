# Advanced usage

Running `packrat` with no subcommand opens the [interactive guide](index.md),
which covers the common case. The commands below are useful for scripting,
debugging, and unusual discs. Every one of them is also documented by
`packrat <command> --help`.

## Inspect a disc without ripping it

```sh
# Title/chapter structure (read-only)
packrat probe /run/media/$USER/DRAGON_BALL_S1_D1

# What it thinks the disc is, and how Play-All titles split into episodes
packrat plan /run/media/$USER/DRAGON_BALL_S1_D1

# Match on TVmaze and print the proposed Plex layout (read-only)
packrat identify /run/media/$USER/DRAGON_BALL_S1_D1 --library /mnt/media
```

## Rip and split

```sh
# Losslessly remux one title (optionally a chapter range) to MKV
packrat rip /run/media/$USER/DRAGON_BALL_S1_D1 --title 11 --chapters 1-5 \
  --out ep1.mkv

# Auto-split every "Play All" title into per-episode MKVs
packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out --dry-run
packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out

# ...or write directly into a Plex library, named from TVmaze metadata
packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out \
  --library /mnt/media --dry-run
```

`--dry-run` prints the plan without remuxing anything, which is the quickest
way to check what a command will do.

## Destinations and preferences

`--tv-dir` and `--movie-dir` point at the folders that already hold show and
movie folders, so an existing library can be used as-is:

```sh
packrat split /run/media/$USER/DRAGON_BALL_S1_D1 --out-dir out \
  --tv-dir /mnt/dvd/media/tv --dry-run
```

Set them once on the [interactive guide](index.md)'s settings screen — press
`s`, or set them during first-run onboarding — and they are saved to
`~/.config/packrat/config.toml` (or `$XDG_CONFIG_HOME/packrat/config.toml`).
Headless commands pick the saved values up automatically, so this is enough:

```sh
packrat watch --include-extras
```

`--library <root>` is shorthand for `--tv-dir <root>/TV Shows --movie-dir
<root>/Movies`, for libraries laid out with Plex's conventional category
folders. An explicit flag wins over `--library`, which wins over a saved
preference.

The settings screen also has **Eject when done**, which opens the tray once a
rip finishes without failures. Press `x` to eject on demand: from a plan it
opens the loaded disc's tray and returns to the picker, and on the result
screen it ejects and goes straight back to configure another disc. While a rip
is running the tray is locked (where the OS supports it) so it cannot be opened
mid-read.

## Encrypted discs

CSS-encrypted discs need `--features dvdcss` at build time (see
[Development](development.md)). Read the VOB data from the raw device while
using the mount for the IFO structure:

```sh
packrat split /run/media/$USER/DVD_LABEL --device /dev/sr0 \
  --out-dir out --library /mnt/media
```

## Drives and watch mode

```sh
# List optical drives and any disc in them
packrat drives

# Watch for a disc and back it up automatically (Ctrl-C to stop)
packrat watch --library /mnt/media --include-extras

# With more than one drive attached, pin the one to watch
packrat watch --device /dev/sr1 --library /mnt/media

# --once processes a present disc and exits; --dry-run previews
packrat watch --once --dry-run --library /mnt/media
```

Without `--device`, `watch` prefers the drive last used in the interactive
guide and otherwise takes the first one holding a disc.

In the guide itself the drive list is selectable: `Tab` moves between the list
and the manual path box, `↑`/`↓` pick a drive, `Enter` loads it and `r`
rescans. Press `d` while reviewing a plan to switch to another drive without
restarting.

## How output is written

Rips are written to `<file>.partial` and renamed only once the file is
complete, so a media server never indexes a half-written episode.

Remuxing copies MPEG-2 video and AC-3/DTS audio bit-for-bit — there is no
re-encode. The output carries the source `Chapter` timeline and (once the
duration is known) a container duration.

Short titles (trailers, featurettes) are skipped by default; pass
`--include-extras` to put them under `<show>/Other/`. Titles that look like a
second copy of the same episodes (with and without the opening/ending) are
collapsed automatically so a season is not ripped twice.
