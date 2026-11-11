#!/bin/sh
# make_md5_ref.sh <clip.ivf> [frames]
#
# Writes <clip.ivf>.md5 in the test player's format (what -save_md5 writes and -md5_frame_check reads),
# with dav1d as the decoder: an md5 reference that does not come from this decoder.
#
#   <32 hex digits>   <frame number>\r\n      one line per output frame, in output order, numbered from 0
#
# The md5 of a frame is over its visible samples: every row of the luma plane, then of U, then of V
# (4:2:0; luma only for a monochrome stream), one byte a sample for 8-bit and two bytes, low byte first,
# above that. That is what ffmpeg's framemd5 muxer hashes for the frames libdav1d returns, so the sixth
# field of its lines is the player's md5. dav1d applies film grain, as the player's decoder does.
#
# A stream whose frames change size (a new sequence at a key frame, or reference scaling: frames coded at
# another size than the one before) is hashed frame by frame at the size each frame has, as the decoder
# returns it. ffmpeg would otherwise scale every frame to the size of the first one (its -autoscale
# default) and the md5 of a scaled frame matches no decoder; -noautoscale switches that off.
#
# [frames]: only the first N output frames, for a task with -limit N. Without it the whole clip.
# Not for a task with -start (the reference then has to start at that frame) or -try10x3.
#
# Needs ffmpeg built with libdav1d. Upload the .md5 file next to the stream; see docs/test-player.md.
set -eu

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: $0 <clip.ivf> [frames]" >&2
  exit 2
fi
clip=$1
frames=${2:-}
if [ ! -f "$clip" ]; then
  echo "$0: no such file: $clip" >&2
  exit 2
fi
case "$frames" in
  '' ) ;;
  *[!0-9]* | 0 ) echo "$0: frames must be a positive number: $frames" >&2; exit 2 ;;
esac
if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "$0: ffmpeg not found" >&2
  exit 1
fi
if ! ffmpeg -hide_banner -decoders 2>/dev/null | grep -q libdav1d; then
  echo "$0: this ffmpeg has no libdav1d decoder" >&2
  exit 1
fi

out="$clip.md5"
tmp="$out.tmp.$$"
trap 'rm -f "$tmp" "$tmp.framemd5"' EXIT

# One line per frame the decoder returns: no frame is dropped or repeated to fit a frame rate, no frame is
# scaled to the size of the first one, and the pixel format stays the decoder's.
run_ffmpeg() {
  if [ -n "$frames" ]; then
    ffmpeg -nostdin -hide_banner -v error -c:v libdav1d -i "$clip" -map 0:v:0 -noautoscale "$@" \
      -frames:v "$frames" -f framemd5 -y "$tmp.framemd5"
  else
    ffmpeg -nostdin -hide_banner -v error -c:v libdav1d -i "$clip" -map 0:v:0 -noautoscale "$@" \
      -f framemd5 -y "$tmp.framemd5"
  fi
}
# -fps_mode is the option of ffmpeg 5.1 and later, -vsync the one before it.
if ! run_ffmpeg -fps_mode passthrough 2>/dev/null; then
  run_ffmpeg -vsync passthrough
fi

# framemd5 lines: "stream, dts, pts, duration, size, md5" and comment lines that start with '#'.
awk -F, '
  /^#/ { next }
  NF >= 6 {
    md5 = $6
    gsub(/[ \t\r]/, "", md5)
    if (length(md5) != 32) { printf "unexpected framemd5 line: %s\n", $0 > "/dev/stderr"; exit 1 }
    printf "%s   %d\r\n", md5, n++
  }
  END { if (n == 0) { print "no frames decoded" > "/dev/stderr"; exit 1 } }
' "$tmp.framemd5" > "$tmp"

mv "$tmp" "$out"
count=$(wc -l < "$out" | tr -d ' ')
pix=$(grep -m1 '^#dimensions' "$tmp.framemd5" 2>/dev/null | sed 's/^#dimensions[^:]*: *//' || true)
# The fifth field of a framemd5 line is the size of the frame in bytes: more than one value means that the
# frames of the clip do not all have the same size.
sizes=$(awk -F, '!/^#/ && NF >= 6 { gsub(/[ \t]/, "", $5); print $5 }' "$tmp.framemd5" | sort -u | wc -l | tr -d ' ')
echo "$out: $count frames${pix:+, $pix}$([ "$sizes" -gt 1 ] && echo ", $sizes frame sizes (first size shown)")"
if [ -n "$frames" ] && [ "$count" -lt "$frames" ]; then
  echo "$0: the clip has only $count frames, fewer than the $frames asked for" >&2
fi
