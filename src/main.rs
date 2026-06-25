mod firmware;
mod utils;

extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use std::collections::HashSet;
use std::io::{Error, ErrorKind};
use std::{
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use serde::Deserialize;

const DEFAULT_EXTRACT_PATH: &str = "/lib/firmware/updates";

const STATUS_FILE_PATH: &str = "/var/lib/blob-wrangler/status.json";
const CONFIG_DIR_PATH: &str = "/usr/share/blob-wrangler/configs";
const CONFIG_FILE_PATH: &str = "/etc/blob-wrangler/config.toml";
const KERNEL_RELEASE_PATH: &str = "/proc/sys/kernel/osrelease";

#[derive(Parser)]
#[command(version, about = "Extract firmware from Android vendor partitions")]
struct Opt {
    /// Device type (default: auto-detect)
    #[arg(short, long)]
    device: Option<String>,

    /// Remove previously extracted files
    #[arg(short, long)]
    cleanup: bool,

    /// Directory containing device config files
    #[arg(long, value_name = "DIR", default_value = CONFIG_DIR_PATH)]
    configs_dir: PathBuf,
}

#[derive(Deserialize)]
struct Config {
    wrangler: firmware::Config,
}

#[derive(Deserialize, PartialEq, Debug)]
#[serde(default)]
struct GeneralConfig {
    extract_path: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        let extract_path = match fs::read_to_string("/sys/module/firmware_class/parameters/path") {
            Ok(firmware_class_path) => {
                let path = firmware_class_path.trim_end();
                if !path.is_empty() {
                    path.to_string()
                } else {
                    DEFAULT_EXTRACT_PATH.to_string()
                }
            }
            Err(_) => DEFAULT_EXTRACT_PATH.to_string(),
        };

        Self { extract_path }
    }
}

#[derive(Deserialize, Default, PartialEq, Debug)]
#[serde(default)]
struct PostProcessConfig {
    commands: Vec<String>,
}

#[derive(Deserialize, Default, PartialEq, Debug)]
#[serde(default)]
struct MainConfig {
    general: GeneralConfig,
    postprocess: PostProcessConfig,
}

fn detect_device(configs_dir: &Path) -> Result<String, Error> {
    let contents = fs::read_to_string("/proc/device-tree/compatible").unwrap_or_default();

    let compatibles: Vec<&str> = contents.split('\0').filter(|s| !s.is_empty()).collect();

    debug!("Device compatible values: {compatibles:#?}");

    for file in fs::read_dir(configs_dir)? {
        let fname = match file {
            Ok(dirent) => dirent.file_name(),
            _ => continue,
        };
        debug!("Checking config file {}", fname.to_str().unwrap());
        for value in compatibles.clone() {
            let full_name = String::from(value) + ".toml";
            if fname == full_name.as_str() {
                debug!("Matched config file for compatible {value}");
                return Ok(value.to_string());
            }
        }
    }

    Err(Error::new(ErrorKind::NotFound, "Unable to detect device!"))
}

