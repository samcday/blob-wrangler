## Device configurations

`blob-wrangler` relies on per-device YAML config files named after the
device's DT `compatible` property, e.g. `configs/oneplus,fajita.yaml`.

A config is a mapping with two keys:

* `dynamic-partition` (optional): the name of the dynamic partition
  container (`super`, `system`, ...) to map before any partition is mounted.
* `extract`: a list of entries, each describing content to copy out of one
  partition.

```yaml
dynamic-partition: super

extract:
  - partition: vendor
    from: firmware
    to: qcom/qcm6490/fairphone5
    files:
      - a660_zap.mdt
      - { from: aw882xx_acf.bin, to: aw88261_acf.bin }

  - partition: vendor
    to: /var/lib/blob-wrangler/sensors
    dirs:
      - { from: etc/acdbdata, to: acdb }
      - etc/sensors
    files:
      - { from: etc/sensors/sns_reg_config, to: sensors/sns_reg.conf }

  - partition: waveform
    raw: true
    to: rockchip/ebc.wbf
```

### Entries

* `partition`: the name of the vendor partition as it appears under
  `/dev/disk/by-partlabel/` (without any A/B slot suffix).
* `from` (optional): directory on the partition that item paths are
  relative to; defaults to the partition root.
* `to`: the destination directory. A relative path is resolved against the
  extract path (`/lib/firmware/updates` unless overridden, see below); an
  absolute path is used verbatim. `.` means the extract path itself. `to`
  can also be a kernel-conditional mapping, see below.
* `files` (optional): individual files to copy. A file named `*.mdt` is
  squashed together with its `.b*` segments into a single `*.mbn`.
* `dirs` (optional): directory trees to copy recursively. Nothing inside a
  copied tree is squashed or renamed.
* `raw` (optional, default `false`): dump the whole partition block device
  into the file named by `to` instead of mounting it. A raw entry cannot
  carry `from`, `files` or `dirs`.

### Items

Each element of `files` and `dirs` is either a bare path, or a mapping:

* `from`: path relative to the entry's `from` directory.
* `to` (optional): name the copy is given under the entry's destination.
  It may contain a subdirectory (`sensors/sns_reg.conf`).
* `required` (optional, default `false`): a missing source or a failed
  copy makes the extraction service fail, after the status file has been
  written. Optional items are only warned about.

A path listed under `files` must be a file and a path under `dirs` must be
a directory; anything else is recorded as a failure.

### Kernel-conditional destinations

Where the kernel looks for firmware sometimes changes between releases.
Rather than duplicating an entry per kernel, `to` can be a mapping from
kernel version constraint to path:

```yaml
  - partition: modem
    from: image
    to:
      "<7.0":  qcom/sdm845/oneplus6
      ">=7.0": qcom/sdm845/OnePlus/enchilada
    files:
      - adsp.mdt
      - modem.mdt
```

Constraints are `<`, `<=`, `>`, `>=` or `=` followed by a version, and are
matched against the numeric prefix of `uname -r` in document order; the
first match wins and `"*"` matches any kernel. If nothing matches, the
entry is skipped.

## Main configuration

`/etc/blob-wrangler/config.yaml` is optional and tunes the tool itself:

```yaml
extract-path: /var/lib/firmware-extract
postprocess:
  - /usr/bin/true
```

* `extract-path`: where relative `to` paths land. Defaults to the kernel's
  `firmware_class.path` parameter if set, otherwise `/lib/firmware/updates`.
* `postprocess`: commands to run after extraction; `%k` expands to the
  running kernel release.

## A/B partitions

`blob-wrangler` reads `androidboot.slot_suffix` (or `androidboot.slot`) from
the kernel command line. If `qbootctl` is available, its reported slot must
match. A known active slot limits partition lookup and dynamic-partition
mapping to that slot or an unsuffixed partition; the opposite slot is never
used as a fallback. If the active slot is unknown, a warning is emitted and
legacy compatibility is preserved: ordinary partitions prefer unsuffixed,
then `_a`, then `_b`, while all available dynamic-partition containers are
mapped.

The status file records the detected slot, selected source block devices, and
all configured items which could not be extracted. A failure marked
`required: true` makes the command fail only after these diagnostics have
been written to `/var/lib/blob-wrangler/status.json`.
