#!/bin/sh
# prepare_streams.sh <output directory>
#
# Fetches or makes the streams of the conformance run (testing/conformance/streams.txt) and their md5
# references, and writes the task list for the decoder:
#
#   <output>/aom/<file>, <file>.md5   AOM's test vectors with AOM's md5 files, as published
#   <output>/cut/<name>, <name>.md5   cuts of Netflix Open Content, reference from dav1d
#   <output>/svt/<name>, <name>.md5   streams encoded here with SVT-AV1, reference from dav1d
#   <output>/manifest.txt             one line per stream: the arguments of a warp_decoder task, with
#                                     paths relative to <output> (warp_decoder -list manifest.txt)
#   <output>/SOURCES.txt              what made the streams: ffmpeg, its dav1d and SVT-AV1
#
# Needs curl and, for the cut and svt streams, ffmpeg with libdav1d and libsvtav1. The streams are media:
# they stay in <output>, which is never committed and never uploaded as an artifact.
#
# AOM's md5 files have one line per output frame, "<md5>  <file>-<width>x<height>-<nnnn>.i420": the md5 of
# the frame as aomdec --i420 writes it, at the size the frame has. That is the test player's hash (visible
# samples of Y, U, V; 8-bit as bytes, 10-bit as 16 bits, low byte first) except for a monochrome frame,
# where aomdec writes two grey chroma planes and the player hashes luma only; warp_decoder -aom_md5 hashes
# a monochrome frame the aomdec way. So the files are used as published, with -aom_md5.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 <output directory>" >&2
  exit 2
fi
here=$(cd "$(dirname "$0")" && pwd)
list="$here/streams.txt"
make_ref="$here/../md5ref/make_md5_ref.sh"
out=$1
mkdir -p "$out/aom" "$out/cut" "$out/svt"
out=$(cd "$out" && pwd)

aom_base=https://storage.googleapis.com/aom-test-data
cut_base=http://download.opencontent.netflix.com.s3.amazonaws.com
# Seconds a task may take before the decoder gives up on it (warp_decoder -timeout). WARP is a software
# rasteriser: the vectors are small, the cut and svt streams are 720p to 2160p.
aom_timeout=300
big_timeout=2400

manifest="$out/manifest.txt"
: > "$manifest"

# AOM's vectors: eight downloads at a time.
aom_files=$(awk '$1 == "aom" { print $2 }' "$list")
aom_count=$(echo "$aom_files" | grep -c . || true)
echo "AOM test vectors: $aom_count"
echo "$aom_files" | xargs -P 8 -I{} sh -c '
  for f in "$2" "$2.md5"; do
    ok=0
    for attempt in 1 2 3; do
      if curl -fsSL --retry 3 --connect-timeout 20 -o "$1/aom/$f" "$0/$f" && [ -s "$1/aom/$f" ]; then ok=1; break; fi
      sleep 5
    done
    if [ $ok -ne 1 ]; then echo "cannot download $0/$f" >&2; exit 255; fi
  done
' "$aom_base" "$out" {}
for f in $aom_files; do
  # Every line of an AOM md5 file starts with the 32 hex digits of a frame.
  lines=$(grep -c -E '^[0-9a-f]{32} ' "$out/aom/$f.md5" || true)
  if [ "$lines" -lt 1 ] || [ "$lines" -ne "$(grep -c . "$out/aom/$f.md5")" ]; then
    echo "$0: unexpected md5 file for $f" >&2
    exit 1
  fi
  echo "-name $f -i aom/$f -check aom/$f.md5 -aom_md5 -timeout $aom_timeout" >> "$manifest"
done

need_ffmpeg=$(awk '$1 == "cut" || $1 == "svt"' "$list" | grep -c . || true)
if [ "$need_ffmpeg" -gt 0 ]; then
  if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "$0: ffmpeg not found" >&2
    exit 1
  fi
  if ! ffmpeg -hide_banner -decoders 2>/dev/null | grep -q libdav1d; then
    echo "$0: this ffmpeg has no libdav1d decoder" >&2
    exit 1
  fi
fi

