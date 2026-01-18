@echo off
REM Continuous scrolling - runs indefinitely until killed
REM This ensures scrolling is active throughout the entire measurement period
:loop
for /L %%i in (1,1,100000) do @echo Line %%i - The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs.
goto loop
