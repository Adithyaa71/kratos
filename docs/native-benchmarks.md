<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->
| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |
|---|---:|---:|---:|---:|---:|---:|
| Notepad | 41 | 78.8 | **39.3** | 45.2 | 52.1 | 39ms |
| Kratos - File Explorer | 204 | 86.7 | **85.8** | 89.1 | 95.2 | 83ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.

### Reading the pixels

| window | size | capture | how | OCR | lines | same frame again |
             |---|---|---:|---|---:|---:|---:|
| Notepad | 970x1032 | 36ms | printwindow | **30ms** | 10 | 0ms |
| Kratos - File Explorer | 1210x735 | 34ms | printwindow | **49ms** | 56 | 0ms |

`Windows.Media.Ocr`, which ships with Windows and needs no install, no model              download and no network. Capture is `PrintWindow` with full content, so the answer              is right even when something is on top of the window -- falling back to the screen              for windows that render nothing into a bitmap. The last column is the frame cache:              the same pixels are not read twice.

### Finding the controls nothing declares

| window | icons | lines | fused | of those from pixels | resize | network | same              frame again |
|---|---:|---:|---:|---:|---:|---:|---:|
| Notepad | 29 | 10 | 29 | 29 | 9ms | **644ms** | 0ms |
| Kratos - File Explorer | 101 | 56 | 122 | 101 | 5ms | **400ms** | 0ms |

OmniParser v2's icon detector, exported to ONNX, on **CPU**: this machine's              onnxruntime offers no GPU provider, so that is the number there is. The spec's              60ms target is a GPU figure and is not met here and not claimed. `resize` is the              letterbox this code does; `network` is onnxruntime, and the split says which of              the two is worth optimising. The weights are AGPL-3.0 and are not in this              repository -- see docs/pixel-path.md.
