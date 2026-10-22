#!/bin/bash
# Remuxes every test stream in testdata/ and checks the result packet by packet.
# Synthetic streams are checked against the file they were cut from; downloaded ones against
# ffmpeg's remux of the same segments (`-c copy`).
set -u
cd "$(dirname "$0")/.."
U=target/release/unmoof
C=tools/compare.py
T=testdata
O=out/regress
mkdir -p "$O"
cargo build --release -q || exit 1
pass=0; fail=0

check() { # name reference output [expected video-against-audio move in ms]
    if $C "$2" "$3" ${4:-} > "$O/$1.txt" 2>&1; then
        echo "PASS $1"; pass=$((pass + 1))
    else
        echo "FAIL $1 (see $O/$1.txt)"; fail=$((fail + 1))
    fi
}

# Lists the media segments of an HLS playlist, with the playlist's directory in front.
segs() { grep -v '^#' "$1" | grep -v '^$' | sed "s#^#$(dirname "$1")/#"; }

if [ -f $T/synth/truth.mp4 ]; then
    S=$T/synth
    $U -o $O/dash.mp4 -s $S/dash/init-stream0.m4s $(ls -v $S/dash/chunk-stream0-*) \
        -s $S/dash/init-stream1.m4s $(ls -v $S/dash/chunk-stream1-*) 2>/dev/null
    check synth-dash $S/truth.mp4 $O/dash.mp4
    $U -o $O/hls.mp4 -s $S/hls/init.mp4 $(segs $S/hls/index.m3u8) 2>/dev/null
    check synth-hls $S/truth.mp4 $O/hls.mp4
    # These two lost the AAC priming when ffmpeg cut them, so they match the cut, not the truth.
    $U -o $O/pos.mp4 -s $S/frag_pos.mp4 2>/dev/null
    check synth-frag $S/frag_pos.mp4 $O/pos.mp4
fi

A=$T/aniboom720
if [ -f $A/media_4.m3u8 ]; then
    $U -o $O/aniboom.mp4 -s $A/60d2572dd2dbe1_init_4.m4s $(segs $A/media_4.m3u8) \
        -s $A/60d2572dd2dbe1_init_1.m4s $(segs $A/media_1.m3u8) 2>/dev/null
    cat $A/60d2572dd2dbe1_init_4.m4s $(segs $A/media_4.m3u8) > $O/v.tmp
    cat $A/60d2572dd2dbe1_init_1.m4s $(segs $A/media_1.m3u8) > $O/a.tmp
    ffmpeg -v error -y -i $O/v.tmp -i $O/a.tmp -map 0 -map 1 -c copy -f mp4 $O/aniboom_ref.mp4
    rm -f $O/v.tmp $O/a.tmp
    check aniboom $O/aniboom_ref.mp4 $O/aniboom.mp4
    rm -f $O/aniboom.mp4 $O/aniboom_ref.mp4
fi

# Other codecs, and two dubs as two audio sources, cut into DASH by ffmpeg.
for k in vp9_opus av1_flac dubs; do
    D=$T/synth/codecs/$k
    [ -d $D ] || continue
    args=()
    for n in 0 1 2; do
        [ -f $D/init-stream$n.m4s ] && args+=(-s $D/init-stream$n.m4s $(ls -v $D/chunk-stream$n-*))
    done
    $U -o $O/$k.mp4 "${args[@]}" 2>/dev/null
    check "synth-$k" $T/synth/codecs/$k.mp4 $O/$k.mp4
    rm -f $O/$k.mp4
done

