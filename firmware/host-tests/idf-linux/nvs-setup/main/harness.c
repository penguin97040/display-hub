// The setup partition's sizing and the factory reset's erase, tried against ESP-IDF's own NVS
// code and the firmware's own partition table (../README.md). The call sequences are the ones in
// main/storage.c.
//
// NVS is started plain here (nvs_flash_init_partition): ESP-IDF doesn't build NVS encryption for
// a computer. Encryption changes what the bytes of an entry are, not how many entries a value
// takes or where they go, so the room, the erase and the start after it are the same. The
// firmware itself must never call that function (main/storage_key.c says why).
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "esp_partition.h"
#include "nvs.h"
#include "nvs_flash.h"

#define SETUP_PART "nvs_setup"
#define CHECK(x) do { if (!(x)) { printf("FAILED line %d: %s\n", __LINE__, #x); exit(1); } } while (0)

static uint8_t big[32768], back[32768], setup[9464], device[195];

static void fill(uint8_t *p, size_t n, unsigned seed)
{
    for (size_t i = 0; i < n; i++) { seed = seed * 1103515245u + 12345u; p[i] = (uint8_t)(seed >> 16); }
}

static esp_err_t put(const char *part, const char *ns, const char *key, const uint8_t *d, size_t n)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open_from_partition(part, ns, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_blob(h, key, d, n);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

static esp_err_t get(const char *part, const char *ns, const char *key, uint8_t *d, size_t *n)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open_from_partition(part, ns, NVS_READONLY, &h);
    if (err != ESP_OK) return err;
    err = nvs_get_blob(h, key, d, n);
    nvs_close(h);
    return err;
}

static bool all_ff(const esp_partition_t *p)
{
    static uint8_t buf[4096];
    for (size_t at = 0; at < p->size; at += sizeof(buf)) {
        CHECK(esp_partition_read_raw(p, at, buf, sizeof(buf)) == ESP_OK);
        for (size_t i = 0; i < sizeof(buf); i++) if (buf[i] != 0xff) return false;
    }
    return true;
}

static void snapshot_sum(const esp_partition_t *p, uint8_t *out, size_t n)
{
    CHECK(esp_partition_read_raw(p, 0, out, n) == ESP_OK);
}

