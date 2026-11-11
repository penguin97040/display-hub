# Running the hub on a Raspberry Pi

`hubd` runs on a Raspberry Pi 5 (a Pi 4 works too) as a systemd service, around the clock. Displays and the desktop manager reach it over HTTPS on port 8443 on your local network.

**What has been tested:** the installer, the service, restarts, a crash, a network cut, a reboot, a bad setting, backup and restore, an upgrade and an uninstall. They ran in a Debian Bookworm container with systemd (`tools/pi-container-test/run.sh`, also in CI). The ARM64 binary was also run under emulation against Bookworm's and Trixie's own C libraries. **Nobody has run it on a real Pi yet.** The steps below are written for that, and the last section lists what to watch for.

## What you need

- A Raspberry Pi running **Raspberry Pi OS Lite (64-bit)**, Bookworm or Trixie, with SSH turned on. In Raspberry Pi Imager, choose the OS, then **Edit settings**: set a host name (`raspberrypi` is assumed below), a user, Wi-Fi or Ethernet, and **Enable SSH** with a password or your public key.
- The Pi and your PC on the same network.
- **Recommended:** a fixed address for the Pi. In your router's settings, reserve its current address for it (often called a "DHCP reservation" or "static lease").

## 1. Get the files

You need a folder holding the ARM64 `hubd` and the four files from `deploy/raspberry-pi/`: `install.sh`, `display-hub.service`, `hubd.toml` and `nftables-display-hub.conf`.

- **Ready-made:** if `dist\display-hub-pi\` exists in your checkout, that's the folder. Alternatively, CI builds `hubd` for every push: the `hubd-raspberry-pi-arm64` artefact on the run's page on GitHub. Copy the four files next to it.
- **Or build it** on Linux, with [zig](https://ziglang.org) and `cargo install cargo-zigbuild`:

  ```sh
  cargo zigbuild --release --locked -p hubd --target aarch64-unknown-linux-gnu.2.36
  mkdir display-hub-pi
  cp target/aarch64-unknown-linux-gnu/release/hubd deploy/raspberry-pi/* display-hub-pi/
  ```

  The `.2.36` suffix builds against the C library of Raspberry Pi OS Bookworm, so one binary runs on Bookworm and Trixie. A binary built on the Pi itself, or by a newer distribution, may not run on Bookworm.

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

Then on the Pi:

```sh
cd display-hub-pi
sudo bash install.sh
```

The installer does the following:

1. Creates a `display-hub` system user with no login.
2. Installs `/usr/local/bin/hubd`, plus `/etc/display-hub/hubd.toml` if you don't have one.
3. Installs the hardened `display-hub.service` (systemd rates its exposure 1.0, "OK"), then enables and starts it.
4. On first start, the hub makes its identity: an ECDSA key and certificate in `/var/lib/display-hub/identity/`, readable only by the service.
5. Prints `hubd check`. An unsynchronised clock is normal for a minute after boot.

## 4. Pair the desktop manager

On the Pi:

```sh
sudo -u display-hub hubd pair-admin
```

Copy the `dh1.…` line, paste it into the desktop manager and choose **Pair**. It works once, within 10 minutes. The manager then remembers the hub, and connects to it by the address in the string (`raspberrypi.local` by default).

If your PC can't resolve `raspberrypi.local`, give the address instead: `sudo -u display-hub hubd pair-admin --host 192.168.1.20`.

## 5. Optional: settings and firewall

- **`/etc/display-hub/hubd.toml`** explains each setting. The one most worth setting is `advertise_hosts`: put the Pi's fixed address after its `.local` name. Displays try those addresses in order, and an ESP32 may not resolve `.local` names. Restart afterwards with `sudo systemctl restart display-hub`.
- **Firewall:** hubd already refuses connections from outside your local network, and the service is limited to local addresses as well. To stop such connections even earlier, run:

  ```sh
  sudo apt install nftables
  sudo nft -f nftables-display-hub.conf
  ```

  This rule is lost on reboot unless you include it from `/etc/nftables.conf` and run `sudo systemctl enable nftables`.
- **No port forwarding.** The hub is for your local network only. Never forward 8443 on your router.

## Day to day

| To | Run on the Pi |
|---|---|
| See whether it's running | `systemctl status display-hub` |
| Read its log | `journalctl -u display-hub -f` (Ctrl+C to stop) |
| Check the setup | `sudo -u display-hub hubd check` |
| See the key fingerprint, to compare with the manager's Settings | `sudo -u display-hub hubd fingerprint` |
| List or cut off paired PCs | `sudo -u display-hub hubd admin list`, then `sudo -u display-hub hubd admin revoke <id>` |
| Restart | `sudo systemctl restart display-hub` |

## Backups

The hub's identity is what every display and PC trusts. If it's lost, each of them has to be set up again. So back it up, and keep the backup as safe as the Pi: **it contains the private key**.

```sh
sudo install -d -m 0700 -o display-hub -g display-hub /var/backups/display-hub
sudo -u display-hub hubd backup /var/backups/display-hub/$(date +%F)
```

This is safe while the hub runs. To keep a copy off the Pi:

```sh
sudo tar -C /var/backups/display-hub -czf ~/display-hub-backup.tgz $(date +%F)
```

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

`--replace` moves the empty hub aside into `/var/lib/display-hub/replaced-<time>/` rather than deleting it. Displays and the manager carry on as before, because the key is the same. If the Pi's address changed, they find it through the other addresses they know, or you can pair the manager again.

## Upgrading

Copy the new folder over, then run `sudo bash install.sh` in it again. It does the following:

1. Backs up the hub.
2. Replaces the binary and the unit.
3. Keeps your `hubd.toml`.
4. Restarts the service.

## Removing

`sudo bash install.sh --uninstall` stops and removes the service and binary. It keeps `/var/lib/display-hub` (the identity and data), `/etc/display-hub` and the backups. Delete those yourself only if you'll never use this hub again.

## On a real Pi: what to watch

These couldn't be tested without one. They're also in `USER_INSTRUCTIONS.md`.

- `install.sh` finishes with "Display Hub is running", and `hubd check` shows no problems except perhaps the clock in the first minute.
- `systemd-analyze security display-hub` reports about 1.0 ("OK").
- `hubd check` says the clock is synchronised once the Pi is online. The service can't make the system call it prefers for this, so it relies on systemd-timesyncd, which Raspberry Pi OS uses by default. If you've swapped in chrony or ntpd, the manager shows the clock as "Unknown". That's harmless.
- After `sudo reboot`, the service is back within a minute, and the manager reconnects.
- Pull the power while it runs, then power it back on. It starts again with its data intact. SQLite runs with full synchronous writes and the identity is written atomically, but only a real power cut proves it.
- Memory, with the service running: `systemctl status display-hub` shows its memory use, which should be well under the 256 MB limit, likely under 20 MB.
