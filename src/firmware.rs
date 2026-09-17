/*
 * Heavily based on https://github.com/andersson/pil-squasher/, and as such:
 *
 * Copyright (c) 2019, Linaro Ltd.
 * Copyright (c) 2022-2024, Arnaud Ferraris.
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice,
 * this list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 * this list of conditions and the following disclaimer in the documentation
 * and/or other materials provided with the distribution.
 *
 * 3. Neither the name of the copyright holder nor the names of its contributors
 * may be used to endorse or promote products derived from this software without
 * specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
 * LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
 * CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
 * SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
 * INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
 * CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
 * ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
 * POSSIBILITY OF SUCH DAMAGE.
 */

use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, thread, time::Duration};

use std::io::prelude::*;

use fs_extra::dir;
use goblin::elf::Elf;
use indexmap::IndexMap;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use serde::{Deserialize, Serialize};

use crate::utils;

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;
const MAPPER_DIR: &str = "/dev/mapper";
const PARTLABEL_DIR: &str = "/dev/disk/by-partlabel";
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";
const MOUNT_FILESYSTEM_TYPES: &[&str] = &["ext4", "erofs", "f2fs", "vfat", "exfat"];

struct MountedPartition {
    path: PathBuf,
    source: PathBuf,
    temporary: bool,
}

impl MountedPartition {
    fn existing(path: PathBuf, source: PathBuf) -> Self {
        Self {
            path,
            source,
            temporary: false,
        }
    }

    fn temporary(path: PathBuf, source: PathBuf) -> Self {
        Self {
            path,
            source,
            temporary: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source(&self) -> &Path {
        &self.source
    }

    fn cleanup(self) {
        if self.temporary {
            let _res = umount2(&self.path, MntFlags::empty());
            let _r = fs::remove_dir(self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    fn suffix(self) -> &'static str {
        match self {
            Self::A => "_a",
            Self::B => "_b",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct KernelVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConstraintOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
}

/// An individual file or directory listed in an extract entry: either a bare
/// path, or a mapping that renames the copy and/or marks it required.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Item {
    Path(String),
    Spec {
        from: String,
        to: Option<String>,
        #[serde(default)]
        required: bool,
    },
}

impl Item {
    fn from(&self) -> &str {
        match self {
            Self::Path(from) => from,
            Self::Spec { from, .. } => from,
        }
    }

    fn to(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Spec { to, .. } => to.as_deref(),
        }
    }

    fn required(&self) -> bool {
        match self {
            Self::Path(_) => false,
            Self::Spec { required, .. } => *required,
        }
    }
}

/// `to:` is either a fixed path, or an ordered map of kernel constraint to
/// path; the first constraint matching the running kernel wins.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Dest {
    Fixed(String),
    ByKernel(IndexMap<String, String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    partition: String,
    #[serde(default)]
    from: Option<String>,
    to: Dest,
    #[serde(default)]
    raw: bool,
    #[serde(default)]
    files: Vec<Item>,
    #[serde(default)]
    dirs: Vec<Item>,
}

impl Entry {
    /// Raw entries dump the partition block device straight into `to:` and
    /// cannot carry any of the mount-based copy attributes.
    fn validate(&self) -> Result<(), Error> {
        if !self.raw {
            return Ok(());
        }

        if self.from.is_some() || !self.files.is_empty() || !self.dirs.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "raw entry on partition {} must not set from, files or dirs",
                    self.partition
                ),
            ));
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "dynamic-partition", default)]
    dynpart: Option<String>,
    extract: Vec<Entry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartitionStatus {
    pub partition: String,
    pub source: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileFailure {
    pub partition: String,
    pub source: String,
    pub destination: String,
    pub required: bool,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub entries: Vec<String>,
    pub kernel_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_slot: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<PartitionStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<FileFailure>,
}

impl Status {
    pub fn has_required_failures(&self) -> bool {
        self.failures.iter().any(|failure| failure.required)
    }
}

fn parse_slot(value: &str) -> Option<Slot> {
    match value.trim().trim_matches('"') {
        "a" | "_a" => Some(Slot::A),
        "b" | "_b" => Some(Slot::B),
        _ => None,
    }
}

fn merge_slot(current: Option<Slot>, candidate: Slot, source: &str) -> Result<Option<Slot>, Error> {
    if let Some(current) = current
        && current != candidate
    {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("Conflicting active slot values in {source}"),
        ));
    }

    Ok(Some(candidate))
}

fn slot_from_cmdline(cmdline: &str) -> Result<Option<Slot>, Error> {
    let mut slot = None;

    for argument in cmdline.split_whitespace() {
        let Some((key, value)) = argument.split_once('=') else {
            continue;
        };
        if key != "androidboot.slot_suffix" && key != "androidboot.slot" {
            continue;
        }

        let candidate = parse_slot(value).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                format!("Unsupported {key} value '{value}' in /proc/cmdline"),
            )
        })?;
        slot = merge_slot(slot, candidate, "/proc/cmdline")?;
    }

    Ok(slot)
}

fn reconcile_slots(cmdline: Option<Slot>, qbootctl: Option<Slot>) -> Result<Option<Slot>, Error> {
    match (cmdline, qbootctl) {
        (Some(cmdline), Some(qbootctl)) if cmdline != qbootctl => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Active slot mismatch: kernel command line reports {}, qbootctl reports {}",
                cmdline.suffix(),
                qbootctl.suffix()
            ),
        )),
        (Some(slot), _) | (None, Some(slot)) => Ok(Some(slot)),
        (None, None) => Ok(None),
    }
}