static int run(void)
{
    const esp_partition_t *main_p = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_DATA_NVS, "nvs");
    const esp_partition_t *setup_p = esp_partition_find_first(ESP_PARTITION_TYPE_DATA, ESP_PARTITION_SUBTYPE_DATA_NVS, SETUP_PART);
    CHECK(main_p && setup_p);
    printf("nvs %u bytes at 0x%x, %s %u bytes at 0x%x\n", (unsigned)main_p->size, (unsigned)main_p->address, SETUP_PART, (unsigned)setup_p->size, (unsigned)setup_p->address);
    CHECK(esp_partition_erase_range(main_p, 0, main_p->size) == ESP_OK);
    CHECK(esp_partition_erase_range(setup_p, 0, setup_p->size) == ESP_OK);

    // Both partitions start with the same keys, as the HMAC scheme gives.
    CHECK(nvs_flash_init_partition("nvs") == ESP_OK);
    CHECK(nvs_flash_init_partition(SETUP_PART) == ESP_OK);

    uint8_t keys[201], record[321], phy[1904];
    fill(keys, sizeof keys, 1); fill(record, sizeof record, 2); fill(phy, sizeof phy, 3);
    CHECK(put("nvs", "dh_fw", "keys", keys, sizeof keys) == ESP_OK);
    CHECK(put("nvs", "dh_fw", "record", record, sizeof record) == ESP_OK);
    CHECK(put("nvs", "phy", "cal_data", phy, sizeof phy) == ESP_OK);

    fill(setup, sizeof setup, 4); fill(device, sizeof device, 5);
    CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
    CHECK(put(SETUP_PART, "dh_setup", "device", device, sizeof device) == ESP_OK);

    // The largest snapshot replaced by the largest, in ONE step, many times; with the setup
    // rewritten (the enrolment secret cleared) and other sizes in between.
    unsigned one_step_fail = 0, saves = 0;
    for (unsigned round = 0; round < 1500; round++) {
        size_t n = (round % 5 == 0) ? 32768 : (round % 5 == 1) ? 32768 : (round % 5 == 2) ? 1 + (round * 7919u) % 32768 : (round % 5 == 3) ? 32768 : 20000 + (round * 31u) % 12768;
        fill(big, n, 100 + round);
        esp_err_t err = put(SETUP_PART, "dh_setup", "snapshot", big, n);
        if (err != ESP_OK) { one_step_fail++; printf("round %u: %u bytes failed: %s\n", round, (unsigned)n, esp_err_to_name(err)); if (one_step_fail > 5) break; continue; }
        saves++;
        size_t got = sizeof back; memset(back, 0, sizeof back);
        CHECK(get(SETUP_PART, "dh_setup", "snapshot", back, &got) == ESP_OK && got == n && memcmp(back, big, n) == 0);
        if (round % 50 == 7) {
            fill(setup, sizeof setup, 1000 + round);
            CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
            fill(device, sizeof device, 2000 + round);
            CHECK(put(SETUP_PART, "dh_setup", "device", device, sizeof device) == ESP_OK);
        }
    }
    printf("one-step snapshot saves: %u ok, %u failed\n", saves, one_step_fail);
    size_t got = sizeof back;
    CHECK(get(SETUP_PART, "dh_setup", "setup", back, &got) == ESP_OK && got == sizeof setup && memcmp(back, setup, got) == 0);
    got = sizeof back;
    CHECK(get(SETUP_PART, "dh_setup", "device", back, &got) == ESP_OK && got == sizeof device && memcmp(back, device, got) == 0);
    nvs_stats_t st; CHECK(nvs_get_stats(SETUP_PART, &st) == ESP_OK);
    printf("setup partition entries: used %u, free %u, available %u, total %u\n", (unsigned)st.used_entries, (unsigned)st.free_entries, (unsigned)st.available_entries, (unsigned)st.total_entries);

    // What nvs_erase_all leaves behind (the old factory reset), for comparison.
    {
        nvs_handle_t h; CHECK(nvs_open_from_partition(SETUP_PART, "dh_setup", NVS_READWRITE, &h) == ESP_OK);
        CHECK(nvs_erase_all(h) == ESP_OK && nvs_commit(h) == ESP_OK); nvs_close(h);
        printf("after nvs_erase_all: partition all 0xFF? %s\n", all_ff(setup_p) ? "yes" : "NO (the old data is still in flash)");
        CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
        CHECK(put(SETUP_PART, "dh_setup", "snapshot", big, 32768) == ESP_OK);
    }

    // The factory reset: deinit, erase the whole partition, check, init again.
    static uint8_t main_before[0x10000], main_after[0x10000];
    snapshot_sum(main_p, main_before, sizeof main_before);
    CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
    CHECK(esp_partition_erase_range(setup_p, 0, setup_p->size) == ESP_OK);
    CHECK(all_ff(setup_p));
    CHECK(nvs_flash_init_partition(SETUP_PART) == ESP_OK);
    got = sizeof back;
    CHECK(get(SETUP_PART, "dh_setup", "setup", back, &got) == ESP_ERR_NVS_NOT_FOUND);
    got = sizeof back;
    CHECK(get(SETUP_PART, "dh_setup", "snapshot", back, &got) == ESP_ERR_NVS_NOT_FOUND);
    snapshot_sum(main_p, main_after, sizeof main_after);
    CHECK(memcmp(main_before, main_after, sizeof main_before) == 0);
    got = sizeof back; CHECK(get("nvs", "dh_fw", "keys", back, &got) == ESP_OK && got == sizeof keys && memcmp(back, keys, got) == 0);
    got = sizeof back; CHECK(get("nvs", "dh_fw", "record", back, &got) == ESP_OK && got == sizeof record && memcmp(back, record, got) == 0);
    got = sizeof back; CHECK(get("nvs", "phy", "cal_data", back, &got) == ESP_OK && got == sizeof phy && memcmp(back, phy, got) == 0);
    printf("factory reset: partition read back all 0xFF, came up empty, nvs partition byte-identical\n");

    // A set-up again after the reset works.
    CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
    CHECK(put(SETUP_PART, "dh_setup", "snapshot", big, 32768) == ESP_OK);

    // A power cut part-way through the erase: some sectors erased, the rest as they were.
    for (unsigned cut = 1; cut < setup_p->size / 4096; cut += 3) {
        CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
        CHECK(esp_partition_erase_range(setup_p, 0, (size_t)cut * 4096) == ESP_OK);
        esp_err_t err = nvs_flash_init_partition(SETUP_PART);
        got = sizeof back;
        esp_err_t found = err == ESP_OK ? get(SETUP_PART, "dh_setup", "setup", back, &got) : ESP_FAIL;
        if (cut == 1 || cut % 9 == 1) printf("erase cut after %u sectors: init %s, setup %s\n", cut, esp_err_to_name(err), err == ESP_OK ? esp_err_to_name(found) : "-");
        if (err == ESP_OK) CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
        // What the next start does: finish the erase and init.
        CHECK(esp_partition_erase_range(setup_p, 0, setup_p->size) == ESP_OK && all_ff(setup_p));
        CHECK(nvs_flash_init_partition(SETUP_PART) == ESP_OK);
        CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
        CHECK(put(SETUP_PART, "dh_setup", "snapshot", big, 32768) == ESP_OK);
    }
    // Why the factory reset leaves a mark first: with the setup stored in the partition's later
    // pages, an erase cut short leaves it readable, and NVS starts as if nothing had happened.
    CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
    CHECK(esp_partition_erase_range(setup_p, 0, setup_p->size) == ESP_OK);
    CHECK(nvs_flash_init_partition(SETUP_PART) == ESP_OK);
    CHECK(put(SETUP_PART, "dh_setup", "snapshot", big, 32768) == ESP_OK); // the first nine pages
    CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
    CHECK(put(SETUP_PART, "dh_setup", "device", device, sizeof device) == ESP_OK);
    CHECK(put("nvs", "dh_reset", "wiping", (const uint8_t *)"\1", 1) == ESP_OK); // the mark
    CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
    CHECK(esp_partition_erase_range(setup_p, 0, 4 * 4096) == ESP_OK); // power cut after 4 sectors
    {
        esp_err_t err = nvs_flash_init_partition(SETUP_PART);
        got = sizeof back;
        esp_err_t s_found = err == ESP_OK ? get(SETUP_PART, "dh_setup", "setup", back, &got) : ESP_FAIL;
        got = sizeof back;
        esp_err_t d_found = err == ESP_OK ? get(SETUP_PART, "dh_setup", "device", back, &got) : ESP_FAIL;
        printf("without the mark, after a cut at 4 sectors: init %s, setup %s, device token %s\n", esp_err_to_name(err), esp_err_to_name(s_found), esp_err_to_name(d_found));
        if (err == ESP_OK) CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
    }
    // What a start does on finding the mark: erase it all, check, start, take the mark out.
    got = sizeof back;
    CHECK(get("nvs", "dh_reset", "wiping", back, &got) == ESP_OK);
    CHECK(esp_partition_erase_range(setup_p, 0, setup_p->size) == ESP_OK && all_ff(setup_p));
    CHECK(nvs_flash_init_partition(SETUP_PART) == ESP_OK);
    got = sizeof back; CHECK(get(SETUP_PART, "dh_setup", "setup", back, &got) == ESP_ERR_NVS_NOT_FOUND);
    got = sizeof back; CHECK(get(SETUP_PART, "dh_setup", "device", back, &got) == ESP_ERR_NVS_NOT_FOUND);
    {
        nvs_handle_t h; CHECK(nvs_open_from_partition("nvs", "dh_reset", NVS_READWRITE, &h) == ESP_OK);
        CHECK(nvs_erase_key(h, "wiping") == ESP_OK && nvs_commit(h) == ESP_OK); nvs_close(h);
    }
    got = sizeof back; CHECK(get("nvs", "dh_fw", "keys", back, &got) == ESP_OK && got == sizeof keys && memcmp(back, keys, got) == 0);
    got = sizeof back; CHECK(get("nvs", "dh_fw", "record", back, &got) == ESP_OK && got == sizeof record && memcmp(back, record, got) == 0);
    printf("with the mark: the erase was finished, nothing of the setup is found, keys and record unchanged\n");
    // A partition full of rubbish (what a wrong key would make of it too): NVS reads every page
    // as corrupt, erases them itself and starts empty, with no error. So main/storage.c's own
    // erase at start-up is only for the two answers NVS gives when it will not do that
    // (ESP_ERR_NVS_NO_FREE_PAGES and ESP_ERR_NVS_NEW_VERSION_FOUND). Neither can be made here
    // without writing NVS's page format by hand, and that branch of storage.c isn't built for a
    // computer, so it is not tried by this harness.
    CHECK(nvs_flash_deinit_partition(SETUP_PART) == ESP_OK);
    for (size_t at = 0; at < setup_p->size; at += sizeof big) {
        fill(big, sizeof big, 7000 + (unsigned)at);
        CHECK(esp_partition_erase_range(setup_p, at, sizeof big) == ESP_OK);
        CHECK(esp_partition_write_raw(setup_p, at, big, sizeof big) == ESP_OK);
    }
    {
        esp_err_t err = nvs_flash_init_partition(SETUP_PART);
        got = sizeof back;
        esp_err_t found = err == ESP_OK ? get(SETUP_PART, "dh_setup", "setup", back, &got) : ESP_FAIL;
        printf("a partition of rubbish: init %s, setup %s\n", esp_err_to_name(err), err == ESP_OK ? esp_err_to_name(found) : "-");
        CHECK(err == ESP_OK && found == ESP_ERR_NVS_NOT_FOUND);
        CHECK(put(SETUP_PART, "dh_setup", "setup", setup, sizeof setup) == ESP_OK);
    }
    printf("ALL PASSED\n");
    return 0;
}

void app_main(void)
{
    exit(run());
}
