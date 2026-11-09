<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->
| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |
|---|---:|---:|---:|---:|---:|---:|
| Notepad | 41 | 70.5 | **25.8** | 27.5 | 34.0 | 25ms |
| File Explorer | 206 | 97.3 | **81.8** | 86.9 | 93.0 | 80ms |
| Settings | 128 | 70.5 | **52.9** | 57.4 | 57.9 | 51ms |
| Example Domain | 129 | 26.0 | **21.1** | 22.7 | 23.5 | 19ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.

### Reading the pixels

| window | size | capture | how | OCR | lines | same frame again |
             |---|---|---:|---|---:|---:|---:|
| Notepad | 970x1032 | 25ms | printwindow | **23ms** | 11 | 0ms |
| File Explorer | 1210x735 | 32ms | printwindow | **50ms** | 53 | 0ms |
| Settings | 1936x1048 | 24ms | printwindow | **60ms** | 59 | 0ms |
| Example Domain | 1936x1048 | 33ms | printwindow | **147ms** | 33 | 0ms |

`Windows.Media.Ocr`, which ships with Windows and needs no install, no model              download and no network. Capture is `PrintWindow` with full content, so the answer              is right even when something is on top of the window -- falling back to the screen              for windows that render nothing into a bitmap. The last column is the frame cache:              the same pixels are not read twice.

### Finding the controls nothing declares

| window | icons | lines | fused | of those from pixels | resize | network | same              frame again |
|---|---:|---:|---:|---:|---:|---:|---:|
| Notepad | 28 | 11 | 28 | 28 | 8ms | **394ms** | 0ms |
| File Explorer | 84 | 53 | 105 | 84 | 5ms | **376ms** | 0ms |
| Settings | 57 | 59 | 62 | 57 | 4ms | **364ms** | 0ms |
| Example Domain | 62 | 33 | 80 | 62 | 6ms | **693ms** | 0ms |

OmniParser v2's icon detector, exported to ONNX, on **CPU**: this machine's              onnxruntime offers no GPU provider, so that is the number there is. The spec's              60ms target is a GPU figure and is not met here and not claimed. `resize` is the              letterbox this code does; `network` is onnxruntime, and the split says which of              the two is worth optimising. The weights are AGPL-3.0 and are not in this              repository -- see docs/pixel-path.md.