pub fn detect_active_slot() -> Result<Option<Slot>, Error> {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let cmdline_slot = slot_from_cmdline(&cmdline)?;

    let qbootctl_slot = match Command::new("qbootctl").arg("-x").output() {
        Ok(output) if output.status.success() => {
            let value = String::from_utf8_lossy(&output.stdout);
            Some(parse_slot(&value).ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "qbootctl returned unsupported slot value '{}'",
                        value.trim()
                    ),
                )
            })?)
        }
        Ok(output) => {
            warn!(
                "Unable to cross-check active slot with qbootctl: {}",
                output.status
            );
            None
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            debug!("qbootctl is unavailable; skipping active-slot cross-check");
            None
        }
        Err(error) => {
            warn!("Unable to execute qbootctl for active-slot cross-check: {error}");
            None
        }
    };

    let slot = reconcile_slots(cmdline_slot, qbootctl_slot)?;
    match slot {
        Some(slot) => info!("Using active Android slot {}", slot.suffix()),
        None => warn!("Unable to determine the active Android slot"),
    }
    Ok(slot)
}

fn parse_kernel_version(version: &str) -> Option<KernelVersion> {
    let prefix = version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>();

    if prefix.is_empty() {
        return None;
    }

    let mut parts = prefix.split('.').filter(|p| !p.is_empty());
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u32>().ok().unwrap_or(0);
    let patch = parts.next().unwrap_or("0").parse::<u32>().ok().unwrap_or(0);

    Some(KernelVersion {
        major,
        minor,
        patch,
    })
}

/// Parse a `to:` map key such as `"<7.0"` into an operator plus version.
fn parse_constraint(constraint: &str) -> Option<(ConstraintOp, KernelVersion)> {
    let (op, value) = if let Some(value) = constraint.strip_prefix("<=") {
        (ConstraintOp::Lte, value)
    } else if let Some(value) = constraint.strip_prefix('<') {
        (ConstraintOp::Lt, value)
    } else if let Some(value) = constraint.strip_prefix(">=") {
        (ConstraintOp::Gte, value)
    } else if let Some(value) = constraint.strip_prefix('>') {
        (ConstraintOp::Gt, value)
    } else if let Some(value) = constraint.strip_prefix('=') {
        (ConstraintOp::Eq, value)
    } else {
        return None;
    };

    Some((op, parse_kernel_version(value)?))
}

/// Match one kernel constraint against the running kernel. `*` is a
/// catch-all; an unparsable constraint never matches.
fn constraint_matches(running: KernelVersion, constraint: &str) -> bool {
    if constraint == "*" {
        return true;
    }

    match parse_constraint(constraint) {
        Some((ConstraintOp::Lt, version)) => running < version,
        Some((ConstraintOp::Lte, version)) => running <= version,
        Some((ConstraintOp::Gt, version)) => running > version,
        Some((ConstraintOp::Gte, version)) => running >= version,
        Some((ConstraintOp::Eq, version)) => running == version,
        None => {
            warn!("Ignoring invalid kernel constraint '{constraint}'");
            false
        }
    }
}

/// Resolve an entry's `to:` for the running kernel. Kernel-conditional
/// destinations are scanned in document order and the first matching
/// constraint wins; `None` means the entry is skipped.
fn resolve_destination(
    dest: &Dest,
    running_kernel: Option<KernelVersion>,
    partition: &str,
) -> Option<String> {
    match dest {
        Dest::Fixed(path) => Some(path.clone()),
        Dest::ByKernel(map) => {
            let running_kernel = match running_kernel {
                Some(running_kernel) => running_kernel,
                None => {
                    warn!(
                        "Unable to parse running kernel release, skipping kernel-conditional entry on partition {partition}"
                    );
                    return None;
                }
            };

            for (constraint, path) in map {
                if constraint_matches(running_kernel, constraint) {
                    return Some(path.clone());
                }
            }

            debug!(
                "No kernel constraint matches the running kernel, skipping entry on partition {partition}"
            );
            None
        }
    }
}

/// `to:` resolves like any Unix path: relative destinations are joined onto
/// the extract path, absolute destinations are used verbatim.
fn destination_path(to: &str, extract_path: &str) -> PathBuf {
    let path = Path::new(to);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(extract_path).join(path)
    };

    joined.components().collect::<PathBuf>()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItemKind {
    File,
    Dir,
}

/// Where one item lands: `to:` renames the last path component (and may
/// itself contain a subdirectory), the source name is used otherwise.
/// Squashing an MDT always produces an MBN, whatever the item is renamed to.
fn item_destination(dest_dir: &Path, item: &Item, kind: ItemKind) -> PathBuf {
    let mut destination = match item.to() {
        Some(to) => dest_dir.join(to),
        None => dest_dir.join(
            Path::new(item.from())
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(item.from()),
        ),
    };

    if kind == ItemKind::File && item.from().ends_with(".mdt") {
        destination.set_extension("mbn");
    }

    destination
}

fn decode_mountinfo_path(path: &str) -> PathBuf {
    PathBuf::from(
        path.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\"),
    )
}

fn device_numbers(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xfffff000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffffff00);

    (major, minor)
}

