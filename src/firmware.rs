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
use std::{fs, thread, time::Duration};

use std::io::prelude::*;

use fs_extra::dir;
use goblin::elf::Elf;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use serde::{Deserialize, Serialize};

use crate::utils;

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;
const PARTLABEL_DIR: &str = "/dev/disk/by-partlabel";
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";
const MOUNT_FILESYSTEM_TYPES: &[&str] = &["ext4", "erofs", "f2fs", "vfat", "exfat"];

struct MountedPartition {
    path: PathBuf,
    temporary: bool,
}

impl MountedPartition {
    fn existing(path: PathBuf) -> Self {
        Self {
            path,
            temporary: false,
        }
    }

    fn temporary(path: PathBuf) -> Self {
        Self {
            path,
            temporary: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(self) {
        if self.temporary {
            let _res = umount2(&self.path, MntFlags::empty());
            let _r = fs::remove_dir(self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct KernelVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

#[derive(Deserialize)]
pub struct KernelConstraint {
    lt: Option<String>,
    lte: Option<String>,
    gt: Option<String>,
    gte: Option<String>,
    eq: Option<String>,
}

#[derive(Deserialize)]
pub struct FwFile {
    name: String,
    rename: Option<String>,
}

#[derive(Deserialize)]
pub struct FwConfig {
    partition: String,
    origin: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    files: Vec<FwFile>,
}

#[derive(Deserialize)]
pub struct FwFolder {
    partition: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    folders: Vec<FwFile>,
}

#[derive(Deserialize)]
pub struct DumpConfig {
    partition: String,
    destination: String,
    kernel: Option<KernelConstraint>,
    filename: String,
}

#[derive(Deserialize)]
pub struct Config {
    dynpart: Option<String>,
    firmware: Vec<FwConfig>,
    folders: Option<Vec<FwFolder>>,
    partdump: Option<Vec<DumpConfig>>,
}

#[derive(Serialize, Deserialize)]
pub struct Status {
    pub files: Vec<String>,
    pub folders: Option<Vec<String>>,
    pub kernel_release: Option<String>,
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
    Some(MountedPartition::existing(mountpoint))
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
            Ok(()) => return Ok(MountedPartition::temporary(mountpath.to_path_buf())),
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

fn mount_part(part: &str, mountpath: &Path) -> Result<MountedPartition, Error> {
    let flags = MsFlags::MS_RDONLY;
    let part_a = format!("{part}_a");
    let part_b = format!("{part}_b");

    let mut srcpath = PathBuf::from("/dev/mapper").join(part);
    if !srcpath.exists() {
        srcpath.set_file_name(&part_a);
    }
    if !srcpath.exists() {
        srcpath.set_file_name(&part_b);
    }
    if !srcpath.exists() {
        srcpath = PathBuf::from(PARTLABEL_DIR).join(part);
    }
    if !srcpath.exists() {
        srcpath.set_file_name(&part_a);
    }
    if !srcpath.exists() {
        srcpath.set_file_name(&part_b);
    }
    if !srcpath.exists() {
        let err_str = format!("Unable to find device file for partition {part}!");
        error!("{err_str}");
        return Err(Error::new(ErrorKind::NotFound, err_str));
    }

    match mount_srcpath(&srcpath, mountpath, flags) {
        Ok(mounted) => Ok(mounted),
        Err(e) => {
            if srcpath.ends_with(&part_a) {
                srcpath.set_file_name(&part_b);
                mount_srcpath(&srcpath, mountpath, flags)
            } else {
                Err(e)
            }
        }
    }
}

fn squash_file(inpath: &PathBuf, outpath: &PathBuf) -> Result<(), Error> {
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

    let mut mdt_fd = fs::File::open(inpath).unwrap();
    let mbn_fd = fs::File::create(outpath).unwrap();

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
            let res = mdt_fd.seek(SeekFrom::Start(hashoffset));
            if let Ok(_seek) = res {
                buffer.resize(phdr.p_filesz as usize, Default::default());
                let _res = mdt_fd.read_exact(buffer.as_mut_slice());
            }
        }

        if buffer.is_empty() {
            let mut bxx_name = inpath.clone();
            bxx_name.set_extension(format!("b{:#02}", count - 1));

            let mut bxx_fd = fs::File::open(&bxx_name).unwrap();
            bxx_fd.read_to_end(&mut buffer).unwrap();
        }

        if buffer.len() != phdr.p_filesz as usize {
            let err_str = format!("Read {} bytes (!= {})", buffer.len(), phdr.p_filesz);
            return Err(Error::new(ErrorKind::UnexpectedEof, err_str));
        }

        mbn_fd.write_at(buffer.as_slice(), phdr.p_offset).unwrap();
    }

    Ok(())
}

fn map_dynpart(part: &str) -> Result<(), Error> {
    let dynpart = PathBuf::from(PARTLABEL_DIR).join(part);
    if dynpart.exists() {
        utils::execute(
            "systemctl",
            Some(vec![
                "start",
                &format!("make-dynpart-mappings@{part}.service"),
            ]),
        )
    } else {
        let err_str = format!("Unable to find super partition '{part}'");
        Err(Error::other(err_str))
    }
}

pub fn process(
    config: Config,
    extract_path: &String,
    mounts_dir: &Path,
    running_kernel_release: Option<&str>,
) -> Result<Status, Error> {
    let mut files: Vec<String> = Vec::new();
    let mut folders: Option<Vec<String>> = None;
    let running_kernel = running_kernel_release.and_then(parse_kernel_version);

    if running_kernel_release.is_some() && running_kernel.is_none() {
        warn!("Unable to parse running kernel release, kernel filtering disabled");
    }

    // Map the "super" partition if we expect one
    if let Some(part) = config.dynpart {
        info!("Mounting {part} as the 'super' partition");
        let mut success = false;

        /*
         * Systems using dynparts right from their initial release usually
         * just have one super partition. On the other hand, systems "converted"
         * to dynparts during their lifetime will likely have 2 super partitions,
         * with the possibility that both of them are valid but only one holds
         * actual filesystems.
         * Mitigate failure risks by mapping all potential super partitions we
         * can find.
         */
        for suffix in ["", "_a", "_b"] {
            let testpart = format!("{part}{suffix}");
            debug!("Attempting to map dynpart {testpart}");
            if map_dynpart(&testpart).is_ok() {
                success = true;
            }
        }

        if !success {
            return Err(Error::other("Failed to map super partition!"));
        } else {
            // Wait up to 500ms to ensure mapped partitions appear under /dev/mapper
            for _ in 0..5 {
                if let Ok(mapped) = fs::read_dir("/dev/mapper")
                    && mapped.count() > 1
                {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    for entry in config.firmware {
        if !kernel_filter_match(
            &entry.kernel,
            running_kernel.as_ref(),
            "firmware",
            &entry.partition,
        ) {
            continue;
        }

        let destpath = PathBuf::from(extract_path).join(entry.destination);

        if let Err(e) = fs::create_dir_all(&destpath) {
            warn!("Unable to create folder {}: {}", destpath.display(), e);
            continue;
        }

        let mntpath = mounts_dir.join(&entry.partition);

        match mount_part(entry.partition.as_str(), &mntpath) {
            Ok(mounted) => {
                debug!(
                    "Processing firmware files from partition {}",
                    entry.partition.as_str()
                );
                for file in entry.files {
                    let origin = mounted.path().join(&entry.origin).join(&file.name);
                    if !origin.exists() {
                        warn!(
                            "Unable to find {} on partition {}",
                            file.name, entry.partition
                        );
                        continue;
                    }

                    let mut destination = PathBuf::from(&destpath).join(&file.name);
                    if let Some(new_name) = file.rename {
                        destination.set_file_name(&new_name);
                    }

                    debug!("Copying firmware file {}", origin.display());

                    if file.name.ends_with(".mdt") {
                        trace!("Squashing MDT file into MBN");
                        destination.set_extension("mbn");
                        if let Err(e) = squash_file(&origin, &destination) {
                            warn!(
                                "Unable to squash {} to {}: {}",
                                origin.display(),
                                destination.display(),
                                e
                            );
                            continue;
                        }
                    } else if let Err(e) = fs::copy(&origin, &destination) {
                        warn!(
                            "Unable to copy {} to {}: {}",
                            origin.display(),
                            destination.display(),
                            e
                        );
                        continue;
                    }

                    files.push(format!("{}", destination.display()));
                }
                mounted.cleanup();
            }
            Err(e) => return Err(e),
        }
    }

    if let Some(dirs) = config.folders {
        let options = dir::CopyOptions::new();
        let mut folder_list = Vec::new();

        for entry in dirs {
            if !kernel_filter_match(
                &entry.kernel,
                running_kernel.as_ref(),
                "folder",
                &entry.partition,
            ) {
                continue;
            }

            let destpath = PathBuf::from(entry.destination);

            if let Err(e) = fs::create_dir_all(&destpath) {
                warn!("Unable to create folder {}: {}", destpath.display(), e);
                continue;
            }

            let mntpath = mounts_dir.join(&entry.partition);

            match mount_part(entry.partition.as_str(), &mntpath) {
                Ok(mounted) => {
                    debug!(
                        "Processing folders from partition {}",
                        entry.partition.as_str()
                    );
                    for folder in entry.folders {
                        let origin = mounted.path().join(&folder.name);
                        if !origin.exists() {
                            warn!(
                                "Unable to find {} on partition {}",
                                folder.name, entry.partition
                            );
                            continue;
                        }

                        debug!("Copying folder {}", origin.display());

                        if let Err(e) = dir::copy(&origin, &destpath, &options) {
                            warn!(
                                "Unable to copy {} to {}: {}",
                                origin.display(),
                                destpath.display(),
                                e
                            );
                            continue;
                        }

                        let mut destination =
                            PathBuf::from(&destpath).join(origin.file_name().unwrap());
                        if let Some(new_name) = folder.rename {
                            let initial_folder = PathBuf::from(&destination);
                            destination.set_file_name(&new_name);
                            let _ = fs::rename(initial_folder, &destination);
                        }
                        folder_list.push(format!("{}", destination.display()));
                    }
                    mounted.cleanup();
                }
                Err(e) => return Err(e),
            }
        }

        if !folder_list.is_empty() {
            folders = Some(folder_list);
        }
    }

    if let Some(dumps) = config.partdump {
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
            let destpath = PathBuf::from(extract_path).join(&entry.destination);
            if let Err(e) = fs::create_dir_all(&destpath) {
                warn!("Unable to create folder {}: {}", destpath.display(), e);
                continue;
            }

            let origin = PathBuf::from(PARTLABEL_DIR).join(&entry.partition);
            if !origin.exists() {
                warn!("Unable to find partition {}", entry.partition);
                continue;
            }

            let destination = destpath.join(entry.filename);
            let mut buffer: Vec<u8> = Vec::new();
            let mut input = fs::File::open(origin)?;
            let mut output = fs::File::create(&destination)?;

            input.read_to_end(&mut buffer)?;
            output.write_all(buffer.as_slice())?;

            files.push(format!("{}", destination.display()));
        }
    }

    Ok(Status {
        files,
        folders,
        kernel_release: running_kernel_release.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
