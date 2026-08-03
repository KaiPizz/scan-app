# Skaner dokumentów

Lekka aplikacja dla Windows 11 do skanowania dokumentów A4 za pomocą
IRIScan Visualizer 7. Interfejs jest w języku polskim i prowadzi użytkownika
przez wybór folderu, przechwycenie obrazu, kadrowanie oraz zapis jedno- lub
wielostronicowego pliku PDF.

## Uruchomienie deweloperskie

```powershell
cargo run
```

## Zbudowanie wersji użytkowej

```powershell
cargo build --release
```

Plik programu znajdzie się w `target\release\skaner-dokumentow.exe`.

Gotowy plik można skopiować z `target\release` do wybranego folderu
dystrybucyjnego. Przed użyciem kamery należy zamknąć Readiris Visual, Aparat,
Teams, Zoom i każdy inny program, który może aktualnie korzystać z IRIScan
Visualizer 7.

## Zakres MVP

- biblioteka swobodnie nazwanych folderów,
- bezpośredni podgląd z IRIScan Visualizer 7,
- automatyczne wykrywanie krawędzi i ręczna korekta czterech narożników,
- obracanie, usuwanie i zmiana kolejności stron,
- zapis obrazu jako jedno- lub wielostronicowy PDF,
- automatyczne unikanie nadpisania istniejącego pliku,
- zapamiętywanie ostatniego folderu i zmiana lokalizacji biblioteki.

OCR nie wchodzi w zakres pierwszej wersji.
