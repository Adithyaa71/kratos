<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->
| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |
|---|---:|---:|---:|---:|---:|---:|
| While Loops Introduction - Brave | 86 | 51.5 | **22.5** | 23.0 | 23.6 | 21ms |
| Kratos - File Explorer | 205 | 118.9 | **117.9** | 123.2 | 124.8 | 115ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.

### Reading the pixels

| window | size | capture | how | OCR | lines | same frame again |
             |---|---|---:|---|---:|---:|---:|
| While Loops Introduction - Brave | 1936x1048 | 25ms | printwindow | **112ms** | 61 | 0ms |
| Kratos - File Explorer | 1210x735 | 38ms | printwindow | **55ms** | 51 | 0ms |

`Windows.Media.Ocr`, which ships with Windows and needs no install, no model              download and no network. Capture is `PrintWindow` with full content, so the answer              is right even when something is on top of the window -- falling back to the screen              for windows that render nothing into a bitmap. The last column is the frame cache:              the same pixels are not read twice.

### Finding the controls nothing declares

| window | icons | lines | fused | of those from pixels | resize | network | same              frame again |
|---|---:|---:|---:|---:|---:|---:|---:|
| While Loops Introduction - Brave | 128 | 61 | 147 | 128 | 6ms | **380ms** | 0ms |
| Kratos - File Explorer | 106 | 51 | 121 | 106 | 5ms | **627ms** | 0ms |

OmniParser v2's icon detector, exported to ONNX, on **CPU**: this machine's              onnxruntime offers no GPU provider, so that is the number there is. The spec's              60ms target is a GPU figure and is not met here and not claimed. `resize` is the              letterbox this code does; `network` is onnxruntime, and the split says which of              the two is worth optimising. The weights are AGPL-3.0 and are not in this              repository -- see docs/pixel-path.md.
