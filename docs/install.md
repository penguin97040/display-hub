# Installing Display Hub

This page gets Display Hub running from the ready-made downloads: the desktop app on your PC, and the hub on a Raspberry Pi. You add displays later, from the app.

**These are previews.** Display Hub works, but it's young: expect rough edges, and read the notes for each release before upgrading.

If you'd rather build everything yourself, see [Getting going](../README.md#getting-going) and [Running the hub on a Raspberry Pi](deploy-raspberry-pi.md).

## 1. Download

Open the [releases page](https://github.com/penguin97040/display-hub/releases) and choose the newest release. Under **Assets**, download the file for your computer, plus `SHA256SUMS`:

| Your computer | File |
|---|---|
| Windows 10 or 11, 64-bit (Intel or AMD) | `display-hub-windows-x64-<version>.zip` |
| A Mac with Apple silicon (M1 or newer), macOS 11 or later | `display-hub-macos-arm64-<version>.zip` |
| Linux, 64-bit (Intel or AMD), a recent distribution such as Ubuntu 24.04, Debian 13 or Fedora 40 | `display-hub-linux-x64-<version>.tar.gz` |

Each holds the desktop app (`hub-manager`, or **Display Hub** on a Mac) and the command-line tools: `hub-send` for sending your own data to the hub, `usage-helper` for sending Claude Code and Codex usage figures, `hub-firmware` for firmware makers and `display-simulator` for trying things without a display. They also hold the official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8 (V2) display, which the app offers first in **Install firmware** and can add to **Firmware on the hub**, so displays can update over Wi-Fi.

You don't need to download the Pi's file (`display-hub-pi-arm64-<version>.tar.gz`) on your PC. The one line in [step 4](#4-set-up-the-hub-on-the-pi) downloads it straight onto the Pi.

## 2. Check the download

A checksum is a long number worked out from a file's contents. If even one byte changes, the checksum changes. `SHA256SUMS` lists the checksum of every file in the release, so you can check that your download is exactly the file the release made, and didn't break or get swapped on the way.

Open `SHA256SUMS` in a text editor and find the line for your file. Then, in the folder you downloaded to:

- **Windows**, in PowerShell or Command Prompt:

  ```powershell
  certutil -hashfile display-hub-windows-x64-<version>.zip SHA256
  ```

- **macOS**, in Terminal:

  ```sh
  shasum -a 256 display-hub-macos-arm64-<version>.zip
  ```

- **Linux**, in a terminal. This finds the line for you and prints `OK`:

  ```sh
  sha256sum -c SHA256SUMS --ignore-missing
  ```

The long number printed must match the one in `SHA256SUMS` (capital or small letters don't matter). If it doesn't, delete the download and try again; don't open it.

What this can't do: someone able to change the release page itself could change `SHA256SUMS` as well. It protects the download, not the page.

## 3. Open the app the first time

Windows and macOS warn you about an app that isn't **signed**, that is, stamped with a certificate from Microsoft or Apple that names who made it. Those certificates cost money every year, and Display Hub is a free project, so its downloads aren't signed. The warning means "we can't tell who made this", not "this is harmful". Check the download (step 2) first, then tell your computer it's fine, once.

### Windows

1. Right-click the `.zip` file and choose **Extract All**, then **Extract**. Running the app from inside the zip doesn't work properly.
2. In the folder it makes, double-click `hub-manager.exe`.
3. If a blue box says **Windows protected your PC** (that's Microsoft Defender SmartScreen), choose **More info**, then **Run anyway**.

If Windows still refuses, or a command-line tool is blocked: delete the extracted folder, right-click the `.zip` file, choose **Properties**, tick **Unblock** at the bottom, choose **OK**, and extract it again.

The command-line tools run from PowerShell in that folder, for example `.\hub-send.exe --help`.

### macOS (Sequoia, macOS 15, and later)

1. Double-click the `.zip` file to unpack it. Drag **Display Hub** into your **Applications** folder if you like.
2. Double-click **Display Hub**. macOS says it can't check the app for malicious software. Choose **Done** (not **Move to Bin**).
3. Open **System Settings**, choose **Privacy & Security** and scroll down to **Security**. Next to the message that Display Hub was blocked, choose **Open Anyway**. Enter your password, then choose **Open Anyway** again.

From then on it opens normally. On macOS 14 (Sonoma) or earlier, Control-click the app, choose **Open**, then **Open** again.

**Or in Terminal**, which also covers the command-line tools: remove the "downloaded from the internet" mark from the folder you unpacked, so macOS doesn't ask. Do this only for a download you've checked.

```sh
xattr -dr com.apple.quarantine ~/Downloads/display-hub-macos-arm64-<version>
```

If you've already moved the app into Applications, run it for that too: `xattr -dr com.apple.quarantine "/Applications/Display Hub.app"`. The command-line tools are in the unpacked folder, beside the app: for example `./hub-send --help`.

### Linux

Unpack it and start the app:

```sh
tar -xzf display-hub-linux-x64-<version>.tar.gz
./display-hub-linux-x64-<version>/hub-manager
```

It needs a desktop (X11 or Wayland), and a keyring (the Secret Service, such as GNOME Keyring or KWallet) to keep the hub's credential safe. Most desktops have both. If it says a library such as `libxkbcommon-x11.so.0` is missing, install it: on Debian and Ubuntu, `sudo apt install libxkbcommon-x11-0`. To add a display over USB, your user may need to be allowed to use serial ports: on Debian and Ubuntu, `sudo usermod -aG dialout "$USER"`, then log out and in again.

## 4. Set up the hub on the Pi

You need a Raspberry Pi running Raspberry Pi OS Lite (64-bit), on the same network as your PC, that you can reach over SSH. [Running the hub on a Raspberry Pi](deploy-raspberry-pi.md) covers setting it up ("What you need") and connecting safely ("Connect over SSH, and check the Pi's key").

Open the desktop app. Its first-run guide shows one line with a **Copy** button. Copy it, paste it into the Pi's terminal and press Enter. The notes on the release page show the same line.

For version 0.1.0 it looks like this, with the real checksum where `<checksum>` is (copy the line from the app or the release notes, which have it filled in):

```sh
d=$(mktemp -d) && cd "$d" && curl -fsSL -o 'display-hub-pi-arm64-0.1.0.tar.gz' 'https://github.com/penguin97040/display-hub/releases/download/v0.1.0/display-hub-pi-arm64-0.1.0.tar.gz' && echo '<checksum>  display-hub-pi-arm64-0.1.0.tar.gz' | sha256sum -c - && tar -xzf 'display-hub-pi-arm64-0.1.0.tar.gz' && sudo bash display-hub-pi/install.sh
```

What each part does:

1. `d=$(mktemp -d)` makes a new, empty folder under `/tmp` that only you can open, and keeps its name in `$d`.
2. `cd "$d"` goes into it, so nothing lands in your home folder and no older copy gets in the way.
3. `curl -fsSL -o '<file>' '<address>'` downloads the hub's files from the release on GitHub. `-f` stops if GitHub sends an error instead of the file, `-sS` keeps it quiet except for errors, and `-L` follows GitHub's hand-over to its download server.
4. `echo '<checksum>  <file>' | sha256sum -c -` checks the download against the checksum built into the app. It prints `OK`, or `FAILED` and stops.
5. `tar -xzf '<file>'` unpacks it into a `display-hub-pi` folder: the hub (`hubd`), `hub-send` and the installer with its files.
6. `sudo bash display-hub-pi/install.sh` runs the installer with administrator rights, so it asks for your password. [What the installer does](deploy-raspberry-pi.md#what-the-installer-does).

The `&&` between the parts means "only if that worked". If the download fails or the checksum doesn't match, nothing is unpacked or installed.

It takes about a minute, and ends by printing a pairing string. The installer keeps what it needs later (a copy of itself, for removing the hub, and the optional firewall rule) in `/usr/local/share/display-hub/`, so the download isn't needed afterwards. Raspberry Pi OS clears old files from `/tmp` by itself; to remove the download now, in the same terminal: `cd && rm -rf "$d"`. If the Pi says `curl: command not found`, run `sudo apt install curl` first.

## 5. Pair the app with the hub

Copy the whole pairing string the installer printed (the line starting with `dh1.`) and paste it into step 3 of the app's guide. It works once, within 10 minutes. If it has run out, make a new one on the Pi: `sudo -u display-hub hubd pair-admin`. [More about pairing](deploy-raspberry-pi.md#4-pair-the-desktop-manager).

## Upgrading

Upgrade the app and the hub together, to the same version.

- **The hub:** copy the one line from the new release's notes and run it on the Pi, as in step 4. The installer sees the hub already there: it backs it up, replaces the programs, keeps your settings and restarts it. [More about upgrading](deploy-raspberry-pi.md#upgrading).
- **The app:** download the new version and check it, as above. On Windows and Linux, replace the old folder with the new one. On a Mac, drag the new **Display Hub** into Applications and choose **Replace**. Your hub, its pairing and your settings are kept, because they're stored elsewhere. Windows or macOS may warn you once more (step 3), and a Mac may ask whether the new version may use the hub's credential in your keychain: choose **Always Allow**.

## Getting help

Ask on the project's [issues page](https://github.com/penguin97040/display-hub/issues). Say what you did, what you expected and what happened, and include:

- the app's version: **Settings**, under **About**. **Copy diagnostics** on the same screen copies more detail, with no secrets in it;
- the hub's version and recent log, from the Pi: `hubd --version` and `journalctl -u display-hub -n 50 --no-pager`.

Never post a pairing string, a token, a backup or anything from `/var/lib/display-hub`: they let someone control your hub.
