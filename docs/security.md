# Security and privacy

## What we protect

- **Provider credentials.** These are tokens for usage sources. They stay on the owner's PC or the Pi, never on a display, in logs, in Git or in URLs.
- **What the pages show (such as usage figures) and the list of pages.** They are private to the household.
- **Control of the hub.** Only paired admins may add, remove or reorder pages, or add and revoke displays.
- **Honesty of figures.** Nothing invented is ever presented as real usage.

## Threat model

| Threat | In scope | Defence |
|---|---|---|
| Another device on the home Wi-Fi reading figures or changing settings | Yes | Every request is authenticated, with separate admin, display and helper credentials. The hub is LAN-only and accepts private source addresses only. |
| A spoofed mDNS answer or a hijacked IP address impersonating the hub | Yes | Discovery is never trust. Clients pin the hub's certificate and key, installed out of band. |
| An unknown or unpaired display | Yes | It has no credential, so it gets nothing. Enrolment needs a short-lived, single-use secret from an admin. |
| A lost or stolen display | Yes | Revoke it in the manager. Its token stops working immediately. It never held provider credentials. |
| Stolen manager configuration | Yes | The admin token lives in the OS credential store, never a plain file. `hubd admin revoke` and `revoke-all` on the Pi cut it off. |
| A hub, app or network that offers hostile firmware | Yes | Displays install only packages signed by a key they already hold, for their own board, at or above their security floor; the hub only stores and passes packages on, and never signs. An attempt budget on each display (five downloads, then one per four hours of its own running time) stops a hostile hub wearing out its flash ([firmware-updates.md](firmware-updates.md), "Threat model"). |
| Malformed or oversized requests and responses | Yes | Body limits, timeouts, a concurrency cap, strict validation and bounded text. |
| Accidental exposure of keys in logs, Git or fixtures | Yes | Credentials sit in redacting types. Request logs record no headers or queries. A test asserts no token reaches the logs. The Git ignore rules and pre-commit checks cover the rest. |
| Someone with root on the Pi, or an unlocked PC | Partly | Documented below. Out of reach of software alone. |
| Attacks from the internet | Out of scope by design | There is no inbound internet service, no UPnP and no port forwarding. Outbound connections to providers are fine. |

## Rules the code follows

1. Development mode (`hubd dev`) is plain HTTP with no authentication. It refuses every address except loopback, and is never deployable. Tested.
2. No listener on the network until HTTPS and authentication are in place (milestone 2).
3. No `danger_accept_invalid_certs` or equivalent, anywhere.
4. No provider credentials or admin tokens in firmware, logs, fixtures, URLs, command-line flags or Git.
5. No browser cookies or session tokens copied from another app, and no undocumented endpoints.
6. A meter item's `percent` (and a big number's `value`) is `null` when unknown, never zero. Every page carries its data quality. Demo figures say so on screen.
7. Text from users and providers is normalised, stripped of control and invisible characters, and bounded before it reaches a display.

## The hub's identity

`hubd` creates an ECDSA P-256 key and a self-signed certificate the first time it starts, in `/var/lib/display-hub/identity/`. The key is mode 0600, in a 0700 folder owned by the service user.

- **How it is protected:**
  - Files are written atomically, so a power cut leaves the old file or the whole new one.
  - The key and certificate are checked against each other at every start.
  - If the database exists but the identity doesn't, `hubd` **refuses to start**. Every display and admin trusts this exact key, so it is never replaced silently.
- **What trusts it:**
  - **The manager and the simulator** pin the SHA-256 of its public key (SPKI). A custom rustls verifier accepts that key only, and still checks handshake signatures normally.
  - **Displays** get the full certificate and check the fixed name `hub.display-hub.internal`, which is what ESP-IDF supports.
  - Addresses and `.local` names are only where to look, never proof of identity. A spoofed mDNS answer or a hijacked IP address gets a TLS failure, and nothing is sent. Tested.

## Pairing the desktop manager

1. On the Pi, over SSH: `sudo -u display-hub hubd pair-admin`. This prints a pairing string, `dh1.<payload>.<crc>`, holding:
   - the hub's address and port;
   - its key's SHA-256;
   - a 128-bit one-time secret.

   The hub stores only the secret's hash. The secret expires after 10 minutes and works once.
