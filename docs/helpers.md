# Helpers: sending your own data to a page

A **helper** is anything that collects a number or a line of text somewhere and sends it to your hub: a script on a Raspberry Pi reading a soil sensor, a weather job, a build checker. The hub keeps the page and the displays show it. The hub doesn't care where the data came from.

A helper is invited to one or more **feeds**. A feed is just a name, such as `garden-weather`. A page is fed by a feed when its source is a helper and it names the feed. Whatever a helper sends to a feed goes to every page of that feed whose kind matches.

### Pages for a helper

For now the manager's own "Add page" makes helper pages for usage limits only (as "PC helper"). **Templates** ([templates.md](templates.md)) describe other pages, such as `pi-status/1`, and the hub keeps the template and settings each page was made with, but the manager doesn't offer templates yet. Until it does, any other page is made with the hub's admin API: `POST /v1/admin/pages` with, for example, `{"title": "Pi status", "kind": "big_number", "source": "helper", "feed": "pi-status"}`. `tools/pi-container-test/run.sh` shows a complete example.

## Pairing

1. Make a helper string: on the hub, run `hubd pair-helper --feed garden-weather` (repeat `--feed` for more feeds). For the usage helper on a PC, the manager's Settings makes one too.
2. On the helper's computer, run `hub-send login` and paste the string when asked. It is read from standard input, never from the command line, and works once, within 10 minutes.

`hub-send status` shows the hub, the feeds this helper may send to and recent activity. `hub-send logout` forgets the hub. Remove the helper in the manager's Settings to cut it off at the hub, or on the hub run `hubd helper list`, then `hubd helper revoke <id>`.

**A helper on the hub's own Raspberry Pi** needs none of these steps: `sudo display-hub-helper add <name> --run <program>` pairs it, runs it on a timer and keeps it in a sandbox of its own, away from the hub's keys and data. See [Helpers on the Pi](deploy-raspberry-pi.md#helpers-on-the-pi).

## Sending

The door is `PUT /v1/helper/feeds/{feed}` with the helper token. The body is a whole page body:

```json
{ "kind": "big_number", "data": { "value": 21.5, "unit": "°C" }, "collected_at": "2026-11-16T09:30:00Z", "note": "Greenhouse" }
```

`collected_at` and `note` are optional. The hub answers with the pages it updated and the pages of the feed it left alone because they are another kind:

```json
{ "updated": ["greenhouse-temp"], "skipped": ["greenhouse-note"] }
```

The content has to pass the same rules as any page (and 3072 bytes at most). The full list of checks is in [architecture.md](architecture.md#helper-api).

With `hub-send`, the same thing is:

```sh
echo '{"value": 21.5, "unit": "°C"}' | hub-send page garden-weather --kind big_number --data - --note Greenhouse
```

It prints which pages were updated or skipped, and exits non-zero with the hub's message if the hub refuses. `--data` takes a file name or `-` for standard input, and `--collected-at` takes a UTC time like `2026-11-16T09:30:00Z`.

A Rust helper can use the `hub_send` library instead: `Profile::login`, `Profile::open` and `Hub::send_page` do the same steps.

## Reading the pages' settings

A page made from a template keeps the owner's answers to its form, its **settings** ([templates.md](templates.md)), such as which garden bed or which service. A helper can read the settings of the pages it fills, so one program can serve pages set up in different ways:

`GET /v1/helper/feeds/{feed}/settings` with the helper token answers, for each page of the feed whose source is a helper, in page order:

```json
{ "pages": [
    { "page_id": "garden-north", "template": "garden/1", "settings": { "bed": "north", "depth": 10 } },
    { "page_id": "greenhouse-temp", "template": null, "settings": {} }
] }
```

A page made without a template has `template: null` and empty settings. With `hub-send`, `hub-send settings garden-weather` prints the same, and a Rust helper can call `Hub::settings`.

Settings are not secret: every helper of the feed can read them. Keep API keys and passwords on the helper's own computer, never in a page's settings.

## Own feeds only

A helper's token can write only to the feeds it was invited for, and only to pages whose source is a helper and whose kind matches what it sent. The only thing it can read is the settings of those pages, as above. It can't read pages' content, change pages, or act as an admin or a display. A feed it wasn't invited for is refused with a 403, for reading and for sending.

## Headless services: the file store

By default the token goes in the computer's credential store (Windows Credential Manager, or the Secret Service on Linux). A headless Linux service, such as a Raspberry Pi helper under systemd, usually has no keyring. For that, use the file store, on Unix only:

```sh
hub-send --store file --config-dir /var/lib/garden-helper login
```

or set `HUB_SEND_STORE=file` and `HUB_SEND_DIR=/var/lib/garden-helper` for the service. The token is then the file `token` in that folder. It is created owner-only (0600) in an owner-only folder (0700). `hub-send` refuses to use it if another user owns it, if it is a link, or if its group or anyone else can read it. Don't put the folder somewhere other users can reach, and run the service as the user that logged in.

## Where things are kept

The settings are in `<your config folder>/display-hub/hub-send/` unless `--config-dir` or `HUB_SEND_DIR` says otherwise: the hub's address and key hash in `connection.json` (no secrets), and a small rotating `helper.log`. The token is never in the log or in anything `hub-send` prints.
