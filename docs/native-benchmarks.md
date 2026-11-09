<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->
| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |
|---|---:|---:|---:|---:|---:|---:|
| Notepad | 57 | 50.8 | **31.9** | 33.2 | 35.4 | 31ms |
| File Explorer | 206 | 85.6 | **80.8** | 146.5 | 196.2 | 78ms |
| Settings | 128 | 51.7 | **42.9** | 53.7 | 55.4 | 42ms |
| Example Domain | 129 | 22.1 | **20.7** | 22.8 | 24.3 | 18ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.

### Reading the pixels

| window | size | capture | how | OCR | lines | same frame again |
             |---|---|---:|---|---:|---:|---:|
| Notepad | 970x1032 | 21ms | printwindow | **43ms** | 37 | 0ms |
| File Explorer | 1210x735 | 19ms | printwindow | **47ms** | 52 | 0ms |
| Settings | 1936x1048 | 35ms | printwindow | **66ms** | 59 | 0ms |
| Example Domain | 1936x1048 | 22ms | printwindow | **92ms** | 33 | 0ms |

`Windows.Media.Ocr`, which ships with Windows and needs no install, no model              download and no network. Capture is `PrintWindow` with full content, so the answer              is right even when something is on top of the window -- falling back to the screen              for windows that render nothing into a bitmap. The last column is the frame cache:              the same pixels are not read twice.