# Public test streams fetched with tools/fetch.py: one source per src* directory.
for d in $T/public/*/; do
    k=$(basename "$d")
    args=(); refin=(); maps=(); i=0
    for s in "$d"src*; do
        args+=(-s $(cat "$s/list.txt"))
        cat $(cat "$s/list.txt") > "$O/$k.$i.tmp"
        refin+=(-i "$O/$k.$i.tmp"); maps+=(-map "$i:v?" -map "$i:a?")
        i=$((i + 1))
    done
    $U -o "$O/$k.mp4" "${args[@]}" 2>/dev/null
    ffmpeg -v error -y "${refin[@]}" "${maps[@]}" -c copy -f mp4 "$O/${k}_ref.mp4"
    rm -f "$O/$k".*.tmp
    # ffmpeg starts video at its first decode time; unmoof at its first presentation time, as
    # the fragments state it. Streams that start video on a positive composition offset with no
    # edit list name the difference in sync_ms.
    check "$k" "$O/${k}_ref.mp4" "$O/$k.mp4" $(cat "$d/sync_ms" 2>/dev/null)
    rm -f "$O/$k.mp4" "$O/${k}_ref.mp4"
done

# MPEG-TS from public HLS streams, against ffmpeg's remux of the joined segments.
for d in $T/ts/*/; do
    k=ts_$(basename "$d")
    files=$(cat "$d/src0/list.txt")
    $U -o "$O/$k.mp4" -s $files 2>/dev/null
    cat $files > "$O/$k.ts"
    # unmoof writes HEVC as hvc1, with the parameter sets only in the sample entry.
    hvc1=()
    if [ "$(ffprobe -v error -select_streams v -show_entries stream=codec_name -of csv=p=0 "$O/$k.ts" | head -1)" = hevc ]; then
        hvc1=(-bsf:v "filter_units=remove_types=32|33|34" -tag:v hvc1)
    fi
    ffmpeg -v error -y -i "$O/$k.ts" -map 0:v? -map 0:a? -c copy "${hvc1[@]}" "$O/${k}_ref.mp4"
    rm -f "$O/$k.ts"
    check "$k" "$O/${k}_ref.mp4" "$O/$k.mp4"
    rm -f "$O/$k.mp4" "$O/${k}_ref.mp4"
done

# MPEG-TS made by ffmpeg, then mangled the ways CDNs and playlists mangle it.
if [ -d $T/synth/ts_plain ]; then
    S=$T/synth
    $U -o $O/ts_plain.mp4 -s $(segs $S/ts_plain/index.m3u8) 2>/dev/null
    cat $(segs $S/ts_plain/index.m3u8) > $O/p.ts
    ffmpeg -v error -y -i $O/p.ts -c copy $O/ts_plain_ref.mp4; rm -f $O/p.ts
    check synth-ts $O/ts_plain_ref.mp4 $O/ts_plain.mp4
    $U -o $O/ts_wrap.mp4 -s $(segs $S/ts_wrap/index.m3u8) 2>/dev/null
    check synth-ts-wrap $O/ts_plain.mp4 $O/ts_wrap.mp4
    $U -o $O/ts_png.mp4 -s $(segs $S/ts_png/index.m3u8) 2>/dev/null
    if cmp -s $O/ts_png.mp4 $O/ts_plain.mp4; then echo "PASS synth-ts-png"; pass=$((pass + 1));
    else echo "FAIL synth-ts-png (differs from the plain segments)"; fail=$((fail + 1)); fi
    # A missing segment and a discontinuity: joined, and decodable without a complaint.
    $U -o $O/ts_gap.mp4 -s $S/ts_gap/s0.ts $S/ts_gap/s1.ts $S/ts_gap/s3.ts $S/ts_gap/s4.ts 2>/dev/null
    $U -o $O/ts_disc.mp4 -s $(segs $S/ts_disc/a.m3u8) $(segs $S/ts_disc/b.m3u8) 2>/dev/null
    for k in ts_gap ts_disc; do
        n=$(ffmpeg -v error -i $O/$k.mp4 -f null - 2>&1 | wc -l)
        if [ "$n" -eq 0 ]; then echo "PASS synth-$k"; pass=$((pass + 1));
        else echo "FAIL synth-$k ($n decoder complaints)"; fail=$((fail + 1)); fi
    done
    rm -f $O/ts_*.mp4
fi

