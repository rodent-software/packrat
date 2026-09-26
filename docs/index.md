# packrat

A free and open source physical media backup utility.

![The packrat interactive guide](assets/preview.png)

## Quick start

Insert a disc, then run:

```sh
packrat
```

Running with no subcommand opens the interactive guide. It detects the disc,
shows what it thinks it is, and asks before writing anything. From there you
can:

- correct the **show** and **season** (packrat searches TVmaze for the name)
- set the **TV directory** and **movie directory** to the folders that already
  hold your show and movie folders, such as an existing Plex library
  (`/mnt/media/tv`, `/mnt/media/movies`). They are remembered between runs.
- include or skip individual files, and add **extras** (trailers/featurettes)
- press `Enter` to apply edits and `r` to rip

Files are written straight into the configured directory with Plex's naming:

```text
<tv directory>/<Show (Year)>/Season 01/<Show (Year>) - s01e01 - <Episode>.mkv
<movie directory>/<Title (Year)>/<Title (Year)>.mkv
```

Leave a directory empty to write plain MKVs into the current directory instead.

A typical session:

1. Run `packrat` with a disc inserted.
2. Type your TV directory (or movie directory) and press `Enter` — packrat
   saves it and looks the disc up on TVmaze.
3. Check the proposed files, unchecking anything you do not want (`Space`).
4. Press `r` to back up. Progress is shown per file, and `q` stops after the
   current file.

`Tab` moves between fields and `?` shows the full key list. The guide needs a
terminal; for scripting and headless use see [Advanced usage](advanced.md).
