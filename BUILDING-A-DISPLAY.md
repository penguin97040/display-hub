# Building for Display Hub

This is the place to start if you want to make something for Display Hub, by hand or with an AI agent. You don't need to change Display Hub itself. You can make:

- **a helper**: a small program that collects data (the weather, a sensor, a build result) and sends it to a page;
- **a template**: a small JSON file that offers a set of pages with a short form, such as "Which garden bed?";
- **display firmware**: the program inside a display, for a screen Display Hub doesn't support yet, or a custom kind of page drawn your own way.

Most people only need a helper. Start with [starters/helper/](starters/helper/), which sends one page and can be copied and changed.

## How the parts fit

```text
 helper (any computer, or on the hub's Pi)
   │  hub-send page <feed> --kind text --data -
   ▼
 hubd, the hub ── keeps the pages ──▶ each display polls it over pinned HTTPS
   ▲                                   and draws the pages it is given
   │
 hub-manager, the desktop app: pairs, adds pages and displays, installs firmware
```

- **The hub (`hubd`)** runs on an always-on computer, usually a Raspberry Pi. It keeps up to 50 pages in one shared order, and knows which pages each display shows. It never goes on the internet. Helpers do that part.
- **A display** asks the hub for its pages every few minutes (every five, unless the hub's settings say otherwise) and draws exactly the pages it is given, at most 10, in order. It never listens for connections itself. If the hub goes quiet, it keeps showing the last pages it had, marked with their age.
- **A helper** sends a page's content to a **feed**: a name such as `garden-weather`. The hub puts it on every page of that feed whose content comes from a helper and whose kind matches. The door it uses is `hub-send`, a command-line tool and Rust library ([docs/helpers.md](docs/helpers.md)).
- **The desktop app (`hub-manager`)** is for the owner. It pairs with the hub, makes pages from templates, adds displays over USB, and installs and updates firmware.
- **`display-simulator`** behaves exactly as a display must, in a terminal. Use it to test anything without hardware.

[docs/architecture.md](docs/architecture.md) has the full picture.

## Pairing and trust, briefly

Nothing talks to the hub until it has been paired, and every pairing works the same way. The hub prints a one-time string (it starts with `dh1.`) or a setup file. It works once, within 10 minutes. It carries the hub's address, a fingerprint of the hub's key, and a secret. The other side checks the hub's key against that fingerprint, then swaps the secret for its own long-lived token (a password the hub issues):

| Who | How they pair | What their token can do |
|---|---|---|
| The desktop app | `hubd pair-admin` | Everything an owner can |
| A display | A setup file (the "provisioning bundle") the app sends over USB | Fetch its own pages, and report on itself |
| A helper | `hubd pair-helper --feed <feed>`, then `hub-send login` | Send content to its own feeds, and read their pages' settings. Nothing else. |
| A firmware maker's computer | `hubd pair-publisher`, then `hub-firmware login` | Upload firmware packages and list them. Nothing installs until the owner presses **Update**. |

Pairing strings are always read from standard input, never from the command line, so they stay out of shell history. Tokens go in the computer's credential store, or an owner-only file for a headless Linux service. The hub has one self-signed certificate, and displays and the app trust exactly that certificate, never a public authority and never an address on its own. [docs/security.md](docs/security.md) has the details, and [docs/protocol-v1.md](docs/protocol-v1.md) is the exact contract with displays.

## Describing pages

Every page has a title (1 to 48 characters), a **kind**, and its content, the `data`. The kind says what shape the data is in.

### The four ready-made shapes

Any display that follows the protocol can draw these. Use one whenever it fits: your page then works everywhere.

| Kind | Data | Limits |
|---|---|---|
| `meter` | `{"items": [{"id": "soil", "label": "Soil moisture", "percent": 41.5}]}` | 1 to 6 bars. Each `percent` is 0 to 100, or `null` for unknown. Labels up to 48 characters. Optional `resets_at`. |
| `big_number` | `{"value": 21.5, "decimals": 1, "unit": "°C", "caption": "Greenhouse"}` | `value` between -1e15 and 1e15, or `null` for unknown. `decimals` 0 to 3, `unit` up to 8 characters, `caption` up to 48. |
| `text` | `{"text": "Bins go out tonight."}` | 1 to 280 characters. |
| `list` | `{"items": [{"text": "Bus 21", "detail": "4 min"}]}` | Up to 8 lines. `text` and `detail` up to 48 characters each. |

**Unknown is `null`, never zero.** A display shows `null` as a dash or "unknown". Never send 0 when you don't know.

All text is plain: no line breaks or invisible characters, and no spaces at the start or end. A short `note` (up to 96 characters) can go with any page.

### Custom kinds

For anything the shapes don't cover, name your own kind, with a version: `nearby-flights/1`. It is lower-case letters, digits and hyphens, optionally in dot-separated parts (`org.example.board/2`), then `/` and a number from 1 to 9999. The data is any JSON object, nested at most 8 levels deep. The hub doesn't look inside it. Your helper and your firmware agree what it means.

- **The version is part of the kind.** If you change what the data means, change the version: `nearby-flights/2` is a different kind, so old firmware never misreads it.
- A display whose firmware doesn't know a kind shows a **"can't show this" card** for that page: the title, then "This display can't show this page." and "It may need a firmware update." The other pages still show. This is the normal, safe outcome, not an error.

### Size limits

| What | Limit |
|---|---|
| One page, as compact JSON | 3072 bytes |
| Everything a display is sent at once | 32 KiB, at most 10 pages |
| Pages on one hub | 50 |
| One helper request | 16 KiB |

The hub refuses content that breaks a rule and says which one. If a stored page somehow breaks the rules, the hub sends it as an unavailable page with a note instead, so one bad page never blanks the others.

### Where the figures came from

Every page carries its **data quality**: `demo`, `provider_reported` (shown as REPORTED), `estimated`, `manual` or `unavailable`. Content from a helper is stored as `provider_reported`. A display always shows which one it is. Never pass off sample or made-up figures as real.

## Writing a helper

A helper is any program that runs on its own and sends a page body with `hub-send`. The simplest is one line:

```sh
echo '{"text": "Bins go out tonight."}' | hub-send page my-helper --kind text --data -
```

`hub-send` prints which pages it updated, and exits with an error and the hub's message if the hub refuses. `--note` adds a note, and `--collected-at 2026-11-16T09:30:00Z` says when the data was collected (UTC, at most five minutes ahead of the hub's clock). A Rust helper can use the `hub_send` library instead (`Profile::login`, `Profile::open` and `Hub::send_page`).

