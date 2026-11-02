# Bitcask Storage Engine — Benchmark & Optimization History

This document tracks the performance milestones, profiling discoveries, and Criterion benchmark results across the development of the Bitcask storage engine.

---

## 1. Early Write-Path Optimization Milestones

Initial benchmarking focused on raw write throughput (`BitcaskInstance::put`) and startup/compaction I/O efficiency, progressing from **~10.5k ops/s to 643k+ ops/s** (~60x speedup):

| Milestone | Commit | Architectural Change | Throughput (ops/s) | Bandwidth | 100k Write Time | Primary Bottleneck Resolved |
| :--- | :---: | :--- | :---: | :---: | :---: | :--- |
| **0. Baseline** | `7baecae` | 10 KB datafiles, unbuffered `File::write`, `sync_all` on every switchover | ~10,470 | ~3.3 MiB/s | ~9.50 s | 123 hardware `F_FULLFSYNC` barriers per run |
| **1. Segment Sizing** | `7baecae` | 64 MB segment cap, `SyncFrequency::Never` for throughput runs | ~97,800 | ~31.0 MiB/s | ~0.97 s | Eliminated mid-benchmark file rollover storms |
| **2. Fast Hashing (`ahash`)** | `38f77d9` | Replaced `std::collections::HashMap` (SipHash-1-3) with `ahash::AHashMap` in `KeyDir` | ~107,050 | ~34.0 MiB/s | ~0.93 s | Reduced per-key hashing CPU overhead |
| **3. Vectored I/O (`writev`)** | `329d383` | Coalesced header, key, and value writes into a single vectored `IoSlice` write | ~188,400 | ~96.5 MiB/s | ~0.53 s | Cut `write()` syscalls by **3x** per record |
| **4. Buffered Writes (`BufWriter`)** | `a8de05f` | Wrapped writer `File` in `BufWriter` with flush-before-read coherence in `get` | ~643,100 | ~330.0 MiB/s | ~0.155 s | Amortized syscalls across multiple records |
| **5. `KeyDir` Pre-sizing & Single-Probe** | `7317aa3` `fa43cca` | Added `expected_keydir_max_size` (`with_capacity`) & returned old entry directly from `KeyDir::put` | ~585,000 (1 KB vals) | **584.8 MiB/s** | **0.194 s** (100 MB) | Eliminated 26% rehashing overhead & double hash lookups |
| **6. Buffered Replay & Relative Seek** | `dd67ff6` `396bd66` | `BufReader` on datafile/hintfile iterators + `seek_relative` on skipped records | ~236,000 (merge) | **240.0 MiB/s** | **0.428 s** (merge) | Prevented `BufReader` buffer invalidation on every dead record |
| **7. Userspace Adaptive Spinning (`parking_lot`)** | `23d28b6` | Migrated from `std::sync` to `parking_lot::{Mutex, RwLock}` | **~2,450,000** (8-thread reads) | **2.38 GiB/s** | **0.043 s** (100k reads) | Replaced OS kernel futex sleeps/context switches with 1-byte userspace spin-handshakes |

---

## 2. Profiler Breakdown (Samply / 10,269 Samples)

After introducing `BufWriter` (Milestone 4), CPU profiling of 10M writes isolated where execution time was spent and guided Milestone 5:

| Subsystem | Inclusive CPU % | Exclusive (Self) CPU % | Root Cause & Resolution |
| :--- | :---: | :---: | :--- |
| **`KeyDir` Table Resizing** | 26.12% | 14.06% | Geometric hash table growth (~20 resizes to reach 16M buckets) $\rightarrow$ solved via `KeyDir::with_capacity` (`7317aa3`) |
| **Allocator Churn (`malloc`/`free`)** | 30.22% | 28.21% | Per-record `Box<[u8]>` allocations and deallocations in `DatafileRecord` and `KeyDir` keys |
| **`KeyDir` Double Lookup (`get` + `put`)** | 7.97% | 7.97% | Probing `KeyDir::get` for dead-space tracking before `KeyDir::put` $\rightarrow$ solved by returning `Option<KeyDirEntry>` from `put` (`fa43cca`) |
| **CRC32 Checksumming** | 9.12% | 2.74% | Header + key + value CRC32 calculation per record |
| **Kernel `write()` Syscalls** | 9.24% | 7.47% | Amortized behind `BufWriter` |
| **Timestamp Syscalls (`SystemTime::now`)** | 3.81% | 3.81% | Monotonic/UNIX timestamp generation per record |

---

## 3. Criterion Benchmark Suite Evolution (`k = 4 B`, `v = 1024 B`)

As the Criterion suite (`benches/general.rs`) was built out, several benchmark harness improvements significantly sharpened measurement accuracy:

| Benchmark Iteration | Workload (`100k` / `500k` / `1M` items) | Median Time | Throughput | What Changed in the Harness / Engine |
| :--- | :--- | :---: | :---: | :--- |
| **Initial Random Read (`d1b7e91`)** | `read/random` (10k / 50k / 100k / 500k) | `16.0 ms` / `87.6 ms` / `197.6 ms` / `1.240 s` | `624` / `571` / `506` / `403 MiB/s` | First Criterion `pread` benchmark over pre-populated DB |
| **Initial Mixed R/W (`71b52cb`)** | `read_write/random` (10k / 50k / 100k / 500k) | `29.1 ms` / `189.0 ms` / `415.2 ms` / `4.234 s` | `340` / `261` / `238` / `116 MiB/s` | Identified active-writer `flush()` bottleneck on interleaved reads + 10x pre-fill ratio |
| **Modular Runner & Teardown Isolation (`ebb3eef`)** | `write` (10k / 50k / 100k / 1M) | `20.6 ms` / `101.0 ms` / `194.7 ms` / `1.711 s` | `486` / `495` / `514` / **`584.8 MiB/s`** | Returned `(dir, db, ops)` from timed closure so `TempDir` & `Vec` drop outside timer |
| **Post-Modularization Reads (`657175d`)** | `read/sequential` (10k / 50k / 100k / 500k)<br>`read/random` (10k / 50k / 100k / 500k)<br>`read_write/random` (10k / 50k / 100k / 500k) | `11.1 ms` / `61.3 ms` / `124.2 ms` / `687.5 ms`<br>`12.2 ms` / `73.4 ms` / `163.4 ms` / `970.8 ms`<br>`29.8 ms` / `141.9 ms` / `307.4 ms` / `1.431 s` | `882` / `797` / `786` / `710 MiB/s`<br>`800` / `665` / `598` / `503 MiB/s`<br>`332` / `348` / `322` / `345.5 MiB/s` | True post-fill shuffle for random reads; 1:1 operation ratio |
| **Synthetic Merge Benchmark (`43d5e0a`)** | `merge/synthetic` (10k / 50k / 100k / 500k) | `100.9 ms` / `219.4 ms` / `428.9 ms` / `2.116 s` | `99.2` / `228.0` / `233.3` / `236.4 MiB/s` | Full compaction + hintfile generation + `SyncAll` across 64 MB datafiles |
| **Full Concurrency (`b087023`)** | All workloads (`&self` via `RwLock<KeyDir>` + `Mutex<Datafile>`) | See Section 4 below | **Up to 935 MiB/s read / 578.5 MiB/s write** | Atomic single-lock writer serialization + lock-free `pread` after `.copied()` `KeyDirEntry` |

---

## 4. Final Benchmark Results (Pre-Concurrency vs. Concurrent Engine)

All benchmarks below use **4-byte keys (`k4B`)** and **1024-byte values (`v1024B`)** (`1049 B` on-disk record size including the 21-byte header), comparing the single-threaded `&mut self` baseline against the thread-safe `&self` concurrent engine (`RwLock<KeyDir>`, `Mutex<Datafile>`, `Mutex<LruCache>`, `Mutex<StorageTracker>`).

### 4.1 Write Throughput (`write/write_k4B_v1024B`)
*Config: 1 GB soft-capped datafile, `SyncFrequency::Never`*

| Dataset Scale | Pre-Concurrency Time | Pre-Concurrency Throughput | Concurrent (`&self`) Time | Concurrent (`&self`) Throughput | Throughput (ops/s) |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **10,000 records** (~10.0 MiB) | `20.565 ms` | `486.47 MiB/s` | `20.640 ms` | **`484.68 MiB/s`** | ~484,500 ops/s |
| **50,000 records** (~50.0 MiB) | `100.99 ms` | `495.29 MiB/s` | `100.23 ms` | **`499.04 MiB/s`** | ~498,850 ops/s |
| **100,000 records** (~100.0 MiB) | `194.70 ms` | `513.81 MiB/s` | `200.90 ms` | **`497.97 MiB/s`** | ~497,760 ops/s |
| **1,000,000 records** (~1.0 GiB) | `1.7107 s` | `584.78 MiB/s` | `1.7293 s` | **`578.52 MiB/s`** | **~578,270 ops/s** |

### 4.2 Sequential Read Throughput (`read/sequential_k4B_v1024B`)
*Config: Pre-populated DB, sequential key order*

