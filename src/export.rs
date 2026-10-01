// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 StickMix contributors
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use indicatif::{ProgressBar, ProgressStyle};
use lofty::{
    file::{AudioFile, FileType, TaggedFileExt},
    prelude::Accessor,
    probe::Probe,
    tag::ItemKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use sustain_analysis::{AnalysisOptions, Analyzer};
use sustain_domain::{MusicalKey, WaveformSegment, WaveformSegments};
use sustain_pioneer::{
    AnlzInput, ArtworkSet, PioneerFileType, PioneerPlaylist, PioneerTrack, anlz, path_hash, pdb,
};

#[derive(Clone, Copy, ValueEnum)]
#[allow(clippy::enum_variant_names)]
pub enum Naming {
    NumberArtistTitle,
    ArtistTitle,
    NumberTitle,
}

#[derive(Serialize, Deserialize)]
struct Analysis {
    version: u32,
    bpm: Option<f32>,
    key: Option<String>,
    preview: Vec<[u8; 4]>,
    detail: Vec<[u8; 4]>,
    preview_step: f32,
    detail_step: f32,
}

pub struct Prepared {
    pub tracks: Vec<PioneerTrack>,
    sources: Vec<PathBuf>,
    hashes: Vec<String>,
    analyses: Vec<(Vec<u8>, Vec<u8>)>,
    artwork: ArtworkSet,
    playlist: PioneerPlaylist,
}

impl Prepared {
    pub fn source_on(&self, mount: &Path) -> bool {
        mount
            .canonicalize()
            .is_ok_and(|root| self.sources.iter().any(|path| path.starts_with(&root)))
    }
}

pub fn safe_name(value: &str) -> String {
    let mut result: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    result = result.trim_matches([' ', '.']).to_string();
    while result.len() > 150 {
        result.pop();
    }
    let first = result.split('.').next().unwrap_or("").to_ascii_uppercase();
    if result.is_empty() {
        result = "Untitled".into();
    }
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&first.as_str())
    {
        result.insert(0, '_');
    }
    result
}

fn spinner(message: &str) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(
        ProgressStyle::with_template("{spinner:.yellow} {msg} [{elapsed_precise}]")
            .expect("static spinner template")
            .tick_strings(&["·", "✢", "✳", "✶", "✳", "✢"]),
    );
    bar.set_message(message.to_string());
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn inputs(source: &Path) -> Result<Vec<PathBuf>> {
    let source = source
        .canonicalize()
        .context("Music folder or playlist not found")?;
    let playlist = if source.is_dir() {
        let candidate = source.join("playlist.m3u8");
        candidate.is_file().then_some(candidate)
    } else {
        Some(source.clone())
    };
    let mut files = Vec::new();
    if let Some(playlist) = playlist {
        ensure!(
            matches!(
                playlist.extension().and_then(|s| s.to_str()),
                Some("m3u" | "m3u8")
            ),
            "Choose an MP3 folder or M3U playlist"
        );
        let parent = playlist.parent().context("Playlist has no folder")?;
        for line in fs::read_to_string(&playlist)?
            .trim_start_matches('\u{feff}')
            .lines()
        {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            ensure!(
                !line.contains("://"),
                "Network playlist entries are not supported: {line}"
            );
            let path = parent
                .join(line)
                .canonicalize()
                .with_context(|| format!("Missing playlist file: {line}"))?;
            ensure!(
                path.starts_with(parent),
                "Playlist entry is outside its music folder: {line}"
            );
            files.push(path);
        }
    } else {
        for entry in walkdir::WalkDir::new(&source)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
        {
            let entry = entry?;
            if entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
            {
                files.push(entry.into_path());
            }
        }
        files.sort();
    }
    ensure!(!files.is_empty(), "No MP3s found");
    for path in &files {
        ensure!(
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mp3")),
            "Only local MP3 files are supported: {}",
            path.display()
        );
    }
    Ok(files)
}

