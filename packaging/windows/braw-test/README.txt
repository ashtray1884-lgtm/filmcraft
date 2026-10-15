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
- Frames are decoded on your graphics card (NVIDIA through CUDA, AMD and Intel through OpenCL),
  as in Premiere Pro. "Check Blackmagic RAW.bat" shows which card it uses ("decoded on: ...").
  If the card can't be used, FilmCraft decodes on the processor instead, automatically.
- Playback decodes at a reduced size when the monitor is small, so 6K stays smooth; export
  always decodes at full size.
- RED R3D: .R3D clips (and Nikon N-RAW .NEV) open through RED's own R3D decoder, which ships
  next to filmcraft.exe (REDR3D-x64.dll and friends, see "RED R3D SDK License Agreement.pdf").
  Right-click a clip > Source Settings... for RED's settings: ISO, white balance, exposure,
  color space, gamma, tone map, highlight roll-off, CDL, lens correction. Open the first file
  of a clip (..._001.R3D); the other parts load with it. Playback at 1/2 or 1/4 resolution is
  much faster for 6K/8K. "Check Blackmagic RAW.bat" works for an .R3D clip too.
- Plug-ins: FilmCraft loads your OpenFX video effects and transitions (the versions made for
  DaVinci Resolve: Boris FX, Red Giant, FilmConvert, Neat Video, Dehancer...) and your VST3
  audio effects (FabFilter, iZotope, Waves, Valhalla...). They appear in the Effects panel:
  Video Effects / Video Transitions / Audio Effects, in a folder named after the maker. The
  first start scans them (a minute or two with many plug-ins); later starts are instant. For an
  audio plug-in, Effect Controls has "Open Plug-in Window..." for its own interface. Premiere-only
  plug-ins (.aex / .prm) and VST2 plug-ins are not supported. "Check Plug-ins.bat" lists what
  FilmCraft found and anything it had to skip.
- Problems: run "Check Blackmagic RAW.bat" with the clip and send the text it prints.
