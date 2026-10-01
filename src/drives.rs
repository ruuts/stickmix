// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 StickMix contributors
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Drive {
    pub id: String,
    model: String,
    serial: String,
    size: u64,
    pub mount: Option<PathBuf>,
    filesystem: String,
    mbr: bool,
    partitions: Vec<String>,
}

impl Drive {
    pub fn verify(&self) -> Result<()> {
        let fresh = find(&self.id)?;
        ensure!(
            fresh.fingerprint() == self.fingerprint()
                && fresh.mount == self.mount
                && fresh.compatible(),
            "USB was removed or changed during export"
        );
        Ok(())
    }
    pub fn description(&self) -> String {
        format!(
            "{} · {} · {:.1} GB · {} · {}",
            self.id,
            self.model,
            self.size as f64 / 1e9,
            self.filesystem,
            self.mount
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not mounted".into())
        )
    }
    pub fn compatible(&self) -> bool {
        self.mbr && self.filesystem.eq_ignore_ascii_case("FAT32") && self.mount.is_some()
    }
    fn fingerprint(&self) -> String {
        Sha256::digest(
            format!(
                "{}\0{}\0{}\0{}",
                self.id, self.model, self.serial, self.size
            )
            .as_bytes(),
        )
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
    }
}

fn output(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let result = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Cannot run {program}"))?;
    ensure!(
        result.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}
fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status()?;
    ensure!(status.success(), "{program} failed");
    Ok(())
}
fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().trim().to_string()
}

fn children<'a>(value: &'a Value, values: &mut Vec<&'a Value>) {
    values.push(value);
    if let Some(items) = value["children"].as_array() {
        for child in items {
            children(child, values);
        }
    }
}

fn protected_mount(value: &str) -> bool {
    value == "/"
        || value == "/home"
        || value.starts_with("/boot")
        || value.starts_with("/usr")
        || value.starts_with("/var")
        || value.starts_with("/etc")
        || value.starts_with("/home/")
        || value == "[SWAP]"
}

fn linux_list(data: &Value) -> Vec<Drive> {
    let mut result = Vec::new();
    for disk in data["blockdevices"].as_array().into_iter().flatten() {
        if disk["type"] != "disk" || disk["tran"] != "usb" || disk["ro"].as_bool().unwrap_or(true) {
            continue;
        }
        let mut all = Vec::new();
        children(disk, &mut all);
        if all.iter().any(|part| {
            part["mountpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(protected_mount)
        }) {
            continue;
        }
        let partitions: Vec<_> = all.iter().filter(|part| part["type"] == "part").collect();
        let first = partitions.first().copied();
        let mount = first
            .and_then(|p| p["mountpoints"].as_array())
            .and_then(|a| a.iter().find_map(Value::as_str))
            .map(PathBuf::from);
        let filesystem = first
            .map(|p| {
                if p["fstype"] == "vfat" && p["fsver"] == "FAT32" {
                    "FAT32".into()
                } else {
                    text(p, "fstype")
                }
            })
            .unwrap_or_default();
        result.push(Drive {
            id: text(disk, "path"),
            model: text(disk, "model"),
            serial: text(disk, "serial"),
            size: disk["size"].as_u64().unwrap_or(0),
            mount,
            filesystem,
            mbr: disk["pttype"] == "dos",
            partitions: partitions.iter().map(|p| text(p, "path")).collect(),
        });
    }
    result
}

fn powershell(script: &str) -> Result<Vec<u8>> {
    let script =
        format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); {script}");
    output(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
}
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn windows_list() -> Result<Vec<Drive>> {
    let script = r#"$ErrorActionPreference='Stop'; $result=@(Get-Disk | Where-Object { $_.BusType -eq 'USB' -and !$_.IsBoot -and !$_.IsSystem -and !$_.IsReadOnly -and !$_.IsOffline } | ForEach-Object { $d=$_; $p=Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | Sort-Object PartitionNumber | Select-Object -First 1; $v=$null; if($p){$v=$p | Get-Volume -ErrorAction SilentlyContinue}; [pscustomobject]@{id=[string]$d.Number;model=$d.FriendlyName;serial=$d.UniqueId;size=$d.Size;mbr=($d.PartitionStyle -eq 'MBR');mount=$(if($p.DriveLetter){[string]$p.DriveLetter+':\'}else{$null});filesystem=$v.FileSystem} }); ConvertTo-Json -Compress -InputObject $result"#;
    let data: Value = serde_json::from_slice(&powershell(script)?)?;
    Ok(data
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| Drive {
            id: text(d, "id"),
            model: text(d, "model"),
            serial: text(d, "serial"),
            size: d["size"].as_u64().unwrap_or(0),
            mount: d["mount"].as_str().map(PathBuf::from),
            filesystem: text(d, "filesystem"),
            mbr: d["mbr"].as_bool().unwrap_or(false),
            partitions: Vec::new(),
        })
        .collect())
}

