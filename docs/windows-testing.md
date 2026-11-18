# Trying the desktop manager on Windows

The manager is built and tested on Linux, cross-built for Windows, and smoke-tested on a Windows runner in CI (the window opens and draws one frame with Windows' software renderer). It has also been used on a real Windows desktop, with a real Pi and an ESP32-S3 board. This page is the checklist for trying it yourself.

You don't need the Raspberry Pi for this. `hubd` also runs on Windows, on your own PC, and the display simulator stands in for a display.

## What you need

From a checkout of this repository, either:

- **The ready-made executables** in `dist\`, if someone has built them there (they're ignored by Git): `hub-manager.exe`, `hubd.exe`, `display-simulator.exe`, `hub-firmware.exe`, `hub-send.exe`, `usage-helper.exe` and `usage-helper-sync.exe`. They come from `cargo build --release --target x86_64-pc-windows-gnu` on Linux, and compile their shaders the first time the manager starts.
- **Or build them yourself**, with Rust and the Visual Studio Build Tools ("Desktop development with C++") installed:

  ```powershell
  cargo build --release --locked -p hub-manager -p hubd -p display-simulator
  ```

  They're then in `target\release\`. Use that folder instead of `dist\` below.

None of them are code-signed. If SmartScreen warns, choose **More info**, then **Run anyway**.

## 1. Start a hub on this PC

In PowerShell, from the checkout:

```powershell
.\dist\hubd.exe serve --data-dir hub-test --listen 127.0.0.1:8443
```

Leave it running. In a second PowerShell window, make a pairing string:

```powershell
.\dist\hubd.exe pair-admin --data-dir hub-test --listen 127.0.0.1:8443 --host 127.0.0.1
```

It prints a line starting with `dh1.`. Select it and copy it (Ctrl+C in Windows Terminal).

`hub-test\` holds a throwaway hub. Delete it when you're done.

## 2. The manager

Some Windows PCs can't look up `.local` names, especially with a VPN connected or on two networks at once (Ethernet and Wi-Fi). If the manager says it can't find the hub's name, pair with the Pi's address instead (`hostname -I` on the Pi shows it).

Start `dist\hub-manager.exe`. Check each of these, and note anything that differs:

| | Do | Expect |
|---|---|---|
| 1 | Look at the first screen | Black, with "Pair with your hub" in large Geist, small upper-case mono labels, white and orange corner ticks on the panels, a faint dot grid between them, and the orange Southern Cross with "Aotearoa New Zealand" in the footer. Text is crisp and grey-white, not colour-fringed. |
| 2 | Choose **Paste**, then **Pair** | The string fills the field. The status at the top right goes "Pairing", then "Online", and the Overview appears **without moving the mouse**. (On Linux the screen sometimes waited for the next mouse movement; check it doesn't here.) |
| 3 | Paste into Notepad | Nothing: the manager cleared the clipboard once it read the string. |
| 4 | Open **Credential Manager** (Start, type "Credential Manager"), then **Windows Credentials** | An entry mentioning `display-hub-manager` under Generic Credentials. That is where the admin token lives; it's in no file. |
| 5 | Close the manager and start it again | It reconnects straight to the Overview, with no pairing. |
| 6 | **Pages**: choose **Add from template**, then **Usage limits**, and add Claude Max ("Typed in"), ChatGPT Plus ("Demo") and OpenCode Go Plus ("Nothing yet") | Each appears in the list with MANUAL, DEMO or UNAVAILABLE, and a message along the bottom. Choosing a different service changes which sources the form offers ("Helper" isn't offered for OpenCode Go Plus). |
| 7 | **Edit** Claude Max: type `42` and `3h 20m` for five-hour, leave weekly empty, press Enter | A meter at 42 % with "Resets in 3 h 19 min". Weekly says "Not known", never 0 %. |
| 7a | **Add from template** again: **Big number**, **Short text**, **List**, **Meter**, then **Pi status** | Each appears with its kind named ("Big number", "Short text", "List", "Meter"). **Edit** lets you type in its content, and the list and the Overview draw it. Pi status offers a command with a **Copy** button; paste it into Notepad to check it, and it starts with `sudo display-hub-helper add pi-status`. The hub holds up to 50 pages. |
| 8 | Move pages with the arrow buttons, then Tab to a page's button and press **Alt+↑** or **Alt+↓** | The order changes and stays changed after **Refresh**. |
| 9 | Remove a page | A check screen asks first. Esc keeps the page. |
| 10 | Keyboard only: Tab and Shift+Tab, Enter and Space, Ctrl+1 to Ctrl+5, Esc | A 2 px orange frame shows around whatever has focus. Every control can be reached and used. |
| 11 | Resize the window from wide to about 720 px | Two columns become one; nothing is cut off or runs off the side. |
| 12 | Windows display scale at 150 % (Settings, System, Display) | Text stays sharp and nothing overlaps. |
| 13 | Turn on **Narrator** (Ctrl+Win+Enter) and Tab around | Buttons, tabs, fields and headings are read by name, and the message along the bottom is read when it appears. |

## 3. A simulated display

In **Displays**, choose **Add a display**, keep the name "Desk", leave Wi-Fi empty, and choose **Create setup details**. There's no USB display, so choose **Save as a file** and save it as `display-setup.dhbundle.json` in the checkout. Then:

```powershell
.\dist\display-simulator.exe enrol --bundle display-setup.dhbundle.json --state sim-test
.\dist\display-simulator.exe once --state sim-test
Remove-Item display-setup.dhbundle.json
```

The first command tries `raspberrypi.local` first and fails there (there's no Pi), then enrols at `127.0.0.1`. Within 5 seconds, the manager shows "Desk is set up" and lists the display. `once` prints the pages as a display would draw them, in the manager's order.

Then:

- Change a page in the manager and run `once` again: the change is there.
- In **Displays**, find "Pages on this display" under "Desk". Untick one page. "Unsaved changes" appears with **Save** and **Cancel**, and nothing has changed on the hub yet. Choose **Save**, run `once` again, and that page is gone. On **Pages**, the page now says "Not on any display", and the others say "On: Desk". Tick it again and save to bring it back.
- Tick pages until 10 are ticked (add pages first if you need to). The unticked boxes go grey and can't be ticked, with the note "A display shows at most 10 pages. Untick one to choose another." Tab to a tick box and press Space or Enter: it changes, with the same orange frame as other controls. Adding another page when "Desk" already shows 10 says "Not shown on Desk yet: it already shows 10 pages."
- Remove "Desk" in the manager and run `once` again: it says the display needs setting up again.

## 4. Things going wrong

| Do | Expect |
|---|---|
| Stop `hubd` (Ctrl+C), then **Refresh** | "Can't reach the hub" along the bottom, with **Try again**. Start `hubd` again and choose **Try again**: the message goes. |
| Stop `hubd`, start a second hub in another folder on the same port (`hubd.exe serve --data-dir hub-other --listen 127.0.0.1:8443`), then **Refresh** | "This isn't your hub", marked STOP in orange. Nothing is sent to the impostor. |
| On the hub: `hubd.exe admin list --data-dir hub-test`, then `hubd.exe admin revoke <id> --data-dir hub-test`, then **Refresh** in the manager | "This PC isn't paired any more", and the pairing screen. |
| **Settings**, **Forget this hub** | A check screen, then the pairing screen. The Credential Manager entry is gone. |

## 5. Over USB, later

Sending setup details over USB needs the display firmware to speak the serial exchange in `docs/protocol-v1.md`. Until it does, a real board won't answer, and the manager says "No display answered on that port" after 10 seconds. Once the firmware supports it:

- the board's port appears in the list with its USB name;
- **Send to COMx** sends the details;
- the board joins Wi-Fi and appears in the list.

The manager opens the port with DTR set and RTS clear, so an ESP32 board shouldn't reset when the port opens. If one does, the manager repeats its hello every 2 seconds until the board answers.

## 6. The helper on the PC (optional)

Only if you use Claude Code or Codex on this PC. [providers.md](providers.md) has the full set-up. If this PC was paired with the old `hubctl`, first remove that helper in **Settings**, then log in with `usage-helper` as below ([providers.md](providers.md), "Upgrading from `hubctl`").

1. In the manager, open **Settings**, then **Helper**. Choose **Create helper string**, then **Copy helper string**.
2. Run `.\dist\usage-helper.exe login` and paste it. Expect "Paired with the hub at 127.0.0.1:8443 …".
3. In **Pages**, **Edit** a Claude Max page and set its figures to **Helper**.
4. **Claude Code:** add `{"statusLine": {"type": "command", "command": "C:/path/to/dist/usage-helper.exe claude-statusline"}}` to `%USERPROFILE%\.claude\settings.json`, and send Claude a message. Use forward slashes: Claude Code runs the command through Git Bash, where backslashes are escape characters.
   - Expect a status line like `Opus 4.7 · 5h 42% · 7d 18%`.
   - Within a minute, the page shows those figures as REPORTED.
   - `.\dist\usage-helper.exe status` shows the send.
5. **Codex:** run `.\dist\usage-helper.exe codex-sync --dry-run`.
   - Expect "Codex reports five-hour …, weekly …".
   - Without `--dry-run`, it sends them to a ChatGPT Plus page set to **Helper**.
   - In Task Scheduler, `dist\usage-helper-sync.exe` should run with no window.
6. Remove the helper in **Settings**. The next send is refused, and `usage-helper status` says why.

Also check, anywhere in the manager: a hover highlight follows the pointer and never stays lit after the pointer leaves.

## 7. Firmware: Update, then Install firmware

[firmware-updates.md](firmware-updates.md) is the design. These steps use a throwaway signing key, made here and deleted afterwards. Never use it for real firmware.

### Update, with the simulated display

Make a key. It asks for a passphrase of at least 12 characters:

```powershell
.\dist\hub-firmware.exe keygen --out test.dhkey
```

Any small file will do as the "app image" for the simulator (it warns that the file isn't an ESP-IDF image, which is expected). Sign two versions of `README.md`:

```powershell
.\dist\hub-firmware.exe sign --key test.dhkey --image README.md --device-type llm-usage-display --hardware simulated --version 1.0.0 --security-counter 1 --out v1.0.0.dhfw
```

```powershell
.\dist\hub-firmware.exe sign --key test.dhkey --image README.md --device-type llm-usage-display --hardware simulated --version 1.1.0 --security-counter 1 --summary "A test release." --out v1.1.0.dhfw
```

Put 1.0.0 on a fresh simulated display, as Install firmware would. It then trusts the test key:

```powershell
.\dist\display-simulator.exe firmware install --state sim-fw --package v1.0.0.dhfw
```

Set it up as in section 3 (**Add a display**, **Save as a file**), then enrol it:

```powershell
.\dist\display-simulator.exe enrol --bundle display-setup.dhbundle.json --state sim-fw
```

| | Do | Expect |
|---|---|---|
| 1 | **Firmware** (Ctrl+4): **Add a firmware file**, choose `v1.1.0.dhfw` | "Added "llm-usage-display" 1.1.0 to the hub", signed by "a maker this app doesn't know" with a fingerprint like `3f9a-01c2-…`. It is listed with its hardware ("For simulated"), its size and "Updates only", and "A test release." under "The maker's note". |
| 2 | Choose a file that isn't a package (any `.txt`) | "This firmware file failed its checks". Nothing reaches the hub. |
| 3 | Under **Makers' keys**, **Name it**: type `Test maker`, press Enter | The key is tagged "The hub's name for it", and the package and the display now say "the key the hub calls "Test maker"". Naming it `Official Display Hub firmware` instead is refused: "That name is kept for official firmware". |
| 4 | **Displays**: the display's card | Under "Firmware": 1.0.0, "Accepts updates signed by the key the hub calls "Test maker" (…)", and **Update available** with **Update to 1.1.0**. |
| 5 | **Update to 1.1.0** | A check screen "Update … to 1.1.0?" naming the key the hub calls "Test maker" and its fingerprint, with "A test release." in a separate box under "The maker's note". Confirm it. The card says "Waiting for the display to check in". |
| 6 | Run the poll command below | Within about 15 seconds the card goes through "Downloading 1.1.0" to "Updated to 1.1.0". |
| 7 | Sign a `1.2.0-beta.1` the same way and add it, then press **Update to 1.2.0-beta.1** on the card | The button shows the version in its own case (`1.2.0-beta.1`, not `1.2.0-BETA.1`). Two check screens: the second says it's a pre-release and asks "Install it anyway?". **Keep it** on the second leaves everything as it was. If a newer full release (say 1.2.0) is on the hub too, it is the one under **Update available**, and the pre-release is under "Other versions on the hub". |
| 8 | The card also offers **Install 1.0.0**, if `v1.0.0.dhfw` is added too | Two check screens: the first asks "Go back to 1.0.0 on …?", the second says 1.0.0 is older and why going back is allowed. |
| 9 | Confirm an update, then **Cancel update** on the card before polling | "The update to … was cancelled". |

```powershell
.\dist\display-simulator.exe poll --state sim-fw --max-wait 5
```

Stop it with Ctrl+C once the card says "Updated to 1.1.0".

### Install firmware, with the real board

This erases the board completely. The firmware repository doesn't speak the USB setup exchange or hold firmware keys yet, so for now this checks the flashing half: the last step is expected to say the setup details weren't sent.

Build the display firmware with ESP-IDF as usual (`idf.py build`). From its `build` folder you need `bootloader\bootloader.bin`, `partition_table\partition-table.bin` and the app's `.bin`. On the ESP32-S3 the bootloader goes at `0x0`, the partition table at `0x8000` and the app at `0x10000` (check `idf.py build`'s last lines, which print the offsets). Sign them into one package. Use `waveshare-amoled-1.8-v1` or `-v2`, as marked on the back of the board:

```powershell
.\dist\hub-firmware.exe sign --key test.dhkey --image <app>.bin --device-type llm-usage-display --hardware waveshare-amoled-1.8-v1 --version 0.9.0 --security-counter 0 --usb-part bootloader@0x0=bootloader.bin --usb-part partition-table@0x8000=partition-table.bin --app-offset 0x10000 --chip esp32s3 --min-flash 0x1000000 --out board.dhfw
```

| | Do | Expect |
|---|---|---|
| 1 | Plug the board in with a data cable. **Firmware**, **Install firmware**, **Choose a file**, `board.dhfw` | The package as `"llm-usage-display" 0.9.0`, "For hardware waveshare-amoled-1.8-v1", "For an esp32s3 board with at least 16 MB of flash", and an orange box: "Signed by a maker this app doesn't know", the fingerprint, and the instruction to compare it. |
| 2 | Leave the Wi-Fi network empty and choose **Erase and install on COMx** | "Enter the Wi-Fi network's name". Nothing happens to the board. |
| 3 | Fill in the Wi-Fi details, choose **Erase and install on COMx** | A check screen, "Erase the display on COMx and install this firmware?", repeating the fingerprint, saying everything is erased and listing every key the display will trust afterwards. The package's device type, version and hardware are in a separate box, "What the package says about itself". **Keep it** leaves the board alone. |
| 4 | Confirm with **Erase and install** | "Connecting", "Erasing the whole flash" (up to a couple of minutes for 16 MB), then "Writing" each part with a percentage that climbs steadily to 100%, then "Restarting". |
| 5 | Watch the board | It restarts into the firmware you built, as after `idf.py flash`. |
| 6 | Wait for the app | For now, "The firmware is installed, but the setup details weren't sent", and **Displays** has them ready to send again, with a box saying the display will trust the keys from the firmware just installed on COMx and a **Leave them out** button. Sending to any other port sends the details without those keys. Once the firmware speaks the USB exchange, it says "Installed …" instead and the display checks in. |
| 7 | Open a serial monitor on the board's port, then try to install | "COMx is busy", saying to close the other program. Nothing is written. |
| 8 | If step 4 says "The board didn't start its loader" | Unplug the cable, hold **BOOT** while you plug it back in, let go, and try again. |
| 9 | Change `--chip` to `esp32` and sign again, then try to install it | "This firmware is for a different chip … the board on COMx is an esp32s3". Nothing is written. |

Delete `test.dhkey`, the `.dhfw` files and `sim-fw\` afterwards.

## Reporting back

Say which rows matched, which didn't, and what you saw instead. A screenshot (Win+Shift+S) helps with anything visual. **Copy diagnostics** in Settings gives a summary with no credentials in it.

Delete `hub-test\`, `hub-other\`, `sim-test\` and `sim-fw\` afterwards.
