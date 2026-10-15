@echo off
rem Lists the OpenFX video effects and transitions FilmCraft found on this PC (the same
rem plug-in folder DaVinci Resolve uses), scanning new ones first. Plug-ins that crash or hang
rem while loading are listed as skipped.
title FilmCraft - plug-in check
"%~dp0filmcraft-cli.exe" plugins
echo.
pause
