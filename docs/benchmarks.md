# Benchmarks

Reproduce, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's handling, the gate, the candidate compression and the screen fingerprints. The decision model is the offline mock, so no network and no key are involved and nothing here is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp tools/list | **0.230** | 0.292 | 0.200 | 9 | 8 tools, answered from a static table |
| mcp cold, spawn included | **9.30** | 15.3 | 9.19 | 9 | process start + initialize + tools/list |
| broker cold start | **25.1** | 36.7 | 24.4 | 5 | spawn to first status reply |
| status round trip | **0.052** | 0.081 | 0.046 | 400 | agent -> pipe -> broker -> agent |
| log.tail 50 | **0.069** | 0.119 | 0.052 | 100 | journal read over the pipe |
| connect + handshake | **0.167** | 0.234 | 0.129 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.030** | 0.031 | 0.030 | 200 | 64 elements -> options |
| decide (mock, offline) | **0.076** | 0.085 | 0.072 | 200 | two typed questions, 64 options |
| gate verdict | **0.005** | 0.006 | 0.004 | 20000 | block, deny, review, brand, rate |
| screen capture | **32.9** | 33.3 | 24.6 | 20 | 1920x1080 virtual desktop, BitBlt + DIB |
| full fingerprint | **2.87** | 3.63 | 2.52 | 200 | 64x36 greyscale from the grab |
| element fingerprint | **0.277** | 0.298 | 0.262 | 200 | 96x96 crop of one control |

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

The Python backend this replaces read 24 elements in 223ms and 168 in 2.1s on the same machine.

## What is not here

The decision model's round trip (70-500ms, somebody else's hardware). It is reported in the README as an observation from a live desktop, which is what it is -- not a benchmark.

---

Machine: Windows, 6 logical cores, release profile (fat LTO, one codegen unit). Percentiles are nearest-rank: no interpolation, so every figure is a sample that was actually taken.
