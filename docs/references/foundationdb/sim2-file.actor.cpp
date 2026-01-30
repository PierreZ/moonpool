/*
 * sim2-file.actor.cpp (EXCERPT)
 *
 * Excerpts from fdbrpc/sim2.actor.cpp focusing on file simulation
 * Copyright 2013-2024 Apple Inc. and the FoundationDB project authors
 * Apache License, Version 2.0
 *
 * Full file: fdbrpc/sim2.actor.cpp
 */

// ============================================================================
// SimpleFile - Base simulated file with disk timing
// Lines 659-1001 in original
// ============================================================================

class SimpleFile : public IAsyncFile, public ReferenceCounted<SimpleFile> {
public:
	static void init() {}

	virtual StringRef getClassName() override { return "SimpleFile"_sr; }

	static bool should_poll() { return false; }

	ACTOR static Future<Reference<IAsyncFile>> open(
	    std::string filename,
	    int flags,
	    int mode,
	    Reference<DiskParameters> diskParameters = makeReference<DiskParameters>(25000, 150000000),
	    bool delayOnWrite = true) {
		state ISimulator::ProcessInfo* currentProcess = g_simulator->getCurrentProcess();
		state TaskPriority currentTaskID = g_network->getCurrentTask();

		if (++openCount >= 6000) {
			TraceEvent(SevError, "TooManyFiles").log();
			ASSERT(false);
		}

		// Filesystems on average these days seem to start to have limits of around 255 characters
		ASSERT(basename(filename).size() < 250);

		wait(g_simulator->onMachine(currentProcess));
		try {
			wait(delay(FLOW_KNOBS->MIN_OPEN_TIME +
			           deterministicRandom()->random01() * (FLOW_KNOBS->MAX_OPEN_TIME - FLOW_KNOBS->MIN_OPEN_TIME)));

			std::string open_filename = filename;
			if (flags & OPEN_ATOMIC_WRITE_AND_CREATE) {
				ASSERT((flags & OPEN_CREATE) && (flags & OPEN_READWRITE) && !(flags & OPEN_EXCLUSIVE));
				open_filename = filename + ".part";
			}

			int h = sf_open(open_filename.c_str(), flags, flagConversion(flags), mode);
			if (h == -1) {
				bool notFound = errno == ENOENT;
				Error e = notFound ? file_not_found() : io_error();
				throw e;
			}

			platform::makeTemporary(open_filename.c_str());
			SimpleFile* simpleFile = new SimpleFile(h, diskParameters, delayOnWrite, filename, open_filename, flags);
			state Reference<IAsyncFile> file = Reference<IAsyncFile>(simpleFile);
			wait(g_simulator->onProcess(currentProcess, currentTaskID));
			return file;
		} catch (Error& e) {
			state Error err = e;
			wait(g_simulator->onProcess(currentProcess, currentTaskID));
			throw err;
		}
	}

private:
	int h;
	Reference<DiskParameters> diskParameters;
	std::string filename, actualFilename;
	int flags;
	UID dbgId;
	bool delayOnWrite;  // If false, writes/truncates skip delay (for AsyncFileNonDurable)

	// ========================================================================
	// read_impl - Simulates read with disk timing
	// ========================================================================
	ACTOR static Future<int> read_impl(SimpleFile* self, void* data, int length, int64_t offset) {
		ASSERT((self->flags & IAsyncFile::OPEN_NO_AIO) != 0 ||
		       ((uintptr_t)data % 4096 == 0 && length % 4096 == 0 && offset % 4096 == 0)); // Required by KAIO

		wait(waitUntilDiskReady(self->diskParameters, length));

		if (_lseeki64(self->h, offset, SEEK_SET) == -1) {
			throw io_error();
		}

		unsigned int read_bytes = _read(self->h, data, (unsigned int)length);
		if (read_bytes == -1) {
			throw io_error();
		}

		INJECT_FAULT(io_timeout, "SimpleFile::read");
		INJECT_FAULT(io_error, "SimpleFile::read");

		return read_bytes;
	}

	// ========================================================================
	// write_impl - Simulates write with disk timing
	// ========================================================================
	ACTOR static Future<Void> write_impl(SimpleFile* self, StringRef data, int64_t offset) {
		if (self->delayOnWrite)
			wait(waitUntilDiskReady(self->diskParameters, data.size()));

		if (_lseeki64(self->h, offset, SEEK_SET) == -1) {
			throw io_error();
		}

		unsigned int write_bytes = _write(self->h, (void*)data.begin(), data.size());
		if (write_bytes == -1 || write_bytes != data.size()) {
			throw io_error();
		}

		INJECT_FAULT(io_timeout, "SimpleFile::write");
		INJECT_FAULT(io_error, "SimpleFile::write");

		return Void();
	}

	// ========================================================================
	// sync_impl - Handles atomic rename on first sync
	// ========================================================================
	ACTOR static Future<Void> sync_impl(SimpleFile* self) {
		if (self->delayOnWrite)
			wait(waitUntilDiskReady(self->diskParameters, 0, true));

		if (self->flags & OPEN_ATOMIC_WRITE_AND_CREATE) {
			self->flags &= ~OPEN_ATOMIC_WRITE_AND_CREATE;
			auto& machineCache = g_simulator->getCurrentProcess()->machine->openFiles;
			std::string sourceFilename = self->filename + ".part";

			if (machineCache.count(sourceFilename)) {
				// Handle corrupted blocks tracking during rename
				// ...
				renameFile(sourceFilename.c_str(), self->filename.c_str());

				machineCache[self->filename] = machineCache[sourceFilename];
				machineCache.erase(sourceFilename);
				self->actualFilename = self->filename;
			}
		}

		INJECT_FAULT(io_timeout, "SimpleFile::sync");
		INJECT_FAULT(io_error, "SimpleFile::sync");

		return Void();
	}
};

