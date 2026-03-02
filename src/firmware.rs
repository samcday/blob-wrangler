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
use std::{fs, thread};
use std::{os::unix::prelude::FileExt, path::PathBuf, time::Duration};

use std::io::prelude::*;

use fs_extra::dir;
use goblin::elf::Elf;
use serde::{Deserialize, Serialize};
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};

use crate::utils;

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;
const PARTLABEL_DIR: &str = "/dev/disk/by-partlabel";

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

    if let Some(lt) = filter.lt.as_deref().and_then(|v| parse_condition("lt", v)) {
        if *running_kernel >= lt {
            return false;
        }
    }

    if let Some(lte) = filter
        .lte
        .as_deref()
        .and_then(|v| parse_condition("lte", v))
    {
        if *running_kernel > lte {
            return false;
        }
    }

    if let Some(gt) = filter.gt.as_deref().and_then(|v| parse_condition("gt", v)) {
        if *running_kernel <= gt {
            return false;
        }
    }

    if let Some(gte) = filter
        .gte
        .as_deref()
        .and_then(|v| parse_condition("gte", v))
    {
        if *running_kernel < gte {
            return false;
        }
    }

    if let Some(eq) = filter.eq.as_deref().and_then(|v| parse_condition("eq", v)) {
        if *running_kernel != eq {
            return false;
        }
    }

    true
}

fn mount_part(part: &str, mountpath: &PathBuf) -> Result<Mount, Error> {
    let _res = fs::DirBuilder::new().recursive(true).create(mountpath);
    let flags = MountFlags::RDONLY;
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

    debug!(
        "Attempting to mount {} to {}",
        srcpath.display(),
        mountpath.display()
    );

    match Mount::builder().flags(flags).mount(&srcpath, mountpath) {
        Ok(m) => Ok(m),
        Err(e) => {
            error!(
                "Unable to mount {} on {}: {}",
                srcpath.display(),
                mountpath.display(),
                e
            );
            if srcpath.ends_with(&part_a) {
                srcpath.set_file_name(&part_b);
                debug!(
                    "Attempting to mount {} to {}",
                    srcpath.display(),
                    mountpath.display()
                );
                Mount::builder().flags(flags).mount(&srcpath, mountpath)
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
                if let Ok(mapped) = fs::read_dir("/dev/mapper") {
                    if mapped.count() > 1 {
                        break;
                    }
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

        let mntpath = PathBuf::from("/tmp").join(&entry.partition);

        match mount_part(entry.partition.as_str(), &mntpath) {
            Ok(m) => {
                debug!(
                    "Processing firmware files from partition {}",
                    entry.partition.as_str()
                );
                for file in entry.files {
                    let origin = PathBuf::from(&mntpath).join(&entry.origin).join(&file.name);
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
                let _res = m.unmount(UnmountFlags::empty());
            }
            Err(e) => return Err(e),
        }

        let _r = fs::remove_dir(mntpath);
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

            let mntpath = PathBuf::from("/tmp").join(&entry.partition);

            match mount_part(entry.partition.as_str(), &mntpath) {
                Ok(m) => {
                    debug!(
                        "Processing folders from partition {}",
                        entry.partition.as_str()
                    );
                    for folder in entry.folders {
                        let origin = PathBuf::from(&mntpath).join(&folder.name);
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
                    let _res = m.unmount(UnmountFlags::empty());
                }
                Err(e) => return Err(e),
            }

            let _r = fs::remove_dir(mntpath);
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

    Ok(Status { files, folders })
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
}
