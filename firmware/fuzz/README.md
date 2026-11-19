# Fuzz targets

Four libFuzzer targets for dh_core:

- `fuzz_json.c`: the JSON reader, with and without its strict options, reading every value of an accepted document every way the API allows and checking the answers agree.
- `fuzz_snapshot.c`: `dh_read_response()` with the input as a 200 body and as an error body, checking everything it returns is safe to draw (text that passes the text rules, counts within bounds, a custom page's data inside the body).

- `fuzz_fw_manifest.c`: what the signed-update core reads without needing a signature first: the manifest reader, the header check, a bundle's `firmware` member and the stored forms of the keys and the update record, checking that whatever is accepted is within every bound the rest of the core relies on.
- `fuzz_fw_download.c`: one download through the streaming verifier, in pieces of sizes the input chooses. The input is either a body as a hub might send it, or a manifest and an image that the target signs itself with a key the display trusts (a fuzzer can't forge a signature, so that is the only way past it). It checks that nothing is taken past the `Content-Length`, nothing is written before the manifest has passed, a download finishes only if what was written is the image the manifest names, and the same body ends the same way however it is cut up.

`corpus/` holds the seed inputs: `json` and `snapshot` are copied from `protocol/v1/examples`, and `fw_manifest` and `fw_download` are made from the vectors in `protocol/v1/firmware` by `make_fw_corpus.py` (run it again when the vectors change). The host tests replay them on every run (`fuzz_json_corpus`, `fuzz_snapshot_corpus`, `fuzz_fw_manifest_corpus` and `fuzz_fw_download_corpus`), using `replay_main.c` in place of libFuzzer, so no long fuzz run is part of CI.

## Running them

You need clang with its runtime libraries (on Debian, `clang` and `libclang-rt-19-dev`), and Mbed TLS 3.6 built as for the host tests: `M` below is its source tree after `make lib`. From the repository root, with a build folder outside the repository:

```sh
B=/tmp/dh-fuzz
M=/path/to/mbedtls
mkdir -p "$B"
for t in json snapshot fw_manifest fw_download; do
  clang -std=c11 -g -O1 -fsanitize=fuzzer,address,undefined -fno-sanitize-recover=all \
    -I firmware/components/dh_core/include -isystem "$M/include" \
    firmware/fuzz/fuzz_$t.c firmware/components/dh_core/src/*.c \
    "$M/library/libmbedcrypto.a" -lm -o "$B/fuzz_$t"
  mkdir -p "$B/corpus-$t"
  cp firmware/fuzz/corpus/$t/* "$B/corpus-$t/"
done
"$B/fuzz_snapshot" -max_len=33000 -max_total_time=600 -dict=firmware/fuzz/snapshot.dict "$B/corpus-snapshot"
"$B/fuzz_fw_manifest" -max_len=4200 -max_total_time=600 -dict=firmware/fuzz/fw_manifest.dict "$B/corpus-fw_manifest"
"$B/fuzz_fw_download" -max_len=8000 -max_total_time=1800 -dict=firmware/fuzz/fw_manifest.dict "$B/corpus-fw_download"
```

`fuzz_fw_download` signs and verifies with real P-256 on every run, so it manages a few hundred runs a second where the others manage tens of thousands; give it longer, or add `-fork=4`.

Use a copy of the seed corpus as the working corpus, as above, so new inputs don't land in the repository. If a run finds a crash, it writes the input to a `crash-…` file: fix the cause, and add the input to `corpus/` so the host tests keep checking it.
