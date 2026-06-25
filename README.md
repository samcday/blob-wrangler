Blob Wrangler
=============

`blob-wrangler` is a tool for extracting binary firmware files from
vendor partitions on Android devices. It allows importing the needed
firmware into the Linux system's `/lib/firmware` folder, avoiding the
need to distribute such firmware and the corresponding legal issues.

## Configuration

A global `/etc/blob-wrangler/config.toml` configuration file can be used
to set device-independent options. The following options are available:

- `general.extract_path`: the absolute path to the base firmware destination folder.
  If this option isn't specified, the value of the `path` parameter of the
  `firmware_class` module will be used. If neither is provided, `blob-wrangler`
  will default to `/lib/firmware/updates`.
- `postprocess.commands`: post-processing commands, written as an array of
  strings. Those commands can include the special `%k` argument, which will
  be substituted at runtime with the revision (value of `uname -r`) for the
  currently running kernel.

A global `/etc/blob-wrangler/config.toml` configuration file can be used
to set device-independent options. This file currently only allows to
configure post-processing commands, written as an array of strings.
Those commands can include the special `%k` argument, which will be
substituted at runtime with the revision (value of `uname -r`) for the
currently running kernel.

An example config (used on Debian systems) can be found in the
[config.toml.sample](config.toml.sample) file.

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

Example configurations can be found in the [configs](configs) folder.

## Usage

`blob-wrangler` can be started by a systemd service or executed manually
as `root`. It copies firmware files according to the corresponding
configuration file, then updates the status file used for cleanup.
Subsequent runs reconcile existing data and remove stale extracted files.

`blob-wrangler --cleanup` can be used to remove all previously extracted
files and folders recorded in the status file.

## License

`blob-wrangler` is licensed under the terms of the
[MIT license](https://spdx.org/licenses/MIT.html).

## Contributing

Feel free to open issues and/or merge requests on the project's
[GitHub](https://github.com/samcday/blob-wrangler) repo.
