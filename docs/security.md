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

Pairing, enrolment, revocation, the trust anchor and recovery are documented here when milestone 2 is complete.