fn parse_mountinfo_device(device: &str) -> Option<(u64, u64)> {
    let (major, minor) = device.split_once(':')?;

    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn mounted_path_from_mountinfo(
    mountinfo: &str,
    source_major: u64,
    source_minor: u64,
) -> Option<PathBuf> {
    let mut fallback = None;

    for line in mountinfo.lines() {
        let fields = line.split(' ').collect::<Vec<_>>();
        if fields.len() < 5 {
            continue;
        }

        if parse_mountinfo_device(fields[2]) != Some((source_major, source_minor)) {
            continue;
        }

        let mountpoint = decode_mountinfo_path(fields[4]);
        if fields[3] == "/" {
            return Some(mountpoint);
        }

        if fallback.is_none() {
            fallback = Some(mountpoint);
        }
    }

    fallback
}

fn already_mounted_path(srcpath: &Path) -> Option<PathBuf> {
    let (source_major, source_minor) = device_numbers(fs::metadata(srcpath).ok()?.rdev());
    let mountinfo = fs::read_to_string(MOUNTINFO_PATH).ok()?;

    mounted_path_from_mountinfo(&mountinfo, source_major, source_minor)
}

fn mounted_partition(srcpath: &Path) -> Option<MountedPartition> {
    let mountpoint = already_mounted_path(srcpath)?;
    debug!(
        "Using already mounted partition {} at {}",
        srcpath.display(),
        mountpoint.display()
    );
    Some(MountedPartition::existing(
        mountpoint,
        srcpath.to_path_buf(),
    ))
}

fn mount_srcpath(
    srcpath: &Path,
    mountpath: &Path,
    flags: MsFlags,
) -> Result<MountedPartition, Error> {
    if let Some(mounted) = mounted_partition(srcpath) {
        return Ok(mounted);
    }

    let _res = fs::DirBuilder::new().recursive(true).create(mountpath);

    let mut last_error = None;
    for fstype in MOUNT_FILESYSTEM_TYPES {
        debug!(
            "Attempting to mount {} to {} as {}",
            srcpath.display(),
            mountpath.display(),
            fstype
        );

        match mount(Some(srcpath), mountpath, Some(*fstype), flags, None::<&str>) {
            Ok(()) => {
                return Ok(MountedPartition::temporary(
                    mountpath.to_path_buf(),
                    srcpath.to_path_buf(),
                ));
            }
            Err(e) => {
                let error = Error::from(e);
                debug!(
                    "Unable to mount {} on {} as {}: {}",
                    srcpath.display(),
                    mountpath.display(),
                    fstype,
                    error
                );

                last_error = Some(error);
            }
        }

        if let Some(mounted) = mounted_partition(srcpath) {
            return Ok(mounted);
        }
    }

    match last_error {
        Some(e) => {
            if let Some(mounted) = mounted_partition(srcpath) {
                return Ok(mounted);
            }

            error!(
                "Unable to mount {} on {} as any supported filesystem: {}",
                srcpath.display(),
                mountpath.display(),
                e
            );
            Err(e)
        }
        None => Err(Error::other("No supported filesystems configured")),
    }
}

fn find_in_roots(name: &str, roots: &[&Path]) -> Option<PathBuf> {
    roots
        .iter()
        .map(|root| root.join(name))
        .find(|path| path.exists())
}

fn select_partition_path(
    part: &str,
    active_slot: Option<Slot>,
    roots: &[&Path],
) -> Result<PathBuf, Error> {
    if let Some(slot) = active_slot {
        let active_name = format!("{part}{}", slot.suffix());
        if let Some(path) = find_in_roots(&active_name, roots) {
            return Ok(path);
        }
        if let Some(path) = find_in_roots(part, roots) {
            return Ok(path);
        }

        return Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "Unable to find active partition {active_name} or unsuffixed partition {part}; refusing to use the opposite slot"
            ),
        ));
    }

    if let Some(path) = find_in_roots(part, roots) {
        return Ok(path);
    }

    let part_a = format!("{part}_a");
    let part_b = format!("{part}_b");
    let selected = find_in_roots(&part_a, roots).or_else(|| find_in_roots(&part_b, roots));

    if let Some(path) = selected {
        warn!(
            "Active slot is unknown; preserving legacy _a-then-_b selection for partition {part}: {}",
            path.display()
        );
        return Ok(path);
    }

    Err(Error::new(
        ErrorKind::NotFound,
        format!("Unable to find device file for partition {part}"),
    ))
}

fn mount_part(
    part: &str,
    mountpath: &Path,
    active_slot: Option<Slot>,
) -> Result<MountedPartition, Error> {
    let mapper_dir = Path::new(MAPPER_DIR);
    let partlabel_dir = Path::new(PARTLABEL_DIR);
    let srcpath = select_partition_path(part, active_slot, &[mapper_dir, partlabel_dir])?;

    mount_srcpath(&srcpath, mountpath, MsFlags::MS_RDONLY)
}

fn squash_file(inpath: &Path, outpath: &Path) -> Result<(), Error> {
    let buffer = match fs::read(inpath) {
        Ok(buf) => buf,
        Err(e) => {
            error!("Unable to read {}: {}", inpath.display(), e);
            return Err(e);
        }
    };

    let elf = match Elf::parse(buffer.as_slice()) {
        Ok(value) => value,
        Err(e) => {
            error!("Unable to parse {}: {}", inpath.display(), e);
            return Err(Error::new(ErrorKind::InvalidData, e));
        }
    };

    let mut count = 0;
    let mut hashoffset = 0;

    let mut mdt_fd = fs::File::open(inpath)?;
    let mbn_fd = fs::File::create(outpath)?;

    for ref phdr in elf.program_headers {
        if count == 0 {
            hashoffset = phdr.p_filesz;
        }

        count += 1;

        if phdr.p_filesz == 0 {
            continue;
        }

        let mut buffer: Vec<u8> = Vec::new();

        if (phdr.p_flags & FLAGS_READ_MASK) == FLAGS_MDT_VALUE {
            mdt_fd.seek(SeekFrom::Start(hashoffset))?;
            buffer.resize(phdr.p_filesz as usize, Default::default());
            mdt_fd.read_exact(buffer.as_mut_slice())?;
        }

        if buffer.is_empty() {
            let mut bxx_name = inpath.to_path_buf();
            bxx_name.set_extension(format!("b{:#02}", count - 1));

            let mut bxx_fd = fs::File::open(&bxx_name)?;
            bxx_fd.read_to_end(&mut buffer)?;
        }

        if buffer.len() != phdr.p_filesz as usize {
            let err_str = format!("Read {} bytes (!= {})", buffer.len(), phdr.p_filesz);
            return Err(Error::new(ErrorKind::UnexpectedEof, err_str));
        }

        mbn_fd.write_all_at(buffer.as_slice(), phdr.p_offset)?;
    }

    Ok(())
}

