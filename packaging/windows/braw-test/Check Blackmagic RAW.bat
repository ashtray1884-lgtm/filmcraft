@echo off
rem Drag a .braw file onto this file (or double-click it with no file) to check that
rem FilmCraft can use the Blackmagic RAW runtime installed on this PC. With a clip, the
rem first frame is saved next to it as <clip>.filmcraft-check.png.
title FilmCraft - Blackmagic RAW check
"%~dp0filmcraft-cli.exe" braw-check %1
echo.
pause
