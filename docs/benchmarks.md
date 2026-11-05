# Benchmarks

Reproduce, on your machine, in one command:

```bash
cargo run --release -p kratos-bench -- --out docs/benchmarks.md
```

Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's handling, the gate, the candidate compression and the screen fingerprints. The decision model is the offline mock, so no network and no key are involved and nothing here is measuring somebody else's GPU.

| what | p50 ms | p95 ms | best ms | n | |
|---|---:|---:|---:|---:|---|
| mcp tools/list | **0.193** | 0.264 | 0.179 | 9 | 7 tools, answered from a static table |
| mcp cold, spawn included | **7.55** | 10.2 | 6.96 | 9 | process start + initialize + tools/list |
| broker cold start | **13.3** | 26.8 | 12.1 | 5 | spawn to first status reply |
| status round trip | **0.031** | 0.075 | 0.030 | 400 | agent -> pipe -> broker -> agent |
| log.tail 50 | **0.038** | 0.060 | 0.037 | 100 | journal read over the pipe |
| connect + handshake | **0.098** | 0.183 | 0.077 | 50 | new client, ACL check, hello/ack |
| candidate compression | **0.025** | 0.047 | 0.024 | 200 | 64 elements -> options |
| decide (mock, offline) | **0.071** | 0.079 | 0.066 | 200 | two typed questions, 64 options |
| gate verdict | **0.005** | 0.006 | 0.004 | 20000 | block, deny, review, brand, rate |
| screen capture | **32.9** | 34.1 | 30.8 | 20 | 1920x1080 virtual desktop, BitBlt + DIB |
| full fingerprint | **2.97** | 3.49 | 2.53 | 200 | 64x36 greyscale from the grab |
| element fingerprint | **0.292** | 0.437 | 0.273 | 200 | 96x96 crop of one control |

## How to read this

**`mcp tools/list` starts no broker.** A harness lists tools at startup and often again per turn. If that woke a daemon, the resting state of this machine would be "kratos is running". It answers from a static table instead, and the benchmark asserts afterwards that no pipe exists.

**Cold start is the first tool call, not the first action.** Nothing is registered at logon and nothing runs until an agent asks for something; this is the price of that choice, paid once per idle period.

**The round trip is the floor under every operation.** An `observe` or an `act` costs this, plus the backend, plus the model. When the total is 500ms, that is where it went.

**The gate is not a latency story.** It is listed because a safety check that cost milliseconds would eventually be made optional, and this one cannot be.

**Screen capture is one grab of the whole virtual desktop, cropped twice.** An earlier build took eight separate grabs per action; both rectangles now come out of one, which is where 407ms became 100ms.

## What is not here

The decision model's round trip (70-500ms, somebody else's hardware) and the perception backends (a separate process, a separate language). Those are reported in the README as observations from a live desktop, which is what they are -- not benchmarks.

---

Machine: Windows, 6 logical cores, release profile (fat LTO, one codegen unit). Percentiles are nearest-rank: no interpolation, so every figure is a sample that was actually taken.
