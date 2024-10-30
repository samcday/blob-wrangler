mod firmware;
mod utils;

use std::io::{Error, ErrorKind};
use std::{fs, path::PathBuf};

use clap::Parser;
use serde::Deserialize;

const STATUS_FILE_PATH: &str = "/var/lib/droid-juicer/status.json";
const CONFIG_DIR_PATH: &str = "/usr/share/droid-juicer/configs";
const CONFIG_FILE_PATH: &str = "/etc/droid-juicer/config.toml";

#[derive(Parser)]
#[command(about = "Extract firmware from Android vendor partitions")]
struct Opt {
    /// Device type (default: auto-detect)
    #[arg(short, long)]
    device: Option<String>,

    /// Remove previously extracted files
    #[arg(short, long)]
    cleanup: bool,
}

#[derive(Deserialize)]
struct Config {
    juicer: firmware::Config,
}

#[derive(Deserialize, Default)]
struct PostProcessConfig {
    commands: Vec<String>,
}

#[derive(Deserialize, Default)]
struct MainConfig {
    postprocess: PostProcessConfig,
}

fn detect_device() -> Result<String, Error> {
    let contents = fs::read_to_string("/proc/device-tree/compatible").unwrap_or_default();

    let compatibles: Vec<&str> = contents.split('\0').filter(|s| !s.is_empty()).collect();

    while let Ok(entry) = fs::read_dir(CONFIG_DIR_PATH) {
        for file in entry {
            let fname = match file {
                Ok(dirent) => dirent.file_name(),
                _ => continue,
            };
            for value in compatibles.clone() {
                let full_name = String::from(value) + ".toml";
                if fname == full_name.as_str() {
                    return Ok(value.to_string());
                }
            }
        }
    }

    Err(Error::new(ErrorKind::NotFound, "Unable to detect device!"))
}

fn main() -> Result<(), Error> {
    let opt = Opt::parse();
    let mut main_config = MainConfig::default();

    let device = match opt.device {
        Some(str) => str,
        _ => match detect_device() {
            Ok(s) => s,
            Err(e) => return Err(e),
        },
    };

    let krel = match uname::uname() {
        Ok(u) => u.release,
        _ => {
            eprintln!("Warning: unable to detect running kernel release!");
            String::from("all")
        }
    };

    if PathBuf::from(CONFIG_FILE_PATH).exists() {
        if let Ok(contents) = fs::read_to_string(CONFIG_FILE_PATH) {
            main_config = toml::from_str(contents.as_str())?;
        }
    }

    if opt.cleanup {
        println!("Cleaning up files for device {}", device);

        if let Ok(f) = fs::File::open(STATUS_FILE_PATH) {
            let status: firmware::Status = match serde_json::from_reader(f) {
                Ok(s) => s,
                Err(e) => return Err(Error::new(ErrorKind::Other, e)),
            };

            if let Err(e) = fs_extra::remove_items(&status.files) {
                eprintln!("Warning: unable to remove files: {}", e);
            }
            if let Some(folders) = status.folders {
                if let Err(e) = fs_extra::remove_items(&folders) {
                    eprintln!("Warning: unable to remove folders: {}", e);
                }
            }
            if let Err(e) = fs::remove_file(STATUS_FILE_PATH) {
                eprintln!("Warning: unable to remove {}: {}", STATUS_FILE_PATH, e);
            }
        }
    } else {
        println!("Starting processing for device {}", device);

        let mut cfg_path = PathBuf::from(CONFIG_DIR_PATH);
        cfg_path.push(&device);
        cfg_path.set_extension("toml");

        let contents = match fs::read_to_string(cfg_path) {
            Ok(str) => str,
            _ => "".to_string(),
        };

        let config: Config = toml::from_str(contents.as_str()).unwrap();
        let status = match firmware::process(config.juicer) {
            Ok(s) => s,
            Err(e) => return Err(e),
        };
        fs::create_dir_all("/var/lib/droid-juicer/")?;
        if let Ok(f) = fs::File::create(STATUS_FILE_PATH) {
            if let Err(e) = serde_json::to_writer_pretty(f, &status) {
                return Err(Error::new(ErrorKind::Other, e));
            }
        }
    }

    for cmdline in main_config.postprocess.commands {
        let full_cmd = cmdline.replace("%k", krel.as_str());
        let mut cmd = full_cmd.split(' ').collect::<Vec<_>>();
        if cmd.is_empty() {
            continue;
        }
        let args_list = cmd.split_off(1);
        let args = match args_list.is_empty() {
            true => None,
            _ => Some(args_list),
        };
        utils::execute(cmd[0], args)?
    }

    Ok(())
}
