# Trying the desktop manager on Windows

The manager is built and tested on Linux, cross-built for Windows, and smoke-tested on a Windows runner in CI (the window opens and draws one frame with Windows' software renderer). Nobody has yet used it on a real Windows desktop. This page is the checklist for doing that.

You don't need the Raspberry Pi for this. `hubd` also runs on Windows, on your own PC, and the display simulator stands in for a display.

## What you need

From a checkout of this repository, either:

- **The ready-made executables** in `dist\`, if someone has built them there (they're ignored by Git): `hub-manager.exe`, `hubd.exe` and `display-simulator.exe`. They come from `cargo build --release --target x86_64-pc-windows-gnu` on Linux, and compile their shaders the first time the manager starts.
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

Start `dist\hub-manager.exe`. Check each of these, and note anything that differs:

| | Do | Expect |
|---|---|---|
| 1 | Look at the first screen | Black, with "Pair with your hub" in large Geist, small upper-case mono labels, white and orange corner ticks on the panels, a faint dot grid between them, and the orange Southern Cross with "Aotearoa New Zealand" in the footer. Text is crisp and grey-white, not colour-fringed. |
| 2 | Choose **Paste**, then **Pair** | The string fills the field. The status at the top right goes "Pairing", then "Online", and the Overview appears **without moving the mouse**. (On Linux the screen sometimes waited for the next mouse movement; check it doesn't here.) |
| 3 | Paste into Notepad | Nothing: the manager cleared the clipboard once it read the string. |
| 4 | Open **Credential Manager** (Start, type "Credential Manager"), then **Windows Credentials** | An entry mentioning `display-hub-manager` under Generic Credentials. That is where the admin token lives; it's in no file. |
| 5 | Close the manager and start it again | It reconnects straight to the Overview, with no pairing. |
| 6 | **Pages**: add Claude Max ("Typed in"), ChatGPT Plus ("Demo figures") and OpenCode Go Plus ("Nothing yet") | Each appears in the list with MANUAL, DEMO or UNAVAILABLE, and a message along the bottom. |
| 7 | **Edit** Claude Max: type `42` and `3h 20m` for five-hour, leave weekly empty, press Enter | A meter at 42 % with "Resets in 3 h 19 min". Weekly says "Not known", never 0 %. |
| 8 | Move pages with the arrow buttons, then Tab to a page's button and press **Alt+↑** or **Alt+↓** | The order changes and stays changed after **Refresh**. |
| 9 | Remove a page | A check screen asks first. Esc keeps the page. |
| 10 | Keyboard only: Tab and Shift+Tab, Enter and Space, Ctrl+1 to Ctrl+4, Esc | A 2 px orange frame shows around whatever has focus. Every control can be reached and used. |
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

## Reporting back

Say which rows matched, which didn't, and what you saw instead. A screenshot (Win+Shift+S) helps with anything visual. **Copy diagnostics** in Settings gives a summary with no credentials in it.

Delete `hub-test\`, `hub-other\` and `sim-test\` afterwards.
