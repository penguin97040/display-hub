/* A stand-in for ESP-IDF's header of this name: see ../README.md. */
#pragma once

/* Never returns: the stand-in jumps back to the test (glue_run_until_restart). */
_Noreturn void esp_restart(void);
