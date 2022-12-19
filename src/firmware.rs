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

use std::{fs, os::unix::prelude::FileExt, path::PathBuf};
use std::io::{Read, Seek, SeekFrom};
use serde::Deserialize;
use goblin::elf::Elf;
use sys_mount::{Mount, Unmount, UnmountFlags};
use uname;

use crate::utils;

const FLAGS_READ_MASK: u32 = 0x07000000;
const FLAGS_MDT_VALUE: u32 = 0x02000000;

#[derive(Deserialize, Debug, Default, Clone)]
pub struct FwConfig {
    partition: String,
    origin: String,
    destination: String,
    files: Vec<String>,
    divert: Option<bool>,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct Config {
    firmware: Vec<FwConfig>
}

fn mount_part(part: &str, mountpath: &PathBuf) -> Result<Mount, std::io::Error> {
    let _res = fs::DirBuilder::new().recursive(true).create(mountpath);

    let mut srcpath = PathBuf::from("/dev/disk/by-partlabel");
    srcpath.push(part);
    if !srcpath.exists() {
        srcpath.set_file_name(format!("{}_a", part));
    }

    match Mount::builder().mount(&srcpath, mountpath) {
        Ok(m) => Ok(m),
        Err(e) => {
            println!("Unable to mount {} on {}: {}", srcpath.display(), mountpath.display(), e);
            srcpath.set_file_name(format!("{}_b", part));
            println!("Mounting {} on {}", srcpath.display(), mountpath.display());
            match Mount::builder().mount(&srcpath, mountpath) {
                Ok(m) => Ok(m),
                Err(e) => Err(e),
            }
        }
    }
}

fn find_file(mntpath: &PathBuf, dir: &str, name: &str) -> String {
    let mut filepath = PathBuf::from(&mntpath);
    filepath.push(dir);
    filepath.push(name);
    if !filepath.exists() {
        println!("File {} not found!", filepath.display());
        return String::new();
    }
    
    filepath.pop();
    String::from(filepath.to_str().unwrap())
}

fn squash_file(inpath: &PathBuf, outpath: &PathBuf) -> Result<(), std::io::Error> {
    let buffer = match fs::read(&inpath) {
        Ok(buf) => buf,
        Err(e) => panic!("Unable to read {}: {}", inpath.display(), e)
    };

    let elf = match Elf::parse(buffer.as_slice()) {
        Ok(value) => value,
        Err(e) => panic!("Unable to parse {}: {}", inpath.display(), e)
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
            panic!("Read {} bytes (!= {}", buffer.len(), phdr.p_filesz);
        }

        mbn_fd.write_at(buffer.as_slice(), phdr.p_offset).unwrap();
    }

    Ok(())
}

pub fn process(config: Config) -> Result<(), std::io::Error> {
    let krel = match uname::uname() {
        Ok(u) => u.release,
        _ => {
            println!("Unable to determine running kernel release!");
            String::from("all")
        },
    };

    for entry in config.firmware {
        let mut destpath = PathBuf::from("/lib/firmware");
        destpath.push(entry.destination);
        let dest = format!("{}", destpath.display());

        let mut mntpath = PathBuf::from("/tmp");
        mntpath.push(entry.partition.as_str());

        match mount_part(entry.partition.as_str(), &mntpath) {
            Ok(m) => {
                for file in entry.files {
                    let basedir = find_file(&mntpath, entry.origin.as_str(), file.as_str());
                    if basedir.len() == 0 {
                        println!("Unable to find {} on partition {}", file, entry.partition);
                        continue;
                    }

                    let mut origpath = PathBuf::from(basedir);
                    origpath.push(&file);

                    let mut destpath = PathBuf::from(&dest);
                    fs::create_dir_all(&destpath)?;
                    destpath.push(&file);

                    if entry.divert == Some(true) {
                        utils::divert(&destpath);
                    }

                    if file.ends_with(".mdt") {
                        destpath.set_extension("mbn");
                        squash_file(&origpath, &destpath)?;
                    } else {
                        let _r = fs::copy(&origpath, &destpath);
                    }
                }
                let _res = m.unmount(UnmountFlags::empty());
            },
            Err(e) => return Err(e),
        }

        let _r = fs::remove_dir(mntpath);
    }

    utils::execute("/usr/sbin/update-initramfs", Some(vec!["-u", "-k", krel.as_str()]));
    utils::execute("/etc/kernel/postinst.d/zz-qcom-bootimg", Some(vec![krel.as_str()]));

    Ok(())
}
