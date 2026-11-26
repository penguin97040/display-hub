# Security

Display Hub is a preview, looked after by one person in their spare time. Thank you for helping to keep it safe.

## Reporting a security problem

Please report it privately, not in a public issue, so it can be fixed before anyone can misuse it:

1. Open the repository on GitHub and choose the **Security** tab.
2. Choose **Report a vulnerability**.
3. Say what you found, how to make it happen, and what someone could do with it. Steps, a short example or a log help a lot. Leave out real tokens, pairing strings and backups.

Only you and the maintainer see the report. Please don't open a public issue, a pull request or a discussion about it, or share details elsewhere, until a fix has been released or you've agreed otherwise in the report.

## What's covered

- **The hub** (`hubd`), including its installer and service for the Raspberry Pi (`deploy/raspberry-pi/`).
- **The desktop app** (`hub-manager`) and the command-line tools (`hub-send`, `usage-helper`, `usage-helper-sync`, `hub-firmware`, `display-simulator`).
- **The official display firmware** (`firmware/`) and how displays are set up over USB.
- **Firmware signing and updates:** the signed packages, the keys the app and displays trust, and the checks the release makes ([firmware/official/README.md](firmware/official/README.md)).
- **The release downloads** and how they're built ([.github/workflows/release.yml](.github/workflows/release.yml)).

A problem in a library Display Hub uses is welcome too, if it affects Display Hub. Please report it to that library's own project as well.

What Display Hub protects, and what it doesn't try to, is set out in [docs/security.md](docs/security.md). Its "Limitations" section lists the known gaps, so they needn't be reported.

## What to expect

- There is no bug bounty or payment.
- There is no promised response time. Reports are read and answered as soon as the maintainer can, and the serious ones come first.
- If you'd like to be thanked by name in the release that fixes it, say so in the report.