**Start from [starters/helper/](starters/helper/).** It has a shell-script helper in the same style as the `pi-status` example, a page to go with it, and a matching template. Its README walks through every step below.

### The page it fills

A helper only fills pages that already exist. The page needs its content to come from a helper, the same kind as the helper sends, and the helper's feed.

- The app makes such pages from its built-in templates, such as **Pi status** for the `pi-status` example: **Pages**, then **Add from template**.
- For a feed of your own, the app makes the page too: **Pages**, then **A page for your own helper**. Give it a title, the kind your helper sends (one of the four shapes, or a kind of your own such as `nearby-flights/1`) and the feed. The app then shows how to feed it, on the Pi or on another computer, with the commands to copy.

### Running it on a computer

1. Make an invite for the helper's feeds. In the app, open **Settings**, tick the feed under **Helper** (or type it), untick the usage limits unless the helper sends those, and press **Create helper string**. Or, on the hub: `hubd pair-helper --feed my-helper` (on a Pi: `sudo -u display-hub hubd pair-helper --feed my-helper`). Repeat `--feed` for more feeds.
2. On the helper's computer, run `hub-send login` and paste the invite when asked.
3. Run your program, by hand or on a schedule. `hub-send status` shows the hub, the feeds and recent activity.

The token goes in the system's credential store: Windows Credential Manager, or the Secret Service on Linux. A headless Linux service with no keyring uses the file store instead: `hub-send --store file --config-dir <folder> login`, or `HUB_SEND_STORE=file` and `HUB_SEND_DIR=<folder>` (Linux and other Unix systems only).

