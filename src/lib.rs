#[macro_use]
extern crate log;

mod configs;
mod firmware;

pub use configs::{BundledConfig, bundled_config, parse_config};
pub use firmware::{
    Config, ExtractOptions, ExtractionReport, FileFailure, MissingItem, PartitionResolver,
    PartitionStatus, ResolvedPartition, Slot, Status, detect_active_slot, dynpart_paths, extract,
    select_partition_path,
};
