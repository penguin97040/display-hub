# Fuzz targets

Two libFuzzer targets for dh_core:

- `fuzz_json.c`: the JSON reader, with and without its strict options, reading every value of an accepted document every way the API allows and checking the answers agree.
- `fuzz_snapshot.c`: `dh_read_response()` with the input as a 200 body and as an error body, checking everything it returns is safe to draw (text that passes the text rules, counts within bounds, a custom page's data inside the body).

`corpus/` holds the seed inputs, copied from `protocol/v1/examples`. The host tests replay them on every run (`fuzz_json_corpus` and `fuzz_snapshot_corpus`), using `replay_main.c` in place of libFuzzer, so no long fuzz run is part of CI.

## Running them

You need clang with its runtime libraries (on Debian, `clang` and `libclang-rt-19-dev`). From the repository root, with a build folder outside the repository:

```sh
B=/tmp/dh-fuzz
mkdir -p "$B"
for t in json snapshot; do
  clang -std=c11 -g -O1 -fsanitize=fuzzer,address,undefined -fno-sanitize-recover=all \
    -I firmware/components/dh_core/include \
    firmware/fuzz/fuzz_$t.c firmware/components/dh_core/src/*.c -lm -o "$B/fuzz_$t"
done
mkdir -p "$B/corpus-snapshot"
cp firmware/fuzz/corpus/snapshot/* "$B/corpus-snapshot/"
"$B/fuzz_snapshot" -max_len=33000 -max_total_time=600 -dict=firmware/fuzz/snapshot.dict "$B/corpus-snapshot"
```

Use a copy of the seed corpus as the working corpus, as above, so new inputs don't land in the repository. If a run finds a crash, it writes the input to a `crash-…` file: fix the cause, and add the input to `corpus/` so the host tests keep checking it.