### Running it on the hub's Raspberry Pi

```sh
sudo display-hub-helper add my-helper --run /full/path/to/my-helper --every 15min
```

That pairs it, runs it once, then runs it on a timer (5 minutes unless `--every` says otherwise, between once a minute and once a day). Each helper runs as its own throwaway user, in a sandbox that can't see the hub's keys or data. It may reach the internet and your network.

- The name is lower-case letters, digits and single hyphens, 20 characters at most. It is also the feed, unless you add `--feed <feed>` (repeat for more).
- The program is copied, so run `sudo display-hub-helper update my-helper --run <program>` after changing it. `list`, `run-now`, `logs` and `remove` look after it.
- It gets `DISPLAY_HUB_HELPER` (its name), `DISPLAY_HUB_FEEDS` (its feeds, separated by spaces) and its own folder in `HUB_SEND_DIR`.
- Each run has 2 minutes, 128 MB of memory and half a processor. It can't read `/home`.
- A helper that needs an API key or password of its own isn't covered by `display-hub-helper` yet.

`deploy/raspberry-pi/helpers/pi-status` is the example that comes with Display Hub, and [Helpers on the Pi](docs/deploy-raspberry-pi.md#helpers-on-the-pi) has the details.

### One program, pages set up different ways

A page made from a template keeps the owner's answers to its form, its **settings**. A helper can read them for its own feeds: `hub-send settings my-helper` prints them as JSON (`Hub::settings` in Rust). Settings are not secret: every helper of the feed can read them.

## Writing a template

A template is a JSON file, at most 16 KiB. It has an `id` with a version (`garden/1`), a name, a one-line description, an optional one-step `setup` command for the owner (such as the `display-hub-helper add` line that starts its helper), a short form of `settings`, and the `pages` the answers make. A page's title and feed can use the answers, as `{key}` placeholders. A template holds no code, so a template from anyone can't run anything.

[docs/templates.md](docs/templates.md) is the full format, with a worked example. `starters/helper/template.json` is a small one that matches the starter helper.

- **Never ask for API keys, passwords or tokens in a template.** Its answers are visible to every helper of the feed and in the app.
- Give a changed template a new version, so pages made from the old one can still be told apart.
- **The app offers only its built-in templates for now.** They are in `crates/templates/templates/`, listed in `crates/templates/src/lib.rs`. To offer yours in Display Hub itself, add it there in a pull request. Before then, try its pages out by making them in the app with **A page for your own helper**, with the kinds and feeds the template would give them.

## Building display firmware

A display follows the display protocol, [docs/protocol-v1.md](docs/protocol-v1.md). In short, it:

1. waits for its setup file over USB serial (and refuses a second one until it is factory-reset on the device);
2. joins the Wi-Fi network, then enrols with the hub using the setup file's one-time secret, and keeps the token it gets in encrypted storage;
3. polls the hub over HTTPS, trusting only the hub's own certificate and checking the name `hub.display-hub.internal`;
4. draws the pages it is given, in order, with their data quality and age, and a "can't show this" card for any page it can't draw;
5. keeps the last good set of pages, and explains problems in plain words;
6. if it takes updates, installs only firmware signed by a key it already holds ([docs/firmware-updates.md](docs/firmware-updates.md)).

**The brief for firmware writers is [docs/firmware-handoff.md](docs/firmware-handoff.md).** It covers the board, the first install and partition layout, storage, TLS settings, the display's side of signed updates and what to copy from here, and is the document to follow.

**Start from the official firmware.** The official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8 is in [firmware/](firmware/README.md): C with ESP-IDF 5.5.6, tested on real boards. It is the starter project for a new display. Its protocol and signed-update core, [firmware/components/dh_core](firmware/components/dh_core/README.md), is plain C with no hardware, and passes every vector below as host tests. Its README says where it differs from the reference display, and why.

What to test against, all in this repository:

- `protocol/v1/`: JSON Schemas for every message, valid and invalid examples, and language-neutral **conformance vectors** (`protocol/v1/vectors/` for pages, `protocol/v1/firmware/` for signed updates). Port the vectors to your firmware's unit tests.
- `tools/display-simulator`: the reference display. Run it beside your board and compare.
- `tools/mbedtls-interop/client.c`: an mbedTLS client configured as an ESP32 firmware's should be, tested against the real hub.

## Signing and shipping firmware

Firmware updates are signed by their maker and delivered through the hub. The hub stores and passes packages on, and never signs. The display checks the signature with its own keys before it writes anything. The signing tool is `hub-firmware`:

| Step | Command |
|---|---|
| Make a key, protected by a passphrase of at least 12 characters | `hub-firmware keygen --out my-displays.dhkey` |
| Sign a release into a package (`.dhfw`) | `hub-firmware sign --key my-displays.dhkey --image app.bin --device-type my-display --hardware my-board-v1 --version 1.0.0 --security-counter 1 --key-set my-displays.dhkey --key-set recovery.dhkey --out my-display-1.0.0.dhfw` |
| Check a package and see who signed it | `hub-firmware inspect my-display-1.0.0.dhfw` |
| Pair with a hub as a publisher | `hub-firmware login`, then paste the string from `hubd pair-publisher` (or from the app) |
| Put a package on the hub's shelf | `hub-firmware add my-display-1.0.0.dhfw`, or `hub-firmware publish` to sign and add in one step |

A first install goes over USB from the app (**Install firmware**), which needs a package made with the USB-install options (`--usb-part`, `--app-offset`, `--chip` and `--min-flash`). Later updates go through the hub when the owner presses **Update**.

- **Keep two keys.** One for everyday signing, and a recovery key kept offline in two places and never used. Name both with `--key-set` from your very first release, so every display trusts both.
- **Publish the fingerprint** that `keygen` prints, where your users will see it. It is how they check a first install.
- **Raise `--security-counter`** in any release that fixes a security hole or retires a key. Displays refuse anything below the highest counter they have installed and confirmed.
- Never commit a key file, and never send it anywhere.

[docs/firmware-updates.md](docs/firmware-updates.md) is the full design, including what to do if a key is lost or leaked. [docs/windows-testing.md](docs/windows-testing.md#7-firmware-update-then-install-firmware) rehearses an update with a throwaway key and the simulator.

## The rules not to break

These keep owners safe. They come from [docs/security.md](docs/security.md), [docs/protocol-v1.md](docs/protocol-v1.md) and [docs/firmware-updates.md](docs/firmware-updates.md).

**Secrets**

1. Never log, print, display or send anywhere a token, a pairing string, a setup file's secret, a Wi-Fi password or a signing key. Tokens start with `dha_`, `dhd_`, `dhc_` or `dhp_`.
2. Never put one on a command line, in a URL, in a template's settings, in a test file or in Git. Pairing strings go in on standard input. Keep a helper's own API keys on the helper's computer.

**Trust**

3. Never skip a certificate check. A display trusts only the certificate in its setup file and checks the name `hub.display-hub.internal`. Never use a bundle of public authorities, and never trust an address or `.local` name on its own.
4. Firmware installs only packages signed by a key it already holds, using its own stored copy of the key. It checks the signature before it reads the package's manifest, and it checks everything before it writes anything to flash. There is no unsigned or development mode.
5. Firmware refuses packages for another device type or board, and anything below its security floor. A factory reset keeps its trusted keys and floor.

**Pages**

6. Stay within the size limits above. A display refuses a response over 32 KiB before parsing it.
7. Unknown is `null`, never zero, and every page says where its figures came from. Never present sample or made-up figures as real.
8. One bad page must never blank the others: show the "can't show this" card for that page and carry on.
9. Change a custom kind's version whenever you change what its data means.

**Helpers and the hub**

10. A helper writes only to its own feeds. Don't try to reach the hub's data, other helpers' feeds or the admin routes.
11. The hub never touches the internet, and has no inbound internet service. Fetching from the internet is a helper's job. Never forward the hub's port on your router.
12. A display only makes outbound connections. It never listens on a port.
13. `hubd dev` is plain HTTP with no security, for trying things on one computer. Never deploy it, and never point firmware at it.

## The finish line

You're done when these pass for what you built. Each command was checked against the code; run them from a checkout of this repository. Use a scratch folder (here `/tmp/dh-test`) for test hubs, never your real hub's data.

### A helper

Start a real hub with a scratch data folder, and leave it running:

```sh
cargo run -p hubd -- serve --data-dir /tmp/dh-test/hub --listen 127.0.0.1:8443
```

In a second terminal, build `hub-send` and put it on your `PATH`:

```sh
cargo build --release --locked -p hub-send
```

It is then in `target/release/` (or under `$CARGO_TARGET_DIR` if you set it). Then pair the helper, using the file store so the test token stays in the scratch folder:

```sh
cargo run -p hubd -- pair-helper --data-dir /tmp/dh-test/hub --listen 127.0.0.1:8443 --host 127.0.0.1 --feed my-helper
mkdir -p -m 700 /tmp/dh-test/helper
hub-send --store file --config-dir /tmp/dh-test/helper login
```

Make the page your helper fills in the app. Pair the app with the test hub using a string from `pair-admin`, then open **Pages**, **A page for your own helper**, with the feed `my-helper` and the kind your helper sends (**Short text** for the starter):

```sh
cargo run -p hubd -- pair-admin --data-dir /tmp/dh-test/hub --listen 127.0.0.1:8443 --host 127.0.0.1
```

Run your helper from its folder, with the same store. It must print `Updated 1 page: …`:

```sh
HUB_SEND_STORE=file HUB_SEND_DIR=/tmp/dh-test/helper ./my-helper
```

Finally, set up a simulated display and check the page shows, with its content and `REPORTED`:

```sh
cargo run -p hubd -- display-bundle --data-dir /tmp/dh-test/hub --listen 127.0.0.1:8443 --host 127.0.0.1 --out /tmp/dh-test/display.dhbundle.json
cargo run -p display-simulator -- enrol --bundle /tmp/dh-test/display.dhbundle.json --state /tmp/dh-test/display
cargo run -p display-simulator -- once --state /tmp/dh-test/display
```

`once` exits with 0 when it reached the hub. Delete `/tmp/dh-test` when you're done: it holds a test hub's key and tokens. On the Pi, `sudo display-hub-helper run-now <name>` is the same check for a helper there.

### A template

Its pages, made in the app with **A page for your own helper** as the template would make them, must fill and show as above. To add it to the app's built-in templates, `cargo test -p hub-templates` must pass, with your template in the list that test checks.

### Display firmware

- Your firmware's unit tests pass every case in `protocol/v1/vectors/` and, if it takes updates, `protocol/v1/firmware/`.
- Beside `display-simulator`, against the same hub, your display shows the same pages, cards, ages and problems.
- The vectors still match the reference display: `cargo run -p display-simulator -- conform check` prints that every vector matches.
- Your package passes `hub-firmware inspect`, and shows the fingerprint you publish.

### Anything changed in this repository

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

[docs/developer-setup.md](docs/developer-setup.md) lists what each platform needs to build.

## Read more

| Document | For |
|---|---|
| [docs/helpers.md](docs/helpers.md) | Sending data: pairing, sending, settings, the file store |
| [docs/templates.md](docs/templates.md) | The template format |
| [docs/protocol-v1.md](docs/protocol-v1.md) | The contract between the hub and a display |
| [docs/firmware-handoff.md](docs/firmware-handoff.md) | The brief for display firmware |
| [firmware/README.md](firmware/README.md) | The official firmware, and the starter project for your own |
| [docs/firmware-updates.md](docs/firmware-updates.md) | Signed firmware updates |
| [docs/security.md](docs/security.md) | How it is kept safe |
| [docs/deploy-raspberry-pi.md](docs/deploy-raspberry-pi.md) | Running the hub and helpers on a Pi |
| [docs/architecture.md](docs/architecture.md) | How it all fits, and the hub's admin API |
