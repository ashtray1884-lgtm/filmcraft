FilmCraft - Blackmagic RAW test build (Windows, 64-bit)
=======================================================

This is a test build of FilmCraft that opens Blackmagic RAW (.braw) clips through the free
Blackmagic RAW runtime already on your PC (it comes with the Blackmagic RAW installer, the same one
that adds the Premiere Pro plug-in). Nothing from Blackmagic is included here.

1. Unzip this folder anywhere (for example your Desktop).
2. Optional first check: drag one .braw clip onto "Check Blackmagic RAW.bat".
   A window shows what was found, how fast a frame decodes, and saves the first frame as a PNG
   next to the clip. Copy the text from that window if something goes wrong.
3. Double-click filmcraft.exe, then File > Import (Ctrl+I), pick your .braw clips, and drag them
   onto the timeline.

Notes
- This build is not code-signed. Windows SmartScreen may say "Windows protected your PC":
  click "More info", then "Run anyway".
- Pictures use the clip's as-shot look. If the camera recorded "Film" gamma they will look flat,
  as in DaVinci Resolve before grading; use Lumetri Color to grade.
- Playback decodes at a reduced size when the monitor is small, so 6K stays smooth; export
  always decodes at full size.
- Problems: run "Check Blackmagic RAW.bat" with the clip and send the text it prints.
