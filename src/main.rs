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
const MOUNTS_DIR_PATH: &str = "/var/lib/blob-wrangler/mounts";
const CONFIG_FILE_PATH: &str = "/etc/blob-wrangler/config.yaml";
const KERNEL_RELEASE_PATH: &str = "/proc/sys/kernel/osrelease";
const FIRMWARE_CLASS_PATH: &str = "/sys/module/firmware_class/parameters/path";

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

    /// Directory used for temporary partition mounts
    #[arg(long, value_name = "DIR", default_value = MOUNTS_DIR_PATH)]
    mounts_dir: PathBuf,
}

#[derive(Deserialize, Default, PartialEq, Debug)]
#[serde(deny_unknown_fields, default)]
struct MainConfig {
    #[serde(rename = "extract-path")]
    extract_path: Option<String>,
    postprocess: Vec<String>,
}

impl MainConfig {
    /// The kernel's firmware_class path is honoured when no explicit
    /// extract-path is configured.
    fn extract_path(&self) -> String {
        self.extract_path
            .to_owned()
            .unwrap_or_else(default_extract_path)
    }
}

fn default_extract_path() -> String {
    match fs::read_to_string(FIRMWARE_CLASS_PATH) {
        Ok(firmware_class_path) => {
            let path = firmware_class_path.trim_end();
            if !path.is_empty() {
                path.to_string()
            } else {
                DEFAULT_EXTRACT_PATH.to_string()
            }
        }
        Err(_) => DEFAULT_EXTRACT_PATH.to_string(),
    }
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
            let full_name = String::from(value) + ".yaml";
            if fname == full_name.as_str() {
                debug!("Matched config file for compatible {value}");
                return Ok(value.to_string());
            }
        }
    }

    Err(Error::new(ErrorKind::NotFound, "Unable to detect device!"))
}

fn remove_stale_entries(previous: &firmware::Status, current: &firmware::Status) {
    let current_entries = current
        .entries
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let stale_entries = previous
        .entries
        .iter()
        .filter(|path| !current_entries.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if !stale_entries.is_empty() {
        debug!("Removing {} stale entries", stale_entries.len());
        if let Err(e) = fs_extra::remove_items(&stale_entries) {
            warn!("Unable to remove stale entries: {e}");
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
        Ok(contents) => serde_norway::from_str(contents.as_str()).unwrap(),
        Err(_) => MainConfig::default(),
    };

    if opt.cleanup {
        info!("Cleaning up files for device {device}");

        if let Ok(f) = fs::File::open(STATUS_FILE_PATH) {
            let status: firmware::Status = match serde_json::from_reader(f) {
                Ok(s) => s,
                Err(e) => return Err(Error::other(e)),
            };

            if let Err(e) = fs_extra::remove_items(&status.entries) {
                warn!("Unable to remove entries: {e}");
            }
            if let Err(e) = fs::remove_file(STATUS_FILE_PATH) {
                warn!("Unable to remove {STATUS_FILE_PATH}: {e}");
            }
        }
    } else {
        info!("Starting processing for device {device}");

        let mut cfg_path = opt.configs_dir.clone();
        cfg_path.push(&device);
        cfg_path.set_extension("yaml");

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

        let config: firmware::Config = serde_norway::from_str(contents.as_str()).unwrap();
        debug!("Extracting firmware for device {device}");
        let active_slot = firmware::detect_active_slot()?;
        let extract_path = main_config.extract_path();
        let status = firmware::process(
            config,
            &extract_path,
            &opt.mounts_dir,
            Some(krel.as_str()),
            active_slot,
        )?;
        let has_required_failures = status.has_required_failures();

        if !has_required_failures && let Some(old_status) = previous_status {
            remove_stale_entries(&old_status, &status);
        }

        debug!("Writing status file");
        fs::create_dir_all("/var/lib/blob-wrangler/")?;
        let status_file = fs::File::create(STATUS_FILE_PATH)?;
        serde_json::to_writer_pretty(status_file, &status).map_err(Error::other)?;

        if has_required_failures {
            let failed = status
                .failures
                .iter()
                .filter(|failure| failure.required)
                .map(|failure| failure.source.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "Required firmware extraction failed for: {failed}; details written to {STATUS_FILE_PATH}"
                ),
            ));
        }
    }

    for cmdline in main_config.postprocess {
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
        let firmware_class_path = fs::read_to_string(FIRMWARE_CLASS_PATH).unwrap();

        assert_eq!(
            serde_norway::from_str::<MainConfig>("{}").unwrap(),
            MainConfig {
                extract_path: None,
                postprocess: Vec::new(),
            }
        );

        let expected_extract_path = if firmware_class_path.trim().is_empty() {
            DEFAULT_EXTRACT_PATH.to_string()
        } else {
            firmware_class_path.trim().to_string()
        };
        assert_eq!(
            serde_norway::from_str::<MainConfig>("{}")
                .unwrap()
                .extract_path(),
            expected_extract_path
        );
    }

    #[test]
    fn custom_main_config() {
        let config_text = r#"
extract-path: /var/lib/firmware-extract
postprocess:
  - /usr/bin/true
"#;

        let expected_config = MainConfig {
            extract_path: Some("/var/lib/firmware-extract".to_string()),
            postprocess: vec!["/usr/bin/true".to_string()],
        };

        assert_eq!(
            serde_norway::from_str::<MainConfig>(config_text).unwrap(),
            expected_config
        );
    }

    #[test]
    fn unknown_main_config_keys_are_rejected() {
        assert!(serde_norway::from_str::<MainConfig>("extract-path: /x\nbogus-key: y").is_err());
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

    #[test]
    fn default_mounts_dir_option() {
        let opt = Opt::parse_from(["blob-wrangler"]);

        assert_eq!(opt.mounts_dir, PathBuf::from(MOUNTS_DIR_PATH));
    }

    #[test]
    fn custom_mounts_dir_option() {
        let opt = Opt::parse_from(["blob-wrangler", "--mounts-dir", "/run/blob-mounts"]);

        assert_eq!(opt.mounts_dir, PathBuf::from("/run/blob-mounts"));
    }
}