fn diskutil_json(args: &[&str]) -> Result<Value> {
    let plist = output("diskutil", args)?;
    let mut child = Command::new("plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("Missing plist input")?
        .write_all(&plist)?;
    let converted = child.wait_with_output()?;
    ensure!(converted.status.success(), "Cannot read diskutil data");
    Ok(serde_json::from_slice(&converted.stdout)?)
}
fn mac_list() -> Result<Vec<Drive>> {
    let data = diskutil_json(&["list", "-plist", "external", "physical"])?;
    let mut result = Vec::new();
    for disk in data["AllDisksAndPartitions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = text(disk, "DeviceIdentifier");
        let info = diskutil_json(&["info", "-plist", &id])?;
        if info["Internal"] != false || info["Whole"] != true || info["BusProtocol"] != "USB" {
            continue;
        }
        let first = disk["Partitions"].as_array().and_then(|p| p.first());
        let part = if let Some(part) = first {
            diskutil_json(&["info", "-plist", &text(part, "DeviceIdentifier")])?
        } else {
            Value::Null
        };
        let fsname = text(&part, "FilesystemName");
        result.push(Drive {
            id,
            model: text(&info, "MediaName"),
            serial: text(&info, "DeviceTreePath"),
            size: info["TotalSize"].as_u64().unwrap_or(0),
            mount: part["MountPoint"].as_str().map(PathBuf::from),
            filesystem: if fsname.contains("FAT32") {
                "FAT32".into()
            } else {
                fsname
            },
            mbr: disk["Content"] == "FDisk_partition_scheme",
            partitions: Vec::new(),
        });
    }
    Ok(result)
}

pub fn list() -> Result<Vec<Drive>> {
    match std::env::consts::OS {
        "linux" => Ok(linux_list(&serde_json::from_slice(&output(
            "lsblk",
            &[
                "--json",
                "--bytes",
                "--output",
                "NAME,PATH,TYPE,TRAN,RM,RO,SIZE,MODEL,SERIAL,FSTYPE,FSVER,MOUNTPOINTS,PTTYPE",
            ],
        )?)?)),
        "windows" => windows_list(),
        "macos" => mac_list(),
        other => bail!("Unsupported OS: {other}"),
    }
}
pub fn find(id: &str) -> Result<Drive> {
    list()?
        .into_iter()
        .find(|d| d.id == id)
        .context("USB is missing, read-only, or is a system disk")
}

pub fn format(drive: &Drive) -> Result<Drive> {
    ensure!(
        !drive.serial.is_empty() && (1024 * 1024 * 1024..=2_000_000_000_000).contains(&drive.size),
        "Cannot safely identify this USB (identity or size missing)"
    );
    let fresh = find(&drive.id)?;
    ensure!(
        fresh.fingerprint() == drive.fingerprint(),
        "USB identity changed; formatting cancelled"
    );
    let executable = std::env::current_exe()?;
    let fingerprint = drive.fingerprint();
    match std::env::consts::OS {
        "linux" => run(
            "pkexec",
            &[
                executable
                    .to_str()
                    .context("Executable path is not UTF-8")?,
                "format-helper",
                &drive.id,
                &fingerprint,
            ],
        )?,
        "windows" => {
            // UAC elevation is limited to the same native program and exact disk
            // identity. No ExecutionPolicy override or downloaded format tool.
            let script = format!(
                "$ErrorActionPreference='Stop'; $p=Start-Process -FilePath {} -ArgumentList @('format-helper',{},{}) -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
                ps_quote(&executable.display().to_string()),
                ps_quote(&drive.id),
                ps_quote(&fingerprint)
            );
            powershell(&script)?;
        }
        "macos" => format_helper(&drive.id, &fingerprint)?,
        other => bail!("Unsupported OS: {other}"),
    }
    for _ in 0..20 {
        if let Ok(fresh) = find(&drive.id) {
            if std::env::consts::OS == "linux"
                && fresh.mount.is_none()
                && let Some(part) = fresh.partitions.first()
            {
                let _ = output("udisksctl", &["mount", "-b", part]);
            }
            if fresh.compatible() {
                return Ok(fresh);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("USB formatted but could not be mounted as FAT32/MBR; reconnect it and retry")
}

pub fn format_helper(id: &str, fingerprint: &str) -> Result<()> {
    let drive = find(id)?;
    ensure!(
        !drive.serial.is_empty() && drive.fingerprint() == fingerprint,
        "USB identity changed; nothing erased"
    );
    match std::env::consts::OS {
        "linux" => {
            ensure!(
                id.starts_with("/dev/sd") && id[7..].chars().all(|c| c.is_ascii_lowercase()),
                "Unexpected Linux disk path"
            );
            ensure!(
                String::from_utf8(output("id", &["-u"])?)?.trim() == "0",
                "Formatting needs system authorization"
            );
            for part in &drive.partitions {
                let mounted = output(
                    "findmnt",
                    &["--noheadings", "--source", part, "--output", "TARGET"],
                );
                if mounted.as_ref().is_ok_and(|o| !o.is_empty()) {
                    run("umount", &[part])?;
                }
            }
            ensure!(
                find(id)?.fingerprint() == fingerprint,
                "USB changed before formatting"
            );
            let mut child = Command::new("sfdisk")
                .args(["--wipe", "always", id])
                .stdin(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .context("Missing partition input")?
                .write_all(b"label: dos\n,,c\n")?;
            ensure!(child.wait()?.success(), "Creating MBR partition failed");
            run("udevadm", &["settle"])?;
            let part = format!("{id}1");
            ensure!(PathBuf::from(&part).exists(), "USB partition not found");
            run("mkfs.fat", &["-F", "32", "-n", "STICKMIX", &part])
        }
        "windows" => {
            let number: u32 = id.parse().context("Unexpected Windows disk number")?;
            let script = format!(
                "$ErrorActionPreference='Stop'; $d=Get-Disk -Number {number}; if($d.BusType -ne 'USB' -or $d.IsBoot -or $d.IsSystem -or $d.IsReadOnly -or $d.UniqueId -ne {}){{throw 'Disk identity changed'}}; $d | Clear-Disk -RemoveData -RemoveOEM -Confirm:$false; Initialize-Disk -Number {number} -PartitionStyle MBR; $d=Get-Disk -Number {number}; if($d.LargestFreeExtent -gt 30GB){{$p=New-Partition -DiskNumber {number} -Size 30GB -AssignDriveLetter}}else{{$p=New-Partition -DiskNumber {number} -UseMaximumSize -AssignDriveLetter}}; $p | Format-Volume -FileSystem FAT32 -NewFileSystemLabel STICKMIX -Confirm:$false | Out-Null",
                ps_quote(&drive.serial)
            );
            powershell(&script)?;
            Ok(())
        }
        "macos" => {
            ensure!(
                id.strip_prefix("disk")
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())),
                "Unexpected Mac disk identifier"
            );
            run("diskutil", &["eraseDisk", "FAT32", "STICKMIX", "MBR", id])
        }
        other => bail!("Unsupported OS: {other}"),
    }
}

pub fn flush(drive: &Drive) -> Result<()> {
    let fresh = find(&drive.id)?;
    ensure!(
        fresh.fingerprint() == drive.fingerprint(),
        "USB changed during export"
    );
    match std::env::consts::OS {
        "linux" => {
            run(
                "sync",
                &[
                    "-f",
                    fresh
                        .mount
                        .as_ref()
                        .context("USB unmounted")?
                        .to_str()
                        .context("USB path not UTF-8")?,
                ],
            )?;
            for part in &fresh.partitions {
                run("udisksctl", &["unmount", "-b", part])?;
            }
            run("udisksctl", &["power-off", "-b", &fresh.id])
        }
        "macos" => run("diskutil", &["eject", &fresh.id]),
        "windows" => {
            let mount = fresh.mount.context("USB unmounted")?;
            let letter = mount
                .display()
                .to_string()
                .chars()
                .next()
                .context("Invalid drive letter")?;
            ensure!(letter.is_ascii_alphabetic(), "Invalid drive letter");
            powershell(&format!(
                "$ErrorActionPreference='Stop'; Write-VolumeCache -DriveLetter '{letter}'; $shell=New-Object -ComObject Shell.Application; $shell.Namespace(17).ParseName('{letter}:').InvokeVerb('Eject')"
            ))?;
            Ok(())
        }
        other => bail!("Unsupported OS: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excludes_system_readonly_and_internal_disks() {
        let data = serde_json::json!({"blockdevices":[
            {"type":"disk","tran":"nvme","ro":false},
            {"type":"disk","tran":"usb","ro":true},
            {"type":"disk","tran":"usb","ro":false,"children":[{"mountpoints":["/"]}]},
            {"type":"disk","tran":"usb","ro":false,"path":"/dev/sdz","model":"Test USB","serial":"123","size":16000000000u64,"pttype":"dos","children":[{"type":"part","path":"/dev/sdz1","fstype":"vfat","fsver":"FAT32","mountpoints":["/run/media/test/STICKMIX"]}]}]});
        let drives = linux_list(&data);
        assert_eq!(drives.len(), 1);
        assert!(drives[0].compatible());
        assert_eq!(drives[0].id, "/dev/sdz");
    }
    #[test]
    fn changed_disk_identity_changes_fingerprint() {
        let data = serde_json::json!({"blockdevices":[{"type":"disk","tran":"usb","ro":false,"serial":"first"}]});
        let mut drive = linux_list(&data).remove(0);
        let old = drive.fingerprint();
        drive.serial = "second".into();
        assert_ne!(old, drive.fingerprint());
    }
}
