# Benchmarks

Reproduce, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's handling, the gate, the candidate compression and the screen fingerprints. The decision model is the offline mock, so no network and no key are involved and nothing here is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp tools/list | **0.223** | 0.953 | 0.203 | 9 | 8 tools, answered from a static table |
| mcp cold, spawn included | **9.92** | 13.8 | 9.25 | 9 | process start + initialize + tools/list |
| broker cold start | **25.4** | 25.6 | 24.4 | 5 | spawn to first status reply |
| status round trip | **0.057** | 0.093 | 0.051 | 400 | agent -> pipe -> broker -> agent |
| log.tail 50 | **0.061** | 0.113 | 0.054 | 100 | journal read over the pipe |
| connect + handshake | **0.189** | 0.262 | 0.129 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.029** | 0.030 | 0.027 | 200 | 64 elements -> options |
| decide (mock, offline) | **0.079** | 0.090 | 0.074 | 200 | two typed questions, 64 options |
| gate verdict | **0.005** | 0.006 | 0.004 | 20000 | block, deny, review, brand, rate |
| screen capture | **32.7** | 33.5 | 27.0 | 20 | 1920x1080 virtual desktop, BitBlt + DIB |
| full fingerprint | **3.09** | 3.75 | 2.66 | 200 | 64x36 greyscale from the grab |
| element fingerprint | **0.285** | 0.338 | 0.276 | 200 | 96x96 crop of one control |

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
| Notepad | 57 | 51.1 | **31.6** | 32.1 | 32.3 | 30ms |
| File Explorer | 206 | 79.9 | **78.0** | 80.3 | 83.4 | 75ms |
| Settings | 128 | 45.2 | **42.4** | 46.1 | 49.0 | 41ms |
| Example Domain | 129 | 20.8 | **21.7** | 22.2 | 23.1 | 19ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.

The Python backend this replaces read 24 elements in 223ms and 168 in 2.1s on the same machine.

## What is not here

The decision model's round trip (70-500ms, somebody else's hardware). It is reported in the README as an observation from a live desktop, which is what it is -- not a benchmark.

---

Machine: Windows, 6 logical cores, release profile (fat LTO, one codegen unit). Percentiles are nearest-rank: no interpolation, so every figure is a sample that was actually taken.
