// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 StickMix contributors
mod drives;
mod export;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(version, about = "StickMix — music in, USB out")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Build a complete Pioneer USB image in a new local folder (no formatting).
    Prepare {
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "number-artist-title")]
        naming: export::Naming,
    },
    /// Show eligible USB drives. Internal/system disks are excluded.
    Drives,
    /// Export local MP3s directly to an eligible USB drive.
    Usb {
        source: PathBuf,
        #[arg(long)]
        device: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "number-artist-title")]
        naming: export::Naming,
    },
    #[command(hide = true)]
    FormatHelper { device: String, fingerprint: String },
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut value = String::new();
    ensure!(io::stdin().read_line(&mut value)? > 0, "Input closed");
    Ok(value.trim().trim_matches('"').to_owned())
}

fn music_path(value: &str) -> PathBuf {
    let literal = PathBuf::from(value);
    if literal.exists() || cfg!(windows) {
        return literal;
    }
    // macOS/Linux terminal drag-and-drop escapes spaces and punctuation.
    let value = value.trim_matches('\'');
    let mut decoded = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                decoded.push(next);
            } else {
                decoded.push(c);
            }
        } else {
            decoded.push(c);
        }
    }
    PathBuf::from(decoded)
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("STICKMIX_CACHE_DIR") {
        return Ok(path.into());
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("Home folder missing")?;
    Ok(PathBuf::from(home).join("Music/StickMix/Cache"))
}

fn usb(source: &Path, device: &str, name: Option<String>, naming: export::Naming) -> Result<()> {
    // Validate and analyze every input before offering to erase anything.
    let prepared = export::prepare(source, name, naming, &cache_dir()?)?;
    let mut drive = drives::find(device)?;
    if drive.compatible_filesystem() && drive.mount.is_none() {
        drive = drives::mount(&drive)?;
    }
    if let Some(mount) = drive.mount.as_ref().and_then(|p| p.canonicalize().ok()) {
        ensure!(
            !std::env::current_exe()?.canonicalize()?.starts_with(mount),
            "Run StickMix from your computer, not from the USB you are preparing"
        );
    }
    // Do not keep a USB busy because this terminal was started inside it.
    // Prepared input paths and the discovered mount path are absolute.
    std::env::set_current_dir(std::env::temp_dir())?;
    println!("\nSelected USB: {}", drive.description());
    let existing_library = drive
        .mount
        .as_ref()
        .is_some_and(|p| p.join("PIONEER").exists() && !p.join(".stickmix.json").exists());
    if !drive.compatible() || existing_library {
        ensure!(
            !drive.mount.as_ref().is_some_and(|p| prepared.source_on(p)),
            "Your source music is on this USB. Move it to the computer before formatting."
        );
        println!(
            "This USB needs FAT32 and an MBR partition table.\nFormatting erases ALL files and partitions on this USB."
        );
        if cfg!(windows) {
            println!(
                "Windows formatting creates a FAT32 partition of up to 30 GiB; extra capacity stays unused."
            );
        }
        let confirmation = format!("ERASE {}", drive.id);
        let answer = prompt(&format!(
            "Type {confirmation} to format it, or press Enter to cancel:\n> "
        ))?;
        ensure!(
            answer == confirmation,
            "Formatting cancelled; USB unchanged"
        );
        drive = drives::format(&drive)?;
    }
    ensure!(drive.compatible(), "USB is not FAT32/MBR after formatting");
    export::write_guarded(
        &prepared,
        drive.mount.as_deref().context("USB is not mounted")?,
        || drive.verify(),
    )?;
    drives::flush(&drive)?;
    println!(
        "\nExport complete: {} tracks. USB eject requested.\nFirst use: check browsing, waveform display and beat alignment on your XDJ-RX.",
        prepared.tracks.len()
    );
    Ok(())
}

fn wizard() -> Result<()> {
    println!(
        "\n  STICKMIX\n  Music in. USB out.\n\nPrepares MP3 playlists for the original Pioneer XDJ-RX.\n"
    );
    let source = prompt("Drop or paste your MP3 folder or M3U playlist here:\n> ")?;
    if source.starts_with("https://open.spotify.com/") || source.starts_with("spotify:") {
        bail!(
            "This version prepares local MP3 files. A Spotify link does not provide downloadable audio. Paste your music folder or M3U playlist instead."
        );
    }
    let name = prompt("Playlist name (Enter to use the folder/playlist name):\n> ")?;
    println!("\nFilename style:\n  1. 001 - Artist - Title\n  2. Artist - Title\n  3. 001 - Title");
    let naming = match prompt("Choose 1, 2 or 3 (Enter = 1): ")?.as_str() {
        "" | "1" => export::Naming::NumberArtistTitle,
        "2" => export::Naming::ArtistTitle,
        "3" => export::Naming::NumberTitle,
        _ => bail!("Choose 1, 2 or 3"),
    };
    let devices = drives::list()?;
    ensure!(
        !devices.is_empty(),
        "No eligible USB drive found. Insert your stick and reopen StickMix."
    );
    println!("\nUSB sticks:");
    for (index, drive) in devices.iter().enumerate() {
        println!("  {}. {}", index + 1, drive.description());
    }
    let index: usize = prompt("Choose your USB number: ")?
        .parse()
        .context("Enter a USB number")?;
    let drive = devices
        .get(index.checked_sub(1).context("Invalid USB number")?)
        .context("Invalid USB number")?;
    usb(
        &music_path(&source),
        &drive.id,
        (!name.is_empty()).then_some(name),
        naming,
    )
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => wizard(),
        Some(Command::Prepare {
            source,
            output,
            name,
            naming,
        }) => {
            let prepared = export::prepare(&source, name, naming, &cache_dir()?)?;
            export::write(&prepared, &output)
        }
        Some(Command::Drives) => {
            for drive in drives::list()? {
                println!("{}", drive.description());
            }
            Ok(())
        }
        Some(Command::Usb {
            source,
            device,
            name,
            naming,
        }) => usb(&source, &device, name, naming),
        Some(Command::FormatHelper {
            device,
            fingerprint,
        }) => drives::format_helper(&device, &fingerprint),
    }
}

fn main() {
    let cli = Cli::parse();
    let interactive = cli.command.is_none();
    let result = run(cli);
    if let Err(error) = &result {
        eprintln!("\nStopped: {error:#}\nReopen StickMix to resume. Completed analysis is cached.");
    }
    if interactive {
        let _ = prompt("\nPress Enter to close. ");
    }
    if result.is_err() {
        std::process::exit(1);
    }
}