fn dynpart_paths(
    part: &str,
    active_slot: Option<Slot>,
    partlabel_dir: &Path,
) -> Result<Vec<PathBuf>, Error> {
    if active_slot.is_some() {
        return select_partition_path(part, active_slot, &[partlabel_dir]).map(|path| vec![path]);
    }

    let paths = ["", "_a", "_b"]
        .iter()
        .map(|suffix| partlabel_dir.join(format!("{part}{suffix}")))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();

    if paths.is_empty() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("Unable to find dynamic partition container {part}"),
        ));
    }

    warn!(
        "Active slot is unknown; preserving legacy behavior by mapping every available {part} dynamic-partition container"
    );
    Ok(paths)
}

fn start_dynpart_mapping(dynpart: &Path) -> Result<(), Error> {
    let mapped_name = dynpart
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Invalid dynamic partition name"))?;

    utils::execute(
        "systemctl",
        Some(vec![
            "start",
            &format!("make-dynpart-mappings@{mapped_name}.service"),
        ]),
    )
}

fn map_dynpart(part: &str, active_slot: Option<Slot>) -> Result<Vec<PathBuf>, Error> {
    let paths = dynpart_paths(part, active_slot, Path::new(PARTLABEL_DIR))?;
    let mut mapped = Vec::new();
    let mut last_error = None;

    for path in paths {
        match start_dynpart_mapping(&path) {
            Ok(()) => mapped.push(path),
            Err(error) if active_slot.is_none() => {
                warn!(
                    "Unable to map dynamic partition container {}: {error}",
                    path.display()
                );
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    if mapped.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            Error::other(format!("Failed to map dynamic partition container {part}"))
        }));
    }

    Ok(mapped)
}

/// Copy a single file, squashing MDT images into a standalone MBN when the
/// source name calls for it.
fn copy_file_item(source: &Path, destination: &Path, squash: bool) -> Result<(), Error> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    if !squash {
        fs::copy(source, destination)?;
        return Ok(());
    }

    trace!("Squashing MDT file into MBN");
    if let Err(e) = squash_file(source, destination) {
        let _ = fs::remove_file(destination);
        return Err(e);
    }

    Ok(())
}

/// Copy a directory tree so that it lands exactly at `destination`, whatever
/// the source directory is called.
fn copy_dir_item(
    source: &Path,
    destination: &Path,
    options: &dir::CopyOptions,
) -> Result<(), Error> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    dir::copy(source, destination, options).map_err(Error::other)?;

    Ok(())
}

fn record_file_failure(
    failures: &mut Vec<FileFailure>,
    partition: &str,
    source: &Path,
    destination: &Path,
    required: bool,
    error: impl Into<String>,
) {
    failures.push(FileFailure {
        partition: partition.to_string(),
        source: source.display().to_string(),
        destination: destination.display().to_string(),
        required,
        error: error.into(),
    });
}

/// Record a failure for every item in an entry, e.g. when the destination
/// directory cannot be created or the source partition cannot be mounted.
fn record_entry_failures(
    failures: &mut Vec<FileFailure>,
    entry: &Entry,
    dest_dir: &Path,
    error: &str,
) {
    let source_base = Path::new(&entry.partition).join(entry.from.as_deref().unwrap_or(""));
    for (items, kind) in [(&entry.files, ItemKind::File), (&entry.dirs, ItemKind::Dir)] {
        for item in items {
            let source = source_base.join(item.from());
            let destination = item_destination(dest_dir, item, kind);
            record_file_failure(
                failures,
                &entry.partition,
                &source,
                &destination,
                item.required(),
                error,
            );
        }
    }
}

fn record_partition_source(partitions: &mut Vec<PartitionStatus>, partition: &str, source: &Path) {
    let status = PartitionStatus {
        partition: partition.to_string(),
        source: source.display().to_string(),
    };
    if !partitions.contains(&status) {
        partitions.push(status);
    }
}

/// Copy one entry's items out of a mounted partition. Files and directories
/// share this code path: a missing source, a source of the wrong kind, or a
/// failed copy is recorded as a failure (honouring the item's `required`
/// flag) and processing continues with the remaining items.
fn process_items(
    items: &[Item],
    kind: ItemKind,
    source_base: &Path,
    dest_dir: &Path,
    partition: &str,
    failures: &mut Vec<FileFailure>,
    extracted: &mut Vec<String>,
) {
    // Without overwrite, copying onto an existing folder fails; the folder
    // is then absent from the new status and remove_stale_entries deletes
    // it on the next run, so a populated tree destroys itself. copy_inside
    // makes the destination the tree itself rather than its parent, which
    // is what lets `to:` rename a directory without a staging copy.
    let options = dir::CopyOptions::new().overwrite(true).copy_inside(true);

    for item in items {
        let source = source_base.join(item.from());
        let destination = item_destination(dest_dir, item, kind);

        let metadata = match fs::metadata(&source) {
            Ok(metadata) => metadata,
            Err(e) => {
                warn!(
                    "Unable to find {} on partition {}: {}",
                    item.from(),
                    partition,
                    e
                );
                record_file_failure(
                    failures,
                    partition,
                    &source,
                    &destination,
                    item.required(),
                    format!("source does not exist: {e}"),
                );
                continue;
            }
        };

        let want_dir = kind == ItemKind::Dir;
        if metadata.is_dir() != want_dir {
            let error = if want_dir {
                "source is not a directory"
            } else {
                "source is not a file"
            };
            warn!("{}: {error}", source.display());
            record_file_failure(
                failures,
                partition,
                &source,
                &destination,
                item.required(),
                error,
            );
            continue;
        }

        let result = match kind {
            ItemKind::File => copy_file_item(&source, &destination, item.from().ends_with(".mdt")),
            ItemKind::Dir => copy_dir_item(&source, &destination, &options),
        };

        match result {
            Ok(()) => {
                debug!("Copied {} to {}", source.display(), destination.display());
                extracted.push(destination.display().to_string());
            }
            Err(e) => {
                warn!(
                    "Unable to copy {} to {}: {}",
                    source.display(),
                    destination.display(),
                    e
                );
                record_file_failure(
                    failures,
                    partition,
                    &source,
                    &destination,
                    item.required(),
                    format!("unable to copy: {e}"),
                );
            }
        }
    }
}

