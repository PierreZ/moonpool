# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-10-10

### 🚀 Features

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

- **moonpool-journal**: Write metainfo only after its batch is durable
- **moonpool-journal**: Sync what recovery keeps before reporting it
- **moonpool-journal**: A durable start before prefix unlinks; no gap is read as the end
- **moonpool-journal**: Reserved slot records, and a torn identifier in the last batch ends the log
- **moonpool-journal**: Start every append batch on a fresh block
- **moonpool-journal**: Spend the metadata generation before writing a copy
- **moonpool-journal**: Make every ancestor of a journal directory durable
- **moonpool-journal**: Report the whole torn last batch as ambiguous

### 📚 Documentation

- **moonpool-journal**: Add a real-filesystem accept_log example
- **moonpool-journal**: Show segment names with their real 20-digit width

### 🚜 Refactor

- **moonpool-journal**: Name the map JournalAtlas, built on LayoutRegion
- **moonpool-journal**: Build empty segments in one place
- **moonpool-journal**: Poison through one helper
- **moonpool-journal**: Follow CLSTORE as the paper specifies it

### 🧪 Testing

- **moonpool-journal**: One crash-loop test per fault model and durability
- **moonpool-journal**: Aim the crash physics with the atlas

### ⚙️ Miscellaneous Tasks

- Bump Rust to 1.99 and update the Nix flake

