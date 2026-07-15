# blob-wrangler

`blob-wrangler` gobbles up firmware blobs from Android partitions and barfs
them into `/usr/lib/firmware`.

This project is a hard fork of [droid-juicer][].

## Usage

Run `blob-wrangler` on a supported device.

Extraction status is written to `/var/lib/blob-wrangler/status.json`, including
the detected Android slot, selected source partitions, and file failures.

## License

`blob-wrangler` is licensed under `MIT AND BSD-3-Clause`.
See [LICENSE][] and [LICENSE.BSD-3-Clause][].

[LICENSE]: LICENSE
[LICENSE.BSD-3-Clause]: LICENSE.BSD-3-Clause
[droid-juicer]: https://gitlab.com/mobian1/droid-juicer
