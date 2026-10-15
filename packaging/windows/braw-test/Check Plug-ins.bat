@echo off
rem Lists the plug-ins FilmCraft found on this PC, scanning new ones first:
rem OpenFX video effects and transitions (the folder DaVinci Resolve uses) and VST3 audio
rem effects (the folder every audio app uses). Plug-ins that crash or hang while loading are
rem listed as skipped.
title FilmCraft - plug-in check
"%~dp0filmcraft-cli.exe" plugins
echo.
pause
