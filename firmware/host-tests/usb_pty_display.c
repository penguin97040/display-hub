/*
 * The display's end of setup over USB (dh_usb.h), on a pseudo-terminal, for trying the desktop
 * app's real sender against the firmware's own code without a board. Linux only.
 *
 *   dh_usb_pty_display <state folder>                  answer on a new pty until killed
 *   dh_usb_pty_display <state folder> --factory-reset  erase the setup, keep the rest, and exit
 *
 * It prints the pty's path on its first line of output; open that as the serial port. The state
 * folder stands for flash: the setup, the firmware keys and the update record are kept there as
 * the same blobs the firmware keeps in NVS, so stopping this and starting it again is a restart.
 * Test values only: the setup file isn't encrypted.
 */
#define _XOPEN_SOURCE 600

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

#include "dh_setup.h"
#include "dh_usb.h"
#include "dh_usb_line.h"

static const char *folder;

static void path_of(const char *name, char *out, size_t cap)
{
    snprintf(out, cap, "%s/%s", folder, name);
}

/* The file's length, 0 if there is none, or -1 if it can't be read or is longer than cap. */
static long read_file(const char *name, uint8_t *out, size_t cap)
{
    char path[1024];
    path_of(name, path, sizeof path);
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        return 0;
    }
    size_t n = fread(out, 1, cap, f);
    int more = fgetc(f);
    int bad = ferror(f);
    fclose(f);
    return bad || more != EOF ? -1 : (long)n;
}

/* One write that happens whole or not at all: a new file, renamed over the old one. */
static bool write_file(const char *name, const uint8_t *data, size_t len)
{
    char path[1024], temp[1040];
    path_of(name, path, sizeof path);
    snprintf(temp, sizeof temp, "%s.new", path);
    FILE *f = fopen(temp, "wb");
    if (f == NULL) {
        return false;
    }
    bool ok = fwrite(data, 1, len, f) == len;
    ok = fclose(f) == 0 && ok;
    return ok && rename(temp, path) == 0;
}

static dh_held store_held(void *ctx)
{
    (void)ctx;
    static uint8_t blob[DH_SETUP_BLOB_MAX];
    long n = read_file("setup.bin", blob, sizeof blob);
    return n < 0 ? DH_HELD_UNKNOWN : n > 0 ? DH_HELD_SETUP : DH_HELD_NOTHING;
}

static bool store_save_keys(void *ctx, const dh_fw_keys *keys)
{
    (void)ctx;
    uint8_t blob[DH_FW_KEYS_BLOB_LEN];
    dh_fw_keys_encode(keys, blob);
    return write_file("keys.bin", blob, sizeof blob);
}

static bool store_save_record(void *ctx, const dh_fw_record *record)
{
    (void)ctx;
    uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    dh_fw_record_encode(record, blob);
    return write_file("record.bin", blob, sizeof blob);
}

static bool store_save_setup(void *ctx, const dh_setup *setup)
{
    (void)ctx;
    static uint8_t blob[DH_SETUP_BLOB_MAX];
    size_t n = dh_setup_encode(setup, blob);
    return n > 0 && write_file("setup.bin", blob, n);
}

static bool store_erase_setup(void *ctx)
{
    (void)ctx;
    char path[1024];
    path_of("setup.bin", path, sizeof path);
    return remove(path) == 0 || store_held(NULL) == DH_HELD_NOTHING;
}

static const dh_setup_store store = {NULL,           store_held,       store_save_keys,
                                     store_save_record, store_save_setup, store_erase_setup,
                                     NULL};

int main(int argc, char **argv)
{
    if (argc < 2 || argc > 3 || (argc == 3 && strcmp(argv[2], "--factory-reset") != 0)) {
        fprintf(stderr, "usage: dh_usb_pty_display <state folder> [--factory-reset]\n");
        return 2;
    }
    folder = argv[1];

    /* A start: load what is in "flash", and settle it as the firmware does. */
    static dh_fw_keys keys;
    static dh_fw_record record;
    static dh_fw_session session;
    static dh_setup work;
    static uint8_t blob[DH_FW_RECORD_BLOB_LEN];
    dh_fw_keys_init(&keys);
    dh_fw_record_init(&record);
    long n = read_file("keys.bin", blob, sizeof blob);
    if (n < 0 || (n > 0 && !dh_fw_keys_decode(&keys, blob, (size_t)n))) {
        fprintf(stderr, "the stored keys can't be read\n");
        return 1;
    }
    n = read_file("record.bin", blob, sizeof blob);
    if (n < 0 || (n > 0 && !dh_fw_record_decode(&record, blob, (size_t)n))) {
        fprintf(stderr, "the stored record can't be read\n");
        return 1;
    }
    unsigned save = 0;
    dh_fw_start(&session, &record, &keys, 0, false, &save);
    if (((save & DH_FW_SAVE_KEYS) != 0 && !store_save_keys(NULL, &keys)) ||
        ((save & DH_FW_SAVE_RECORD) != 0 && !store_save_record(NULL, &record))) {
        fprintf(stderr, "couldn't save after the start\n");
        return 1;
    }

    const dh_usb_display display = {"display-hub-waveshare-amoled-1.8", "0.1.0",
                                    "waveshare-amoled-1.8-v2", 6291456, &store, &keys, &record,
                                    &work, false, NULL};
    if (argc == 3) {
        return dh_usb_factory_reset(&display) ? 0 : 1;
    }

    int fd = posix_openpt(O_RDWR | O_NOCTTY);
    struct termios raw;
    if (fd < 0 || grantpt(fd) != 0 || unlockpt(fd) != 0 || tcgetattr(fd, &raw) != 0) {
        perror("pty");
        return 1;
    }
    /* Bytes as they are: no echo, no line editing, no newline translation. */
    raw.c_iflag = 0;
    raw.c_oflag = 0;
    raw.c_lflag = 0;
    raw.c_cc[VMIN] = 1;
    raw.c_cc[VTIME] = 0;
    if (tcsetattr(fd, TCSANOW, &raw) != 0) {
        perror("pty");
        return 1;
    }
    printf("%s\n", ptsname(fd));
    fflush(stdout);

    static dh_usb_line_reader reader;
    static char reply[1 + DH_USB_REPLY_MAX];
    dh_usb_line_init(&reader);
    for (;;) {
        uint8_t chunk[256];
        ssize_t got = read(fd, chunk, sizeof chunk);
        if (got <= 0) {
            /* Nothing has the other end open yet, or it was closed: wait for the next opening. */
            usleep(20000);
            continue;
        }
        const uint8_t *piece = chunk;
        size_t left = (size_t)got;
        while (left > 0) {
            size_t used = 0, len = 0;
            const uint8_t *line = NULL;
            dh_line_event e = dh_usb_line_feed(&reader, piece, left, &used, &line, &len);
            piece += used;
            left -= used;
            if (e != DH_LINE_JSON) {
                continue;
            }
            /* As the firmware sends it: a newline first, then the reply line. */
            reply[0] = '\n';
            dh_usb_answer a =
                dh_usb_handle_line(&display, line, len, reply + 1, DH_USB_REPLY_MAX);
            dh_setup_wipe(&work);
            dh_usb_line_wipe(&reader);
            if (a.reply_len > 0 && write(fd, reply, a.reply_len + 1) < 0) {
                perror("write");
            }
        }
    }
}
