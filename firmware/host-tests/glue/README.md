# A stand-in for the board, for `main/update.c`

`main/update.c` is the part of the firmware that reads an update from the hub, writes it to the spare app partition and runs the trial boot's clock. It can't run without ESP-IDF, so these files stand in for the few ESP-IDF calls it makes: a connection that hands out a body in pieces, a spare partition that is an array (with the one erase the firmware may make of it, of its first sector), a clock the test moves, and storage that is the update core itself.

- `include/` has headers with the names `update.c` includes (`esp_ota_ops.h`, `freertos/task.h` and so on), holding only what it uses.
- `glue.h` and `glue.c` are the stand-in, and the switches a test sets.

Two tests use it (see `../CMakeLists.txt`):

- `dh_fw_conform_glue` is `../fw_conform_cli.c` built so that a `package` vector's download goes through `update_receive()` in `main/update.c` instead of straight through the update core. Every firmware vector must still give exactly its `expect`.
- `dh_update_glue_tests` (`../update_glue_tests.c`) covers what the vectors can't: the two time limits, failing flash calls, a failed save, a setup that goes mid-download, and the trial boot's clock against a check-in.

Nothing here is built into the firmware.
