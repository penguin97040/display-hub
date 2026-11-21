{{PREVIEW}}**Display Hub {{VERSION}}**, a preview. It works, but it's young: expect rough edges. The installer backs your hub up before an upgrade. The downloads include the official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8 (V2) display: the app offers it in **Install firmware**.
{{STABLE}}**Display Hub {{VERSION}}**. The installer backs your hub up before an upgrade. The downloads include the official firmware for the Waveshare ESP32-S3-Touch-AMOLED-1.8 (V2) display: the app offers it in **Install firmware**.

<!-- Write what's new in this release here, then publish. -->

## Downloads

| Your computer | File |
|---|---|
| Windows 10 or 11, 64-bit | `display-hub-windows-x64-{{VERSION}}.zip` |
| A Mac with Apple silicon, macOS 11 or later | `display-hub-macos-arm64-{{VERSION}}.zip` |
| Linux, 64-bit, a recent distribution such as Ubuntu 24.04 | `display-hub-linux-x64-{{VERSION}}.tar.gz` |
| The hub, for a Raspberry Pi with 64-bit Raspberry Pi OS (the line below fetches it) | `{{PI_TARBALL}}` |

`SHA256SUMS` lists every file's checksum: [how to check a download]({{REPO_URL}}/blob/{{TAG}}/docs/install.md#2-check-the-download).

The apps aren't signed (that costs money every year), so Windows and macOS warn you the first time you open one: [how to open them]({{REPO_URL}}/blob/{{TAG}}/docs/install.md#3-open-the-app-the-first-time).

## Set up or upgrade the hub on the Pi

In a terminal on the Pi, run this one line. The desktop app's first-run guide shows the same line, with a Copy button.

```sh
d=$(mktemp -d) && cd "$d" && curl -fsSL -o '{{PI_TARBALL}}' '{{PI_URL}}' && echo '{{PI_SHA256}}  {{PI_TARBALL}}' | sha256sum -c - && tar -xzf '{{PI_TARBALL}}' && sudo bash display-hub-pi/install.sh
```

It downloads the hub's files, checks them against the checksum `{{PI_SHA256}}`, and installs them only if they match. [What each part does]({{REPO_URL}}/blob/{{TAG}}/docs/install.md#4-set-up-the-hub-on-the-pi). Upgrade the app and the hub together.
