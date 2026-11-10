# Benchmarks

Reproduce, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's handling, the gate, the candidate compression and the screen fingerprints. The decision model is the offline mock, so no network and no key are involved and nothing here is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp tools/list | **0.234** | 0.256 | 0.193 | 9 | 8 tools, answered from a static table |
| mcp cold, spawn included | **16.3** | 18.7 | 15.4 | 9 | process start + initialize + tools/list |
| broker cold start | **36.3** | 40.2 | 25.2 | 5 | spawn to first status reply |
| status round trip | **0.064** | 0.080 | 0.052 | 400 | agent -> pipe -> broker -> agent |
| log.tail 50 | **0.070** | 0.103 | 0.059 | 100 | journal read over the pipe |
| connect + handshake | **0.178** | 0.283 | 0.138 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.032** | 0.037 | 0.031 | 200 | 64 elements -> options |
| decide (mock, offline) | **0.134** | 0.162 | 0.093 | 200 | two typed questions, 64 options |
| gate verdict | **0.006** | 0.007 | 0.005 | 20000 | block, deny, review, brand, rate |
| screen capture | **--** | -- | -- | 0 | not measured: this session was locked |

**The screen rows were not measured.** This session was locked when the benchmark ran. A locked desktop has nothing drawn to copy, so a capture of it is both blank and slow -- 1.7s against the 30-odd ms it costs on a desktop in use -- and recording that as a capture cost would describe the lock screen rather than this code. Run it again from an unlocked session.

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

The Python backend this replaces read 24 elements in 223ms and 168 in 2.1s on the same machine.

## What is not here

The decision model's round trip (70-500ms, somebody else's hardware). It is reported in the README as an observation from a live desktop, which is what it is -- not a benchmark.

---

Machine: Windows, 6 logical cores, release profile (fat LTO, one codegen unit). Percentiles are nearest-rank: no interpolation, so every figure is a sample that was actually taken.