/// Dump a raw partition block device into the entry's `to:` file path.
fn dump_raw_entry(
    entry: &Entry,
    destination: &Path,
    active_slot: Option<Slot>,
    partitions: &mut Vec<PartitionStatus>,
    failures: &mut Vec<FileFailure>,
    extracted: &mut Vec<String>,
) {
    debug!(
        "Processing partition {} for raw dump",
        entry.partition.as_str()
    );
    let partlabel_dir = Path::new(PARTLABEL_DIR);
    let origin = match select_partition_path(&entry.partition, active_slot, &[partlabel_dir]) {
        Ok(origin) => origin,
        Err(e) => {
            warn!("Unable to find partition {}: {e}", entry.partition);
            record_file_failure(
                failures,
                &entry.partition,
                Path::new(&entry.partition),
                destination,
                false,
                format!("unable to find partition: {e}"),
            );
            return;
        }
    };
    record_partition_source(partitions, &entry.partition, &origin);

    if let Err(e) = dump_raw(&origin, destination) {
        warn!(
            "Unable to dump partition {} to {}: {}",
            entry.partition,
            destination.display(),
            e
        );
        record_file_failure(
            failures,
            &entry.partition,
            &origin,
            destination,
            false,
            format!("unable to dump partition: {e}"),
        );
        return;
    }

    extracted.push(destination.display().to_string());
}

fn dump_raw(origin: &Path, destination: &Path) -> Result<(), Error> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut input = fs::File::open(origin)?;
    let mut output = fs::File::create(destination)?;

    let mut buffer: Vec<u8> = Vec::new();
    input.read_to_end(&mut buffer)?;
    output.write_all(buffer.as_slice())
}

