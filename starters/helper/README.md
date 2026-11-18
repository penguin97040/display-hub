# Starter helper

A helper is a small program that collects something and sends it to a page on your hub. This one sends a short line of text, "Hello from my helper. It last ran at 14:05.", so you can see the whole path working before you change it to send something real.

| File | What it is |
|---|---|
| `my-helper` | The helper: a shell script that sends one `text` page to the feed `my-helper` with `hub-send`. |
| `page.json` | The page it fills, written out: a text page whose content comes from a helper, on the feed `my-helper`. The desktop app makes it for you (step 1). |
| `template.json` | The same page as a template, with a title to fill in, for when you share it ([docs/templates.md](../../docs/templates.md)). |

It is written like `pi-status`, the example helper that comes with Display Hub (`deploy/raspberry-pi/helpers/pi-status`). [docs/helpers.md](../../docs/helpers.md) explains helpers in full, and [BUILDING-A-DISPLAY.md](../../BUILDING-A-DISPLAY.md) is the guide for makers.

## What you need

- A hub that is running, and a display or the display simulator to look at.
- The desktop app, paired with the hub, to make the page.
- `hub-send` on the computer that runs the helper. The hub's Raspberry Pi already has it. Anywhere else, build it from this repository with `cargo build --release --locked -p hub-send`, and put `target/release/hub-send` on your `PATH`.

The script is a POSIX shell script, for Linux, a Raspberry Pi or another Unix system. On Windows, `hub-send` works, but the script needs a shell such as Git Bash; that hasn't been tried yet.

## 1. Make the page

In the desktop app, open **Pages**, then **A page for your own helper**, and fill it in:

- **Title**: `My helper`, or anything you like.
- **Kind**: **Short text**, which is what `my-helper` sends.
- **Feed**: `my-helper`.

Press **Add page**. The page appears in the list, waiting for its first update, and the app shows how to feed it, on the Pi or on another computer, with commands to copy.

(For a hub you started yourself for testing, pair the app with it first: run `hubd pair-admin` with the same `--data-dir` and `--listen` you gave `hubd serve`, and `--host 127.0.0.1`, then paste the string into the app.)

## 2. Pair the helper

**On the hub's Raspberry Pi,** skip this step: `display-hub-helper` pairs it for you (step 3).

**On another computer,** make an invite. In the app, press **Invite a helper** under the new page's steps (or open **Settings**, tick `my-helper` under **Helper** and untick the usage limits), then **Create helper string**. Or make one on the hub:

```sh
sudo -u display-hub hubd pair-helper --feed my-helper
```

Then, on the helper's computer, run `hub-send login` and paste the invite when asked. It works once, within 10 minutes. The helper can then send only to `my-helper`.

## 3. Run it

**On another computer:**

```sh
./my-helper
```

It prints `Updated 1 page: my-helper`, and the display shows the text within a few minutes. Run it on a schedule, such as with cron or a systemd timer, to keep the page fresh.

**On the hub's Raspberry Pi,** copy this folder to the Pi, then in it:

```sh
sudo display-hub-helper add my-helper --run "$PWD/my-helper"
```

That pairs it, runs it once, shows what it printed, and from then on runs it every 5 minutes in a sandbox of its own. `sudo display-hub-helper list` shows how it's going. After changing the script, run `sudo display-hub-helper update my-helper --run "$PWD/my-helper"`.

## 4. Make it yours

1. Copy this folder, and rename `my-helper` to your helper's name: lower-case letters, digits and single hyphens, 20 characters at most, such as `bus-times`.
2. Change the feed name `my-helper` to match, in the script, `page.json` and `template.json`, and make its page in the app with that feed.
3. Change the part of the script marked "What to show".
4. To send another kind of page, change `--kind` in the script, and choose the same kind for its page in the app (and in `page.json`). For example, a big number is `--kind big_number` with data like `{"value": 21.5, "decimals": 1, "unit": "°C"}`. The kinds and their limits are in [BUILDING-A-DISPLAY.md](../../BUILDING-A-DISPLAY.md#describing-pages).

Keep a page's content small (3072 bytes at most), send `null` rather than 0 when you don't know a figure, and keep any API keys or passwords on the helper's computer, never in a page or template.

## Checking it end to end

These steps were run on Linux against a real hub with a scratch data folder, and passed. They are the helper part of [the finish line](../../BUILDING-A-DISPLAY.md#the-finish-line): `hubd serve` with a scratch `--data-dir`, `hubd pair-helper`, `hub-send login` with the file store, the page made, `./my-helper` printing `Updated 1 page: my-helper`, then `display-simulator once` showing the page as `REPORTED`. The page was made then with a script that the app has since replaced; the app's own tests make the same kind of page against a real hub, invite a helper for its feed and fill it with `hub-send`.
