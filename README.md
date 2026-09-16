# blob-wrangler

`blob-wrangler` gobbles up firmware blobs from Android partitions and barfs
them into `/usr/lib/firmware`.

This project is a hard fork of [droid-juicer][].

## Usage

Run `blob-wrangler` on a supported device.

Extraction status is written to `/var/lib/blob-wrangler/status.json`, including
the detected Android slot, selected source partitions, and file failures.

## Library

The crate also exposes the extraction engine as a library. Disable default
features when embedding it without the command-line program:

```toml
blob-wrangler = { version = "0.0.1", default-features = false }
```

Use `bundled_config` to select an embedded device configuration in device-tree
compatible order, then call `extract` with an `ExtractOptions` value and a
`PartitionResolver`. The resolver owns platform-specific partition discovery
and dynamic-partition setup; the extraction library does not invoke systemd or
assume a `/dev/disk/by-partlabel` layout. A `MountedDirectory` returned by a
resolver must be a read-only view. Block devices are mounted read-only by the
library and unmounted automatically.

## License

`blob-wrangler` is licensed under `MIT AND BSD-3-Clause`.
See [LICENSE][] and [LICENSE.BSD-3-Clause][].

[LICENSE]: LICENSE
[LICENSE.BSD-3-Clause]: LICENSE.BSD-3-Clause
[droid-juicer]: https://gitlab.com/mobian1/droid-juicer
