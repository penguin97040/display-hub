// The factory reset button: BOOT (GPIO0), held for 10 seconds while the display is running.
//
// Holding it shows a countdown; letting go before the end cancels. The button is debounced, and
// one that is already down when the display starts does nothing until it has been let go.
// This file only watches the button. What a reset erases is storage.h's business.

#pragma once

#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

#define RESET_BUTTON_HOLD_SECONDS 10

typedef enum {
    RESET_BUTTON_COUNTING,  // held: `seconds_left` to go, sent once a second
    RESET_BUTTON_CANCELLED, // let go before the end
    RESET_BUTTON_HELD,      // held to the end: do the reset. Sent once per hold.
} reset_button_event_t;

// Called from the button's task. It should only pass the event on to another task. It may wait
// to do so: the button isn't watched meanwhile, which is the right way round for the two events
// that end a countdown (a lost one would leave the countdown on the screen).
typedef void (*reset_button_handler_t)(reset_button_event_t event, int seconds_left);

esp_err_t reset_button_start(reset_button_handler_t handler);

#ifdef __cplusplus
}
#endif
