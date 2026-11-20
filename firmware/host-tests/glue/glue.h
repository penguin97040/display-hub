/*
 * A stand-in for the board, for building main/update.c on a computer: see README.md. A test sets
 * the switches in `glue`, calls into update.c, and reads back what it did.
 */
#ifndef DH_GLUE_H
#define DH_GLUE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "dh_update.h"

typedef struct {
    /* ---- The connection: what hub_http_download_read() hands out ---- */
    const uint8_t *body;
    size_t body_len;
    /* The most one read hands out. */
    size_t piece;
    /* After the body: true, the connection stays open and sends nothing (each read waits 50 ms
     * and gives nothing); false, it has ended. */
    bool stalls;
    /* How long a read that gives bytes takes, and how much longer for each byte it gives (a hub
     * that trickles: the firmware can't cut one read short). */
    int64_t read_takes_us;
    int64_t read_takes_us_per_byte;
    /* After each read that gives bytes, the next gives nothing for 50 ms, once: bytes that
     * arrive in bursts. */
    bool pauses;
    bool paused;
    /* How many bytes have been handed out, and the most that were ever asked for at once. */
    size_t handed;
    size_t most_asked;
    /* No read may ask for a byte past this many in all: the answer's Content-Length. */
    uint64_t may_read;
    bool read_past_the_form;

    /* ---- The clock (esp_timer_get_time), which only the stand-in and the test move ---- */
    int64_t now_us;

    /* ---- The spare partition ---- */
    bool begin_fails;
    bool end_fails;
    bool set_boot_fails;
    /* With set_boot_fails: the OTA data names the spare partition all the same (the write landed
     * and was reported as failed), or can't be read at all. */
    bool set_boot_lands;
    bool boot_unreadable;
    /* Called from inside esp_ota_set_boot_partition(), before it answers: for a test to change
     * something behind the firmware's back at that moment. */
    void (*on_set_boot)(void);
    /* A write that would take the bytes written past this fails. */
    size_t write_fails_after;
    /* A piece that isn't in internal memory (see update.c): a write of one fails the test. */
    bool piece_is_external;
    bool begun;
    bool sequential; /* opened with OTA_WITH_SEQUENTIAL_WRITES */
    bool open;
    bool ended;
    bool aborted;
    bool set_to_boot;
    bool wrote_while_closed;
    uint8_t *written;
    size_t written_len;
    size_t written_cap;
    /* How many times the record had been written when the partition was set to boot. */
    unsigned record_writes_at_set_boot;

    /* ---- Storage: what storage.c provides to storage_fw.c, with flash as two arrays ---- */
    bool storage_unusable;
    /* Writes of the keys, or of the record, fail from now on. With failed_write_lands, a write
     * that fails has reached flash all the same (committed, then read back wrong). */
    bool keys_write_fails;
    bool record_write_fails;
    bool failed_write_lands;
    uint8_t flash_keys[DH_FW_KEYS_BLOB_LEN];
    uint8_t flash_record[DH_FW_RECORD_BLOB_LEN];
    unsigned keys_writes;
    unsigned record_writes;
    /* Every write that succeeded, in order: 'k' for the keys, 'r' for the record. */
    char writes[64];
    /* The lock: how deep it is held, and whether anything was written without it, or it was
     * given back more often than taken. */
    int lock_depth;
    bool lock_misused;
    /* What had been written when the image was marked good. */
    unsigned writes_at_marked_valid;

    /* ---- The download update.c made, kept after it has let go of it ---- */
    /* Set before calling update_receive(): the next block of memory it frees is its download,
     * and is kept here instead. */
    bool keep_next_free;
    dh_fw_download *download;

    /* ---- The bootloader's record, and restarts ---- */
    bool image_on_trial;
    int mark_valid_error;  /* an esp_err_t; 0 to succeed */
    int roll_back_error;   /* an esp_err_t; 0 to succeed, which restarts */
    unsigned marked_valid;
    unsigned rolled_back;
    unsigned restarts;
    int64_t rolled_back_at_us;
    bool task_made;
    bool task_ended;
    bool mutex_misused;
} glue_state;

extern glue_state glue;

/* Everything back to nothing, with a piece of 4096 bytes and no failures. Frees what the last
 * run kept. Flash holds a display that has never tried an update and trusts no keys. */
void glue_reset(void);

/* Puts keys and a record straight into flash and into storage_fw.c's memory, then starts the
 * display (storage_fw_settle_start): what a restart does. */
void glue_start(const dh_fw_keys *keys, const dh_fw_record *record, uint8_t running_slot,
                bool on_trial);
/* A restart: memory is lost, flash is read again. False if what is in flash doesn't read. */
bool glue_restart(uint8_t running_slot, bool on_trial);

/* Calls f(arg), and comes back when it returns, or when it restarts the display or ends its
 * task. True if it restarted. */
bool glue_run_until_restart(void (*f)(void *), void *arg);

/* Runs the task update.c made with xTaskCreate(), until it ends or restarts the display. False
 * if no task was made. */
bool glue_run_task(void);

#endif