// ============================================================================
// DiskParameters - Simulated disk performance characteristics
// From fdbrpc/include/fdbrpc/simulator.h:459-468
// ============================================================================

struct DiskParameters : ReferenceCounted<DiskParameters> {
	double nextOperation;
	int64_t iops;       // Default: 25,000 IOPS
	int64_t bandwidth;  // Default: 150 MB/s

	DiskParameters(int64_t iops, int64_t bandwidth) : nextOperation(0), iops(iops), bandwidth(bandwidth) {}
};

// ============================================================================
// waitUntilDiskReady - Simulates disk I/O timing
// Lines 3000-3018 in original
// ============================================================================

Future<Void> waitUntilDiskReady(Reference<DiskParameters> diskParameters, int64_t size, bool sync) {
	if (g_simulator->getCurrentProcess()->failedDisk) {
		return Never();  // Simulated disk failure - operation never completes
	}
	if (g_simulator->connectionFailuresDisableDuration > 1e4)
		return delay(0.0001);  // Speed up simulation mode

	if (diskParameters->nextOperation < now())
		diskParameters->nextOperation = now();

	// Delay calculation: (1/iops) + (size/bandwidth)
	diskParameters->nextOperation += (1.0 / diskParameters->iops) + (size / diskParameters->bandwidth);

	double randomLatency;
	if (sync) {
		// Sync operations have higher latency
		randomLatency = .005 + deterministicRandom()->random01() * (BUGGIFY ? 1.0 : .010);
	} else
		randomLatency = 10 * deterministicRandom()->random01() / diskParameters->iops;

	return delayUntil(diskParameters->nextOperation + randomLatency);
}

// ============================================================================
// Sim2FileSystem - File system wrapper with decorator stack
// Lines 3087-3142 in original
// ============================================================================

Future<Reference<class IAsyncFile>> Sim2FileSystem::open(const std::string& filename, int64_t flags, int64_t mode) {
	ASSERT((flags & IAsyncFile::OPEN_ATOMIC_WRITE_AND_CREATE) || !(flags & IAsyncFile::OPEN_CREATE) ||
	       StringRef(filename).endsWith(".fdb-lock"_sr));

	if ((flags & IAsyncFile::OPEN_EXCLUSIVE))
		ASSERT(flags & IAsyncFile::OPEN_CREATE);

	if (flags & IAsyncFile::OPEN_UNCACHED) {
		auto& machineCache = g_simulator->getCurrentProcess()->machine->openFiles;
		std::string actualFilename = filename;
		if (flags & IAsyncFile::OPEN_ATOMIC_WRITE_AND_CREATE) {
			actualFilename = filename + ".part";
			auto partFile = machineCache.find(actualFilename);
			if (partFile != machineCache.end()) {
				Future<Reference<IAsyncFile>> f = AsyncFileDetachable::open(partFile->second.get());
				return f;
			}
		}

		Future<Reference<IAsyncFile>> f;
		auto itr = machineCache.find(actualFilename);
		if (itr == machineCache.end()) {
			// Create new file with shared disk parameters
			auto diskParameters =
			    makeReference<DiskParameters>(FLOW_KNOBS->SIM_DISK_IOPS, FLOW_KNOBS->SIM_DISK_BANDWIDTH);

			// ================================================================
			// DECORATOR STACK CONSTRUCTION (bottom to top):
			// ================================================================

			// 1. SimpleFile - Base disk I/O with timing simulation
			f = SimpleFile::open(filename, flags, mode, diskParameters, false);

			// 2. AsyncFileWriteChecker - Optional checksum verification
			if (FLOW_KNOBS->PAGE_WRITE_CHECKSUM_HISTORY > 0) {
				f = map(f,
				        [=](Reference<IAsyncFile> r) { return Reference<IAsyncFile>(new AsyncFileWriteChecker(r)); });
			}

			// 3. AsyncFileNonDurable - Power failure simulation
			f = AsyncFileNonDurable::open(
			    filename, actualFilename, f, diskParameters, (flags & IAsyncFile::OPEN_NO_AIO) == 0);

			machineCache[actualFilename] = UnsafeWeakFutureReference<IAsyncFile>(f);
		} else {
			f = itr->second.get();
		}

		// 4. AsyncFileDetachable - Shutdown handling
		f = AsyncFileDetachable::open(f);

		// 5. AsyncFileChaos - Bit flips and disk delays (when chaos enabled)
		if (FLOW_KNOBS->ENABLE_CHAOS_FEATURES)
			f = map(f, [=](Reference<IAsyncFile> r) { return Reference<IAsyncFile>(new AsyncFileChaos(r)); });

		// 6. AsyncFileEncrypted - Optional encryption layer
		if (flags & IAsyncFile::OPEN_ENCRYPTED)
			f = map(f, [flags](Reference<IAsyncFile> r) {
				auto mode = flags & IAsyncFile::OPEN_READWRITE ? AsyncFileEncrypted::Mode::APPEND_ONLY
				                                               : AsyncFileEncrypted::Mode::READ_ONLY;
				return Reference<IAsyncFile>(new AsyncFileEncrypted(r, mode));
			});
		return f;
	} else
		return AsyncFileCached::open(filename, flags, mode);
}
