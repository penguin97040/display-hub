# Running the hub on a Raspberry Pi

`hubd` runs on a Raspberry Pi 5 (a Pi 4 works too) as a systemd service, around the clock. Displays and the desktop manager reach it over HTTPS on port 8443 on your local network.

**What has been tested:** the installer, the service, restarts, a crash, a network cut, a reboot, a bad setting, backup and restore, helpers in their sandboxes, an upgrade and an uninstall. They ran in Debian Bookworm and Trixie containers with systemd (`tools/pi-container-test/run.sh`, also in CI). The ARM64 binaries were also run under emulation against Bookworm's and Trixie's own C libraries. **Nobody has run it on a real Pi yet.** The steps below are written for that, and the last section lists what to watch for.

## What you need

- A Raspberry Pi running **Raspberry Pi OS Lite (64-bit)**, Bookworm or Trixie, with SSH turned on. In Raspberry Pi Imager, choose the OS, then **Edit settings**: set a host name (`raspberrypi` is assumed below), a user, Wi-Fi or Ethernet, and **Enable SSH** with a password or your public key.
- The Pi and your PC on the same network.
- **Recommended:** a fixed address for the Pi. In your router's settings, reserve its current address for it (often called a "DHCP reservation" or "static lease").

## 1. Get the files

You need a folder holding the two ARM64 programs, `hubd` (the hub) and `hub-send` (for helpers), and everything in `deploy/raspberry-pi/`: `install.sh`, `display-hub.service`, `hubd.toml`, `nftables-display-hub.conf`, the helper tools `display-hub-helper`, `display-hub-helper@.service` and `display-hub-helper@.timer`, and the `helpers` folder.

