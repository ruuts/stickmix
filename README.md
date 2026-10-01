# StickMix

**Music in. USB out.** An experimental automatic USB preparer for the original
Pioneer XDJ-RX, with native Linux, Windows and macOS builds.

Open StickMix, choose your MP3 folder or M3U playlist, choose a filename style,
and select your USB. StickMix analyzes the audio and writes the conventional
Pioneer device library (`export.pdb`), monochrome/color waveform analysis files,
estimated BPM/key and beatgrid, artwork, and the original tagged MP3 files.
Rekordbox does not need to be installed or operated.

This is a **local-audio tool**. It does not download Spotify's protected audio.
Use music you own or have permission to download. A Spotify link alone cannot
produce a playable USB in this version. Spotify metadata import and authorized
music-source adapters are future work; no Spotify credentials are required.

## Using a portable build

Extract the entire ZIP. Double-click `Start StickMix.command` on macOS,
`Start StickMix.cmd` on Windows, or run `./stickmix` in a Linux terminal.
Drag/paste a music folder or a local `.m3u8` playlist into the prompt.
An existing `playlist.m3u8` inside a selected folder takes precedence over
alphabetical file order. Repeated entries keep their order without copying
the same recording twice.

Choose a USB explicitly. Internal, read-only and detected system disks are
excluded. Existing FAT32/MBR sticks can be used without formatting; a foreign
Pioneer library requires a blank stick or formatting. When formatting is needed,
the program displays the selected device and requires `ERASE DEVICE_ID`.
This destroys **all files and partitions on that USB**. Formatting also needs
the operating system's administrator authorization. Sources located on that
same USB are refused before formatting.

Linux uses `lsblk`, `udisksctl`, `pkexec`, `sfdisk`, `mkfs.fat`, `udevadm`,
`findmnt`, `umount` and `sync`. On Arch these come from udisks2, polkit,
util-linux, dosfstools and systemd. Windows uses built-in PowerShell storage
commands; macOS uses `diskutil` and `plutil`. Windows creates a FAT32 partition
of **up to 30 GiB**, leaving larger sticks' remaining capacity unused.

Your originals and their ID3 tags are copied byte-for-byte, with no encoding
or gain change. Names on the USB are sanitized for FAT/Windows and include a
short content hash to prevent collisions. Displayed artist/title metadata comes
from ID3 tags. Analysis is cached in `~/Music/StickMix/Cache`; reopening resumes
copying and reuses analysis. `STICKMIX_CACHE_DIR` can override that folder.
Exporting publishes the database after audio/analysis, and requests safe ejection.

## Current limits and verification

- **Hardware compatibility is experimental.** The underlying format writer is
  derived from an exporter tested by its author on an XDJ-XZ. StickMix itself has
  not yet been tested on an XDJ-RX or a physical USB; native build tests do not
  verify real-device formatting/ejection. Test browsing, loading and waveforms
  on your panel before using it for a set.
- BPM/key are estimates. The constant-tempo grid estimates phase from low-band
  onsets; it does not detect reliable bar downbeats or variable tempo. There is
  no promise that automatic beat sync is correct. User cue points/hot cues are
  not generated.
- Accepts local MP3s at 44.1/48 kHz with one or two channels, up to 15 minutes.
  No transcode/conversion is applied; unsupported inputs stop before formatting.
- One playlist per export. Re-export replaces the current StickMix playlist.
  Older copied audio is retained, so removed/reordered tracks can use extra space.
  Foreign libraries and symlinked export paths are refused.
- This writes the **conventional Device Library**, not OneLibrary. It targets the
  original XDJ-RX, not newer equipment that requires OneLibrary.
- Builds are unsigned; Windows/macOS may show normal publisher/security checks.

The tests use only a generated synthetic tone and validate exported PDB and
ANLZ files using the independent `rekordcrate` reader. They also check byte-exact
audio copying, resume, playlist duplicates, filename/path handling, and system
disk exclusion. No user's music, queues or credentials are published.

## Development

```sh
cargo build --release --locked
cargo test --locked
cargo test -p sustain-pioneer --locked
cargo clippy --package stickmix --all-targets -- -D warnings
./target/release/stickmix prepare /path/to/music --output /path/to/new/test-folder
./target/release/stickmix drives
```

`prepare` writes a local folder and never formats/ejects a drive. `usb SOURCE
--device DEVICE_ID` runs the automatic USB flow with the same erase confirmation
as the wizard. Test in a throwaway folder first. The manual GitHub Actions build
creates native Windows x64, macOS Intel/Apple Silicon and Linux x64 ZIPs.

## License and format references

GPL-3.0-or-later; see `LICENSE`. The analysis and Pioneer writer crates are
vendored from [Sustain](https://github.com/open-sustain/sustain) at the revision
documented in `vendor/sustain/PROVENANCE.md`. Original copyrights are preserved;
its DSP component retains MIT/Apache-2.0 notices. Portable packages include
corresponding sources and dependency license notices.

Format references: [Deep Symmetry DeviceSQL analysis](https://djl-analysis.deepsymmetry.org/rekordbox-export-analysis/exports.html),
[rekordcrate](https://github.com/holzhaus/rekordcrate),
[Pioneer XDJ-RX specifications](https://www.pioneerdj.com/ro/news/2015/xdj-rx/).
Independent project, unaffiliated with Spotify or AlphaTheta/Pioneer DJ.
