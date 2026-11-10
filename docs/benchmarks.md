# Benchmarks

Reproduce, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's handling, the gate, the candidate compression and the screen fingerprints. The decision model is the offline mock, so no network and no key are involved and nothing here is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp tools/list | **0.212** | 0.257 | 0.192 | 9 | 8 tools, answered from a static table |
| mcp cold, spawn included | **17.0** | 217 | 15.8 | 9 | process start + initialize + tools/list |
| broker cold start | **38.9** | 40.3 | 32.1 | 5 | spawn to first status reply |
| status round trip | **0.055** | 0.074 | 0.048 | 400 | agent -> pipe -> broker -> agent |
| log.tail 50 | **0.058** | 0.124 | 0.050 | 100 | journal read over the pipe |
| connect + handshake | **0.172** | 0.237 | 0.158 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.031** | 0.043 | 0.029 | 200 | 64 elements -> options |
| decide (mock, offline) | **0.092** | 0.104 | 0.083 | 200 | two typed questions, 64 options |
| gate verdict | **0.005** | 0.008 | 0.005 | 20000 | block, deny, review, brand, rate |
| screen capture | **32.8** | 46.9 | 31.7 | 20 | 1920x1080 virtual desktop, BitBlt + DIB |
| full fingerprint | **3.14** | 3.49 | 2.68 | 200 | 64x36 greyscale from the grab |
| element fingerprint | **0.359** | 0.424 | 0.340 | 200 | 96x96 crop of one control |

## How to read this

**`mcp tools/list` starts no broker.** A harness lists tools at startup and often again per turn. If that woke a daemon, the resting state of this machine would be "kratos is running". It answers from a static table instead, and the benchmark asserts afterwards that no pipe exists.

**Cold start is the first tool call, not the first action.** Nothing is registered at logon and nothing runs until an agent asks for something; this is the price of that choice, paid once per idle period.

**The round trip is the floor under every operation.** An `observe` or an `act` costs this, plus the backend, plus the model. When the total is 500ms, that is where it went.

**The gate is not a latency story.** It is listed because a safety check that cost milliseconds would eventually be made optional, and this one cannot be.

**Screen capture is one grab of the whole virtual desktop, cropped twice.** An earlier build took eight separate grabs per action; both rectangles now come out of one, which is where 407ms became 100ms.

## Native perception, measured live

Not reproducible the way everything above is: these depend on which windows are open and what is in them. They are here because the claim that matters about          `kratos-native` -- one cached fetch per window instead of a call per property -- is          only meaningful against real applications.

```bash
cargo run --release -p kratos-native -- --bench "Notepad" "Settings" --out          docs/native-benchmarks.md
```

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

The Python backend this replaces read 24 elements in 223ms and 168 in 2.1s on the same machine.

## What is not here

The decision model's round trip (70-500ms, somebody else's hardware). It is reported in the README as an observation from a live desktop, which is what it is -- not a benchmark.

---

Machine: Windows, 6 logical cores, release profile (fat LTO, one codegen unit). Percentiles are nearest-rank: no interpolation, so every figure is a sample that was actually taken.
