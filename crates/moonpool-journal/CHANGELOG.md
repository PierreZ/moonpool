# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-11-24

### 🚀 Features

- **moonpool-journal**: Format 5, split entry CRC, metainfo stamps, header copy B at the end ([#566](https://github.com/PierreZ/moonpool/pull/566))
- **moonpool-journal**: A budgeted full scrub, and spares prepared ahead of the rolls ([#565](https://github.com/PierreZ/moonpool/pull/565))
- **site**: Technique pages, playground, extras and the finished site ([#425](https://github.com/PierreZ/moonpool/pull/425))
- **moonpool-sim**: A seed corpus and a wall-clock budget for the sweep, layouts updated in place ([#408](https://github.com/PierreZ/moonpool/pull/408))
- **moonpool-journal, moonpool-sim**: Whole-copy layouts and replicas lost past the gate ([#375](https://github.com/PierreZ/moonpool/pull/375))
- **moonpool-sim**: Shard replicas named by host, and store layouts from the provider ([#372](https://github.com/PierreZ/moonpool/pull/372))
- **moonpool-sim**: Shards, the code declares its replication ([#369](https://github.com/PierreZ/moonpool/pull/369))
- **moonpool-sim**: Disk capacity, ENOSPC and free bytes ([#343](https://github.com/PierreZ/moonpool/pull/343))
- **moonpool-journal**: Add a duration-based commit benchmark ([#333](https://github.com/PierreZ/moonpool/pull/333))
- **moonpool-journal**: Hint the moments inside a commit; HintVeto for a harness budget
- **moonpool-journal**: Batch::fits and fits_another bound a commit by one segment
- **moonpool-journal**: Journal::peek_meta reads a closed journal's metainfo
- **moonpool-journal**: Rewrite as a position-keyed CLSTORE journal
- **moonpool-journal**: Checkpoint batches, a batch that supersedes the prefix before it
- **moonpool-journal**: Journal::peek_meta, a read-only look at the metadata
- **moonpool-sim**: Replicated fault patterns, minority and helical
- **moonpool-core**: LayoutRegion, a format-neutral map for aimed faults
- **moonpool-journal**: An atlas of the on-disk layout, for aimed faults
- **moonpool-journal**: Caller tags, kept ambiguous tail, batched replay, metadata repair
- **moonpool-sim-examples**: Sim-journal, a journal under crash attrition
- **moonpool-journal**: CLSTORE-style write-ahead journal over BlockFile

### 🐛 Bug Fixes

- **moonpool-journal**: A dropped commit poisons the journal ([#575](https://github.com/PierreZ/moonpool/pull/575))
- Bound reliable rpc calls from the network ([#572](https://github.com/PierreZ/moonpool/pull/572)); keep a crash-torn Batched entry ambiguous ([#573](https://github.com/PierreZ/moonpool/pull/573)) ([#574](https://github.com/PierreZ/moonpool/pull/574))
- **moonpool-journal**: A retired slot stays gone; judge the journal example's damage on what was in flight ([#571](https://github.com/PierreZ/moonpool/pull/571))
- **moonpool-journal**: Never lose an acknowledged entry to a torn rewrite or a lost batch ([#568](https://github.com/PierreZ/moonpool/pull/568))
- **moonpool-journal**: Read what the device holds at open (fsyncgate) ([#564](https://github.com/PierreZ/moonpool/pull/564))
- **moonpool-journal**: A retired first batch of a segment stays in its segment ([#537](https://github.com/PierreZ/moonpool/pull/537))
- **moonpool-journal**: One scrub read for each persist block, batch number limits, other format versions ([#526](https://github.com/PierreZ/moonpool/pull/526))
- **moonpool-journal**: Open scan fixes for #485, #484 and #487 ([#522](https://github.com/PierreZ/moonpool/pull/522))
- **moonpool-journal**: A retire record replaces the void record, format version 4 ([#527](https://github.com/PierreZ/moonpool/pull/527))
- **moonpool-journal**: Repair a damaged persist record in place ([#516](https://github.com/PierreZ/moonpool/pull/516))
- **moonpool-journal**: Create and peek_meta refuse segments with no meta file ([#502](https://github.com/PierreZ/moonpool/pull/502))
- **moonpool-journal**: Refuse two faults on a witnessed batch and a rolled-back metainfo ([#421](https://github.com/PierreZ/moonpool/pull/421))
- **moonpool-rpc**: Keep replies of a replaced connection, and stop false damage verdicts in the journal ([#400](https://github.com/PierreZ/moonpool/pull/400))
- **moonpool-journal**: A Batched commit rewrites its batch before a retried sync ([#362](https://github.com/PierreZ/moonpool/pull/362))
- **moonpool-journal**: Never keep a spare that is a second name of a live segment ([#353](https://github.com/PierreZ/moonpool/pull/353))
- **moonpool-journal**: Write metainfo only after its batch is durable
- **moonpool-journal**: Sync what recovery keeps before reporting it
- **moonpool-journal**: A durable start before prefix unlinks; no gap is read as the end
- **moonpool-journal**: Reserved slot records, and a torn identifier in the last batch ends the log
- **moonpool-journal**: Start every append batch on a fresh block
- **moonpool-journal**: Spend the metadata generation before writing a copy
- **moonpool-journal**: Make every ancestor of a journal directory durable
- **moonpool-journal**: Report the whole torn last batch as ambiguous

### 📚 Documentation

- Rewrite the README, the book and the crate docs for every feature
- **moonpool-journal**: Add a real-filesystem accept_log example
- **moonpool-journal**: Show segment names with their real 20-digit width

### ⚡ Performance

- **moonpool-journal**: Open reads a sealed segment only up to its last batch ([#567](https://github.com/PierreZ/moonpool/pull/567))
- **moonpool-journal**: Recycle dead segments, never zero-fill one ([#352](https://github.com/PierreZ/moonpool/pull/352))
- **moonpool-journal**: Benchmark the journal on Tokio and compio ([#350](https://github.com/PierreZ/moonpool/pull/350))
- **moonpool-core**: Owned aligned transfers, no bounce buffer on O_DIRECT ([#341](https://github.com/PierreZ/moonpool/pull/341))
- **moonpool-journal**: One durable write per ordered-commit step (RWF_DSYNC) ([#339](https://github.com/PierreZ/moonpool/pull/339))
- **moonpool-journal**: Roll onto a prepared spare segment off the commit path ([#338](https://github.com/PierreZ/moonpool/pull/338))
- **moonpool-journal**: Balance the default geometry, keep a journal's own shape ([#335](https://github.com/PierreZ/moonpool/pull/335)) ([#336](https://github.com/PierreZ/moonpool/pull/336))
- **moonpool-journal**: Replay reads runs of entries at once ([#334](https://github.com/PierreZ/moonpool/pull/334))

### 🚜 Refactor

- **moonpool-core**: Files are positioned only, with one FileCursor
- **moonpool-core**: One feature per backend
- Remove the Orleans analysis, unused public items and six numeric assertion macros
- **moonpool-journal**: Run the journal on the local traits, generic over its spawner ([#349](https://github.com/PierreZ/moonpool/pull/349))
- **moonpool-journal**: Refuse a geometry other than the journal's own again ([#337](https://github.com/PierreZ/moonpool/pull/337))
- **moonpool-journal**: Name the map JournalAtlas, built on LayoutRegion
- **moonpool-journal**: Build empty segments in one place
- **moonpool-journal**: Poison through one helper
- **moonpool-journal**: Follow CLSTORE as the paper specifies it

### 🧪 Testing

- **moonpool-sim-examples**: Make the journal simulation able to see fsyncgate ([#569](https://github.com/PierreZ/moonpool/pull/569))
- **moonpool-journal**: Run the Batched full-physics crash loop over 480 seeds ([#531](https://github.com/PierreZ/moonpool/pull/531))
- **moonpool-journal**: One crash-loop test per fault model and durability
- **moonpool-journal**: Aim the crash physics with the atlas

### ⚙️ Miscellaneous Tasks

- Bump Rust to 1.99 and update the Nix flake

### 📦 Other

- Bump flake and Rust 1.100.0; pass clippy pedantic with no allows ([#557](https://github.com/PierreZ/moonpool/pull/557))
- Test-only moonpool dependencies carry no version

