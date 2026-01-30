/*
 * AsyncFileNonDurable.actor.h (EXCERPT)
 *
 * This source file is part of the FoundationDB open source project
 * Copyright 2013-2024 Apple Inc. and the FoundationDB project authors
 * Apache License, Version 2.0
 *
 * NOTE: This is an excerpt of the original file focusing on key sections.
 * Full file: fdbrpc/include/fdbrpc/AsyncFileNonDurable.actor.h
 */

// ============================================================================
// AsyncFileDetachable - Wrapper for handling file shutdown during kill
// Lines 67-102 in original
// ============================================================================

class AsyncFileDetachable final : public IAsyncFile, public ReferenceCounted<AsyncFileDetachable> {
private:
	Reference<IAsyncFile> file;
	Future<Void> shutdown;
	bool assertOnReadWriteCancel;

public:
	virtual StringRef getClassName() override { return "AsyncFileDetachable"_sr; }

	explicit AsyncFileDetachable(Reference<IAsyncFile> file) : file(file), assertOnReadWriteCancel(true) {
		shutdown = doShutdown(this);
	}

	ACTOR Future<Void> doShutdown(AsyncFileDetachable* self);
	ACTOR static Future<Reference<IAsyncFile>> open(Future<Reference<IAsyncFile>> wrappedFile);

	// ... standard IAsyncFile interface methods that delegate to wrapped file
};

// ============================================================================
// AsyncFileNonDurable - Power failure and crash simulation
// Lines 104-280 in original
// ============================================================================

// An async file implementation which wraps another async file and will randomly destroy sectors that it is writing when
// killed. This is used to simulate a power failure which prevents all written data from being persisted to disk
class AsyncFileNonDurable final : public IAsyncFile, public ReferenceCounted<AsyncFileNonDurable> {
public:
	virtual StringRef getClassName() override { return "AsyncFileNonDurable"_sr; }

	UID id;
	std::string filename;
	std::string initialFilename;  // For atomic write and create
	mutable int64_t approximateSize;
	NetworkAddress openedAddress;
	bool aio;

private:
	Reference<IAsyncFile> file;
	double maxWriteDelay;

	// Modifications which haven't been pushed to file, mapped by location
	RangeMap<uint64_t, Future<Void>> pendingModifications;
	mutable int64_t minSizeAfterPendingModifications = 0;
	mutable bool minSizeAfterPendingModificationsIsExact = false;

	Promise<Void> killed;
	Promise<Void> killComplete;
	Promise<bool> startSyncPromise;

	Reference<DiskParameters> diskParameters;
	bool hasBeenSynced;

	// ========================================================================
	// KillMode - Defines corruption behavior during simulated power failure
	// ========================================================================
	enum KillMode { NO_CORRUPTION = 0, DROP_ONLY = 1, FULL_CORRUPTION = 2 };

	KillMode killMode;  // Randomly chosen at file open (1-2)
	ActorCollection responses;

	// Constructor - randomly selects killMode for this file
	AsyncFileNonDurable(...) {
		// ...
		killMode = (KillMode)deterministicRandom()->randomInt(1, 3);
		// ...
	}

public:
	// Forces a non-durable sync (some writes are not made or made incorrectly)
	// This is used when the file should 'die' without first completing its operations
	// (e.g. to simulate power failure)
	Future<Void> kill() {
		TraceEvent("AsyncFileNonDurable_Kill", id).detail("Filename", filename);
		CODE_PROBE(true, "AsyncFileNonDurable was killed", probe::decoration::rare);
		return sync(this, false);
	}
};

// ============================================================================
// Key ACTOR: write() - Simulates non-durable writes with corruption
// Lines 353-526 in original
// ============================================================================

