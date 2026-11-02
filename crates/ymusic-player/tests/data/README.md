Test files for the decoder (`src/decode.rs` of this crate): 2 s of a stereo signal at 44.1 kHz, the same in
both channels — 440 Hz at 0.3, 3 kHz at 0.2 and a noise-like band of 50 tones over 9–14 kHz
(0.1 RMS in all), where HE-AAC's spectral band replication has to recreate it.

The signal is written as WAV by a small program using the `fdk-aac` crate, which also encodes it as
HE-AAC (64 kbit/s, ADTS); the rest is ffmpeg:

```bash
ffmpeg -i he.adts -c:a copy -movflags +faststart he-aac.m4a
ffmpeg -i tones.wav -c:a aac -b:a 128k -movflags +faststart aac-lc.m4a
ffmpeg -i tones.wav -c:a libmp3lame -b:a 128k mp3.mp3
```

`he-aac.m4a` signals SBR implicitly (an LC config at 22050 Hz), unlike the server's files.

`pcm16.flac` and `pcm24.m4a` contain 22050 stereo frames at 44100 Hz. These synthetic integer
signals test exact decoding and seeking for native 16-bit FLAC and 24-bit FLAC in MP4.
For frame `i`, left/right samples are `(i * step) % modulus - offset`, with steps 97/71;
16-bit modulus/offset are 60001/30000, 24-bit are 6000001/3000000. They were encoded from
signed little-endian PCM by ffmpeg with `-c:a flac`; MP4 additionally uses
`-strict -2 -movflags +faststart -f mp4`. No music recordings are included.