fn pack(tier: &WaveformSegments) -> Vec<[u8; 4]> {
    tier.segments
        .iter()
        .map(|s| [s.amplitude, s.low_band, s.mid_band, s.high_band])
        .collect()
}
fn unpack(data: &[[u8; 4]], step: f32) -> WaveformSegments {
    WaveformSegments {
        segment_duration_ms: step,
        segments: data
            .iter()
            .map(|s| WaveformSegment {
                amplitude: s[0],
                low_band: s[1],
                mid_band: s[2],
                high_band: s[3],
            })
            .collect(),
    }
}

// Estimate a beat phase from repeated low-frequency onsets. This is an
// automatic constant-tempo grid, not a claim of downbeat/variable-tempo accuracy.
fn beat_offset(analysis: &Analysis) -> u32 {
    let Some(bpm) = analysis.bpm else {
        return 0;
    };
    let period = 60_000.0 / bpm;
    let mut bins = [0.0f32; 100];
    for (index, pair) in analysis.detail.windows(2).enumerate() {
        let rise = (f32::from(pair[1][1]) - f32::from(pair[0][1])).max(0.0);
        let phase = ((index + 1) as f32 * analysis.detail_step) % period;
        let bin = ((phase / period * 100.0) as usize).min(99);
        bins[bin] += rise * rise;
    }
    let (index, weight) = bins
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap_or((0, &0.0));
    if *weight <= bins.iter().sum::<f32>() / 100.0 * 2.0 {
        return 0;
    }
    (index as f32 / 100.0 * period).round() as u32
}

fn aligned_dat(input: &AnlzInput, offset_ms: u32) -> Vec<u8> {
    let mut data = anlz::dat_bytes(input);
    let mut position = 28usize;
    while position + 12 <= data.len() {
        let length = u32::from_be_bytes(
            data[position + 8..position + 12]
                .try_into()
                .expect("section size"),
        ) as usize;
        if &data[position..position + 4] == b"PQTZ" {
            let mut section = data[position..position + 24].to_vec();
            let mut count = 0u32;
            for beat in data[position + 24..position + length].as_chunks::<8>().0 {
                let time = u32::from_be_bytes(beat[4..8].try_into().expect("beat time"))
                    .saturating_add(offset_ms);
                if time >= input.duration_ms {
                    continue;
                }
                section.extend_from_slice(&beat[..4]);
                section.extend_from_slice(&time.to_be_bytes());
                count += 1;
            }
            let new_length = section.len() as u32;
            section[8..12].copy_from_slice(&new_length.to_be_bytes());
            section[20..24].copy_from_slice(&count.to_be_bytes());
            data.splice(position..position + length, section);
            let total = data.len() as u32;
            data[8..12].copy_from_slice(&total.to_be_bytes());
            break;
        }
        position += length;
    }
    data
}

