/*
 * Heavily based on https://github.com/andersson/pil-squasher/, and as such:
 *
 * Copyright (c) 2019, Linaro Ltd.
 * Copyright (c) 2022, Arnaud Ferraris.
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

use std::{ fs, os::unix::prelude::FileExt, path::PathBuf };
use std::io::{ Error, ErrorKind, Read, Seek, SeekFrom };

use goblin::elf::Elf;
use serde::{ Serialize, Deserialize };
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;

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
    files: Vec<FwFile>,
}

#[derive(Deserialize)]
pub struct Config {
    firmware: Vec<FwConfig>
}

#[derive(Serialize, Deserialize)]
pub struct Status {
    pub files: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct OldStatus {
    pub files: Vec<String>,
    pub diversions: Option<Vec<String>>,
}

fn mount_part(part: &str, mountpath: &PathBuf) -> Result<Mount, Error> {
    let _res = fs::DirBuilder::new().recursive(true).create(mountpath);
    let flags = MountFlags::RDONLY;

    let mut srcpath = PathBuf::from("/dev/mapper");
    srcpath.push(part);
    if !srcpath.exists() {
        srcpath.set_file_name(format!("{}_a", part));
    }
    if !srcpath.exists() {
        srcpath = PathBuf::from("/dev/disk/by-partlabel");
        srcpath.push(part);
    }
    if !srcpath.exists() {
        srcpath.set_file_name(format!("{}_a", part));
    }

    match Mount::builder().flags(flags).mount(&srcpath, mountpath) {
        Ok(m) => Ok(m),
        Err(e) => {
            if srcpath.ends_with("_a") {
                eprintln!("Unable to mount {} on {}: {}",
                          srcpath.display(), mountpath.display(), e);
                srcpath.set_file_name(format!("{}_b", part));
                println!("Mounting {} on {}", srcpath.display(), mountpath.display());
                Mount::builder().flags(flags).mount(&srcpath, mountpath)
            } else {
                Err(e)
            }
        }
    }
}

fn squash_file(inpath: &PathBuf, outpath: &PathBuf) -> Result<(), Error> {
    let buffer = match fs::read(&inpath) {
        Ok(buf) => buf,
        Err(e) => {
            eprintln!("Unable to read {}: {}", inpath.display(), e);
            return Err(e);
        }
    };

    let elf = match Elf::parse(buffer.as_slice()) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("Unable to parse {}: {}", inpath.display(), e);
            return Err(Error::new(ErrorKind::InvalidData, e));
        }
    };

    let mut count = 0;
    let mut hashoffset = 0;
    
    let mut mdt_fd = fs::File::open(&inpath).unwrap();
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
        
        if buffer.len() == 0 {
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

pub fn process(config: Config) -> Result<Status, Error> {
    let mut files: Vec<String> = Vec::new();

    for entry in config.firmware {
        let mut destpath = PathBuf::from("/lib/firmware/updates");
        destpath.push(entry.destination);

        if let Err(e) = fs::create_dir_all(&destpath) {
            eprintln!("Warning: unable to create folder {}: {}",
                      destpath.display(), e);
            continue;
        }

        let mut mntpath = PathBuf::from("/tmp");
        mntpath.push(entry.partition.as_str());

        match mount_part(entry.partition.as_str(), &mntpath) {
            Ok(m) => {
                for file in entry.files {
                    let mut origin = PathBuf::from(&mntpath);
                    origin.push(&entry.origin);
                    origin.push(&file.name);
                    if !origin.exists() {
                        eprintln!("Warning: unable to find {} on partition {}",
                                  file.name, entry.partition);
                        continue;
                    }

                    let mut destination = PathBuf::from(&destpath);
                    if let Some(new_name) = file.rename {
                        destination.push(&new_name);
                    } else {
                        destination.push(&file.name);
                    }

                    if file.name.ends_with(".mdt") {
                        destination.set_extension("mbn");
                        if let Err(e) = squash_file(&origin, &destination) {
                            eprintln!("Warning: unable to squash {} to {}: {}",
                                      origin.display(), destination.display(), e);
                            continue;
                        }
                    } else {
                        if let Err(e) = fs::copy(&origin, &destination) {
                            eprintln!("Warning: unable to copy {} to {}: {}",
                                      origin.display(), destination.display(), e);
                            continue;
                        }
                    }

                    files.push(format!("{}", destination.display()));
                }
                let _res = m.unmount(UnmountFlags::empty());
            },
            Err(e) => return Err(e),
        }

        let _r = fs::remove_dir(mntpath);
    }

    Ok(Status { files })
}