# MPEG audio and HLS packed audio: an .mp3 out of TS or packed segments must be the original
# file byte for byte; packed AAC is checked against ffmpeg's MP4 of the same frames.
M=$T/synth/mp3
if [ -d $M/hls_mp3 ]; then
    same_file() { # name expected actual
        if cmp -s "$2" "$3"; then echo "PASS $1"; pass=$((pass + 1)); else echo "FAIL $1 (differs from $2)"; fail=$((fail + 1)); fi
    }
    $U -o $O/hls.mp3 -s $(segs $M/hls_mp3/index.m3u8) 2>/dev/null
    same_file mp3-from-ts $M/truth.mp3 $O/hls.mp3
    $U -o $O/packed.mp3 -s $(ls -v $M/packed_mp3/*) 2>/dev/null
    same_file mp3-from-packed $M/truth.mp3 $O/packed.mp3
    $U -o $O/packed_aac.mp4 -s $(ls -v $M/packed_aac/*) 2>/dev/null
    ffmpeg -v error -y -i $M/truth.aac -c copy $O/packed_aac_ref.mp4
    check aac-from-packed $O/packed_aac_ref.mp4 $O/packed_aac.mp4
    $U -o $O/vid_mp3.mp4 -s $(segs $M/vid_mp3/index.m3u8) 2>/dev/null
    cat $(segs $M/vid_mp3/index.m3u8) > $O/v.ts
    ffmpeg -v error -y -i $O/v.ts -c copy $O/vid_mp3_ref.mp4; rm -f $O/v.ts
    check video-mp3-ts $O/vid_mp3_ref.mp4 $O/vid_mp3.mp4
    rm -f $O/*.mp3 $O/packed_aac*.mp4 $O/vid_mp3*.mp4
fi

# Subtitles: SubRip against ffmpeg's mov_text; segmented HLS WebVTT over TS video and ASS
# against SRT dumps checked by hand once (sync, overlap, styles, drawings left out).
B=$T/synth/subs
if [ -f $B/sub.srt ]; then
    ffmpeg -v error -y -i $B/sub_v.mp4 -i $B/sub.srt -map 0 -map 1 -c:v copy -c:s mov_text $O/sub_ref.mp4
    $U -o $O/sub.mp4 -s $B/sub_v_frag.mp4 -s $B/sub.srt 2>/dev/null
    check subtitles-srt $O/sub_ref.mp4 $O/sub.mp4
    $U -o $O/sub_hls.mp4 -s $(segs $B/hls/v.m3u8) -s $B/hls/s0.vtt $B/hls/s1.vtt 2>/dev/null
    $U -o $O/sub_ass.mp4 -s $B/sub_v_frag.mp4 -s $B/s.ass 2>/dev/null
    for k in hls ass; do
        if ffmpeg -v error -i $O/sub_$k.mp4 -map 0:s -f srt - 2>/dev/null | cmp -s - $B/$k.expected.srt; then
            echo "PASS subtitles-$k"; pass=$((pass + 1))
        else
            echo "FAIL subtitles-$k (SRT dump differs from $B/$k.expected.srt)"; fail=$((fail + 1))
        fi
    done
    rm -f $O/sub*.mp4
fi

# A real VK track (the user's own upload): HLS, MP3 320 in TS, AES-128 segments already
# decrypted by the fetch, with an ID3 tag inside the audio stream.
if [ -f $T/vk/seg-00.ts ]; then
    $U -o $O/vk.mp3 -s $(ls -v $T/vk/seg-*.ts) 2>/dev/null
    cat $(ls -v $T/vk/seg-*.ts) > $O/vk.ts
    ffmpeg -v error -y -i $O/vk.ts -c copy $O/vk_ref.mp3 2>/dev/null; rm -f $O/vk.ts
    check vk-mp3 $O/vk_ref.mp3 $O/vk.mp3
    rm -f $O/vk.mp3 $O/vk_ref.mp3
fi

echo "$pass passed, $fail failed"
[ $fail -eq 0 ]
