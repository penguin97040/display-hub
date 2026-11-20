// The display's dealings with the hub once it is set up (docs/firmware-handoff.md, "Joining and
// enrolling", "Every poll", "Heartbeat", "Errors and back-off"): join Wi-Fi, enrol once, keep the
// device ID and token, poll the snapshot, send heartbeats, and say plainly what is wrong.
//
// One task does all of it, and it is the only one that touches Wi-Fi or the network. It never
// draws: it keeps a small status and a copy of the pages, which the main task reads and shows.
// What to send, what an answer means and when to try next are decided by dh_core (dh_hub.h,
// dh_snapshot.h, dh_poll.h).
//
// A FACTORY RESET never waits for this task. The task works from its own copy of the setup and
// holds storage's lock only for a moment at a time, never across the network. A reset erases
// the stored setup at once; the task notices that the setup has gone (storage.h, "The setup's
// generation") between steps, gives up what it was doing within one network wait (about ten
// seconds at most), stops Wi-Fi, wipes what it held and waits for a new setup. Nothing it had in
// flight can be stored afterwards.
//
// Nothing here logs or shows the Wi-Fi network's name or password, a hub address, the enrolment
// secret or the device token.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "dh_limits.h"
#include "dh_poll.h"
#include "dh_snapshot.h"
#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    HUB_IDLE,         // no setup to work from
    HUB_JOINING_WIFI, // set up, and joining the Wi-Fi network
    HUB_ENROLLING,    // on Wi-Fi, asking the hub to take this display
    HUB_WAITING,      // enrolled, and about to ask for its pages for the first time
    HUB_ONLINE,       // the last poll succeeded
    HUB_TROUBLE,      // `problem` says what is wrong; the last good pages stay
    HUB_NEEDS_SETUP,  // nothing more can be done until the display is set up again
    HUB_FAILED,       // the task couldn't start (hub_start failed): only a restart helps
} hub_state_t;

// More about a problem than its headline says.
typedef enum {
    HUB_DETAIL_NONE,
    HUB_DETAIL_WIFI,            // can't reach the hub because the display isn't on Wi-Fi
    HUB_DETAIL_WIFI_START,      // the display's Wi-Fi didn't start; it is tried again
    HUB_DETAIL_NO_WIFI_DETAILS, // the setup came without Wi-Fi details
    HUB_DETAIL_NO_ANCHOR,       // no certificate in the setup can be read
    HUB_DETAIL_ENROL_REFUSED,   // the hub refused the one-time secret (used or expired)
    HUB_DETAIL_NO_TOKEN,        // set up, but with neither a secret nor a readable token
    HUB_DETAIL_REVOKED,         // the hub no longer accepts this display's token
} hub_detail_t;

typedef struct {
    hub_state_t state;
    dh_problem problem;
    hub_detail_t detail;
    // The device ID and token couldn't be written to flash: they are kept in memory, and the
    // display will need setting up again if it restarts before a write succeeds.
    bool token_unsaved;
    // The pages held: the last good snapshot. hub_pages() gives a copy of them to draw.
    bool has_snapshot;
    // They are the ones saved before this start, and no poll has succeeded since: their age
    // isn't known.
    bool saved_pages;
} hub_status_t;

// A copy of the pages held, for the screen to draw. It is about 38 KB, so keep it off the stack.
typedef struct {
    uint32_t seq;        // changes whenever anything below does
    bool has_snapshot;   // false: no pages are held, and `snapshot` is all zeros
    bool saved;          // restored from flash, and no poll has succeeded since: age unknown
    int64_t received_ms; // when it arrived, or was restored, by esp_timer_get_time() / 1000
    dh_snapshot snapshot;
} hub_pages_t;

// Starts the task. The two strings must stay valid for good. `on_change` is called from the task
// whenever the status changes, and should only wake another task. If it fails, the status says
// HUB_FAILED from then on.
esp_err_t hub_start(const char *firmware_version, const char *hardware_revision,
                    void (*on_change)(void));

// Tells the task to look at storage again now: a setup was stored or erased.
void hub_wake(void);

void hub_status(hub_status_t *out);

// Brings *out up to date with the pages held, copying them only if they have changed since *out
// was last brought up to date (by `seq`; start with *out all zeros). Returns whether it copied.
bool hub_pages(hub_pages_t *out);

// How many bytes of the task's stack have never been used. 0 before it has started.
unsigned hub_stack_unused(void);

#ifdef __cplusplus
}
#endif
