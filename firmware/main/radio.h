// Restarts that reset the radio, and what is remembered about them across a restart.
//
// After a first install, real boards have been seen to join Wi-Fi and be given an address, yet
// get nothing through to the hub for as long as they were left; unplugging them fixed it at
// once. Every such start was the one that made the radio's first calibration (ESP-IDF keeps it
// in the "nvs" partition, namespace "phy", and a stop and start of Wi-Fi doesn't redo it; a
// restart does). So the hub task (hub.c) restarts the board:
//
// - once, straight after Wi-Fi first starts, if that start made and stored a new calibration
//   (none was stored, the one stored was refused, or it was replaced), so that the session runs
//   on a start that loads one (radio_wifi_started); and
// - on a start that has heard nothing from any hub address yet, when Wi-Fi is joined but no
//   address has answered for a few rounds (dh_radio_restart_due), at most DH_RADIO_RESTARTS_MAX
//   times until something comes back from a hub address or a new setup is stored.
//
// What is remembered is kept in RTC memory, which a restart keeps and a power cut clears. It is
// believed only after this firmware's own restart (ESP_RST_SW): a new install over USB, a power
// cut, a crash or a watchdog all start again from nothing.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    RADIO_RESTART_CALIBRATED, // a new calibration has been made and stored
    RADIO_RESTART_STUCK,      // Wi-Fi is joined but nothing gets through
} radio_restart_t;

// Reads what was remembered, and notes the setup generation held now, once at start-up: after
// storage has started, and before anything that can store a setup or erase one (the USB task,
// the BOOT button) does.
void radio_start(void);
// The setup generation radio_start() noted. A session with any other is for a setup stored, or
// a factory reset made, since start-up.
uint32_t radio_start_generation(void);

// Restarts made for a stuck radio since something last came back from a hub address.
uint32_t radio_stuck_restarts(void);
// Something came back from a hub address (an answer, an error, or a connection made or refused):
// the radio works, and the count of restarts for a stuck one starts again from 0.
void radio_heard(void);
// A setup different from the one held at start-up is in use (a new one over USB, after a
// factory reset or not): it starts with a count of 0 too. Never for the setup a restart came
// back to, or the count would never reach its limit.
void radio_setup_changed(void);

// Call before each try at starting Wi-Fi until one succeeds. The first call in a start notes the
// calibration stored then, and later ones change nothing: a try that failed may already have
// made and stored a new one. Nothing is noted once a restart for a calibration has been made
// since the board was powered on or installed, or when NVS can't be read at all.
void radio_calibration_before(void);
// Call when Wi-Fi has started. True if a new calibration has been made since the first
// radio_calibration_before() and ESP-IDF will load it at the next start: one stored now, where
// none was, the one stored was refused, or it has changed. False ever after in this start.
bool radio_wifi_started(void);

// Restarts, unless the setup has changed from `generation` (a new one stored just now): then it
// returns, having done nothing. Under storage's lock, so no storage write is cut short and no
// new setup comes in meanwhile, it notes why, calls wipe(ctx) to wipe the secrets held in
// memory, lets the console and any reply waiting for the USB port reach the computer (for up to
// a second), and restarts.
void radio_restart(radio_restart_t why, uint32_t generation, void (*wipe)(void *ctx), void *ctx);

#ifdef __cplusplus
}
#endif