# Streams encoded here. The pattern, the size and the encoder settings are fixed; the line gives the
# number of frames and the SVT-AV1 parameters that switch the feature on.
awk '$1 == "svt"' "$list" | while read -r kind name frames params rest; do
  if ! ffmpeg -hide_banner -encoders 2>/dev/null | grep -q libsvtav1; then
    echo "$0: this ffmpeg has no libsvtav1 encoder" >&2
    exit 1
  fi
  echo "svt $name: $frames frames, $params"
  ffmpeg -nostdin -hide_banner -v error -f lavfi -i testsrc2=size=1920x1080:rate=30 -frames:v "$frames" \
    -pix_fmt yuv420p10le -c:v libsvtav1 -preset 10 -crf 35 -svtav1-params "$params" -f ivf -y "$out/svt/$name"
  sh "$make_ref" "$out/svt/$name"
  got=$(grep -c . "$out/svt/$name.md5")
  if [ "$got" -ne "$frames" ]; then
    echo "$0: $name: dav1d returned $got frames, $frames were encoded" >&2
    exit 1
  fi
  # The stream must have what it is here for: frame headers with use_superres, or more than one coded width.
  headers=$(ffmpeg -nostdin -hide_banner -i "$out/svt/$name" -c copy -bsf:v trace_headers -f null - 2>&1 || true)
  superres=$(echo "$headers" | grep -c -E ' use_superres +1 = 1$' || true)
  widths=$(echo "$headers" | grep -E ' frame_width_minus_1 ' | awk '{ print $NF }' | sort -u | grep -c . || true)
  echo "svt $name: $superres frame headers with use_superres, $widths coded widths that differ from the sequence's"
  case "$params" in
    *superres-mode*) [ "$superres" -gt 0 ] || { echo "$0: $name has no superres frames" >&2; exit 1; } ;;
  esac
  case "$params" in
    *resize-mode*) [ "$widths" -gt 1 ] || { echo "$0: $name has no frames of another size" >&2; exit 1; } ;;
  esac
  # Frame headers with more than one tile column, and key frames.
  tiles=$(echo "$headers" | grep -E ' tile_cols_log2 ' | awk '$NF > 0' | grep -c . || true)
  keys=$(echo "$headers" | grep -c -E ' frame_type +0+ = 0$' || true)
  case "$params" in
    *tile-columns*)
      echo "svt $name: $tiles frame headers with several tile columns, $keys key frames"
      [ "$tiles" -gt 0 ] || { echo "$0: $name has no frames with several tiles" >&2; exit 1; } ;;
  esac
  case "$params" in
    *keyint*) [ "$keys" -gt 1 ] || { echo "$0: $name has one key frame" >&2; exit 1; } ;;
  esac
  echo "-name $name -i svt/$name -check svt/$name.md5 -timeout $big_timeout" >> "$manifest"
done

# Cuts of published streams: from the key frame at or before the start, without re-encoding.
awk '$1 == "cut"' "$list" | while read -r kind name start frames path rest; do
  echo "cut $name: $frames frames from ${start}s of $path"
  ok=0
  for attempt in 1 2 3; do
    if ffmpeg -nostdin -hide_banner -v error -ss "$start" -i "$cut_base/$path" -map 0:v:0 -frames:v "$frames" \
        -c copy -f ivf -y "$out/cut/$name" && [ -s "$out/cut/$name" ]; then
      ok=1
      break
    fi
    sleep 10
  done
  if [ $ok -ne 1 ]; then
    echo "$0: cannot cut $name from $cut_base/$path" >&2
    exit 1
  fi
  sh "$make_ref" "$out/cut/$name"
  got=$(grep -c . "$out/cut/$name.md5")
  if [ "$got" -ne "$frames" ]; then
    echo "$0: $name: dav1d returned $got frames, $frames were cut" >&2
    exit 1
  fi
  echo "-name $name -i cut/$name -check cut/$name.md5 -timeout $big_timeout" >> "$manifest"
done

{
  echo "Stream list: testing/conformance/streams.txt"
  echo "AOM test vectors: $aom_base"
  echo "Cuts: $cut_base"
  if [ "$need_ffmpeg" -gt 0 ]; then
    ffmpeg -hide_banner -version | head -n 1
    ffmpeg -hide_banner -version | tr ' ' '\n' | grep -E '^--enable-lib(dav1d|svtav1)$' || true
  fi
} > "$out/SOURCES.txt"

tasks=$(grep -c . "$manifest")
expected=$(awk '$1 == "aom" || $1 == "cut" || $1 == "svt"' "$list" | grep -c .)
if [ "$tasks" -ne "$expected" ]; then
  echo "$0: $tasks tasks written for $expected streams of the list" >&2
  exit 1
fi
echo "$manifest: $tasks streams"
