@echo off
echo started %date% %time% > D:\scan-app\run.log
D:\scan-app\target\release\skaner-dokumentow.exe
echo exitcode %errorlevel% at %time% >> D:\scan-app\run.log
