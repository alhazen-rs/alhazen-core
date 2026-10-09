#!/usr/bin/env bash
# Regenerates the audio-file fixtures (alhazen-core 0.5). Needs ffmpeg with libmp3lame, libvorbis, libopus.
# Signals are chirps, so a wrong position after a seek cannot line up by accident.
set -euo pipefail
cd "$(dirname "$0")"
F=(ffmpeg -v error -y)
CHIRP1="aevalsrc=exprs='0.5*sin(2*PI*(220+300*t)*t)'"
CHIRP2="aevalsrc=exprs='0.5*sin(2*PI*(220+300*t)*t)|0.4*sin(2*PI*(330+200*t)*t)'"
PAN51="pan=5.1|c0=c0|c1=0.8*c0|c2=0.6*c0|c3=0.1*c0|c4=0.4*c0|c5=0.3*c0"
T=(-metadata "title=Test Title" -metadata "artist=Test Artist" -metadata "album=Test Album"
   -metadata "album_artist=Test Album Artist" -metadata "track=3/12" -metadata "date=2024" -metadata "genre=Rock")

"${F[@]}" -f lavfi -i "color=c=red:s=16x16" -frames:v 1 cover.png
# MP3
"${F[@]}" -f lavfi -i "$CHIRP2:s=44100:d=2" -c:a libmp3lame -b:a 96k mp3_cbr.mp3
"${F[@]}" -f lavfi -i "$CHIRP2:s=48000:d=2" -c:a libmp3lame -q:a 4 mp3_vbr.mp3
"${F[@]}" -f lavfi -i "$CHIRP1:s=22050:d=2" -c:a libmp3lame -b:a 32k mp3_mpeg2.mp3
"${F[@]}" -f lavfi -i "$CHIRP1:s=8000:d=2" -c:a libmp3lame -b:a 16k mp3_mpeg25.mp3
"${F[@]}" -f lavfi -i "$CHIRP1:s=44100:d=2" -c:a libmp3lame -b:a 64k -write_xing 0 mp3_no_xing.mp3
"${F[@]}" -f lavfi -i "$CHIRP1:s=44100:d=1" -i cover.png -map 0:a -map 1:v -c:a libmp3lame -b:a 64k -c:v copy \
  -id3v2_version 3 -metadata:s:v "comment=Cover (front)" "${T[@]}" mp3_tagged.mp3
# ADTS
"${F[@]}" -i aac_only.m4a -c copy -f adts aac.aac
"${F[@]}" -i aac_only.m4a -c copy -f adts -write_id3v2 1 "${T[@]}" aac_tagged.aac
# FLAC
"${F[@]}" -f lavfi -i "$CHIRP2:s=16000:d=2" -c:a flac flac.flac
"${F[@]}" -f lavfi -i "$CHIRP1:s=16000:d=1" -i cover.png -map 0:a -map 1:v -c:a flac -c:v copy \
  -disposition:v attached_pic "${T[@]}" flac_tagged.flac
# WAV (8 kHz keeps them small)
"${F[@]}" -f lavfi -i "$CHIRP2:s=8000:d=1" -c:a pcm_s16le wav_s16.wav
"${F[@]}" -f lavfi -i "$CHIRP2:s=8000:d=0.5" -c:a pcm_s24le wav_s24.wav
"${F[@]}" -f lavfi -i "$CHIRP2:s=8000:d=0.5" -c:a pcm_f32le wav_f32.wav
"${F[@]}" -f lavfi -i "$CHIRP1:s=8000:d=1" -c:a pcm_u8 wav_u8.wav
"${F[@]}" -f lavfi -i "$CHIRP1:s=8000:d=0.5" -af "$PAN51" -c:a pcm_s16le wav_51.wav
"${F[@]}" -f lavfi -i "$CHIRP1:s=8000:d=1" -c:a adpcm_ima_wav wav_adpcm.wav
"${F[@]}" -f lavfi -i "$CHIRP1:s=8000:d=0.5" -c:a pcm_s16le "${T[@]}" wav_tagged.wav
# Ogg
"${F[@]}" -f lavfi -i "$CHIRP2:s=44100:d=2" -c:a libvorbis -q:a 2 "${T[@]}" vorbis.ogg
"${F[@]}" -f lavfi -i "$CHIRP2:s=48000:d=2" -c:a libopus -b:a 48k "${T[@]}" opus.opus
"${F[@]}" -f lavfi -i "$CHIRP1:s=48000:d=1" -af "$PAN51" -c:a libopus -b:a 96k opus_51.opus
"${F[@]}" -f lavfi -i "$CHIRP2:s=16000:d=1" -c:a flac -f ogg flac.oga
# Tagged MP4 and Matroska (used in Task 11)
"${F[@]}" -i aac_only.m4a -i cover.png -map 0:a -map 1:v -c copy -disposition:v attached_pic "${T[@]}" m4a_tagged.m4a
"${F[@]}" -f lavfi -i "$CHIRP1:s=48000:d=1" -c:a libopus -b:a 32k "${T[@]}" \
  -attach cover.png -metadata:s:t mimetype=image/png -metadata:s:t filename=cover.png mka_tagged.mka
