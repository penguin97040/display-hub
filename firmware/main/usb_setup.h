// Setup over USB: the display's end of docs/protocol-v1.md, "Over USB", on the chip's own USB
// port (USB-Serial-JTAG), which the console's log lines share.
//
// A task reads the port for as long as the display runs, so the desktop app's writes never stall,
// and answers every `hello` and every `provision`. What to answer is decided by dh_core
// (dh_usb.h); this file is only the port.
//
// Nothing here ever logs a line it received, a bundle, or anything from one.

#pragma once

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

// The display's `device_type`. Firmware packages are matched to it byte for byte, so it never
// changes.
#define USB_SETUP_DEVICE_TYPE "display-hub-waveshare-amoled-1.8"

// Called from the USB task once a bundle has been stored, and also if handling one left storage
// out of use (a new board whose storage key couldn't be made). It should only wake another task.
typedef void (*usb_setup_stored_t)(void);

// Installs the USB-Serial-JTAG driver and moves the console onto it. Call it first thing in
// app_main: from then on whatever the app sends is kept until the task below reads it.
esp_err_t usb_setup_install(void);

// Starts the task. Call it once storage has started and the board's revision is known. The two
// strings must stay valid for good.
esp_err_t usb_setup_start(const char *firmware_version, const char *hardware_revision,
                          usb_setup_stored_t on_stored);

// For a factory reset: the USB task forgets whatever line it has part-read (it may be the start
// of a bundle that never finished) and wipes its buffers, within a second. Call it from any
// task. What a reset does NOT clear is at the top of usb_setup.c, "What stays in memory".
void usb_setup_forget(void);

// How many bytes of the USB task's stack have never been used, so the headroom is seen, not
// guessed. 0 before the task has started.
unsigned usb_setup_stack_unused(void);

#ifdef __cplusplus
}
#endif