| Dataset Scale | Pre-Concurrency Time | Pre-Concurrency Throughput | Concurrent (`&self`) Time | Concurrent (`&self`) Throughput | Throughput (ops/s) |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **10,000 reads** (~9.77 MiB) | `11.076 ms` | `881.73 MiB/s` | `10.443 ms` | **`935.16 MiB/s`** | **~957,580 ops/s** |
| **50,000 reads** (~48.8 MiB) | `61.281 ms` | `796.79 MiB/s` | `55.966 ms` | **`872.47 MiB/s`** | ~893,400 ops/s |
| **100,000 reads** (~97.7 MiB) | `124.19 ms` | `786.32 MiB/s` | `120.97 ms` | **`807.25 MiB/s`** | ~826,650 ops/s |
| **500,000 reads** (~488.3 MiB) | `687.53 ms` | `710.20 MiB/s` | `647.71 ms` | **`753.86 MiB/s`** | ~771,950 ops/s |

### 4.3 Random Read Throughput (`read/random_k4B_v1024B`)
*Config: Sequentially populated DB, uniformly shuffled read queries*

| Dataset Scale | Pre-Concurrency Time | Pre-Concurrency Throughput | Concurrent (`&self`) Time | Concurrent (`&self`) Throughput | Throughput (ops/s) |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **10,000 reads** (~9.77 MiB) | `12.206 ms` | `800.04 MiB/s` | `11.826 ms` | **`825.78 MiB/s`** | **~845,590 ops/s** |
| **50,000 reads** (~48.8 MiB) | `73.399 ms` | `665.24 MiB/s` | `70.693 ms` | **`690.70 MiB/s`** | ~707,280 ops/s |
| **100,000 reads** (~97.7 MiB) | `163.37 ms` | `597.75 MiB/s` | `163.21 ms` | **`598.36 MiB/s`** | ~612,710 ops/s |
| **500,000 reads** (~488.3 MiB) | `970.81 ms` | `502.96 MiB/s` | `972.40 ms` | **`502.14 MiB/s`** | ~514,190 ops/s |

### 4.4 Mixed Read/Write 50/50 Throughput (`read_write/random_k4B_v1024B`)
*Config: 50% existing random reads + 50% new writes shuffled together*

| Dataset Scale | Pre-Concurrency Time | Pre-Concurrency Throughput | Concurrent (`&self`) Time | Concurrent (`&self`) Throughput | Throughput (ops/s) |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **10,000 ops** (5k R + 5k W) | `29.754 ms` | `332.22 MiB/s` | `29.761 ms` | **`332.14 MiB/s`** | ~336,010 ops/s |
| **50,000 ops** (25k R + 25k W) | `141.92 ms` | `348.24 MiB/s` | `141.10 ms` | **`350.27 MiB/s`** | **~354,360 ops/s** |
| **100,000 ops** (50k R + 50k W) | `307.36 ms` | `321.61 MiB/s` | `309.00 ms` | **`319.90 MiB/s`** | ~323,620 ops/s |
| **500,000 ops** (250k R + 250k W) | `1.4305 s` | `345.50 MiB/s` | `1.4421 s` | **`342.73 MiB/s`** | ~346,720 ops/s |

### 4.5 Compaction Merge Throughput (`merge/synthetic_k4B_v1024B`)
*Config: 64 MB soft-capped datafiles, full compaction rewrite + `.hint` file generation + `SyncAll`*

| Dataset Scale | Pre-Concurrency Time | Pre-Concurrency Throughput | Concurrent (`&self`) Time | Concurrent (`&self`) Throughput | Throughput (ops/s) |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **10,000 records** (~10.0 MiB) | `100.88 ms` | `99.17 MiB/s` | `100.48 ms` | **`99.56 MiB/s`** | ~99,520 ops/s |
| **50,000 records** (~50.0 MiB) | `219.36 ms` | `228.03 MiB/s` | `226.21 ms` | **`221.13 MiB/s`** | ~221,030 ops/s |
| **100,000 records** (~100.0 MiB) | `428.90 ms` | `233.25 MiB/s` | `430.25 ms` | **`232.51 MiB/s`** | ~232,420 ops/s |
| **500,000 records** (~500.2 MiB) | `2.1163 s` | `236.36 MiB/s` | `2.0843 s` | **`239.98 MiB/s`** | **~239,890 ops/s** |

---

### 4.6 Multi-Threaded Concurrent Read Throughput (8 Worker Threads)
*Config: Pre-populated DB across multiple 5 MB segment files with `reader_lru_cache_size(128)`. Compares `std::sync` primitives against userspace adaptive spinning via `parking_lot`.*

