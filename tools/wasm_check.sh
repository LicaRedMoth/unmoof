#!/bin/bash
# Builds unmoof.wasm and checks that, run under Node, it writes the same bytes as the native
# build for every test stream in testdata/.
set -u
cd "$(dirname "$0")/.."
O=out/wasm
mkdir -p $O
cargo build --release -q || exit 1
(cd wasm && cargo build --release -q --target wasm32-unknown-unknown) || exit 1
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals wasm/target/wasm32-unknown-unknown/release/unmoof_wasm.wasm -o wasm/unmoof.wasm || exit 1
echo "unmoof.wasm: $(stat -c %s wasm/unmoof.wasm) bytes, $(gzip -9 -c wasm/unmoof.wasm | wc -c) gzipped"

pass=0; fail=0
same() { # name, then the arguments both builds get
    local name=$1; shift
    target/release/unmoof -o "$O/$name.native.mp4" "$@" 2>/dev/null
    node wasm/cli.mjs -o "$O/$name.wasm.mp4" "$@" 2>/dev/null
    if cmp -s "$O/$name.native.mp4" "$O/$name.wasm.mp4"; then
        echo "SAME $name"; pass=$((pass + 1))
    else
        echo "DIFF $name"; fail=$((fail + 1))
    fi
    rm -f "$O/$name.native.mp4" "$O/$name.wasm.mp4"
}
segs() { grep -v '^#' "$1" | grep -v '^$' | sed "s#^#$(dirname "$1")/#"; }

T=testdata
A=$T/aniboom720
[ -f $A/media_4.m3u8 ] && same aniboom -s $A/60d2572dd2dbe1_init_4.m4s $(segs $A/media_4.m3u8) \
    -s $A/60d2572dd2dbe1_init_1.m4s $(segs $A/media_1.m3u8)
for d in $T/public/*/ $T/ts/*/; do
    args=()
    for s in "$d"src*; do args+=(-s $(cat "$s/list.txt")); done
    same "$(basename "$(dirname "$d")")-$(basename "$d")" "${args[@]}"
done
S=$T/synth
if [ -d $S/ts_plain ]; then
    same synth-dash -s $S/dash/init-stream0.m4s $(ls -v $S/dash/chunk-stream0-*) \
        -s $S/dash/init-stream1.m4s $(ls -v $S/dash/chunk-stream1-*)
    same synth-frag -s $S/frag_neg.mp4
    same synth-ts-png -s $(segs $S/ts_png/index.m3u8)
    same synth-ts-wrap -s $(segs $S/ts_wrap/index.m3u8)
    same synth-ts-disc -s $(segs $S/ts_disc/a.m3u8) $(segs $S/ts_disc/b.m3u8)
fi
echo "$pass same, $fail different"
[ $fail -eq 0 ]