fn remove_stale_entries(previous: &firmware::Status, current: &firmware::Status) {
    let current_files = current
        .files
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let stale_files = previous
        .files
        .iter()
        .filter(|path| !current_files.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if !stale_files.is_empty() {
        debug!("Removing {} stale firmware files", stale_files.len());
        if let Err(e) = fs_extra::remove_items(&stale_files) {
            warn!("Unable to remove stale files: {e}");
        }
    }

    let current_folders = current
        .folders
        .as_ref()
        .map(|folders| folders.iter().map(String::as_str).collect::<HashSet<_>>())
        .unwrap_or_default();
    let stale_folders = previous
        .folders
        .as_ref()
        .map(|folders| {
            folders
                .iter()
                .filter(|path| !current_folders.contains(path.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !stale_folders.is_empty() {
        debug!("Removing {} stale folders", stale_folders.len());
        if let Err(e) = fs_extra::remove_items(&stale_folders) {
            warn!("Unable to remove stale folders: {e}");
        }
    }
}

fn main() -> Result<(), Error> {
    let opt = Opt::parse();

    pretty_env_logger::init();

    let device = match opt.device {
        Some(str) => str,
        _ => detect_device(&opt.configs_dir)?,
    };

    let krel = match fs::read_to_string(KERNEL_RELEASE_PATH) {
        Ok(release) => release.trim_end().to_string(),
        _ => {
            warn!("Unable to detect running kernel release!");
            String::from("all")
        }
    };

    let main_config = match fs::read_to_string(CONFIG_FILE_PATH) {
        Ok(contents) => toml::from_str(contents.as_str()).unwrap(),
        Err(_) => MainConfig::default(),
    };

    if opt.cleanup {
        info!("Cleaning up files for device {device}");

        if let Ok(f) = fs::File::open(STATUS_FILE_PATH) {
            let status: firmware::Status = match serde_json::from_reader(f) {
                Ok(s) => s,
                Err(e) => return Err(Error::other(e)),
            };

            if let Err(e) = fs_extra::remove_items(&status.files) {
                warn!("Unable to remove files: {e}");
            }
            if let Some(folders) = status.folders
                && let Err(e) = fs_extra::remove_items(&folders)
            {
                warn!("Unable to remove folders: {e}");
            }
            if let Err(e) = fs::remove_file(STATUS_FILE_PATH) {
                warn!("Unable to remove {STATUS_FILE_PATH}: {e}");
            }
        }
    } else {
        info!("Starting processing for device {device}");

        let mut cfg_path = opt.configs_dir.clone();
        cfg_path.push(&device);
        cfg_path.set_extension("toml");

        let contents = match fs::read_to_string(cfg_path) {
            Ok(str) => str,
            _ => "".to_string(),
        };

        let previous_status = match fs::File::open(STATUS_FILE_PATH) {
            Ok(f) => match serde_json::from_reader(f) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("Unable to parse existing status file: {e}");
                    None
                }
            },
            Err(_) => None,
        };

        let config: Config = toml::from_str(contents.as_str()).unwrap();
        debug!("Extracting firmware for device {device}");
        let status = firmware::process(
            config.wrangler,
            &main_config.general.extract_path,
            Some(krel.as_str()),
        )?;

        if let Some(old_status) = previous_status {
            remove_stale_entries(&old_status, &status);
        }

        debug!("Writing status file");
        fs::create_dir_all("/var/lib/blob-wrangler/")?;
        if let Ok(f) = fs::File::create(STATUS_FILE_PATH)
            && let Err(e) = serde_json::to_writer_pretty(f, &status)
        {
            return Err(Error::other(e));
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
        debug!("Executing post-process command '{full_cmd}'");
        utils::execute(cmd[0], args)?
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_main_config() {
        let firmware_class_path =
            fs::read_to_string("/sys/module/firmware_class/parameters/path").unwrap();

        let expected_config = MainConfig {
            general: GeneralConfig {
                extract_path: if firmware_class_path.trim().is_empty() {
                    DEFAULT_EXTRACT_PATH.to_string()
                } else {
                    firmware_class_path.trim().to_string()
                },
            },
            postprocess: PostProcessConfig {
                commands: Vec::new(),
            },
        };

        assert_eq!(toml::from_str::<MainConfig>("").unwrap(), expected_config);
    }

    #[test]
    fn custom_main_config() {
        let config_text = r#"
            [general]
            extract_path = "/var/lib/firmware-extract"

            [postprocess]
            commands = [ "/usr/bin/true" ]
        "#;

        let expected_config = MainConfig {
            general: GeneralConfig {
                extract_path: "/var/lib/firmware-extract".to_string(),
            },
            postprocess: PostProcessConfig {
                commands: vec!["/usr/bin/true".to_string()],
            },
        };

        assert_eq!(
            toml::from_str::<MainConfig>(config_text).unwrap(),
            expected_config
        );
    }

    #[test]
    fn default_configs_dir_option() {
        let opt = Opt::parse_from(["blob-wrangler"]);

        assert_eq!(opt.configs_dir, PathBuf::from(CONFIG_DIR_PATH));
    }

    #[test]
    fn custom_configs_dir_option() {
        let opt = Opt::parse_from(["blob-wrangler", "--configs-dir", "/tmp/blob-configs"]);

        assert_eq!(opt.configs_dir, PathBuf::from("/tmp/blob-configs"));
    }
}