pub fn prepare(
    source: &Path,
    name: Option<String>,
    naming: Naming,
    cache: &Path,
) -> Result<Prepared> {
    let files = inputs(source)?;
    let name = name.unwrap_or_else(|| {
        source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    ensure!(
        !name.trim().is_empty() && name.len() <= 250 && !name.chars().any(char::is_control),
        "Invalid playlist name"
    );
    fs::create_dir_all(cache)?;
    let mut prepared = Prepared {
        tracks: Vec::new(),
        sources: Vec::new(),
        hashes: Vec::new(),
        analyses: Vec::new(),
        artwork: ArtworkSet::new(),
        playlist: PioneerPlaylist {
            name,
            entries: Vec::new(),
        },
    };
    let mut by_hash = HashMap::new();
    let mut paths = HashMap::new();
    let bar = spinner("Reading music");
    for (index, path) in files.iter().enumerate() {
        bar.set_message(format!(
            "{}/{} · {}",
            index + 1,
            files.len(),
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        let hash = digest(path)?;
        if let Some(&track_index) = by_hash.get(&hash) {
            prepared.playlist.entries.push(track_index);
            continue;
        }
        let tagged = Probe::open(path)?
            .read()
            .with_context(|| format!("Cannot read MP3: {}", path.display()))?;
        ensure!(
            tagged.file_type() == FileType::Mpeg,
            "File is not MPEG audio: {}",
            path.display()
        );
        let properties = tagged.properties();
        let duration = properties.duration();
        let rate = properties
            .sample_rate()
            .context("Missing MP3 sample rate")?;
        ensure!(
            matches!(rate, 44100 | 48000)
                && matches!(properties.channels(), Some(1 | 2))
                && duration > Duration::ZERO,
            "MP3 is not compatible with the XDJ-RX: {}",
            path.display()
        );
        ensure!(
            duration.as_secs() <= 15 * 60,
            "Tracks over 15 minutes need a streaming analyzer; refusing unbounded analysis: {}",
            path.display()
        );
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
        let title = tag
            .and_then(|t| t.title())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let artist = tag
            .and_then(|t| t.artist())
            .map(|s| s.into_owned())
            .unwrap_or_else(|| "Unknown artist".into());
        let album = tag
            .and_then(|t| t.album())
            .map(|s| s.into_owned())
            .unwrap_or_default();
        let filename = match naming {
            Naming::NumberArtistTitle => format!("{:03} - {artist} - {title}", index + 1),
            Naming::ArtistTitle => format!("{artist} - {title}"),
            Naming::NumberTitle => format!("{:03} - {title}", index + 1),
        };
        // The content suffix prevents clashes across playlists, duplicate titles,
        // FAT case folding, and tracks changed after an earlier export.
        let mut usb_path = format!(
            "/Contents/{}/{} [{}].mp3",
            safe_name(&prepared.playlist.name),
            safe_name(&filename),
            &hash[..12]
        );
        let mut salt = 0u32;
        while paths.contains_key(&path_hash::path_hash(&usb_path)) {
            salt += 1;
            usb_path = format!(
                "/Contents/{}/{} [{}-{salt}].mp3",
                safe_name(&prepared.playlist.name),
                safe_name(&filename),
                &hash[..12]
            );
        }
        paths.insert(path_hash::path_hash(&usb_path), ());
        let cache_path = cache.join(format!("{hash}-analysis-v1.json"));
        let analysis = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Analysis>(&bytes).ok())
            .filter(|a| a.version == 1 && a.preview.len() == 400 && !a.detail.is_empty());
        let analysis = if let Some(analysis) = analysis {
            analysis
        } else {
            bar.set_message(format!(
                "{}/{} · Analyzing {artist} - {title}",
                index + 1,
                files.len()
            ));
            let analyzer = Analyzer::new(
                path,
                AnalysisOptions {
                    min_bpm: 70.0,
                    max_bpm: 180.0,
                },
                Some(duration),
            );
            let waveform = analyzer
                .waveform()
                .context("Cannot decode complete audio for waveform")?;
            let bpm = tag
                .and_then(|t| t.get_string(ItemKey::Bpm))
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|b| b.is_finite() && *b >= 20.0 && *b <= 300.0)
                .or_else(|| analyzer.bpm());
            let analysis = Analysis {
                version: 1,
                bpm,
                key: analyzer.key().map(|k| k.short_code().to_string()),
                preview: pack(&waveform.preview),
                detail: pack(&waveform.detail),
                preview_step: waveform.preview.segment_duration_ms,
                detail_step: waveform.detail.segment_duration_ms,
            };
            atomic_write(&cache_path, &serde_json::to_vec(&analysis)?)?;
            analysis
        };
        let preview = unpack(&analysis.preview, analysis.preview_step);
        let detail = unpack(&analysis.detail, analysis.detail_step);
        let input = AnlzInput {
            device_audio_path: &usb_path,
            bpm: analysis.bpm,
            duration_ms: u32::try_from(duration.as_millis())?,
            waveform_preview: &preview,
            waveform_detail: &detail,
        };
        let artwork_id = match tag.and_then(|t| t.pictures().first()) {
            Some(picture) => prepared.artwork.add(picture.data()).unwrap_or(0),
            None => 0,
        };
        let track = PioneerTrack {
            title,
            artist,
            album,
            genre: tag.and_then(|t| t.genre()).map(|s| s.into_owned()),
            bpm: analysis.bpm,
            key: analysis
                .key
                .as_deref()
                .and_then(MusicalKey::from_short_code),
            duration_secs: u32::try_from(duration.as_secs())?,
            file_size: fs::metadata(path)?.len(),
            track_number: tag.and_then(|t| t.track()),
            year: tag.and_then(|t| t.date()).map(|d| u32::from(d.year)),
            rating: 0,
            bitrate_kbps: properties.audio_bitrate(),
            sample_rate_hz: rate,
            bit_depth: 16,
            file_type: PioneerFileType::Mp3,
            artwork_id,
            date_added: None,
            device_audio_path: usb_path.clone(),
            device_anlz_path: path_hash::anlz_file(&usb_path, "DAT"),
        };
        let track_index = prepared.tracks.len();
        by_hash.insert(hash.clone(), track_index);
        prepared.playlist.entries.push(track_index);
        prepared.tracks.push(track);
        prepared.sources.push(path.clone());
        prepared.hashes.push(hash);
        prepared.analyses.push((
            aligned_dat(&input, beat_offset(&analysis)),
            anlz::ext_bytes(&input),
        ));
    }
    bar.finish_and_clear();
    println!(
        "Prepared {} unique tracks, {} playlist entries; waveform, BPM and key analysis complete.",
        prepared.tracks.len(),
        prepared.playlist.entries.len()
    );
    Ok(prepared)
}

fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative.trim_start_matches('/'));
    ensure!(
        path.components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "Invalid output path"
    );
    let mut current = root.to_path_buf();
    for component in path.components() {
        current.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "Refusing symlink in export: {}",
                current.display()
            );
        }
    }
    Ok(current)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent folder")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_file_name(format!(
        ".{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