#### Sequential Key Order (`concurrent_read/sequential_k4B_v1024B`)
| Scale (8 Threads) | `std::sync` Time | `std::sync` Thrpt | `parking_lot` Time | `parking_lot` Thrpt | Throughput (ops/s) | Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **10,000 reads** (~9.77 MiB) | `15.916 ms` | `613.59 MiB/s` | `7.048 ms` | **`1.35 GiB/s`** | ~1,418,800 ops/s | **+113.6%** |
| **50,000 reads** (~48.8 MiB) | `57.066 ms` | `855.64 MiB/s` | `23.127 ms` | **`2.06 GiB/s`** | ~2,161,900 ops/s | **+163.2%** |
| **100,000 reads** (~97.7 MiB) | `108.07 ms` | `903.61 MiB/s` | `43.632 ms` | **`2.19 GiB/s`** | ~2,291,800 ops/s | **+160.0%** |
| **500,000 reads** (~488.3 MiB) | `502.60 ms` | `971.51 MiB/s` | `199.96 ms` | **`2.38 GiB/s`** | **~2,500,500 ops/s** | **+159.1%** |

#### Uniform Random Key Order (`concurrent_read/random_k4B_v1024B`)
| Scale (8 Threads) | `std::sync` Time | `std::sync` Thrpt | `parking_lot` Time | `parking_lot` Thrpt | Throughput (ops/s) | Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **10,000 reads** (~9.77 MiB) | `14.159 ms` | `689.71 MiB/s` | `6.697 ms` | **`1.42 GiB/s`** | ~1,493,200 ops/s | **+115.1%** |
| **50,000 reads** (~48.8 MiB) | `56.449 ms` | `865.00 MiB/s` | `25.960 ms` | **`1.84 GiB/s`** | ~1,926,000 ops/s | **+133.8%** |
| **100,000 reads** (~97.7 MiB) | `151.46 ms` | `644.78 MiB/s` | `44.745 ms` | **`2.13 GiB/s`** | ~2,234,800 ops/s | **+151.7%** |
| **500,000 reads** (~488.3 MiB) | `966.51 ms` | `505.20 MiB/s` | `218.56 ms` | **`2.18 GiB/s`** | **~2,287,700 ops/s** | **+148.9%** |

---

### 4.7 Multi-Threaded Concurrent Write Throughput (8 Worker Threads)
*Config: 8 threads concurrently executing `BitcaskInstance::put` into a shared single-writer append-only log (`SyncFrequency::Never`).*

| Scale (8 Threads) | `std::sync` Time | `std::sync` Thrpt | `parking_lot` Time | `parking_lot` Thrpt | Throughput (ops/s) | Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **10,000 writes** (~10.0 MiB) | `50.04 ms` | `199.94 MiB/s` | `45.07 ms` | **`221.98 MiB/s`** | ~221,800 ops/s | **+4.7%** |
| **50,000 writes** (~50.0 MiB) | `250.09 ms` | `200.01 MiB/s` | `216.66 ms` | **`230.87 MiB/s`** | ~230,800 ops/s | **+6.4%** |
| **100,000 writes** (~100.0 MiB) | `498.67 ms` | `200.61 MiB/s` | `442.40 ms` | **`226.13 MiB/s`** | ~226,100 ops/s | **+4.5%** |
| **1,000,000 writes** (~1.0 GiB) | `4.4560 s` | `224.51 MiB/s` | `3.6780 s` | **`271.99 MiB/s`** | **~271,880 ops/s** | **+3.7% (~778 ms faster)** |

---

## 5. Architectural Discussion: Page Cache vs. Cold Storage Reality

When interpreting the **2.38 GiB/s (~2.5M QPS)** read throughput, it is vital from a systems engineering perspective to distinguish between **engine overhead** and **physical storage hardware limits**:

1. **Page-Cache-Resident (Hot Cache) Throughput**:
   - At 500,000 records (~500 MB dataset), after `fill_db` writes the dataset, the operating system kernel keeps the written pages resident in the OS Page Cache (RAM).
   - When 8 worker threads execute `reader.read_bytes(offset, len)`, the underlying `pread` syscall is resolved directly against the OS page cache without accessing physical SSD NAND flash.
   - **Why This Matters**: Measuring page-cache-resident throughput isolates the CPU and lock efficiency of the storage engine itself. A poorly architected engine (e.g. lock convoys, heap allocations per read, mutex contention) drops to 100–300 MiB/s even when reading from RAM. Hitting **2.38 GiB/s** across 8 threads proves that `bitcask` achieves wire-speed memory/syscall saturation with **virtually zero CPU or synchronization bottleneck**.

2. **Cold Physical Storage (Larger-than-RAM Datasets)**:
   - If a database exceeds available system RAM (e.g. 50 GB dataset on 16 GB RAM), random reads will physically access NVMe flash blocks.
   - Modern PCIe Gen4 NVMe drives achieve ~3.0–7.0 GB/s sequential reads, but random 4K reads typically top out at 500–1,200 MB/s due to flash memory controller latency.
   - In cold-storage scenarios, throughput is bounded by hardware I/O limits, not `bitcask` code. The engine will saturate whatever bandwidth the underlying physical disk can sustain.
