mod firmware;
mod utils;

use std::{ path::PathBuf, fs };
use clap::Parser;
use serde::Deserialize;

#[derive(Parser)]
#[clap(about = "Extract firmware from Android vendor partitions")]
struct Opt {
    /// Device type (default: auto-detect)
    #[clap(short, long)]
    device: Option<String>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Config {
    juicer: firmware::Config,
}

fn detect_device() -> Option<String> {
    let contents = match fs::read_to_string("/proc/device-tree/compatible") {
        Ok(str) => str,
        _ => Default::default(),
    };

    let compatibles: Vec<&str> = contents.split("\0").filter(|s| s.len() > 0).collect();

    for entry in fs::read_dir("/usr/share/droid-juicer/configs") {
        for file in entry {
            let fname = match file {
                Ok(dirent) => dirent.file_name(),
                _ => continue,
            };
            for value in compatibles.clone() {
                let full_name = String::from(value) + ".toml";
                if &fname == full_name.as_str() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

fn main() -> Result<(), std::io::Error> {
    let opt = Opt::parse();

    let device = match opt.device {
        Some(str) => str,
        _ => detect_device().unwrap(),
    };

    let mut cfg_path = PathBuf::from("/usr/share/droid-juicer/configs");
    cfg_path.push(&device);
    cfg_path.set_extension("toml");

    let contents = match fs::read_to_string(cfg_path) {
        Ok(str) => str,
        _ => "".to_string(),
    };

    let config: Config = toml::from_str(contents.as_str()).unwrap();
    firmware::process(config.juicer)?;

    Ok(())
}