ACTOR Future<Void> write(AsyncFileNonDurable* self,
                         Promise<Void> writeStarted,
                         Future<Future<Void>> ownFuture,
                         void const* data,
                         int length,
                         int64_t offset) {
	// ... setup and delay ...

	// In AIO mode, only page-aligned writes are supported
	ASSERT(!self->aio || (offset % 4096 == 0 && length % 4096 == 0));

	// Non-durable writes should introduce errors at the page level and corrupt at the sector level
	// Otherwise, we can perform the entire write at once
	int diskPageLength = saveDurable ? length : 4096;
	int diskSectorLength = saveDurable ? length : 512;

	std::vector<Future<Void>> writeFutures;
	for (int writeOffset = 0; writeOffset < length;) {
		// choose a random action to perform on this page write (write correctly, corrupt, or don't write)
		KillMode pageKillMode = (KillMode)deterministicRandom()->randomInt(0, self->killMode + 1);

		for (int pageOffset = 0; pageOffset < pageLength;) {
			// If saving durable, then perform the write correctly.
			// If corrupting the write, then this sector will be written correctly with a 1/4 chance
			if (saveDurable || pageKillMode == NO_CORRUPTION ||
			    (pageKillMode == FULL_CORRUPTION && deterministicRandom()->random01() < 0.25)) {
				// Write correctly
				writeFutures.push_back(self->file->write(...));
			}
			// Corrupt: 1/4 chance sector not written, otherwise write garbage
			else if (pageKillMode == FULL_CORRUPTION && deterministicRandom()->random01() < 0.66667) {
				// The incorrect part of the write can be the rightmost bytes (side = 0),
				// the leftmost bytes (side = 1), or the entire write (side = 2)
				int side = deterministicRandom()->randomInt(0, 3);

				// There is a 1/2 chance that a bad write will have garbage written
				// The chance is increased to 1 if the entire write is bad
				bool garbage = side == 2 || deterministicRandom()->random01() < 0.5;

				// ... calculate goodStart, goodEnd, badStart, badEnd ...

				// Write randomly generated bytes, if required
				if (garbage && badStart != badEnd) {
					for (int i = 0; i < badEnd - badStart; i += sizeof(uint32_t)) {
						uint32_t val = deterministicRandom()->randomUInt32();
						memcpy(&badData[i], &val, ...);
					}
					writeFutures.push_back(self->file->write(corruptedData, ...));
				}
				CODE_PROBE(true, "AsyncFileNonDurable bad write", probe::decoration::rare);
			} else {
				// Drop the write entirely
				CODE_PROBE(true, "AsyncFileNonDurable dropped write", probe::decoration::rare);
			}

			pageOffset += sectorLength;
		}
		writeOffset += pageLength;
	}
	wait(waitForAll(writeFutures));
	return Void();
}

// ============================================================================
// Key ACTOR: onSync() - Sync or kill with durability control
// Lines 605-677 in original
// ============================================================================

ACTOR Future<Void> onSync(AsyncFileNonDurable* self, bool durable) {
	ASSERT(durable || !self->killed.isSet()); // this file is kill()ed only once

	if (durable) {
		self->hasBeenSynced = true;
		wait(waitUntilDiskReady(self->diskParameters, 0, true) || self->killed.getFuture());
	}

	wait(checkKilled(self, durable ? "Sync" : "Kill"));

	if (!durable)
		self->killed.send(Void());

	// Get all outstanding modifications
	// ...

	// Signal all modifications to end their delay
	Promise<bool> startSyncPromise = self->startSyncPromise;
	self->startSyncPromise = Promise<bool>();

	// Writes will be durable in a kill with a 10% probability
	state bool writeDurable = durable || deterministicRandom()->random01() < 0.1;
	startSyncPromise.send(writeDurable);

	// Wait for outstanding writes to complete
	if (durable)
		wait(allModifications);
	else
		wait(success(errorOr(allModifications)));

	if (!durable) {
		// Sometimes sync the file if writes were made durably
		if (self->hasBeenSynced && writeDurable && deterministicRandom()->random01() < 0.5) {
			CODE_PROBE(true, "AsyncFileNonDurable kill was durable and synced", probe::decoration::rare);
			wait(success(errorOr(self->file->sync())));
		}
		self->killComplete.send(Void());
	} else {
		wait(checkKilled(self, "SyncEnd"));
		wait(self->file->sync());
	}

	return Void();
}
