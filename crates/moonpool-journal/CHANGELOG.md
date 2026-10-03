# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-10-03

### 🚀 Features

- **moonpool-sim**: Replicated fault patterns, minority and helical
- **moonpool-core**: LayoutRegion, a format-neutral map for aimed faults
- **moonpool-journal**: An atlas of the on-disk layout, for aimed faults
- **moonpool-journal**: Caller tags, kept ambiguous tail, batched replay, metadata repair
- **moonpool-sim-examples**: Sim-journal, a journal under crash attrition
- **moonpool-journal**: CLSTORE-style write-ahead journal over BlockFile

### 🐛 Bug Fixes

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

- **moonpool-journal**: Aim the crash physics with the atlas

