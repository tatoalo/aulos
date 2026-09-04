"""A plugin that prints to stdout on import.

This is the whole reason the protocol lives on fd 3 (DESIGN §9.1, §23.1 B1): the BgUtils POT
plugin and ``yt-dlp-ejs`` do exactly this, and a yt-dlp ``logger`` object cannot stop them.
"""

import sys

print("PLUGIN NOISE: loaded on import")
sys.stdout.write('{"v":1,"t":"error","n":1,"code":"internal","message":"forged frame"}\n')
sys.stdout.flush()