- **Ready-made:** if `dist\display-hub-pi\` exists in your checkout, that's the folder. Alternatively, CI makes the whole folder for every push: the `display-hub-raspberry-pi-arm64` artefact on the run's page on GitHub. Unzip it; it doesn't matter that the download loses the programs' "executable" marks, because the installer sets them.
- **Or build it** on Linux, with [zig](https://ziglang.org) and `cargo install cargo-zigbuild`:

  ```sh
  cargo zigbuild --release --locked -p hubd -p hub-send --target aarch64-unknown-linux-gnu.2.36
  mkdir display-hub-pi
  cp -r deploy/raspberry-pi/. display-hub-pi/
  cp target/aarch64-unknown-linux-gnu/release/hubd target/aarch64-unknown-linux-gnu/release/hub-send display-hub-pi/
  ```

  The `.2.36` suffix builds against the C library of Raspberry Pi OS Bookworm, so one build runs on Bookworm and Trixie. A program built on the Pi itself, or by a newer distribution, may not run on Bookworm.

## 2. Connect over SSH, and check the Pi's key

From PowerShell on Windows (OpenSSH is built in), or a terminal elsewhere:

```powershell
ssh <user>@raspberrypi.local
```

The first time, SSH shows the Pi's key fingerprint and asks whether to trust it. **This check matters:** the pairing string you'll copy over this connection carries the hub's key, so the pairing is only as trustworthy as this SSH connection. If you can, compare it with the fingerprint shown on the Pi's own screen and keyboard: `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`. Type `exit` to leave.

## 3. Copy and install

From your PC:

```powershell
scp -r dist\display-hub-pi <user>@raspberrypi.local:
ssh <user>@raspberrypi.local
```

Then on the Pi, from the folder you land in (your home folder), run the one command the manager's first-run guide shows with a **Copy** button:

```sh
sudo bash display-hub-pi/install.sh
```

The installer does the following:

1. Creates a `display-hub` system user with no login.
2. Installs `/usr/local/bin/hubd`, plus `/etc/display-hub/hubd.toml` if you don't have one.
3. Installs the hardened `display-hub.service` (systemd rates its exposure 1.0, "OK"), then enables and starts it.
4. Installs what helpers need: `hub-send`, the `display-hub-helper` command and the example helper. It turns no helper on (see [Helpers on the Pi](#helpers-on-the-pi)).
5. On first start, the hub makes its identity: an ECDSA key and certificate in `/var/lib/display-hub/identity/`, readable only by the service.
6. Prints `hubd check`. An unsynchronised clock is normal for a minute after boot.
7. On a first install only, ends by printing a pairing string (the `dh1.…` line), ready for step 4. It is shown on the terminal only: the installer doesn't save it or send it to the log. An upgrade doesn't print one.

## 4. Pair the desktop manager

After a first install, the installer has just printed a pairing string: skip to the copying below. If it has run out (it lasts 10 minutes), or you are pairing another PC, make a new one on the Pi:

```sh
sudo -u display-hub hubd pair-admin
```

Copy the `dh1.…` line, paste it into the desktop manager and choose **Pair**. It works once, within 10 minutes. The manager then remembers the hub, and connects to it by the address in the string (`raspberrypi.local` by default).

Some PCs can't look up `.local` names, especially Windows with a VPN connected, or a PC on two networks at once (for example Ethernet and Wi-Fi). If the manager says it can't find the hub's name, make a string with the Pi's address instead (`hostname -I` on the Pi shows it): `sudo -u display-hub hubd pair-admin --host 192.168.1.20`. `pair-admin` prints this command for you when the hub knows its address (see `advertise_hosts` below).

## 5. Optional: settings and firewall

- **`/etc/display-hub/hubd.toml`** explains each setting. The one most worth setting is `advertise_hosts`: put the Pi's fixed address after its `.local` name. Displays try those addresses in order, and an ESP32 may not resolve `.local` names. Restart afterwards with `sudo systemctl restart display-hub`.
- **Firewall:** hubd already refuses connections from outside your local network, and the service is limited to local addresses as well. To stop such connections even earlier, run:

  ```sh
  sudo apt install nftables
  sudo nft -f nftables-display-hub.conf
  ```

  This rule is lost on reboot unless you include it from `/etc/nftables.conf` and run `sudo systemctl enable nftables`.
- **No port forwarding.** The hub is for your local network only. Never forward 8443 on your router.

## Helpers on the Pi

A **helper** is a small program that fetches something, such as the weather or flights overhead, and sends it to a page on the hub ([helpers.md](helpers.md)). The Pi can run helpers too, next to the hub. The `display-hub-helper` command sets each one up to run every few minutes, kept well apart from the hub:

- Each helper runs as its own throwaway user, made up by systemd for each run, with its own private folder.
- It can reach the internet and the hub, but it can't see the hub's key, data, settings or backups, another helper's folder, your home folder, or the hub's process. It can't change its own program, or anything else on the Pi.
- Its pass to the hub only lets it send to its own feeds. If a helper goes wrong, the worst it can do is put wrong figures on its own pages.

### Try the example

The example helper sends the Pi's CPU temperature as a "big number" page. On the Pi:

```sh
sudo display-hub-helper add pi-status --run /usr/local/share/display-hub/helpers/pi-status
```

It pairs the helper with the hub, runs it once and shows what happened. From then on it runs every 5 minutes, and after every restart.

The hub shows what a helper sends on pages of the same kind whose content comes from a helper ("Filled by a helper" in the manager) and whose feed is the helper's, here `pi-status`. "Updated 1 page: pi-status" means such a page exists and now has the temperature.

**Making the page:** in the desktop manager, open **Pages**, choose **Add from template**, then **Pi status**. The manager adds the page, then shows the command above with a **Copy** button. Run it once on the Pi. Until it has run, the page says it is waiting for the helper's first update. (For developers: `POST /v1/admin/pages` with `{"title": "Pi status", "kind": "big_number", "source": "helper", "feed": "pi-status"}` makes the same page; `tools/pi-container-test/run.sh` does this.)

### Your own helper

A helper can be any program that runs on its own, without anyone typing at it: a shell script or a Python script, for instance. It sends a page with `hub-send`, for example:

```sh
echo '{"value": 21.5, "unit": "°C"}' | hub-send page garden-weather --kind big_number --data -
```

The example in `/usr/local/share/display-hub/helpers/pi-status` is a good start. Copy it, change it, then set it up:

```sh
sudo display-hub-helper add garden-weather --run /home/<user>/garden-weather --every 15min
```

- **The name** (here `garden-weather`) is lower case, with letters, digits and single hyphens, 20 characters at most. It is also the feed the helper sends to. To send to other feeds, add `--feed <feed>` for each.
- **`--every`** is how often it runs: 5 minutes unless you say, between once a minute and once a day (`1min`, `15min`, `2h`, `1d`).
- **The program is copied**, so changing your file later changes nothing until you run `sudo display-hub-helper update garden-weather --run /home/<user>/garden-weather`. It keeps its pairing.
- **What it gets:** `DISPLAY_HUB_HELPER` (its name), `DISPLAY_HUB_FEEDS` (its feeds, separated by spaces) and its own folder, `HUB_SEND_DIR`, where it may keep files between runs.
- **Limits:** each run has 2 minutes, 128 MB of memory and half of one processor. It can't read `/home`, and most of `/proc` is hidden (for example `/proc/loadavg`). It may reach other devices on your network as well as the internet.
- **A program that needs a password or key of its own**, such as a weather service's API key, isn't covered by `display-hub-helper` yet.
- **Tighter still:** a helper that is a shell or plain Python script can also be stopped from making code at run time. Run `sudo systemctl edit display-hub-helper@garden-weather.service`, add the two lines `[Service]` and `MemoryDenyWriteExecute=yes`, save, then `sudo display-hub-helper run-now garden-weather` to check it still works. It's off by default because it breaks Node.js, Java and some Python packages.

### Looking after helpers

| To | Run on the Pi |
|---|---|
| See every helper and how its last run went | `sudo display-hub-helper list` |
| Run one now and see what it said | `sudo display-hub-helper run-now pi-status` |
| Read its log | `sudo display-hub-helper logs pi-status` (add `--follow` to keep watching; Ctrl+C stops) |
| Replace its program | `sudo display-hub-helper update pi-status --run <program>` |
| Remove it | `sudo display-hub-helper remove pi-status` |
| See every helper the hub knows, including PCs | `sudo -u display-hub hubd helper list` |

`remove` stops the helper, cuts it off at the hub (its token stops working at once) and deletes its program, folder and token. Its pages keep their last figures until they go stale. To pause a helper instead: `sudo systemctl disable --now display-hub-helper@pi-status.timer`, and `enable --now` to carry on.

## Day to day

| To | Run on the Pi |
|---|---|
| See whether it's running | `systemctl status display-hub` |
| Read its log | `journalctl -u display-hub -f` (Ctrl+C to stop) |
| Check the setup | `sudo -u display-hub hubd check` |
| See the key fingerprint, to compare with the manager's Settings | `sudo -u display-hub hubd fingerprint` |
| List or cut off paired PCs | `sudo -u display-hub hubd admin list`, then `sudo -u display-hub hubd admin revoke <id>` |
| Check on helpers | `sudo display-hub-helper list` (see [Helpers on the Pi](#helpers-on-the-pi)) |
| Let a firmware maker's computer upload packages, without the app | `sudo -u display-hub hubd pair-publisher` (see [Firmware packages](#firmware-packages)) |
| Restart | `sudo systemctl restart display-hub` |

## Firmware packages

Signed firmware updates reach displays through the hub ([firmware-updates.md](firmware-updates.md)). The hub keeps the packages it is given in `/var/lib/display-hub/firmware/`, one owner-only file per package, named by its SHA-256. The service already may write there: it is inside its state folder, so the unit needs nothing extra.

- **Limits:** 3 packages for each kind of display (a fourth replaces that kind's lowest version that isn't on offer), 256 MiB in all, and the hub refuses an upload that would leave less than 64 MiB free on the SD card.
- **Memory:** uploads and downloads stream through a 64 KiB buffer, so a 16 MiB package never sits in the hub's memory, and the service's `MemoryMax=256M` is plenty. Files written pass through the kernel's page cache, which counts towards that limit but is given back under pressure.
- **Who can add one:** the app, or a maker's `hub-firmware` tool paired as a *publisher*. To make a publisher string on the Pi: `sudo -u display-hub hubd pair-publisher`, with `--host <address>` if the maker's computer can't find the Pi's `.local` name. It works once, within 10 minutes, and the publisher can only add and list packages. Revoke publishers in the app.
- **Nothing installs by itself:** an update goes to a display only when you press **Update** for it, and the display checks the signature with its own keys before it writes anything.

## Backups

The hub's identity is what every display and PC trusts. If it's lost, each of them has to be set up again. So back it up, and keep the backup as safe as the Pi: **it contains the private key**.

```sh
sudo install -d -m 0700 -o display-hub -g display-hub /var/backups/display-hub
sudo -u display-hub hubd backup /var/backups/display-hub/$(date +%F)
```

This is safe while the hub runs. To keep a copy off the Pi:

```sh
sudo sh -c "umask 077; tar -C /var/backups/display-hub -czf $HOME/display-hub-backup.tgz $(date +%F)"
sudo chown "$USER" ~/display-hub-backup.tgz
```

`umask 077` makes the copy readable by you alone; a plain `sudo tar` would leave it readable by everyone on the Pi.

Then, from your PC:

```powershell
scp <user>@raspberrypi.local:display-hub-backup.tgz .
```

Store it somewhere private, then delete `~/display-hub-backup.tgz` on the Pi.

The installer also backs up automatically before every upgrade, into `/var/backups/display-hub/before-upgrade-<time>`.

### Restoring, for example onto a new SD card or a new Pi

1. Install as above (steps 1 to 3). This makes a new, empty hub.
2. Copy the backup over and unpack it: `tar -xzf display-hub-backup.tgz`.
3. Restore it:

   ```sh
   sudo systemctl stop display-hub
   sudo install -d -m 0700 -o display-hub -g display-hub /var/backups/display-hub
   sudo mv <date> /var/backups/display-hub/
   sudo chown -R display-hub:display-hub /var/backups/display-hub/<date>
   sudo -u display-hub hubd restore /var/backups/display-hub/<date> --replace
   sudo systemctl start display-hub
   ```

`--replace` moves the empty hub aside into `/var/lib/display-hub/replaced-<time>/` rather than deleting it.

Firmware packages aren't in the backup either: they are large, and can be uploaded again. After a restore, the hub forgets any package whose file is missing.

Helpers on the Pi aren't in the backup. On a new SD card or Pi, set them up again with `display-hub-helper add`. The old ones are still listed by `sudo -u display-hub hubd helper list`; cut them off with `sudo -u display-hub hubd helper revoke <id>`. Displays and the manager carry on as before, because the key is the same. If the Pi's address changed, they find it through the other addresses they know, or you can pair the manager again.

## Upgrading

Copy the new folder over, then run `sudo bash install.sh` in it again. It does the following:

1. Backs up the hub.
2. Replaces the programs and the units. Helpers you've set up keep running, and use the new versions from their next run.
3. Keeps your `hubd.toml`.
4. Restarts the service.

When a new hubd needs a newer database layout, it upgrades the database the first time it starts. Before it changes anything, it saves a copy of the database beside it, readable by the hub's user only, named after the old layout's version and the time in UTC: for example `/var/lib/display-hub/hub-before-upgrade-schema-2-2026-11-16T05-39-17Z.db`. The upgrade then runs as one step: if it fails, the database is left exactly as it was and the hub doesn't start; `journalctl -u display-hub` says why. Either way the copy is kept, and the hub never deletes it. Once the upgraded hub has run well for a while, you may delete it yourself.

To go back to the old version after an upgrade, reinstall the old release, then restore the backup the installer made in step 1 (see "Restoring" above). The old hubd refuses an upgraded database rather than guess at it.

## Removing

`sudo bash install.sh --uninstall` stops and removes the service, the programs and the helper tools. It keeps `/var/lib/display-hub` (the identity and data), `/etc/display-hub` and the backups. Delete those yourself only if you'll never use this hub again.

It refuses while helpers are set up: remove each one first with `sudo display-hub-helper remove <name>`, which also cuts it off at the hub.

## On a real Pi: what to watch

These couldn't be tested without one. They're also in `USER_INSTRUCTIONS.md`.

- `install.sh` finishes with "Display Hub is running", and `hubd check` shows no problems except perhaps the clock in the first minute.
- `systemd-analyze security display-hub` reports about 1.0 ("OK").
- The example helper (see [Try the example](#try-the-example)): `sudo display-hub-helper run-now pi-status` succeeds, and `sudo cat /sys/class/thermal/thermal_zone0/temp` shows a number (the temperature in thousandths of a degree), so its page shows a real temperature rather than "No temperature sensor". `systemd-analyze security display-hub-helper@pi-status` reports about 1.1 ("OK").
- `hubd check` says the clock is synchronised once the Pi is online. The service can't make the system call it prefers for this, so it relies on systemd-timesyncd, which Raspberry Pi OS uses by default. If you've swapped in chrony or ntpd, the manager shows the clock as "Unknown". That's harmless.
- After `sudo reboot`, the service is back within a minute, and the manager reconnects.
- Pull the power while it runs, then power it back on. It starts again with its data intact. SQLite runs with full synchronous writes and the identity is written atomically, but only a real power cut proves it.
- Memory, with the service running: `systemctl status display-hub` shows its memory use, which should be well under the 256 MB limit, likely under 20 MB.