pub fn write(prepared: &Prepared, output: &Path) -> Result<()> {
    write_guarded(prepared, output, || Ok(()))
}

pub fn write_guarded(
    prepared: &Prepared,
    output: &Path,
    guard: impl Fn() -> Result<()>,
) -> Result<()> {
    guard()?;
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    let lock_path = output.join(".stickmix.lock");
    ensure!(
        !fs::symlink_metadata(&lock_path).is_ok_and(|m| m.file_type().is_symlink()),
        "Invalid export lock"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.try_lock()
        .context("Another StickMix export is using this folder")?;
    let marker = checked_path(&output, ".stickmix.json")?;
    let owned = fs::read(&marker)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["product"] == "StickMix" && v["version"] == 1);
    ensure!(
        owned || (!output.join("PIONEER").exists() && !output.join("Contents").exists()),
        "This USB contains an existing library. Use a blank stick; StickMix will not replace another library."
    );
    let bar = spinner("Writing Pioneer USB library");
    let mut required = 16 * 1024 * 1024u64;
    for track in &prepared.tracks {
        if !checked_path(&output, &track.device_audio_path)?.is_file() {
            required = required
                .checked_add(track.file_size)
                .context("Export size overflow")?;
        }
    }
    for (dat, ext) in &prepared.analyses {
        required = required
            .checked_add((dat.len() + ext.len()) as u64)
            .context("Analysis size overflow")?;
    }
    ensure!(
        fs2::available_space(&output)? >= required,
        "USB does not have enough free space (need at least {} MB)",
        required / 1024 / 1024
    );
    // Claim a previously blank library before the first audio copy, so an
    // interruption at any later phase remains resumable.
    if !owned {
        atomic_write(
            &marker,
            b"{\"product\":\"StickMix\",\"version\":1,\"status\":\"in-progress\"}",
        )?;
    }
    for (index, track) in prepared.tracks.iter().enumerate() {
        guard()?;
        bar.set_message(format!(
            "{}/{} · Copying {}",
            index + 1,
            prepared.tracks.len(),
            track.title
        ));
        let destination = checked_path(&output, &track.device_audio_path)?;
        if !destination.is_file() || digest(&destination)? != prepared.hashes[index] {
            fs::create_dir_all(destination.parent().context("Missing audio folder")?)?;
            let partial = checked_path(&output, &format!("{}.partial", track.device_audio_path))?;
            fs::copy(&prepared.sources[index], &partial)?;
            ensure!(
                digest(&partial)? == prepared.hashes[index],
                "Audio changed during copy or USB write was corrupted"
            );
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&partial)?
                .sync_all()?;
            fs::rename(partial, destination)?;
        }
        let (dat, ext) = &prepared.analyses[index];
        atomic_write(&checked_path(&output, &track.device_anlz_path)?, dat)?;
        atomic_write(
            &checked_path(
                &output,
                &path_hash::anlz_file(&track.device_audio_path, "EXT"),
            )?,
            ext,
        )?;
    }
    for (path, bytes) in prepared.artwork.files() {
        guard()?;
        atomic_write(&checked_path(&output, &path)?, bytes)?;
    }
    let database = pdb::build(
        &prepared.tracks,
        std::slice::from_ref(&prepared.playlist),
        &prepared.artwork.rows(),
        &chrono::Utc::now().format("%Y-%m-%d").to_string(),
    )
    .map_err(|e| anyhow::anyhow!("Pioneer database: {e:?}"))?;
    guard()?;
    // Ownership marker exists before publishing the first database: an
    // interrupted export can resume even if Contents has already been copied.
    atomic_write(
        &marker,
        &serde_json::to_vec_pretty(
            &serde_json::json!({"product":"StickMix","version":1,"playlist":prepared.playlist.name,"tracks":prepared.tracks.len()}),
        )?,
    )?;
    atomic_write(
        &checked_path(&output, sustain_pioneer::PDB_RELATIVE_PATH)?,
        &database,
    )?;
    let mut playlist = String::from("#EXTM3U\n");
    for &index in &prepared.playlist.entries {
        let track = &prepared.tracks[index];
        playlist.push_str(&format!(
            "#EXTINF:{},{} - {}\n{}\n",
            track.duration_secs,
            track.artist.replace(['\n', '\r'], " "),
            track.title.replace(['\n', '\r'], " "),
            track.device_audio_path.trim_start_matches('/')
        ));
    }
    atomic_write(
        &checked_path(&output, "playlist.m3u8")?,
        playlist.as_bytes(),
    )?;
    bar.finish_and_clear();
    println!(
        "Pioneer device database and analysis files written to {}",
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use binrw::BinRead;
    use rekordcrate::{
        anlz::{ANLZ, Content},
        pdb::{Header, PageType, Row},
    };
    use std::io::Cursor;

    fn fixture(root: &Path) -> PathBuf {
        let folder = root.join("music");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("tone.mp3"),
            include_bytes!("../tests/fixtures/tone.mp3"),
        )
        .unwrap();
        folder
    }
    #[test]
    fn filenames_are_safe_on_fat_and_windows() {
        assert_eq!(safe_name("../CON"), "_CON");
        assert_eq!(safe_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_name("A/B:C?*\n"), "A_B_C___");
        assert!(safe_name(&"🐊".repeat(100)).len() <= 150);
        assert_eq!(safe_name("..."), "Untitled");
    }
    #[test]
    fn rejects_missing_and_outside_playlist_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path());
        fs::write(source.join("playlist.m3u8"), "missing.mp3\n").unwrap();
        assert!(inputs(&source).is_err());
        fs::copy(source.join("tone.mp3"), temp.path().join("outside.mp3")).unwrap();
        fs::write(source.join("playlist.m3u8"), "../outside.mp3\n").unwrap();
        assert!(inputs(&source).is_err());
    }
    #[test]
    fn independent_parser_accepts_pdb_and_waveforms_and_resume_keeps_audio() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path());
        let cache = temp.path().join("cache");
        fs::write(
            source.join("playlist.m3u8"),
            "#EXTM3U\ntone.mp3\ntone.mp3\n",
        )
        .unwrap();
        let prepared = prepare(
            &source,
            Some("Test & <playlist>".into()),
            Naming::NumberArtistTitle,
            &cache,
        )
        .unwrap();
        assert_eq!(prepared.tracks.len(), 1);
        assert_eq!(prepared.playlist.entries, vec![0, 0]);
        let output = temp.path().join("usb");
        write(&prepared, &output).unwrap();
        let audio = output.join(prepared.tracks[0].device_audio_path.trim_start_matches('/'));
        let mtime = fs::metadata(&audio).unwrap().modified().unwrap();
        assert_eq!(
            fs::read(&audio).unwrap(),
            include_bytes!("../tests/fixtures/tone.mp3")
        );
        let mut reader =
            Cursor::new(fs::read(output.join(sustain_pioneer::PDB_RELATIVE_PATH)).unwrap());
        let header = Header::read_le(&mut reader).unwrap();
        assert_eq!(header.page_size, 4096);
        let mut tracks = 0;
        let mut entries = 0;
        for table in header
            .tables
            .iter()
            .filter(|t| matches!(t.page_type, PageType::Tracks | PageType::PlaylistEntries))
        {
            for page in header
                .read_pages(
                    &mut reader,
                    binrw::Endian::Little,
                    (&table.first_page, &table.last_page),
                )
                .unwrap()
            {
                for group in &page.row_groups {
                    for row in group.present_rows() {
                        match row {
                            Row::Track(_) => tracks += 1,
                            Row::PlaylistEntry(_) => entries += 1,
                            _ => {}
                        }
                    }
                }
            }
        }
        assert_eq!(tracks, 1);
        assert_eq!(entries, 2);
        for bytes in [&prepared.analyses[0].0, &prepared.analyses[0].1] {
            let parsed = ANLZ::read_be(&mut Cursor::new(bytes)).unwrap();
            assert!(parsed.sections.len() >= 5);
            for section in &parsed.sections {
                if let Content::BeatGrid(grid) = &section.content {
                    assert!(!grid.beats.is_empty());
                    assert!(
                        (90..=180).contains(&grid.beats[0].time),
                        "first beat: {}",
                        grid.beats[0].time
                    );
                    assert!(
                        grid.beats
                            .windows(2)
                            .all(|pair| pair[0].time < pair[1].time)
                    );
                }
            }
        }
        let cached = prepare(
            &source,
            Some("Test & <playlist>".into()),
            Naming::NumberArtistTitle,
            &cache,
        )
        .unwrap();
        write(&cached, &output).unwrap();
        assert_eq!(fs::metadata(&audio).unwrap().modified().unwrap(), mtime);
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
        // A corrupted prior USB copy is repaired on restart, with analysis reused.
        fs::write(&audio, b"incomplete copy").unwrap();
        write(&cached, &output).unwrap();
        assert_eq!(
            fs::read(&audio).unwrap(),
            include_bytes!("../tests/fixtures/tone.mp3")
        );
        assert!(prepared.source_on(&source));
        assert!(!prepared.source_on(&output));
    }
    #[test]
    fn foreign_library_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path());
        let prepared = prepare(
            &source,
            None,
            Naming::ArtistTitle,
            &temp.path().join("cache"),
        )
        .unwrap();
        let output = temp.path().join("usb");
        fs::create_dir_all(output.join("PIONEER/rekordbox")).unwrap();
        fs::write(
            output.join("PIONEER/rekordbox/export.pdb"),
            b"existing library",
        )
        .unwrap();
        assert!(write(&prepared, &output).is_err());
        assert_eq!(
            fs::read(output.join("PIONEER/rekordbox/export.pdb")).unwrap(),
            b"existing library"
        );
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_export_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = fixture(temp.path());
        let prepared = prepare(
            &source,
            None,
            Naming::ArtistTitle,
            &temp.path().join("cache"),
        )
        .unwrap();
        let output = temp.path().join("usb");
        fs::create_dir_all(&output).unwrap();
        std::os::unix::fs::symlink(&source, output.join("Contents")).unwrap();
        assert!(write(&prepared, &output).is_err());
        assert_eq!(fs::read_dir(&source).unwrap().count(), 1);
    }
}
