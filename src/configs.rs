use serde::Deserialize;

use crate::Config;

const BUNDLED_CONFIGS: &[(&str, &str)] = &[
    (
        "fairphone,fp4",
        include_str!("../configs/fairphone,fp4.toml"),
    ),
    (
        "fairphone,fp5",
        include_str!("../configs/fairphone,fp5.toml"),
    ),
    (
        "google,blueline",
        include_str!("../configs/google,blueline.toml"),
    ),
    (
        "google,bonito-sdc",
        include_str!("../configs/google,bonito-sdc.toml"),
    ),
    (
        "google,crosshatch",
        include_str!("../configs/google,crosshatch.toml"),
    ),
    ("google,sargo", include_str!("../configs/google,sargo.toml")),
    (
        "google,sunfish",
        include_str!("../configs/google,sunfish.toml"),
    ),
    (
        "nothing,spacewar",
        include_str!("../configs/nothing,spacewar.toml"),
    ),
    (
        "oneplus,enchilada",
        include_str!("../configs/oneplus,enchilada.toml"),
    ),
    (
        "oneplus,fajita",
        include_str!("../configs/oneplus,fajita.toml"),
    ),
    (
        "pine64,pinenote",
        include_str!("../configs/pine64,pinenote.toml"),
    ),
    (
        "samsung,starqltechn",
        include_str!("../configs/samsung,starqltechn.toml"),
    ),
    (
        "shift,axolotl",
        include_str!("../configs/shift,axolotl.toml"),
    ),
    ("shift,otter", include_str!("../configs/shift,otter.toml")),
    (
        "xiaomi,beryllium",
        include_str!("../configs/xiaomi,beryllium.toml"),
    ),
    (
        "xiaomi,beryllium-ebbg",
        include_str!("../configs/xiaomi,beryllium-ebbg.toml"),
    ),
    (
        "xiaomi,davinci",
        include_str!("../configs/xiaomi,davinci.toml"),
    ),
    (
        "xiaomi,polaris",
        include_str!("../configs/xiaomi,polaris.toml"),
    ),
];

#[derive(Debug)]
pub struct BundledConfig {
    pub compatible: &'static str,
    pub config: Config,
}

#[derive(Deserialize)]
struct ConfigFile {
    wrangler: Config,
}

pub fn parse_config(contents: &str) -> Result<Config, toml::de::Error> {
    toml::from_str::<ConfigFile>(contents).map(|file| file.wrangler)
}

pub fn bundled_config<I, S>(compatibles: I) -> Result<Option<BundledConfig>, toml::de::Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for compatible in compatibles {
        let compatible = compatible.as_ref();
        if let Some((compatible, contents)) = BUNDLED_CONFIGS
            .iter()
            .find(|(candidate, _)| *candidate == compatible)
        {
            return Ok(Some(BundledConfig {
                compatible,
                config: parse_config(contents)?,
            }));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_first_supported_compatible() {
        let bundled = bundled_config(["vendor,board", "google,sargo", "google,blueline"])
            .unwrap()
            .unwrap();

        assert_eq!(bundled.compatible, "google,sargo");
        assert_eq!(bundled.config.dynpart(), Some("system"));
    }

    #[test]
    fn returns_none_for_unknown_compatibles() {
        assert!(bundled_config(["vendor,board"]).unwrap().is_none());
    }

    #[test]
    fn every_bundled_config_parses() {
        for (_, contents) in BUNDLED_CONFIGS {
            parse_config(contents).unwrap();
        }
    }
}