2. Paste the string into the manager. The manager connects, pins the key from the string, and exchanges the secret for a long-lived admin token (`dha_…`, 256 bits). It keeps the token in the operating system's credential store (Windows Credential Manager).
3. **Why this resists a man in the middle:** the key pin travels with the secret, out of band, over SSH. The guarantee is only as good as SSH's own host-key check, so accept the Pi's SSH host key with care the first time.
4. **Code reuse:** a code used a second time is refused, logged as a warning and recorded as an `ALARM_` event in the audit log. It may mean someone else saw the string.
5. **Revoking admins:** `hubd admin list` shows every admin. `hubd admin revoke <id>` and `hubd admin revoke-all` cut them off immediately, for example after losing a PC.

## Enrolling a display

1. In the manager, choose to add a display. The hub issues a one-time enrolment secret (10 minutes, single use) and returns its certificate.
2. The manager builds a provisioning bundle (hub addresses, the certificate and its hash, the secret, and optionally Wi-Fi details) and sends it over USB. A display that is already set up refuses a new bundle until it is factory-reset on the device itself.
3. The display connects over Wi-Fi, checks the certificate and name, and enrols. It receives a revocable device token (`dhd_…`) and keeps it in encrypted flash.
4. **Revoking a display:** remove it in the manager. Its token stops working on the next poll, and the display shows that it needs setting up again. It never held a provider credential.

## On the Pi

- The service runs as its own `display-hub` user with no capabilities, a read-only view of the system, private `/tmp` and devices, a system-call filter, and only local network addresses. `systemd-analyze security` rates it 1.0 ("OK").
- `/var/lib/display-hub` is 0700 and the key 0600. `hubd check` reports a data folder others can read.
- **Backups contain the private key.** `hubd backup` writes them 0700/0600 and says so. Keep copies off the Pi somewhere private.
- `hubd restore` refuses while the hub runs, and moves whatever it replaces aside instead of deleting it.
- `hubd display-bundle` writes a setup file, owner-only, for adding a display from the Pi itself. It works once, within 10 minutes.
- **Firmware packages** are in `/var/lib/display-hub/firmware/` (0700), each file 0600 and named by its SHA-256. The hub checks every upload in full before keeping it, but a display never relies on that: it checks the signature with its own keys. Packages aren't in backups; they can be uploaded again.
- **Helpers on the Pi** run apart from the hub (next section).

## Helpers on the Pi

A helper fetches data from the internet, so it is the part most likely to meet something hostile, and it may be someone else's code. The hub never touches the internet and holds the key every display and PC trusts, so a helper on the same Pi must never be able to reach it. `display-hub-helper` (installed by `install.sh`) runs each helper through `display-hub-helper@.service` and its timer:

