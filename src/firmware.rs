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

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Error, ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use fs_extra::dir;
use goblin::elf::Elf;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use serde::{Deserialize, Serialize};

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";
const MOUNT_FILESYSTEM_TYPES: &[&str] = &["ext4", "erofs", "f2fs", "vfat", "exfat"];
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct MountedPartition {
    path: PathBuf,
    source: PathBuf,
    temporary: bool,
    remove_mountpoint: bool,
}

impl MountedPartition {
    fn existing(path: PathBuf, source: PathBuf) -> Self {
        Self {
            path,
            source,
            temporary: false,
            remove_mountpoint: false,
        }
    }

    fn temporary(path: PathBuf, source: PathBuf, remove_mountpoint: bool) -> Self {
        Self {
            path,
            source,
            temporary: true,
            remove_mountpoint,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn source(&self) -> &Path {
        &self.source
    }
}

impl Drop for MountedPartition {
    fn drop(&mut self) {
        if !self.temporary {
            return;
        }

        if let Err(error) = umount2(&self.path, MntFlags::empty()) {
            warn!(
                "Unable to unmount temporary firmware mount {}: {}",
                self.path.display(),
                error
            );
            return;
        }

        if self.remove_mountpoint
            && let Err(error) = fs::remove_dir(&self.path)
        {
            warn!(
                "Unable to remove temporary firmware mount {}: {}",
                self.path.display(),
                error
            );
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

#[derive(Debug, Deserialize)]
pub struct KernelConstraint {
    lt: Option<String>,
    lte: Option<String>,
    gt: Option<String>,
    gte: Option<String>,
    eq: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FwFile {
    name: String,
    rename: Option<String>,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
pub struct FwConfig {
    partition: String,
    origin: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    files: Vec<FwFile>,
}

#[derive(Debug, Deserialize)]
pub struct FwFolder {
    partition: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    #[serde(default)]
    folders: Vec<FwFile>,
    /// Individual files copied to the same absolute destination. Firmware
    /// entries can already rename a file, but folder entries could only copy
    /// whole directories, so a tree that needs one file under a different name
    /// could not be expressed.
    #[serde(default)]
    files: Vec<FwFile>,
}

#[derive(Debug, Deserialize)]
pub struct DumpConfig {
    partition: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    filename: String,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    dynpart: Option<String>,
    #[serde(default)]
    firmware: Vec<FwConfig>,
    folders: Option<Vec<FwFolder>>,
    partdump: Option<Vec<DumpConfig>>,
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

impl Config {
    pub fn dynpart(&self) -> Option<&str> {
        self.dynpart.as_deref()
    }

    pub fn referenced_partitions(&self) -> impl Iterator<Item = &str> {
        let mut partitions = BTreeSet::new();
        partitions.extend(self.firmware.iter().map(|entry| entry.partition.as_str()));
        partitions.extend(
            self.folders
                .iter()
                .flatten()
                .map(|entry| entry.partition.as_str()),
        );
        partitions.extend(
            self.partdump
                .iter()
                .flatten()
                .map(|entry| entry.partition.as_str()),
        );
        partitions.into_iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedPartition {
    BlockDevice(PathBuf),
    MountedDirectory(PathBuf),
}

pub trait PartitionResolver: Send + Sync {
    fn resolve(&self, partition: &str) -> io::Result<Option<ResolvedPartition>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractOptions {
    pub extract_path: PathBuf,
    pub mounts_dir: PathBuf,
    pub running_kernel_release: Option<String>,
    pub active_slot: Option<Slot>,
}

impl ExtractOptions {
    pub fn new(extract_path: impl Into<PathBuf>, mounts_dir: impl Into<PathBuf>) -> Self {
        Self {
            extract_path: extract_path.into(),
            mounts_dir: mounts_dir.into(),
            running_kernel_release: None,
            active_slot: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MissingItem {
    Partition { partition: String },
    File { partition: String, path: PathBuf },
    Directory { partition: String, path: PathBuf },
    PartitionDump { partition: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExtractionReport {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub missing: Vec<MissingItem>,
    pub kernel_release: Option<String>,
    pub active_slot: Option<String>,
    pub partitions: Vec<PartitionStatus>,
    pub failures: Vec<FileFailure>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub files: Vec<String>,
    pub folders: Option<Vec<String>>,
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

impl From<&ExtractionReport> for Status {
    fn from(report: &ExtractionReport) -> Self {
        let folders = (!report.directories.is_empty()).then(|| {
            report
                .directories
                .iter()
                .map(|path| path.display().to_string())
                .collect()
        });

        Self {
            files: report
                .files
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            folders,
            kernel_release: report.kernel_release.clone(),
            active_slot: report.active_slot.clone(),
            partitions: report.partitions.clone(),
            failures: report.failures.clone(),
        }
    }
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

fn kernel_filter_match(
    filter: &Option<KernelConstraint>,
    running_kernel: Option<&KernelVersion>,
    entry_type: &str,
    partition: &str,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };

    let Some(running_kernel) = running_kernel else {
        warn!(
            "Unable to parse running kernel release, processing {entry_type} entry on partition {} without kernel filtering",
            partition
        );
        return true;
    };

    let parse_condition = |condition: &str, value: &str| {
        let parsed = parse_kernel_version(value);
        if parsed.is_none() {
            warn!(
                "Ignoring invalid kernel filter '{}' = '{}' for {} entry on partition {}",
                condition, value, entry_type, partition
            );
        }
        parsed
    };

    if let Some(lt) = filter.lt.as_deref().and_then(|v| parse_condition("lt", v))
        && *running_kernel >= lt
    {
        return false;
    }

    if let Some(lte) = filter
        .lte
        .as_deref()
        .and_then(|v| parse_condition("lte", v))
        && *running_kernel > lte
    {
        return false;
    }

    if let Some(gt) = filter.gt.as_deref().and_then(|v| parse_condition("gt", v))
        && *running_kernel <= gt
    {
        return false;
    }

    if let Some(gte) = filter
        .gte
        .as_deref()
        .and_then(|v| parse_condition("gte", v))
        && *running_kernel < gte
    {
        return false;
    }

    if let Some(eq) = filter.eq.as_deref().and_then(|v| parse_condition("eq", v))
        && *running_kernel != eq
    {
        return false;
    }

    true
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

#[derive(Debug, Eq, PartialEq)]
struct ExistingMount {
    path: PathBuf,
    read_only: bool,
}

fn mounted_path_from_mountinfo(
    mountinfo: &str,
    source_major: u64,
    source_minor: u64,
) -> Option<ExistingMount> {
    let mut fallback = None;

    for line in mountinfo.lines() {
        let fields = line.split(' ').collect::<Vec<_>>();
        if fields.len() < 6 {
            continue;
        }

        if parse_mountinfo_device(fields[2]) != Some((source_major, source_minor)) {
            continue;
        }

        let mountpoint = ExistingMount {
            path: decode_mountinfo_path(fields[4]),
            read_only: fields[5].split(',').any(|option| option == "ro"),
        };
        if fields[3] == "/" {
            return Some(mountpoint);
        }

        if fallback.is_none() {
            fallback = Some(mountpoint);
        }
    }

    fallback
}

fn already_mounted_path(srcpath: &Path) -> Option<ExistingMount> {
    let (source_major, source_minor) = device_numbers(fs::metadata(srcpath).ok()?.rdev());
    let mountinfo = fs::read_to_string(MOUNTINFO_PATH).ok()?;

    mounted_path_from_mountinfo(&mountinfo, source_major, source_minor)
}

fn mounted_partition(srcpath: &Path) -> Result<Option<MountedPartition>, Error> {
    let Some(mountpoint) = already_mounted_path(srcpath) else {
        return Ok(None);
    };
    if !mountpoint.read_only {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "Refusing to use read-write mount {} for partition {}",
                mountpoint.path.display(),
                srcpath.display()
            ),
        ));
    }
    debug!(
        "Using already mounted partition {} at {}",
        srcpath.display(),
        mountpoint.path.display()
    );
    Ok(Some(MountedPartition::existing(
        mountpoint.path,
        srcpath.to_path_buf(),
    )))
}

fn mount_srcpath(srcpath: &Path, mountpath: &Path) -> Result<MountedPartition, Error> {
    let metadata = fs::metadata(srcpath)?;
    if !metadata.file_type().is_block_device() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{} is not a block device", srcpath.display()),
        ));
    }

    if let Some(mounted) = mounted_partition(srcpath)? {
        return Ok(mounted);
    }

    let remove_mountpoint = !mountpath.exists();
    fs::DirBuilder::new().recursive(true).create(mountpath)?;
    let flags = MsFlags::MS_RDONLY | MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC;

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
                    remove_mountpoint,
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

        if let Some(mounted) = mounted_partition(srcpath)? {
            return Ok(mounted);
        }
    }

    match last_error {
        Some(e) => {
            if let Some(mounted) = mounted_partition(srcpath)? {
                return Ok(mounted);
            }

            error!(
                "Unable to mount {} on {} as any supported filesystem: {}",
                srcpath.display(),
                mountpath.display(),
                e
            );
            if remove_mountpoint {
                let _ = fs::remove_dir(mountpath);
            }
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

pub fn select_partition_path(
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

fn resolve_partition(
    resolver: &dyn PartitionResolver,
    partition: &str,
    mountpath: &Path,
) -> Result<Option<MountedPartition>, Error> {
    match resolver.resolve(partition)? {
        Some(ResolvedPartition::BlockDevice(path)) => mount_srcpath(&path, mountpath).map(Some),
        Some(ResolvedPartition::MountedDirectory(path)) => {
            if !fs::metadata(&path)?.is_dir() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("{} is not a mounted directory", path.display()),
                ));
            }
            Ok(Some(MountedPartition::existing(path.clone(), path)))
        }
        None => Ok(None),
    }
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

    let mut count = 0_u32;
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

        let segment_size = usize::try_from(phdr.p_filesz).map_err(|_| {
            Error::new(
                ErrorKind::InvalidData,
                format!("ELF segment is too large in {}", inpath.display()),
            )
        })?;
        let mut segment = Vec::new();

        if (phdr.p_flags & FLAGS_READ_MASK) == FLAGS_MDT_VALUE {
            mdt_fd.seek(SeekFrom::Start(hashoffset))?;
            segment.resize(segment_size, 0);
            mdt_fd.read_exact(&mut segment)?;
        } else {
            let mut bxx_name = inpath.to_path_buf();
            bxx_name.set_extension(format!("b{:#02}", count - 1));

            let mut bxx_fd = fs::File::open(&bxx_name)?;
            bxx_fd.read_to_end(&mut segment)?;
        }

        if segment.len() != segment_size {
            let err_str = format!("Read {} bytes (!= {})", segment.len(), phdr.p_filesz);
            return Err(Error::new(ErrorKind::UnexpectedEof, err_str));
        }

        write_all_at(&mbn_fd, &segment, phdr.p_offset)?;
    }

    Ok(())
}

pub fn dynpart_paths(
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

fn firmware_destination(destpath: &Path, file: &FwFile) -> PathBuf {
    let mut destination = destpath.join(&file.name);
    if let Some(new_name) = &file.rename {
        destination.set_file_name(new_name);
    }
    if file.name.ends_with(".mdt") {
        destination.set_extension("mbn");
    }
    destination
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

fn record_entry_failures(
    failures: &mut Vec<FileFailure>,
    entry: &FwConfig,
    destpath: &Path,
    error: &str,
) {
    for file in &entry.files {
        let source = Path::new(&entry.partition)
            .join(&entry.origin)
            .join(&file.name);
        let destination = firmware_destination(destpath, file);
        record_file_failure(
            failures,
            &entry.partition,
            &source,
            &destination,
            file.required,
            error,
        );
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

fn write_all_at(file: &fs::File, mut buffer: &[u8], mut offset: u64) -> Result<(), Error> {
    while !buffer.is_empty() {
        let written = file.write_at(buffer, offset)?;
        if written == 0 {
            return Err(Error::new(
                ErrorKind::WriteZero,
                "unable to write squashed firmware segment",
            ));
        }
        buffer = &buffer[written..];
        offset += written as u64;
    }

    Ok(())
}

struct TemporaryOutput {
    path: PathBuf,
    committed: bool,
}

impl TemporaryOutput {
    fn create(destination: &Path) -> Result<Self, Error> {
        let parent = destination.parent().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Output path {} has no parent", destination.display()),
            )
        })?;
        let file_name = destination.file_name().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("Output path {} has no file name", destination.display()),
            )
        })?;

        for _ in 0..128 {
            let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temp_name = OsString::from(".");
            temp_name.push(file_name);
            temp_name.push(format!(
                ".blob-wrangler-{}-{sequence}.tmp",
                std::process::id()
            ));
            let path = parent.join(temp_name);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {
                    return Ok(Self {
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "Unable to allocate a temporary output beside {}",
                destination.display()
            ),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(mut self, destination: &Path) -> Result<(), Error> {
        fs::rename(&self.path, destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            warn!(
                "Unable to remove temporary output {}: {}",
                self.path.display(),
                error
            );
        }
    }
}

fn copy_file(origin: &Path, destination: &Path) -> Result<(), Error> {
    let temporary = TemporaryOutput::create(destination)?;
    fs::copy(origin, temporary.path())?;
    temporary.commit(destination)
}

fn squash_firmware(origin: &Path, destination: &Path) -> Result<(), Error> {
    let temporary = TemporaryOutput::create(destination)?;
    squash_file(origin, temporary.path())?;
    temporary.commit(destination)
}

fn dump_partition(origin: &Path, destination: &Path) -> Result<(), Error> {
    let temporary = TemporaryOutput::create(destination)?;
    let mut input = fs::File::open(origin)?;
    let mut output = fs::File::create(temporary.path())?;
    io::copy(&mut input, &mut output)?;
    drop(output);
    temporary.commit(destination)
}

fn missing_partition(report: &mut ExtractionReport, partition: &str) {
    let item = MissingItem::Partition {
        partition: partition.to_string(),
    };
    if !report.missing.contains(&item) {
        report.missing.push(item);
    }
}

pub fn extract(
    config: &Config,
    resolver: &dyn PartitionResolver,
    options: &ExtractOptions,
) -> Result<ExtractionReport, Error> {
    let running_kernel_release = options.running_kernel_release.as_deref();
    let running_kernel = running_kernel_release.and_then(parse_kernel_version);
    let mut report = ExtractionReport {
        kernel_release: options.running_kernel_release.clone(),
        active_slot: options.active_slot.map(|slot| slot.suffix().to_string()),
        ..ExtractionReport::default()
    };

    if running_kernel_release.is_some() && running_kernel.is_none() {
        warn!("Unable to parse running kernel release, kernel filtering disabled");
    }

    for entry in &config.firmware {
        if !kernel_filter_match(
            &entry.kernel,
            running_kernel.as_ref(),
            "firmware",
            &entry.partition,
        ) {
            continue;
        }

        let destpath = options.extract_path.join(&entry.destination);

        if let Err(e) = fs::create_dir_all(&destpath) {
            warn!("Unable to create folder {}: {}", destpath.display(), e);
            record_entry_failures(
                &mut report.failures,
                entry,
                &destpath,
                &format!("unable to create destination directory: {e}"),
            );
            report
                .missing
                .extend(entry.files.iter().map(|file| MissingItem::File {
                    partition: entry.partition.clone(),
                    path: PathBuf::from(&entry.origin).join(&file.name),
                }));
            continue;
        }

        let mntpath = options.mounts_dir.join(&entry.partition);
        let mounted = match resolve_partition(resolver, &entry.partition, &mntpath) {
            Ok(Some(mounted)) => {
                record_partition_source(&mut report.partitions, &entry.partition, mounted.source());
                mounted
            }
            Ok(None) => {
                warn!("Unable to resolve partition {}", entry.partition);
                missing_partition(&mut report, &entry.partition);
                record_entry_failures(
                    &mut report.failures,
                    entry,
                    &destpath,
                    "unable to resolve source partition",
                );
                continue;
            }
            Err(e) => {
                warn!("Unable to mount partition {}: {e}", entry.partition);
                record_entry_failures(
                    &mut report.failures,
                    entry,
                    &destpath,
                    &format!("unable to mount source partition: {e}"),
                );
                continue;
            }
        };

        debug!(
            "Processing firmware files from partition {}",
            entry.partition
        );
        for file in &entry.files {
            let relative_origin = PathBuf::from(&entry.origin).join(&file.name);
            let origin = mounted.path().join(&relative_origin);
            let destination = firmware_destination(&destpath, file);
            if !origin.exists() {
                warn!(
                    "Unable to find {} on partition {}",
                    file.name, entry.partition
                );
                report.missing.push(MissingItem::File {
                    partition: entry.partition.clone(),
                    path: relative_origin,
                });
                record_file_failure(
                    &mut report.failures,
                    &entry.partition,
                    &origin,
                    &destination,
                    file.required,
                    "source file does not exist",
                );
                continue;
            }

            debug!("Copying firmware file {}", origin.display());

            let squashed = file.name.ends_with(".mdt");
            let result = if squashed {
                trace!("Squashing MDT file into MBN");
                squash_firmware(&origin, &destination)
            } else {
                copy_file(&origin, &destination)
            };

            if let Err(e) = result {
                warn!(
                    "Unable to copy {} to {}: {}",
                    origin.display(),
                    destination.display(),
                    e
                );
                report.missing.push(MissingItem::File {
                    partition: entry.partition.clone(),
                    path: relative_origin,
                });
                record_file_failure(
                    &mut report.failures,
                    &entry.partition,
                    &origin,
                    &destination,
                    file.required,
                    if squashed {
                        format!("unable to squash MDT: {e}")
                    } else {
                        format!("unable to copy file: {e}")
                    },
                );
                continue;
            }

            report.files.push(destination);
        }
    }

    if let Some(dirs) = &config.folders {
        // Without overwrite, copying onto an existing folder fails; the folder
        // is then absent from the new status and remove_stale_entries deletes
        // it on the next run, so a populated tree destroys itself.
        let copy_options = dir::CopyOptions::new().overwrite(true);

        for entry in dirs {
            if !kernel_filter_match(
                &entry.kernel,
                running_kernel.as_ref(),
                "folder",
                &entry.partition,
            ) {
                continue;
            }

            let destpath = PathBuf::from(&entry.destination);

            if let Err(e) = fs::create_dir_all(&destpath) {
                warn!("Unable to create folder {}: {}", destpath.display(), e);
                report
                    .missing
                    .extend(entry.folders.iter().map(|folder| MissingItem::Directory {
                        partition: entry.partition.clone(),
                        path: PathBuf::from(&folder.name),
                    }));
                continue;
            }

            let mntpath = options.mounts_dir.join(&entry.partition);
            let mounted = match resolve_partition(resolver, &entry.partition, &mntpath) {
                Ok(Some(mounted)) => {
                    record_partition_source(
                        &mut report.partitions,
                        &entry.partition,
                        mounted.source(),
                    );
                    mounted
                }
                Ok(None) => {
                    warn!("Unable to resolve partition {}", entry.partition);
                    missing_partition(&mut report, &entry.partition);
                    continue;
                }
                Err(e) => {
                    warn!("Unable to mount partition {}: {e}", entry.partition);
                    continue;
                }
            };

            debug!("Processing folders from partition {}", entry.partition);
            for folder in &entry.folders {
                let relative_origin = PathBuf::from(&folder.name);
                let origin = mounted.path().join(&relative_origin);
                if !origin.exists() {
                    warn!(
                        "Unable to find {} on partition {}",
                        folder.name, entry.partition
                    );
                    report.missing.push(MissingItem::Directory {
                        partition: entry.partition.clone(),
                        path: relative_origin,
                    });
                    continue;
                }

                debug!("Copying folder {}", origin.display());

                if let Err(e) = dir::copy(&origin, &destpath, &copy_options) {
                    warn!(
                        "Unable to copy {} to {}: {}",
                        origin.display(),
                        destpath.display(),
                        e
                    );
                    report.missing.push(MissingItem::Directory {
                        partition: entry.partition.clone(),
                        path: relative_origin,
                    });
                    continue;
                }

                let Some(folder_name) = origin.file_name() else {
                    warn!("Unable to determine folder name for {}", origin.display());
                    report.missing.push(MissingItem::Directory {
                        partition: entry.partition.clone(),
                        path: relative_origin,
                    });
                    continue;
                };
                let initial_folder = destpath.join(folder_name);
                let mut destination = initial_folder.clone();
                if let Some(new_name) = &folder.rename {
                    destination.set_file_name(new_name);
                    if let Err(error) = fs::rename(&initial_folder, &destination) {
                        warn!(
                            "Unable to rename {} to {}: {}",
                            initial_folder.display(),
                            destination.display(),
                            error
                        );
                        destination = initial_folder;
                        report.missing.push(MissingItem::Directory {
                            partition: entry.partition.clone(),
                            path: relative_origin,
                        });
                    }
                }
                report.directories.push(destination);
            }

            for file in &entry.files {
                let origin = mounted.path().join(&file.name);
                if !origin.exists() {
                    warn!(
                        "Unable to find {} on partition {}",
                        file.name, entry.partition
                    );
                    record_file_failure(
                        &mut report.failures,
                        &entry.partition,
                        &origin,
                        &destpath.join(&file.name),
                        file.required,
                        "source file does not exist",
                    );
                    continue;
                }

                let target = match (&file.rename, origin.file_name()) {
                    (Some(new_name), _) => destpath.join(new_name),
                    (None, Some(name)) => destpath.join(name),
                    (None, None) => {
                        warn!("Unable to determine file name for {}", origin.display());
                        record_file_failure(
                            &mut report.failures,
                            &entry.partition,
                            &origin,
                            &destpath.join(&file.name),
                            file.required,
                            "unable to determine file name",
                        );
                        continue;
                    }
                };

                debug!("Copying file {} to {}", origin.display(), target.display());

                if let Err(e) = copy_file(&origin, &target) {
                    warn!(
                        "Unable to copy {} to {}: {}",
                        origin.display(),
                        target.display(),
                        e
                    );
                    record_file_failure(
                        &mut report.failures,
                        &entry.partition,
                        &origin,
                        &target,
                        file.required,
                        format!("unable to copy file: {e}"),
                    );
                    continue;
                }

                report.directories.push(target);
            }
        }
    }

    if let Some(dumps) = &config.partdump {
        for entry in dumps {
            if !kernel_filter_match(
                &entry.kernel,
                running_kernel.as_ref(),
                "partdump",
                &entry.partition,
            ) {
                continue;
            }

            debug!(
                "Processing partition {} for raw dump",
                entry.partition.as_str()
            );
            let destpath = options.extract_path.join(&entry.destination);
            if let Err(e) = fs::create_dir_all(&destpath) {
                warn!("Unable to create folder {}: {}", destpath.display(), e);
                report.missing.push(MissingItem::PartitionDump {
                    partition: entry.partition.clone(),
                });
                continue;
            }

            let origin = match resolver.resolve(&entry.partition)? {
                Some(ResolvedPartition::BlockDevice(path)) => {
                    record_partition_source(&mut report.partitions, &entry.partition, &path);
                    path
                }
                Some(ResolvedPartition::MountedDirectory(path)) => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!(
                            "Cannot make a raw dump of mounted directory {} for partition {}",
                            path.display(),
                            entry.partition
                        ),
                    ));
                }
                None => {
                    warn!("Unable to resolve partition {}", entry.partition);
                    report.missing.push(MissingItem::PartitionDump {
                        partition: entry.partition.clone(),
                    });
                    continue;
                }
            };

            let destination = destpath.join(&entry.filename);
            if let Err(error) = dump_partition(&origin, &destination) {
                warn!(
                    "Unable to dump partition {} to {}: {}",
                    entry.partition,
                    destination.display(),
                    error
                );
                report.missing.push(MissingItem::PartitionDump {
                    partition: entry.partition.clone(),
                });
                continue;
            }

            report.files.push(destination);
        }
    }

    Ok(report)
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

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Self {
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "blob-wrangler-test-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestResolver {
        partition: &'static str,
        directory: PathBuf,
    }

    impl PartitionResolver for TestResolver {
        fn resolve(&self, partition: &str) -> io::Result<Option<ResolvedPartition>> {
            Ok((partition == self.partition)
                .then(|| ResolvedPartition::MountedDirectory(self.directory.clone())))
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
    fn kernel_filter_range_matching() {
        let running = parse_kernel_version("6.17.0").unwrap();
        let old_path_filter = Some(KernelConstraint {
            lt: Some("7.0".to_string()),
            lte: None,
            gt: None,
            gte: None,
            eq: None,
        });
        let new_path_filter = Some(KernelConstraint {
            lt: None,
            lte: None,
            gt: None,
            gte: Some("7.0".to_string()),
            eq: None,
        });

        assert!(kernel_filter_match(
            &old_path_filter,
            Some(&running),
            "firmware",
            "modem"
        ));
        assert!(!kernel_filter_match(
            &new_path_filter,
            Some(&running),
            "firmware",
            "modem"
        ));
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

    #[derive(Deserialize)]
    struct ConfigFile {
        wrangler: Config,
    }

    #[test]
    fn crosshatch_config_uses_kernel_7_paths_and_required_files() {
        let config: ConfigFile =
            toml::from_str(include_str!("../configs/google,crosshatch.toml")).unwrap();
        let config = config.wrangler;
        let running = parse_kernel_version("7.2.0").unwrap();

        assert_eq!(config.dynpart.as_deref(), Some("system"));
        assert!(
            config
                .firmware
                .iter()
                .all(|entry| { !entry.files.iter().any(|file| file.name == "ftm5_fw.ftb") })
        );

        let qcom_entries = config
            .firmware
            .iter()
            .filter(|entry| {
                kernel_filter_match(&entry.kernel, Some(&running), "firmware", &entry.partition)
                    && entry.destination.starts_with("qcom/")
            })
            .collect::<Vec<_>>();
        assert_eq!(qcom_entries.len(), 2);
        assert!(
            qcom_entries
                .iter()
                .all(|entry| { entry.destination == "qcom/sdm845/Google/blueline" })
        );

        let required = config
            .firmware
            .iter()
            .filter(|entry| {
                kernel_filter_match(&entry.kernel, Some(&running), "firmware", &entry.partition)
            })
            .flat_map(|entry| entry.files.iter())
            .filter(|file| file.required)
            .map(|file| file.name.as_str())
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

        let old_kernel = parse_kernel_version("6.17.0").unwrap();
        let old_destinations = config
            .firmware
            .iter()
            .filter(|entry| {
                kernel_filter_match(
                    &entry.kernel,
                    Some(&old_kernel),
                    "firmware",
                    &entry.partition,
                )
            })
            .map(|entry| entry.destination.as_str())
            .collect::<Vec<_>>();
        assert!(old_destinations.contains(&"qcom/sdm845/pixel3"));
        assert!(old_destinations.contains(&"qca/pixel3"));
        assert!(!old_destinations.contains(&"qcom/sdm845/Google/blueline"));
    }

    #[test]
    fn sargo_config_extracts_separate_stock_australian_carrier_profiles() {
        let config: ConfigFile =
            toml::from_str(include_str!("../configs/google,sargo.toml")).unwrap();
        for (carrier_path, output) in [
            ("Telstra/Commercial", "telstra-au-commercial.mbn"),
            ("Optus/Commercial/AU", "optus-au-commercial.mbn"),
        ] {
            let origin =
                format!("rfs/msm/mpss/readonly/vendor/mbn/mcfg_sw/generic/AUNZ/{carrier_path}");
            let entries: Vec<_> = config
                .wrangler
                .firmware
                .iter()
                .filter(|entry| entry.origin == origin)
                .collect();
            assert_eq!(entries.len(), 1);
            let entry = entries[0];
            assert_eq!(entry.partition, "vendor");
            assert_eq!(entry.destination, "qcom/sdm670/sargo/mcfg");
            assert_eq!(entry.files.len(), 1);
            assert_eq!(entry.files[0].name, "mcfg_sw.mbn");
            assert_eq!(entry.files[0].rename.as_deref(), Some(output));
            assert!(!entry.files[0].required);
        }
    }

    #[test]
    fn every_device_config_parses() {
        for contents in [
            include_str!("../configs/fairphone,fp4.toml"),
            include_str!("../configs/fairphone,fp5.toml"),
            include_str!("../configs/google,blueline.toml"),
            include_str!("../configs/google,bonito-sdc.toml"),
            include_str!("../configs/google,crosshatch.toml"),
            include_str!("../configs/google,sargo.toml"),
            include_str!("../configs/google,sunfish.toml"),
            include_str!("../configs/nothing,spacewar.toml"),
            include_str!("../configs/oneplus,enchilada.toml"),
            include_str!("../configs/oneplus,fajita.toml"),
            include_str!("../configs/pine64,pinenote.toml"),
            include_str!("../configs/samsung,starqltechn.toml"),
            include_str!("../configs/shift,axolotl.toml"),
            include_str!("../configs/shift,otter.toml"),
            include_str!("../configs/xiaomi,beryllium.toml"),
            include_str!("../configs/xiaomi,davinci.toml"),
            include_str!("../configs/xiaomi,polaris.toml"),
        ] {
            toml::from_str::<ConfigFile>(contents).unwrap();
        }
    }

    #[test]
    fn file_failure_records_final_mbn_destination() {
        let file = FwFile {
            name: "adsp.mdt".to_string(),
            rename: None,
            required: true,
        };
        let mut failures = Vec::new();
        let destination = firmware_destination(Path::new("/updates/qcom/device"), &file);
        record_file_failure(
            &mut failures,
            "vendor",
            Path::new("/vendor/firmware/adsp.mdt"),
            &destination,
            true,
            "source file does not exist",
        );

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].destination, "/updates/qcom/device/adsp.mbn");
        assert!(failures[0].required);
    }

    #[test]
    fn entry_failure_preserves_declared_required_flags() {
        let entry = FwConfig {
            partition: "vendor".to_string(),
            origin: "firmware".to_string(),
            destination: "qcom/device".to_string(),
            kernel: None,
            files: vec![
                FwFile {
                    name: "optional.bin".to_string(),
                    rename: None,
                    required: false,
                },
                FwFile {
                    name: "required.bin".to_string(),
                    rename: None,
                    required: true,
                },
            ],
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
    }

    #[test]
    fn old_status_json_remains_readable() {
        let status: Status = serde_json::from_str(
            r#"{"files":["/firmware/adsp.mbn"],"folders":null,"kernel_release":"7.2.0"}"#,
        )
        .unwrap();

        assert_eq!(status.active_slot, None);
        assert!(status.partitions.is_empty());
        assert!(status.failures.is_empty());
        assert!(!status.has_required_failures());
    }

    #[test]
    fn required_failures_are_distinguished_from_optional_failures() {
        let failure = |required| FileFailure {
            partition: "vendor".to_string(),
            source: "vendor/firmware/adsp.mdt".to_string(),
            destination: "/lib/firmware/updates/adsp.mbn".to_string(),
            required,
            error: "source file does not exist".to_string(),
        };
        let mut status = Status {
            files: Vec::new(),
            folders: None,
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
    fn mounted_path_prefers_filesystem_root() {
        let mountinfo = "
36 25 259:2 /NetworkManager/system-connections /etc/NetworkManager/system-connections rw,relatime - ext4 /dev/sda2 rw
35 25 259:2 / /var/lib/persist rw,relatime - ext4 /dev/sda2 rw
";

        assert_eq!(
            mounted_path_from_mountinfo(mountinfo, 259, 2),
            Some(ExistingMount {
                path: PathBuf::from("/var/lib/persist"),
                read_only: false,
            })
        );
    }

    #[test]
    fn mounted_path_decodes_mountinfo_escapes() {
        let mountinfo = "1 0 8:1 / /mnt/foo\\040bar rw,relatime - ext4 /dev/sda1 rw";

        assert_eq!(
            mounted_path_from_mountinfo(mountinfo, 8, 1),
            Some(ExistingMount {
                path: PathBuf::from("/mnt/foo bar"),
                read_only: false,
            })
        );
    }

    #[test]
    fn mounted_path_records_read_only_state() {
        let mountinfo = "1 0 8:1 / /mnt/vendor ro,nodev - ext4 /dev/sda1 ro";

        assert_eq!(
            mounted_path_from_mountinfo(mountinfo, 8, 1),
            Some(ExistingMount {
                path: PathBuf::from("/mnt/vendor"),
                read_only: true,
            })
        );
    }

    #[test]
    fn referenced_partitions_are_unique() {
        let config = Config {
            dynpart: Some("super".to_string()),
            firmware: vec![FwConfig {
                partition: "vendor".to_string(),
                origin: "firmware".to_string(),
                destination: "device".to_string(),
                kernel: None,
                files: Vec::new(),
            }],
            folders: Some(vec![FwFolder {
                partition: "vendor".to_string(),
                destination: "/tmp/sensors".to_string(),
                kernel: None,
                folders: Vec::new(),
                files: Vec::new(),
            }]),
            partdump: Some(vec![DumpConfig {
                partition: "waveform".to_string(),
                destination: "device".to_string(),
                kernel: None,
                filename: "waveform.bin".to_string(),
            }]),
        };

        assert_eq!(config.dynpart(), Some("super"));
        assert_eq!(
            config.referenced_partitions().collect::<Vec<_>>(),
            vec!["vendor", "waveform"]
        );
    }

    #[test]
    fn extracts_from_mounted_directory_and_reports_missing_items() {
        let test = TestDirectory::create();
        let source = test.path().join("source");
        let firmware = source.join("firmware");
        fs::create_dir_all(&firmware).unwrap();
        fs::write(firmware.join("present.bin"), b"firmware").unwrap();

        let config = Config {
            dynpart: None,
            firmware: vec![FwConfig {
                partition: "vendor".to_string(),
                origin: "firmware".to_string(),
                destination: "device".to_string(),
                kernel: None,
                files: vec![
                    FwFile {
                        name: "present.bin".to_string(),
                        rename: None,
                        required: false,
                    },
                    FwFile {
                        name: "missing.bin".to_string(),
                        rename: None,
                        required: false,
                    },
                ],
            }],
            folders: None,
            partdump: None,
        };
        let resolver = TestResolver {
            partition: "vendor",
            directory: source,
        };
        let output = test.path().join("output");
        let options = ExtractOptions {
            extract_path: output.clone(),
            mounts_dir: test.path().join("mounts"),
            running_kernel_release: Some("6.12.0-test".to_string()),
            active_slot: None,
        };

        let report = extract(&config, &resolver, &options).unwrap();

        let destination = output.join("device/present.bin");
        assert_eq!(fs::read(&destination).unwrap(), b"firmware");
        assert_eq!(report.files, vec![destination]);
        assert_eq!(
            report.missing,
            vec![MissingItem::File {
                partition: "vendor".to_string(),
                path: PathBuf::from("firmware/missing.bin"),
            }]
        );
        assert_eq!(report.kernel_release.as_deref(), Some("6.12.0-test"));
        assert!(fs::read_dir(output.join("device")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));
    }

    #[test]
    fn failed_atomic_copy_preserves_existing_destination() {
        let test = TestDirectory::create();
        let destination = test.path().join("firmware.bin");
        fs::write(&destination, b"existing").unwrap();

        assert!(copy_file(&test.path().join("missing.bin"), &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
        assert_eq!(fs::read_dir(test.path()).unwrap().count(), 1);
    }
}
