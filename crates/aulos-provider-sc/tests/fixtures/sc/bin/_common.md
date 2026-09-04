The stand-in binaries the WP-09 engine tests point `EngineCfg`'s `argv[0]` at.

They exist so the suite can prove the argv, the progress plumbing, the automatic ffmpeg retry,
cancellation and partial cleanup on a machine with no `N_m3u8DL-RE` installed. Each one parses the
arguments it cares about the way the real tool does, writes (or deliberately does not write) the
output file, and prints captured output. The 0.5 s sleeps are deliberate: both engines throttle
progress to at most one frame every 500 ms, so a stand-in that printed everything instantly would
have every frame suppressed.