pub fn process(
    config: Config,
    extract_path: &str,
    mounts_dir: &Path,
    running_kernel_release: Option<&str>,
    active_slot: Option<Slot>,
) -> Result<Status, Error> {
    let mut entries: Vec<String> = Vec::new();
    let mut partitions = Vec::new();
    let mut failures = Vec::new();
    let running_kernel = running_kernel_release.and_then(parse_kernel_version);

    for entry in &config.extract {
        entry.validate()?;
    }

    // Map the "super" partition if we expect one
    if let Some(part) = &config.dynpart {
        info!("Mapping {part} as the dynamic partition container");
        let mapped = map_dynpart(part, active_slot)?;
        for source in mapped {
            partitions.push(PartitionStatus {
                partition: format!("dynpart:{part}"),
                source: source.display().to_string(),
            });
        }

        // Wait up to 500ms to ensure mapped partitions appear under /dev/mapper.
        for _ in 0..5 {
            if let Ok(mapped) = fs::read_dir(MAPPER_DIR)
                && mapped.count() > 1
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    for entry in config.extract {
        let Some(destination) = resolve_destination(&entry.to, running_kernel, &entry.partition)
        else {
            continue;
        };
        let dest_path = destination_path(&destination, extract_path);

        if entry.raw {
            dump_raw_entry(
                &entry,
                &dest_path,
                active_slot,
                &mut partitions,
                &mut failures,
                &mut entries,
            );
            continue;
        }

        if let Err(e) = fs::create_dir_all(&dest_path) {
            warn!("Unable to create folder {}: {}", dest_path.display(), e);
            record_entry_failures(
                &mut failures,
                &entry,
                &dest_path,
                &format!("unable to create destination directory: {e}"),
            );
            continue;
        }

        let mntpath = mounts_dir.join(&entry.partition);

        match mount_part(entry.partition.as_str(), &mntpath, active_slot) {
            Ok(mounted) => {
                record_partition_source(&mut partitions, &entry.partition, mounted.source());
                debug!("Processing items from partition {}", entry.partition);
                let source_base = mounted.path().join(entry.from.as_deref().unwrap_or(""));
                process_items(
                    &entry.files,
                    ItemKind::File,
                    &source_base,
                    &dest_path,
                    &entry.partition,
                    &mut failures,
                    &mut entries,
                );
                process_items(
                    &entry.dirs,
                    ItemKind::Dir,
                    &source_base,
                    &dest_path,
                    &entry.partition,
                    &mut failures,
                    &mut entries,
                );
                mounted.cleanup();
            }
            Err(e) => {
                warn!("Unable to mount partition {}: {e}", entry.partition);
                record_entry_failures(
                    &mut failures,
                    &entry,
                    &dest_path,
                    &format!("unable to mount source partition: {e}"),
                );
            }
        }
    }

    Ok(Status {
        entries,
        kernel_release: running_kernel_release.map(str::to_string),
        active_slot: active_slot.map(|slot| slot.suffix().to_string()),
        partitions,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestRoots {
        base: PathBuf,
        mapper: PathBuf,
        partlabel: PathBuf,
    }

    impl TestRoots {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir().join(format!(
                "blob-wrangler-partitions-{}-{id}",
                std::process::id()
            ));
            let mapper = base.join("mapper");
            let partlabel = base.join("partlabel");
            fs::create_dir_all(&mapper).unwrap();
            fs::create_dir_all(&partlabel).unwrap();
            Self {
                base,
                mapper,
                partlabel,
            }
        }

        fn add_mapper(&self, name: &str) -> PathBuf {
            let path = self.mapper.join(name);
            fs::File::create(&path).unwrap();
            path
        }

        fn add_partlabel(&self, name: &str) -> PathBuf {
            let path = self.partlabel.join(name);
            fs::File::create(&path).unwrap();
            path
        }

        fn roots(&self) -> [&Path; 2] {
            [&self.mapper, &self.partlabel]
        }
    }

    impl Drop for TestRoots {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.base).unwrap();
        }
    }

    #[test]
    fn parse_kernel_release() {
        assert_eq!(
            parse_kernel_version("6.12.58-1.1+sam1"),
            Some(KernelVersion {
                major: 6,
                minor: 12,
                patch: 58,
            })
        );
        assert_eq!(
            parse_kernel_version("7.0.0-rc1"),
            Some(KernelVersion {
                major: 7,
                minor: 0,
                patch: 0,
            })
        );
    }

    #[test]
    fn parses_kernel_constraints() {
        assert_eq!(
            parse_constraint("<6.1"),
            Some((
                ConstraintOp::Lt,
                KernelVersion {
                    major: 6,
                    minor: 1,
                    patch: 0,
                }
            ))
        );
        assert_eq!(
            parse_constraint("<=6.1"),
            Some((
                ConstraintOp::Lte,
                KernelVersion {
                    major: 6,
                    minor: 1,
                    patch: 0,
                }
            ))
        );
        assert_eq!(
            parse_constraint(">7.0"),
            Some((
                ConstraintOp::Gt,
                KernelVersion {
                    major: 7,
                    minor: 0,
                    patch: 0,
                }
            ))
        );
        assert_eq!(
            parse_constraint(">=7.0"),
            Some((
                ConstraintOp::Gte,
                KernelVersion {
                    major: 7,
                    minor: 0,
                    patch: 0,
                }
            ))
        );
        assert_eq!(
            parse_constraint("=5.4"),
            Some((
                ConstraintOp::Eq,
                KernelVersion {
                    major: 5,
                    minor: 4,
                    patch: 0,
                }
            ))
        );
        assert_eq!(parse_constraint("7.0"), None);
        assert_eq!(parse_constraint(""), None);
        assert_eq!(parse_constraint("<bogus"), None);
    }

    #[test]
    fn constraint_operators_match_against_running_kernel() {
        let running = parse_kernel_version("6.6.30").unwrap();

        assert!(constraint_matches(running, "<7.0"));
        assert!(!constraint_matches(running, "<=6.5"));
        assert!(constraint_matches(running, ">6.5"));
        assert!(constraint_matches(running, ">=6.6"));
        assert!(constraint_matches(running, "=6.6.30"));
        assert!(!constraint_matches(running, "=6.7"));
        assert!(constraint_matches(running, "*"));
        assert!(!constraint_matches(running, "bogus"));
    }

    fn kernel_dest(pairs: &[(&str, &str)]) -> Dest {
        Dest::ByKernel(
            pairs
                .iter()
                .map(|(constraint, path)| (constraint.to_string(), path.to_string()))
                .collect(),
        )
    }

    #[test]
    fn kernel_map_first_match_in_document_order_wins() {
        let dest = kernel_dest(&[("<7.0", "qcom/old"), (">=7.0", "qcom/new")]);
        let old = parse_kernel_version("6.17.0").unwrap();
        let new = parse_kernel_version("7.2.0").unwrap();

        assert_eq!(
            resolve_destination(&dest, Some(old), "modem").as_deref(),
            Some("qcom/old")
        );
        assert_eq!(
            resolve_destination(&dest, Some(new), "modem").as_deref(),
            Some("qcom/new")
        );
    }

    #[test]
    fn kernel_map_catch_all_matches_every_kernel() {
        let dest = kernel_dest(&[("<7.0", "qcom/old"), ("*", "qcom/fallback")]);
        let new = parse_kernel_version("7.2.0").unwrap();
        assert_eq!(
            resolve_destination(&dest, Some(new), "modem").as_deref(),
            Some("qcom/fallback")
        );

        let dest = kernel_dest(&[("*", "qcom/fallback"), ("<7.0", "qcom/old")]);
        let old = parse_kernel_version("6.17.0").unwrap();
        assert_eq!(
            resolve_destination(&dest, Some(old), "modem").as_deref(),
            Some("qcom/fallback")
        );
    }

    #[test]
    fn kernel_map_without_match_skips_entry() {
        let dest = kernel_dest(&[("<7.0", "qcom/old")]);
        let new = parse_kernel_version("7.2.0").unwrap();

        assert_eq!(resolve_destination(&dest, Some(new), "modem"), None);
        assert_eq!(resolve_destination(&dest, None, "modem"), None);
    }

    #[test]
    fn fixed_to_always_resolves() {
        let dest = Dest::Fixed("qcom/device".to_string());

        assert_eq!(
            resolve_destination(&dest, None, "modem").as_deref(),
            Some("qcom/device")
        );
    }

    #[test]
    fn relative_to_resolves_against_extract_path_absolute_verbatim() {
        assert_eq!(
            destination_path("qcom/device", "/lib/firmware/updates"),
            PathBuf::from("/lib/firmware/updates/qcom/device")
        );
        assert_eq!(
            destination_path("/var/lib/blob-wrangler/sensors", "/lib/firmware/updates"),
            PathBuf::from("/var/lib/blob-wrangler/sensors")
        );
        assert_eq!(
            destination_path(".", "/lib/firmware/updates"),
            PathBuf::from("/lib/firmware/updates")
        );
    }

    #[test]
    fn items_accept_bare_strings_and_mappings() {
        let items: Vec<Item> = serde_norway::from_str(
            "- adsp.mdt\n- { from: sns_reg_config, to: sensors/sns_reg.conf, required: true }\n",
        )
        .unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].from(), "adsp.mdt");
        assert_eq!(items[0].to(), None);
        assert!(!items[0].required());
        assert_eq!(items[1].from(), "sns_reg_config");
        assert_eq!(items[1].to(), Some("sensors/sns_reg.conf"));
        assert!(items[1].required());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(
            serde_norway::from_str::<Entry>("partition: vendor\nto: qcom\norigin: firmware")
                .is_err()
        );
        assert!(serde_norway::from_str::<Config>("dynpart: system\nextract: []").is_err());
        assert!(serde_norway::from_str::<Config>("dynamic-partition: system\nextract: []").is_ok());
    }

    #[test]
    fn raw_entry_rejects_from_files_and_dirs() {
        let entry: Entry =
            serde_norway::from_str("partition: waveform\nraw: true\nto: rockchip/ebc.wbf").unwrap();
        assert!(entry.validate().is_ok());

        for extra in ["from: image", "files: [adsp.mdt]", "dirs: [image]"] {
            let text = format!("partition: waveform\nraw: true\nto: rockchip/ebc.wbf\n{extra}\n");
            let entry: Entry = serde_norway::from_str(&text).unwrap();
            assert!(
                entry.validate().is_err(),
                "raw entry with '{extra}' must be rejected"
            );
        }
    }

    #[test]
    fn crosshatch_config_uses_kernel_conditional_paths_and_required_files() {
        let config: Config =
            serde_norway::from_str(include_str!("../configs/google,crosshatch.yaml")).unwrap();
        let running = parse_kernel_version("7.2.0").unwrap();
        let old = parse_kernel_version("6.17.0").unwrap();

        assert_eq!(config.dynpart.as_deref(), Some("system"));
        assert!(
            config
                .extract
                .iter()
                .all(|entry| !entry.files.iter().any(|item| item.from() == "ftm5_fw.ftb"))
        );

        let destinations_for = |kernel: Option<KernelVersion>| -> Vec<String> {
            config
                .extract
                .iter()
                .filter_map(|entry| resolve_destination(&entry.to, kernel, &entry.partition))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            destinations_for(Some(running)),
            [
                "qcom/sdm845/Google/blueline",
                "qca",
                "qca",
                "qcom/sdm845/Google/blueline",
            ]
        );

        let old_destinations = destinations_for(Some(old));
        assert!(old_destinations.contains(&"qcom/sdm845/pixel3".to_string()));
        assert!(old_destinations.contains(&"qca/pixel3".to_string()));
        assert!(!old_destinations.contains(&"qcom/sdm845/Google/blueline".to_string()));

        let required = config
            .extract
            .iter()
            .flat_map(|entry| entry.files.iter())
            .filter(|item| item.required())
            .map(|item| item.from())
            .collect::<Vec<_>>();
        assert_eq!(
            required,
            [
                "a630_zap.mdt",
                "adsp.mdt",
                "cdsp.mdt",
                "ipa_fws.mdt",
                "venus.mdt",
                "crnv21.bin",
                "crbtfw21.tlv",
                "mba.mbn",
                "modem.mdt",
            ]
        );
    }

    #[test]
    fn sargo_config_extracts_separate_stock_australian_carrier_profiles() {
        let config: Config =
            serde_norway::from_str(include_str!("../configs/google,sargo.yaml")).unwrap();
        for (carrier_path, output) in [
            ("Telstra/Commercial", "telstra-au-commercial.mbn"),
            ("Optus/Commercial/AU", "optus-au-commercial.mbn"),
        ] {
            let origin =
                format!("rfs/msm/mpss/readonly/vendor/mbn/mcfg_sw/generic/AUNZ/{carrier_path}");
            let entries: Vec<_> = config
                .extract
                .iter()
                .filter(|entry| entry.from.as_deref() == Some(origin.as_str()))
                .collect();
            assert_eq!(entries.len(), 1);
            let entry = entries[0];
            assert_eq!(entry.partition, "vendor");
            assert_eq!(
                resolve_destination(&entry.to, None, &entry.partition).as_deref(),
                Some("qcom/sdm670/sargo/mcfg")
            );
            assert_eq!(entry.files.len(), 1);
            assert_eq!(entry.files[0].from(), "mcfg_sw.mbn");
            assert_eq!(entry.files[0].to(), Some(output));
            assert!(!entry.files[0].required());
        }
    }

    #[test]
    fn fajita_config_collapses_kernel_split_pairs_into_single_entries() {
        let config: Config =
            serde_norway::from_str(include_str!("../configs/oneplus,fajita.yaml")).unwrap();

        assert_eq!(config.extract.len(), 8);
        assert_eq!(
            config
                .extract
                .iter()
                .filter(|entry| matches!(entry.to, Dest::ByKernel(_)))
                .count(),
            3
        );
    }

    #[test]
    fn every_device_config_parses() {
        let configs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("configs");
        for dirent in fs::read_dir(configs_dir).unwrap() {
            let path = dirent.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let contents = fs::read_to_string(&path).unwrap();
            serde_norway::from_str::<Config>(&contents)
                .unwrap_or_else(|e| panic!("unable to parse {}: {e}", path.display()));
        }
    }

    #[test]
    fn file_failure_records_final_mbn_destination() {
        let item = Item::Spec {
            from: "adsp.mdt".to_string(),
            to: None,
            required: true,
        };
        let destination =
            item_destination(Path::new("/updates/qcom/device"), &item, ItemKind::File);
        assert_eq!(destination, PathBuf::from("/updates/qcom/device/adsp.mbn"));

        let mut failures = Vec::new();
        record_file_failure(
            &mut failures,
            "vendor",
            Path::new("/vendor/firmware/adsp.mdt"),
            &destination,
            true,
            "source does not exist",
        );

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].destination, "/updates/qcom/device/adsp.mbn");
        assert!(failures[0].required);
    }

    #[test]
    fn renamed_mdt_files_still_land_as_mbn() {
        let item = Item::Spec {
            from: "vpu20_1v.mdt".to_string(),
            to: Some("venus.mdt".to_string()),
            required: false,
        };

        assert_eq!(
            item_destination(Path::new("/updates/qcom/device"), &item, ItemKind::File),
            PathBuf::from("/updates/qcom/device/venus.mbn")
        );
    }

    #[test]
    fn dir_items_are_named_after_to_or_the_source_directory() {
        let renamed = Item::Spec {
            from: "etc/acdbdata".to_string(),
            to: Some("acdb".to_string()),
            required: false,
        };
        let bare = Item::Path("etc/sensors".to_string());

        assert_eq!(
            item_destination(Path::new("/var/lib/sensors"), &renamed, ItemKind::Dir),
            PathBuf::from("/var/lib/sensors/acdb")
        );
        assert_eq!(
            item_destination(Path::new("/var/lib/sensors"), &bare, ItemKind::Dir),
            PathBuf::from("/var/lib/sensors/sensors")
        );
    }

    #[test]
    fn entry_failure_preserves_declared_required_flags() {
        let entry = Entry {
            partition: "vendor".to_string(),
            from: Some("firmware".to_string()),
            to: Dest::Fixed("qcom/device".to_string()),
            raw: false,
            files: vec![
                Item::Spec {
                    from: "optional.bin".to_string(),
                    to: None,
                    required: false,
                },
                Item::Spec {
                    from: "required.bin".to_string(),
                    to: None,
                    required: true,
                },
            ],
            dirs: Vec::new(),
        };
        let mut failures = Vec::new();

        record_entry_failures(
            &mut failures,
            &entry,
            Path::new("/updates/qcom/device"),
            "unable to mount source partition",
        );

        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.required)
                .collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(failures[0].source, "vendor/firmware/optional.bin");
        assert_eq!(failures[0].destination, "/updates/qcom/device/optional.bin");
    }

    #[test]
    fn required_failures_are_distinguished_from_optional_failures() {
        let failure = |required| FileFailure {
            partition: "vendor".to_string(),
            source: "vendor/firmware/adsp.mdt".to_string(),
            destination: "/lib/firmware/updates/adsp.mbn".to_string(),
            required,
            error: "source does not exist".to_string(),
        };
        let mut status = Status {
            entries: Vec::new(),
            kernel_release: Some("7.2.0".to_string()),
            active_slot: Some("_a".to_string()),
            partitions: Vec::new(),
            failures: vec![failure(false)],
        };
        assert!(!status.has_required_failures());

        status.failures.push(failure(true));
        assert!(status.has_required_failures());
    }

    #[test]
    fn parses_android_slot_command_line_variants() {
        assert_eq!(
            slot_from_cmdline("quiet androidboot.slot_suffix=_a rootwait").unwrap(),
            Some(Slot::A)
        );
        assert_eq!(
            slot_from_cmdline("androidboot.slot=b androidboot.slot_suffix=_b").unwrap(),
            Some(Slot::B)
        );
        assert_eq!(slot_from_cmdline("quiet rootwait").unwrap(), None);
    }

    #[test]
    fn rejects_conflicting_slot_sources() {
        assert!(slot_from_cmdline("androidboot.slot=a androidboot.slot_suffix=_b").is_err());

        let error = reconcile_slots(Some(Slot::A), Some(Slot::B)).unwrap_err();
        assert!(error.to_string().contains("Active slot mismatch"));
        assert_eq!(
            reconcile_slots(Some(Slot::A), Some(Slot::A)).unwrap(),
            Some(Slot::A)
        );
        assert_eq!(reconcile_slots(None, Some(Slot::B)).unwrap(), Some(Slot::B));
    }

    #[test]
    fn known_slot_prefers_active_name_across_all_roots() {
        let roots = TestRoots::new();
        roots.add_mapper("vendor");
        let active = roots.add_partlabel("vendor_a");
        roots.add_mapper("vendor_b");

        assert_eq!(
            select_partition_path("vendor", Some(Slot::A), &roots.roots()).unwrap(),
            active
        );
    }

    #[test]
    fn known_slot_never_falls_through_to_opposite_slot() {
        let roots = TestRoots::new();
        roots.add_mapper("modem_b");

        let error = select_partition_path("modem", Some(Slot::A), &roots.roots()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to use the opposite slot")
        );
    }

    #[test]
    fn known_slot_can_use_unsuffixed_partition() {
        let roots = TestRoots::new();
        let unsuffixed = roots.add_mapper("super");
        roots.add_partlabel("super_b");

        assert_eq!(
            select_partition_path("super", Some(Slot::A), &roots.roots()).unwrap(),
            unsuffixed
        );
    }

    #[test]
    fn unknown_slot_preserves_legacy_a_then_b_preference() {
        let roots = TestRoots::new();
        let part_a = roots.add_partlabel("system_a");
        roots.add_partlabel("system_b");

        assert_eq!(
            select_partition_path("system", None, &roots.roots()).unwrap(),
            part_a
        );
    }

    #[test]
    fn unknown_slot_uses_the_only_available_suffix() {
        let roots = TestRoots::new();
        let only = roots.add_partlabel("persist_b");

        assert_eq!(
            select_partition_path("persist", None, &roots.roots()).unwrap(),
            only
        );
    }

    #[test]
    fn unknown_slot_dynpart_maps_every_available_container() {
        let roots = TestRoots::new();
        let unsuffixed = roots.add_partlabel("system");
        let part_a = roots.add_partlabel("system_a");
        let part_b = roots.add_partlabel("system_b");

        assert_eq!(
            dynpart_paths("system", None, &roots.partlabel).unwrap(),
            [unsuffixed, part_a, part_b]
        );
    }

    #[test]
    fn known_slot_dynpart_maps_only_the_active_container() {
        let roots = TestRoots::new();
        roots.add_partlabel("system_a");
        let part_b = roots.add_partlabel("system_b");

        assert_eq!(
            dynpart_paths("system", Some(Slot::B), &roots.partlabel).unwrap(),
            [part_b]
        );
    }

    #[test]
    fn mounted_path_prefers_filesystem_root() {
        let mountinfo = "
36 25 259:2 /NetworkManager/system-connections /etc/NetworkManager/system-connections rw,relatime - ext4 /dev/sda2 rw
35 25 259:2 / /var/lib/persist rw,relatime - ext4 /dev/sda2 rw
";

        assert_eq!(
            mounted_path_from_mountinfo(mountinfo, 259, 2),
            Some(PathBuf::from("/var/lib/persist"))
        );
    }

    #[test]
    fn mounted_path_decodes_mountinfo_escapes() {
        let mountinfo = "1 0 8:1 / /mnt/foo\\040bar rw,relatime - ext4 /dev/sda1 rw";

        assert_eq!(
            mounted_path_from_mountinfo(mountinfo, 8, 1),
            Some(PathBuf::from("/mnt/foo bar"))
        );
    }
}
