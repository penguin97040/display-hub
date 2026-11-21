# The official firmware package

This folder holds what makes a build of [the firmware](../README.md) *official*:

| | |
|---|---|
| `keys.txt` | The official public keys, one to a line: the everyday signing key and the recovery key. Public keys only. |
| `display-hub-waveshare-amoled-1.8-<version>.dhfw` | The signed package for one release. Exactly one at a time. |

Neither is here yet: the official keys haven't been made. Until they are, the apps ship without firmware and CI skips the check.

## Who signs it, and who checks it

The maintainer signs the package on their own computer with `hub-firmware`. The key files and their passphrases never go into this repository, to GitHub or to CI. A package holds only public things (the firmware, a signature and public keys), so it is committed here.

A release doesn't take the package on trust. The release workflow builds the firmware again from source, in the same pinned ESP-IDF image CI uses, and [tools/release/check-official-firmware.py](../../tools/release/check-official-firmware.py) refuses the release unless:

- the package is well formed and its signature is good (`hub-firmware inspect`);
- its app image and every USB-install part are, byte for byte, what that fresh build made (SHA-256 and size), at the same offsets, for the same chip and flash size;
- its version is the firmware's version and the release's, and is in its file name;
- its device type and hardware revision are the official board's;
- it is signed by a key in `keys.txt`, and its key set is exactly the keys in `keys.txt`.

Only then does the package go into each app download, in a `firmware` folder beside the app (inside `Display Hub.app/Contents/Resources` on macOS). The app offers it as *Official Display Hub firmware* only if it passes the app's own checks and is signed by a key built into the app (`OFFICIAL_KEYS` in [apps/hub-manager/src/firmware.rs](../../apps/hub-manager/src/firmware.rs)), which a test keeps the same as `keys.txt`.

The firmware builds reproducibly (two clean builds give the same bytes), which is what makes the comparison possible. CI runs the same check on every push. There, a package that is merely older than the source is a warning, not a failure, so work on the firmware can carry on between releases.

## Making the package for a release

1. Set the release's version in `PROJECT_VER` in [../CMakeLists.txt](../CMakeLists.txt), the same as the programs' version, and commit it.
2. Build the firmware from that commit, in a new build folder, with ESP-IDF v5.5.6 (see [Building](../README.md)).
3. Sign it with the everyday key, naming both keys in the key set. The offsets and the version come from the build's own `flasher_args.json` and app image, as in [../tools/package-dev.sh](../tools/package-dev.sh):

   ```sh
   hub-firmware sign --key everyday.dhkey --image <build>/display-hub.bin \
     --device-type display-hub-waveshare-amoled-1.8 --hardware waveshare-amoled-1.8-v2 \
     --version <version> --security-counter <counter> --summary "<what is new>" \
     --key-set everyday.dhkey --key-set recovery-public.txt \
     --usb-part bootloader@0x0=<build>/bootloader/bootloader.bin \
     --usb-part partition-table@0x8000=<build>/partition_table/partition-table.bin \
     --usb-part otadata@0x19000=<build>/ota_data_initial.bin \
     --app-offset 0x20000 --chip esp32s3 --min-flash 0x1000000 \
     --out firmware/official/display-hub-waveshare-amoled-1.8-<version>.dhfw
   ```

   `recovery-public.txt` holds the recovery key's 130 hex digits, so the recovery key file can stay offline. Take the offsets from `flasher_args.json` if the partition layout has changed.
4. Delete the previous release's package from this folder, so there is one.
5. Check it as the release will:

   ```sh
   python3 -I tools/release/check-official-firmware.py --dir firmware/official --build <build> \
     --hub-firmware hub-firmware --version <version>
   ```

6. Commit the package, then tag the release.

Changing the keys (a lost or leaked key) changes `keys.txt`, `OFFICIAL_KEYS` and the package's key set together, with a higher security counter: see "If a key is lost or leaked" in [docs/firmware-updates.md](../../docs/firmware-updates.md).
