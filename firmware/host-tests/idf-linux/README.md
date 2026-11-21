# Tests over ESP-IDF's own code, on a computer

Two harnesses that need ESP-IDF itself, built for its Linux target, so they are not part of `ctest` and the GitHub checks don't run them. They are here so that what the firmware's comments say was "tried on a computer" can be tried again. Both need ESP-IDF v5.5.6 exported (`. "$IDF_PATH/export.sh"`) and a build folder outside the repository; `hub-http` also needs Python 3 and `openssl`. Linux only.

## nvs-setup

ESP-IDF's NVS over the firmware's own [partitions.csv](../../partitions.csv), with the call sequences of [main/storage.c](../../main/storage.c):

```sh
firmware/host-tests/idf-linux/nvs-setup/run.sh /tmp/dh-nvs-setup
```

It checks that the 128 KiB `nvs_setup` partition holds the largest setup, token and snapshot, and takes 1500 snapshot saves in one step each (900 of them 32768 bytes) with nothing else changed; that `nvs_erase_all()` leaves the old bytes in flash; that the factory reset's erase leaves every byte of the partition 0xFF, NVS starts on it empty, and the `nvs` partition is byte for byte what it was; that a start after an erase cut short at any sector can finish the job; what the reset's mark is for; and that NVS starts empty, by itself, on a partition full of rubbish. It ends with `ALL PASSED`. It does not try `storage.c`'s own choice of when to erase at start-up (only for `ESP_ERR_NVS_NO_FREE_PAGES` and `ESP_ERR_NVS_NEW_VERSION_FOUND`): `storage.c` isn't built for a computer, and those two answers can't be had from NVS without writing its page format by hand.

NVS is started plain there, because ESP-IDF doesn't build NVS encryption for a computer. Encryption changes an entry's bytes, not how many entries a value takes, so the room and the erase are the same.

## hub-http

[main/hub_http.c](../../main/hub_http.c) exactly as the firmware builds it, over ESP-IDF's HTTP client, esp-tls and Mbed TLS, against a stand-in hub (`server.py`, on 127.0.0.1) that misbehaves on purpose:

```sh
firmware/host-tests/idf-linux/hub-http/run.sh /tmp/dh-hub-http
```

It takes about five minutes, because seven of the cases wait out the 30-second limit. It prints a line for each case and ends with the count of checks and failures:

- honest answers, GET and POST, with TLS 1.2 and 1.3, also with the answer cut into 7-byte TCP segments; a `302` handed back and not followed; an answer longer than allowed;
- a certificate from another key, and the right key under another name: both `DH_TRY_WRONG_HUB`;
- nothing listening, and a name that doesn't exist;
- a TLS handshake sent a byte at a time, a connection that says nothing, headers sent a byte at a time, headers that never end, and rubbish after the handshake: each given up at 30 seconds, and each ended at once when `stop()` turns true;
- a body that trickles (30 seconds) and one that goes quiet (10 seconds);
- a download: opened, read to the end and closed; its headers trickled; stopped during the handshake; an error answer.

`run.sh` makes throwaway test certificates in the build folder on each run. None is kept in the repository.

The times it prints come in steps of about 50 ms: the Linux target's stand-in for `select()` only looks at the socket every so often (ESP-IDF's `FreeRTOSSimulator_wrappers.c`). On the chip, lwIP's `select()` returns as soon as bytes arrive.
