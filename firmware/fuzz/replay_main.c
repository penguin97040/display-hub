/*
 * Runs a fuzz target over files, for the host tests: each argument is a file or a folder of
 * files. Built in place of libFuzzer's own main.
 */
#define _POSIX_C_SOURCE 200809L

#include <dirent.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

static int run_file(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "%s: can't read\n", path);
        return 1;
    }
    size_t cap = 1 << 16, n = 0;
    uint8_t *buf = malloc(cap);
    while (buf) {
        size_t got = fread(buf + n, 1, cap - n, f);
        n += got;
        if (got == 0) {
            break;
        }
        if (n == cap) {
            cap *= 2;
            uint8_t *bigger = realloc(buf, cap);
            if (!bigger) {
                free(buf);
            }
            buf = bigger;
        }
    }
    fclose(f);
    if (!buf) {
        return 1;
    }
    /* A copy of exactly the input's size, so the sanitizers see any read past its end. */
    uint8_t *exact = malloc(n ? n : 1);
    if (!exact) {
        free(buf);
        return 1;
    }
    memcpy(exact, buf, n);
    free(buf);
    (void)LLVMFuzzerTestOneInput(exact, n);
    free(exact);
    return 0;
}

int main(int argc, char **argv)
{
    int failures = 0, files = 0;
    for (int i = 1; i < argc; i++) {
        struct stat st;
        if (stat(argv[i], &st) != 0) {
            fprintf(stderr, "%s: not found\n", argv[i]);
            failures++;
            continue;
        }
        if (!S_ISDIR(st.st_mode)) {
            failures += run_file(argv[i]);
            files++;
            continue;
        }
        DIR *d = opendir(argv[i]);
        struct dirent *e;
        while (d && (e = readdir(d)) != NULL) {
            if (e->d_name[0] == '.') {
                continue;
            }
            char path[4096];
            (void)snprintf(path, sizeof path, "%s/%s", argv[i], e->d_name);
            failures += run_file(path);
            files++;
        }
        if (d) {
            closedir(d);
        }
    }
    printf("replayed %d inputs\n", files);
    return failures || files == 0 ? 1 : 0;
}