- **Its own user, made up for it.** systemd's `DynamicUser=` gives each helper its own throwaway user (`dh-helper-<name>`), never the hub's and never another helper's. Its only writable place is its own folder, `/var/lib/private/display-hub-helper-<name>` (0700), which holds the hub's address and key hash and the token (0600). systemd shows each helper only its own folder.
- **The hub out of sight.** `/var/lib/display-hub`, `/etc/display-hub` and `/var/backups/display-hub` are made inaccessible, on top of their own permissions. The hub's process is hidden (`ProtectProc=invisible`), and so is most of `/proc`.
- **No way up.** No capabilities, no new privileges, no set-user-ID programs, a system-call filter, a read-only system (`ProtectSystem=strict`), no `/home`, private `/tmp`, devices and IPC, its own user namespace, and no changes to the kernel, clock, host name or control groups. Its program is a root-owned copy it can't change. Limits: 2 minutes a run, 128 MB, half a processor, 32 tasks.
- **Network:** IPv4, IPv6 and local sockets (for name lookups) only. It may reach the internet, which is its job, and the hub, on loopback when the hub listens on every address (the default; `hubd pair-helper --local` picks the address). It still pins the hub's key, so the address only says where to look.
- **`MemoryDenyWriteExecute=` is off by default**, because it breaks programs that compile code as they run (Node.js, Java, PyPy, LuaJIT) and some Python packages. A shell or plain Python helper can have it with a drop-in ([deploy-raspberry-pi.md](deploy-raspberry-pi.md#your-own-helper)).
- **The invite never rests anywhere.** `display-hub-helper add` makes it with `hubd pair-helper` and passes it on standard input to `hub-send login`, which runs inside the helper's own sandbox (`systemd-run --pipe`, with every sandbox setting of the helper's unit), so the token is created by and belongs to the helper's user. The invite is never in a command line, an environment variable, a file or the journal. The container test checks the journal and the helper's folder for it.
- **`systemd-analyze security`** rates the helper's unit 1.1 ("OK"), against 1.0 for the hub. The difference is the internet access a helper needs.
- **Cutting one off:** `display-hub-helper remove <name>` stops it, revokes it on the hub with `hubd helper revoke`, and deletes its program, folder and token.

The container test (`tools/pi-container-test/run.sh`) runs a probe helper that tries each of these: it can't read the hub's key, data, settings or backups, another helper's token, change its own program or `hub-send`, write outside its folder, see the hub's process, or send to another helper's feed.

What it doesn't stop: a helper can reach other devices on your network, as any program on the Pi can, and whatever it sends to its own feeds is shown. Someone with root on the Pi can do anything, as before.

## The desktop manager

- **The admin token** goes to the operating system's credential store: Windows Credential Manager, or the Secret Service on Linux. The manager checks the store works before pairing, so the hub never issues a token the PC can't keep, and it never falls back to a file.
- **`manager.json`** (in `%AppData%\display-hub\` on Windows) holds only the hub's address, port, key hash, the PC's name and when it paired. A test checks no token reaches it.
- **The pairing string** is cleared from the clipboard as soon as it's pasted, and from the field once used.
- **Wi-Fi passwords** show as dots, can't be copied out of the field, and are wiped from memory once the setup details are built. **Copy diagnostics** includes no credential, secret or Wi-Fi detail.
- **Setup details over USB** go only to the port you choose. **Save as a file** writes a file you should delete once the display has checked in; the manager says so.
- **What it talks to:** only the paired hub, pinned by key. It makes no other network requests and collects nothing.

## Helpers (`hub-send`, and the helper on a PC, `usage-helper`)

- **What it reads:** only what Claude Code passes its status line on stdin, and what `codex app-server` answers. It reads no browser data, passwords or other apps' tokens, and calls no provider endpoint itself ([providers.md](providers.md)).
- **What it holds:** a helper token (`dhc_`), in the OS credential store by default. `connection.json` has only the hub's address and key hash. The helper string is read from stdin, never the command line, so it stays out of shell history and process lists.
- **The file store:** a headless Linux service with no keyring (a Raspberry Pi helper under systemd) can ask `hub-send` for `--store file` (or `HUB_SEND_STORE=file`) instead. The token is then the file `token` in the helper's folder, created owner-only (0600) in an owner-only folder (0700), by writing a new file and renaming it into place. `hub-send` refuses to use it if it is a link, belongs to another user, or can be read by its group or anyone else. It is Unix only, and never chosen unless asked for.
- **What its token can do:** send page content to the feeds it was invited for (for the helper on the PC, Claude Max's, ChatGPT Plus's or both), and only to pages of those feeds whose figures come from "Helper" and whose kind matches. The hub refuses other feeds (403), content dated more than five minutes ahead, and content that breaks the page rules, such as shares outside 0–100 or a page over 3072 bytes. It can't read anything, change pages, or act as an admin or a display. `usage-helper` also checks the figures against the service's windows before sending them.
- **Cutting it off:** **Settings**, **Helper**, **Remove** in the manager, or `hubd helper revoke <id>` on the Pi (`hubd helper list` shows the ids). Its token stops working at once. `logout` (`usage-helper logout` or `hub-send logout`) deletes it from that computer.
- **Invites** work once, within 10 minutes. A reused invite is recorded as an `ALARM_` audit event, as pairing codes are.

## Credentials

| Credential | Who holds it | Can do | Stored on the hub as |
|---|---|---|---|
| Admin token `dha_` | Desktop manager (OS credential store) | Everything under `/v1/admin/` | SHA-256 hash |
| Device token `dhd_` | One display (encrypted NVS) | Read the snapshot, send heartbeats, as itself only | SHA-256 hash |
| Helper token `dhc_` | A helper, such as the helper on a PC, `usage-helper` (OS credential store, or an owner-only file for headless `hub-send`) | Send page content to the feeds it was invited for, for pages whose source is "Helper" and whose kind matches. Nothing else. | SHA-256 hash |
| Publisher token `dhp_` | A firmware maker's `hub-firmware` (OS credential store, or an owner-only file on a headless Linux build machine) | Upload firmware packages and list them. Nothing installs without the owner pressing **Update** and the display's own checks. | SHA-256 hash |
| Pairing and enrolment secrets | Pairing string, publisher string, bundle | Exchanged once for a token, within 10 minutes | SHA-256 hash |

- Each token type has its own prefix. A token used on another role's route gets `403`.
- Tokens and one-time secrets come from the OS's secure random source. They are held in types that print as `[redacted]` and are zeroed on drop.
- They are never written to logs. Request logs record the method, path, status, time and source address only. A test captures every log line at TRACE level, from every crate, during pairing, enrolment and revocation, and finds no secret.

## Network exposure

- `hubd serve` listens on IPv4 port 8443 by default.
- Before TLS starts, it closes connections from anything except private, link-local and loopback addresses.
- IPv6 is off by default. When it is on, only link-local and unique local sources are accepted, never global addresses, because a home router's IPv6 firewall can't be relied on.
- The systemd unit allows the service only local addresses too (`IPAddressAllow=`), and `deploy/raspberry-pi/nftables-display-hub.conf` can drop other sources before they reach it. There is no UPnP and no port forwarding.
- **Request limits:** request bodies are limited (2 KiB for displays, 16 KiB for admin, 16 MiB for a firmware upload, which is streamed to disk). Requests time out after 10 seconds (15 minutes for a firmware upload or download, and an upload that sends nothing for 60 seconds is dropped), TLS handshakes after 10 seconds, and headers after 10 seconds. At most 128 connections and 64 requests are handled at once.
- **Firmware downloads:** only to a display with an open offer for that package, one at a time for each display, at most 2 at once across the hub and 6 starts per display per hour.
- **Firmware uploads:** a publisher's upload may make room only by removing that publisher's own packages, and each publisher has at most 64 MiB of packages. The same release signed again (the same signer and manifest bytes) is kept as one package, since an ECDSA signature can be changed without the key. Signer labels claiming to be the official firmware are refused.
- **Rate limits:** failed sign-ins are limited to 20 a minute per address. Pairing and enrolment attempts are limited to 10 per 10 minutes per address. A 128-bit code can't be guessed within those limits.

## Recovery

| What happened | What to do |
|---|---|
| A display is lost or stolen | Remove it in the manager. Its token stops working immediately. |
| A display's Wi-Fi password changed | Factory-reset the display, then add it again (new bundle). Its old entry can be removed. |
| The PC with the manager is lost | On the Pi: `hubd admin list`, then `hubd admin revoke <id>`, or `revoke-all`. Pair the new PC with `pair-admin`. |
| The Pi's address changed | Nothing. Displays try every address in their bundle, and the key, not the address, identifies the hub. A fixed DHCP lease is still recommended. |
| The SD card failed or the Pi was replaced | Install, then `hubd restore` the latest backup, which includes the identity ([deploy-raspberry-pi.md](deploy-raspberry-pi.md#backups)). Without the identity, every display and admin must be set up again. |
| The key must change (suspected compromise) | Not automated in v0. Stop the hub, move `identity/` and the database aside, start fresh, then pair the manager and re-provision every display. Bundles already have room for two trust anchors, so a later version can roll keys over without re-provisioning. |
| A helper on the Pi misbehaves | `sudo display-hub-helper remove <name>`: it stops, its token is revoked and its files are deleted. |
| A publisher token is stolen | Revoke the publisher in the app. It could only add packages, which install on nothing without the owner's **Update** and the display's checks. |
| A pairing or enrolment code appears in the audit log as `ALARM_` | Someone tried a code that had already been used. Check `hubd admin list` and the manager's list of displays, and revoke anything you don't recognise. |

## Limitations (stated plainly)

- **Data at rest on the Pi isn't encrypted.** The database and key rely on file permissions and the service user. Anyone with root on the Pi, or the SD card in hand, can read them. Real at-rest encryption on a Pi that boots unattended would need a key-management story (a TPM or secure element), which it doesn't have.
- **The admin token is as safe as the PC's user account.** It sits in the OS credential store, which protects it from other users but not from software running as you.
- **The pairing string is only as safe as where you paste it.** The manager clears the clipboard after reading it. It expires after 10 minutes and works once.
- **Display-side protections are the firmware's job:** encrypted NVS, factory reset, and refusing a second bundle. They are specified here, in `docs/protocol-v1.md` and in `docs/firmware-handoff.md`, and implemented by each display's firmware.
- **Testing:** this has been tested with automated tests and an mbedTLS client on Linux, and the hub has run on a real Raspberry Pi 5. On an ESP32, only the app's first install over USB has been tried, with a test image; real display firmware hasn't been tested yet.
