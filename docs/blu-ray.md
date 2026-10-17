# Blu-ray and AACS keys

Blu-ray scanning, AACS decryption, and remuxing are implemented for H.264
titles read from a mounted disc. The remuxer assembles H.264 access units,
TrueHD frames, and DTS-HD MA core plus extension frames before writing
Matroska, so PES boundaries no longer have to line up with codec frames.

Blu-ray discs use **AACS** (Advanced Access Content System), not the CSS
protection used by DVDs. AACS encrypts a disc's `.m2ts` streams with a per-disc
key. packrat does not bundle, mirror, or download AACS keys. To read an
encrypted Blu-ray it needs a `KEYDB.cfg`, the same user-supplied key database
used by libaacs and VLC.

## Why a KEYDB is needed

The AACS key chain is:

```text
Device Key + MKB
        -> Processing Key
        -> Media Key
        -> Volume Unique Key (VUK)  (with the drive's Volume ID)
        -> Title Key                (with AACS/Unit_Key_RO.inf)
        -> decrypted content
```

The optical drive can supply the AACS Volume ID, but it does not hold the player
device key or the VUK. Those come from licensed player software or a key
database. There is no CSS-style crack to fall back on, and packrat cannot derive
the key from the disc or the drive alone.

A `KEYDB.cfg` maps the disc's ID to its VUK. The disc ID is the SHA-1 of
`AACS/Unit_Key_RO.inf`; the Blu-ray backend displays it when it needs a key. A
legacy entry looks like:

```text
0x<40-hex disc ID> = V <32-hex VUK> ; optional label
```

The extended `| DISCID |` / `| VUK |` form is also accepted. Some databases also
carry device keys and Key Conversion Data, which packrat can use to derive a
VUK for discs whose MKB requires it. A VUK entry is the common and simplest
case.

Without a matching entry, the Blu-ray backend can still read the disc's
unencrypted filesystem and list its titles, chapters, and streams. It cannot
decrypt the `.m2ts` content.

## Where to get a KEYDB

The [FindVUK Online Database](https://fvonline-db.bplaced.net/) publishes a
community-maintained `KEYDB.cfg`. packrat is not affiliated with it and cannot
vouch for its contents, availability, or completeness. Download `keydb.cfg`
from that site.

The easiest way to install it is the **Import KEYDB…** button on packrat's
settings screen. It opens a file browser, copies the selected file into
packrat's own configuration directory, and uses that copy for every later
backup. A later import replaces the copy.

On Linux the imported copy is, for example,
`~/.config/packrat/KEYDB.cfg`. The Blu-ray backend also recognizes the standard
libaacs search locations:

- `$XDG_CONFIG_HOME/aacs/KEYDB.cfg`
- `aacs/KEYDB.cfg` under each directory in `$XDG_CONFIG_DIRS`
- `~/.config/aacs/KEYDB.cfg`
- `~/Library/Preferences/aacs/KEYDB.cfg` on macOS

[libaacs](https://www.videolan.org/developers/libaacs.html), the reference AACS
implementation, does not include any key or certificate either; the database is
supplied by the user.

Key databases are community-maintained. A missing, stale, or mismatched entry
will leave a disc unreadable even though the filesystem itself is fine.

## Legal

AACS key distribution is legally restricted in some jurisdictions. packrat does
not bundle, mirror, or download a `KEYDB.cfg`, and it does not provide keys. It
only reads a database you supply. Obtaining and using that database is your
decision and responsibility; use it only with discs you own and follow your
local laws. This is the same posture packrat takes for `libdvdcss`: the
circumvention material is not part of the project.

## Without a KEYDB

If you do not have a key database, decrypt the disc once with a tool that has
its own AACS support — for example MakeMKV's **Backup** with **Decrypt video
files** enabled — then open the decrypted `BDMV` folder in packrat. That path
needs no keys.

UHD Blu-ray (AACS 2.0) and BD+ are separate protection schemes and are not
covered by this key path.

## Current limits

- Video is remuxed for H.264 titles. HEVC, VC-1 and MPEG-2 Blu-ray video are
  not handled yet.
- A rip takes the whole playlist. Chapter-range jobs (a TV episode that is one
  chapter range inside a longer playlist) are not supported yet.
- The disc must be mounted so `BDMV/` is readable. A raw device with no mount
  is not handled yet.
- HDMV playlists are backed up. BD-J-only discs, and BD-J menus, are not.
- Forced-subtitle flags are not set on the remuxed subtitle tracks yet.
- Dolby Vision, 3D/MVC, and other secondary-video layouts are not remuxed yet.
- DTS-HD MA is carried as the core frame plus its extension in one block.
  A DTS track whose core is missing cannot be decoded.
