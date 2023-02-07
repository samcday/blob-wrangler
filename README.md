Droid Juicer
============

`droid-juicer` is a tool for extracting binary firmware files from
vendor partitions on Android devices. It allows importing the needed
firmware into the Linux system's `/lib/firmware` folder, avoiding the
need to distribute such firmware and the corresponding legal issues.

## Configuration

A global `/etc/droid-juicer/config.toml` configuration file can be used
to set device-independent options. This file currently only allows to
configure post-processing commands, written as an array of strings.
Those commands can include the special `%k` argument, which will be
substituted at runtime with the revision (value of `uname -r`) for the
currently running kernel.

An example config (used on Debian systems) can be found in the
[config.toml.sample](config.toml.sample) file.

## Device configurations

`droid-juicer` relies on per-device TOML config files named after the
device's DT `compatible` property.

The config files contain a single section named `juicer` with a single
`firmware` key. The expected value for this key is an array of
"objects" with the following attributes:
* `partition`: the name of the vendor partition containing the firmware
  files as it appears under `/dev/disk/by-partlabel/`.
* `origin`: the folder of the vendor partition containing the firmware
  files
* `destination`: the `/lib/firmware` subfolder under which the firmware
  files must be copied; this folder will be created if it doesn't exist
* `files`: those are the firmware files to be copied by `droid-juicer`,
  stored as simple objects with the following attributes:
  * `name`: original file name
  * `rename` (optional): name to rename the file to

Example configurations can be found in the [configs](configs) folder.

## Usage

`droid-juicer` is started during the device's first boot by a systemd
service. It copies the firmware files according to the corresponding
configuration file, then updates the initramfs and the Android boot
image so extracted firmware are available on subsequent boots. Finally,
it reboots the device.

`droid-juicer` can also be executed manually (as `root`). In such
cases, it is however recommended to first run `droid-juicer --cleanup`
so the existing files and diversions are removed before the new run.

## License

`droid-juicer` is licensed under the terms of the
[MIT license](https://spdx.org/licenses/MIT.html).

## Contributing

Feel free to open issues and/or merge requests on the project's
[gitlab](https://gitlab.com/mobian1/droid-juicer) repo.
