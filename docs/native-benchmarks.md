<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->
| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |
|---|---:|---:|---:|---:|---:|---:|
| Notepad | 57 | 51.1 | **31.6** | 32.1 | 32.3 | 30ms |
| File Explorer | 206 | 79.9 | **78.0** | 80.3 | 83.4 | 75ms |
| Settings | 128 | 45.2 | **42.4** | 46.1 | 49.0 | 41ms |
| Example Domain | 129 | 20.8 | **21.7** | 22.2 | 23.1 | 19ms |

One cached fetch per window, 21 runs each, the first reported separately as cold. `fetch` is the single cross-process call; everything else is reading the cache locally, which is why it is the whole cost.
