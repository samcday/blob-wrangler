
## Device configurations

`blob-wrangler` relies on per-device TOML config files named after the
device's DT `compatible` property.

The config files contain a single section named `wrangler` with a mandatory
`firmware` key. The expected value for this key is an array of
"objects" with the following attributes:
* `partition`: the name of the vendor partition containing the firmware
  files as it appears under `/dev/disk/by-partlabel/`.
* `origin`: the folder of the vendor partition containing the firmware
  files
* `destination`: the base extraction directory subfolder under which the
  firmware files must be copied; this folder will be created if it
  doesn't exist
* `kernel` (optional): an object used to conditionally process the entry
  based on the running kernel release. Supported keys are `lt`, `lte`,
  `gt`, `gte` and `eq`; each expects a version string such as `"7.0"`.
  Conditions are combined with a logical AND and matched against the
  numeric prefix of `uname -r`.
* `files`: those are the firmware files to be copied by `blob-wrangler`,
  stored as simple objects with the following attributes:
  * `name`: original file name
  * `rename` (optional): name to rename the file to

An optional `folders` key can be added to the config, in order to easily
copy entire folders. It expects an array of "object" very similar to
`firmware` entries; those have the following attributes:
* `partition`
* `destination`: the absolute path to the destination folder, which will
  be created if needed; unlike `firmware` entries, this folder can be
  located anywhere, not only under the base extraction directory
* `folders`: those are the folders to be copied by `blob-wrangler`,
  stored as simple objects with the following attributes:
  * `name`: folder path on the source partition; the last path component
    will be used as the name of the copied folder
  * `rename` (optional): name to rename the folder to

An optional `partdump` key can be added to the config, allowing to dump
entire partitions into a single file. It expects an array of "object"
very similar to `firmware` entries, with the following attributes:
* `partition`: the name of the vendor partition containing the firmware
  files as it appears under `/dev/disk/by-partlabel/`.
* `destination`: the base extraction directory subfolder under which the
  firmware files must be copied; this folder will be created if it
  doesn't exist
* `filename`: the name of the file to which the partition will be dumped
