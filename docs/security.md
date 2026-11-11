# Security and privacy

## What we protect

- **Provider credentials.** These are tokens for usage sources. They stay on the owner's PC or the Pi, never on a display, in logs, in Git or in URLs.
- **Usage figures and the subscription list.** They are private to the household.
- **Control of the hub.** Only paired admins may add, remove or reorder pages, or add and revoke displays.
- **Honesty of figures.** Nothing invented is ever presented as real usage.

## Threat model

| Threat | In scope | Defence |
|---|---|---|
| Another device on the home Wi-Fi reading figures or changing settings | Yes | Every request is authenticated, with separate admin, display and collector credentials. The hub is LAN-only and accepts private source addresses only. |
| A spoofed mDNS answer or a hijacked IP address impersonating the hub | Yes | Discovery is never trust. Clients pin the hub's certificate and key, installed out of band. |
| An unknown or unpaired display | Yes | It has no credential, so it gets nothing. Enrolment needs a short-lived, single-use secret from an admin. |
| A lost or stolen display | Yes | Revoke it in the manager. Its token stops working immediately. It never held provider credentials. |
| Stolen manager configuration | Yes | The admin token lives in the OS credential store, never a plain file. `hubd admin revoke` and `revoke-all` on the Pi cut it off. |
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
6. `used_percent` is `null` when unknown, never zero. Every page carries its data quality. Demo figures say so on screen.
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

## The desktop manager

- **The admin token** goes to the operating system's credential store: Windows Credential Manager, or the Secret Service on Linux. The manager checks the store works before pairing, so the hub never issues a token the PC can't keep, and it never falls back to a file.
- **`manager.json`** (in `%AppData%\display-hub\` on Windows) holds only the hub's address, port, key hash, the PC's name and when it paired. A test checks no token reaches it.
- **The pairing string** is cleared from the clipboard as soon as it's pasted, and from the field once used.
- **Wi-Fi passwords** show as dots, can't be copied out of the field, and are wiped from memory once the setup details are built. **Copy diagnostics** includes no credential, secret or Wi-Fi detail.
- **Setup details over USB** go only to the port you choose. **Save as a file** writes a file you should delete once the display has checked in; the manager says so.
- **What it talks to:** only the paired hub, pinned by key. It makes no other network requests and collects nothing.

## Credentials

| Credential | Who holds it | Can do | Stored on the hub as |
|---|---|---|---|
| Admin token `dha_` | Desktop manager (OS credential store) | Everything under `/v1/admin/` | SHA-256 hash |
| Device token `dhd_` | One display (encrypted NVS) | Read the snapshot, send heartbeats, as itself only | SHA-256 hash |
| Collector token `dhc_` (milestone 5) | PC helper (OS credential store) | Report figures for the pages it is assigned | SHA-256 hash |
| Pairing and enrolment secrets | Pairing string, bundle | Exchanged once for a token, within 10 minutes | SHA-256 hash |

- Each token type has its own prefix. A token used on another role's route gets `403`.
- Tokens and one-time secrets come from the OS's secure random source. They are held in types that print as `[redacted]` and are zeroed on drop.
- They are never written to logs. Request logs record the method, path, status, time and source address only. A test captures every log line at TRACE level, from every crate, during pairing, enrolment and revocation, and finds no secret.

## Network exposure

- `hubd serve` listens on IPv4 port 8443 by default.
- Before TLS starts, it closes connections from anything except private, link-local and loopback addresses.
- IPv6 is off by default. When it is on, only link-local and unique local sources are accepted, never global addresses, because a home router's IPv6 firewall can't be relied on.
- Use the firewall rule in `deploy/raspberry-pi/` (milestone 4) as well. There is no UPnP and no port forwarding.
- **Request limits:** request bodies are limited (2 KiB for displays, 16 KiB for admin). Requests time out after 10 seconds, TLS handshakes after 10 seconds, and headers after 10 seconds. At most 128 connections and 64 requests are handled at once.
- **Rate limits:** failed sign-ins are limited to 20 a minute per address. Pairing and enrolment attempts are limited to 10 per 10 minutes per address. A 128-bit code can't be guessed within those limits.

## Recovery

| What happened | What to do |
|---|---|
| A display is lost or stolen | Remove it in the manager. Its token stops working immediately. |
| A display's Wi-Fi password changed | Factory-reset the display, then add it again (new bundle). Its old entry can be removed. |
| The PC with the manager is lost | On the Pi: `hubd admin list`, then `hubd admin revoke <id>`, or `revoke-all`. Pair the new PC with `pair-admin`. |
| The Pi's address changed | Nothing. Displays try every address in their bundle, and the key, not the address, identifies the hub. A fixed DHCP lease is still recommended. |
| The SD card failed or the Pi was replaced | Restore the latest backup, which includes the identity (milestone 4). Without the identity, every display and admin must be set up again. |
| The key must change (suspected compromise) | Not automated in v0. Stop the hub, move `identity/` and the database aside, start fresh, then pair the manager and re-provision every display. Bundles already have room for two trust anchors, so a later version can roll keys over without re-provisioning. |
| A pairing or enrolment code appears in the audit log as `ALARM_` | Someone tried a code that had already been used. Check `hubd admin list` and the manager's list of displays, and revoke anything you don't recognise. |

## Limitations (stated plainly)

- **Data at rest on the Pi isn't encrypted.** The database and key rely on file permissions and the service user. Anyone with root on the Pi, or the SD card in hand, can read them. Real at-rest encryption on a Pi that boots unattended would need a key-management story (a TPM or secure element), which it doesn't have.
- **The admin token is as safe as the PC's user account.** It sits in the OS credential store, which protects it from other users but not from software running as you.
- **The pairing string is only as safe as where you paste it.** The manager clears the clipboard after reading it. It expires after 10 minutes and works once.
- **Display-side protections are the firmware's job:** encrypted NVS, factory reset, and refusing a second bundle. They are specified here and in `docs/protocol-v1.md`, but implemented in `llm-usage-display`.
- **Testing:** this has been tested with automated tests and an mbedTLS client on Linux. It has **not** been tested on a Raspberry Pi or an ESP32.
