# Orleans State Persistence Analysis

**Purpose**: Durable state management for virtual actors with optimistic concurrency and migration support

**Key insight**: Orleans provides IPersistentState<T> as a lifecycle-integrated abstraction over storage providers, using ETags for optimistic concurrency control and participating in grain migration to transfer state seamlessly between silos. The design separates storage interface (IGrainStorage) from grain-facing API (IPersistentState<T>), enabling pluggable backends while maintaining a consistent programming model.

---

## Overview

State persistence solves the problem: **How do virtual actors durably store state with minimal boilerplate, automatic loading, optimistic concurrency, and migration support?**

### Key Responsibilities

1. **State Lifecycle** - Automatic loading on activation, dehydration on migration
2. **Storage Abstraction** - Pluggable storage providers (Azure, SQL, In-Memory, etc.)
3. **Optimistic Concurrency** - ETag-based conflict detection
4. **Read/Write/Clear** - Simple CRUD operations
5. **Migration Integration** - Dehydrate/Rehydrate for activation handoff
6. **Lifecycle Integration** - Load state during `SetupState` stage
7. **Error Handling** - InconsistentStateException triggers deactivation

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│ Grain Code                                               │
│   [PersistentState("myState", "myStorage")]             │
│   public IPersistentState<MyState> State { get; set; }  │
│                                                          │
│   await State.WriteStateAsync();                        │
│   await State.ReadStateAsync();                         │
│   await State.ClearStateAsync();                        │
├──────────────────────────────────────────────────────────┤
│ IPersistentState<T> (Interface)                         │
│   - State: T                                             │
│   - Etag: string                                         │
│   - RecordExists: bool                                   │
│   - Read/Write/Clear async methods                      │
├──────────────────────────────────────────────────────────┤
│ PersistentState<T> (Lifecycle Participant)              │
│   - OnStart() → ReadStateAsync()                        │
│   - OnDehydrate() → Save to MigrationContext            │
│   - OnRehydrate() → Load from MigrationContext          │
├──────────────────────────────────────────────────────────┤
│ StateStorageBridge<T> (Storage Bridge)                  │
│   - Read/Write/Clear with error handling                │
│   - Instrumentation (timing, errors, type names)        │
│   - Runtime context checking                            │
├──────────────────────────────────────────────────────────┤
│ IGrainStorage (Provider Interface)                      │
│   - ReadStateAsync()                                     │
│   - WriteStateAsync()                                    │
│   - ClearStateAsync()                                    │
├──────────────────────────────────────────────────────────┤
│ Storage Provider Implementation                         │
│   - Azure Table Storage                                 │
│   - SQL Database                                        │
│   - In-Memory                                           │
│   - Custom implementations                              │
└──────────────────────────────────────────────────────────┘
```

---

## Core Components

### 1. IPersistentState<TState> (Interface)

**File**: `IPersistentState.cs:10-12`

The grain-facing interface for persistent state.

```csharp
public interface IPersistentState<TState> : IStorage<TState>
{
}
```

**Inherits from IStorage<TState>**:

**File**: `IStorage.cs:9-106` (`IStorage` at lines 9-94, `IStorage<TState>` at lines 99-105)

```csharp
public interface IStorage
{
    string? Etag { get; }
    bool RecordExists { get; }

    Task ClearStateAsync();
    Task WriteStateAsync();
    Task ReadStateAsync();

    // CancellationToken overloads with default interface implementations
    Task ClearStateAsync(CancellationToken cancellationToken) => ClearStateAsync();
    Task WriteStateAsync(CancellationToken cancellationToken) => WriteStateAsync();
    Task ReadStateAsync(CancellationToken cancellationToken) => ReadStateAsync();
}

public interface IStorage<TState> : IStorage
{
    TState State { get; set; }
}
```

**Properties**:
- **State**: The actual state object (e.g., `UserProfile`, `ShoppingCart`)
- **Etag**: Optimistic concurrency token from storage (nullable)
- **RecordExists**: Whether state exists in storage

**Operations**:
- **ReadStateAsync()**: Load state from storage (overwrites current state)
- **WriteStateAsync()**: Save state to storage (fails if ETag mismatch)
- **ClearStateAsync()**: Delete state from storage

---

### 2. PersistentState<TState> (Lifecycle Integration)

**File**: `PersistentStateStorageFactory.cs:57-84`

Implementation of `IPersistentState<T>` that integrates with grain lifecycle and migration.

```csharp
internal sealed class PersistentState<TState> : StateStorageBridge<TState>, IPersistentState<TState>, ILifecycleObserver
{
    public PersistentState(string stateName, IGrainContext context, IGrainStorage storageProvider) : base(stateName, context, storageProvider)
    {
        var lifecycle = context.ObservableLifecycle;
        lifecycle.Subscribe(RuntimeTypeNameFormatter.Format(GetType()), GrainLifecycleStage.SetupState, this);
        lifecycle.AddMigrationParticipant(this);
    }

    public Task OnStart(CancellationToken cancellationToken = default)
    {
        if (cancellationToken.IsCancellationRequested)
        {
            return Task.CompletedTask;
        }

        // No need to load state if it has been loaded already via rehydration.
        if (IsStateInitialized)
        {
            return Task.CompletedTask;
        }

        return ReadStateAsync();
    }

    public Task OnStop(CancellationToken cancellationToken = default) => Task.CompletedTask;
}
```

**Lifecycle Integration**:
1. **SetupState Stage**: Automatically loads state before grain's `OnActivateAsync()`
2. **Migration Participant**: Implements `OnDehydrate()` and `OnRehydrate()`

**Key Insight**: State loading happens **automatically** during activation. Grains don't need to explicitly call `ReadStateAsync()` in `OnActivateAsync()`.

---

### 3. StateStorageBridge<TState> (Storage Bridge)

**File**: `StateStorageBridge.cs:22-202`

Bridges between the grain-facing interface and the storage provider, adding error handling, instrumentation, and migration support.

```csharp
public partial class StateStorageBridge<TState> : IStorage<TState>, IGrainMigrationParticipant
{
    private readonly IGrainContext _grainContext;
    private readonly StateStorageBridgeShared<TState> _shared;
    private GrainState<TState>? _grainState;

    public StateStorageBridge(string name, IGrainContext grainContext, IGrainStorage store)
    {
        ArgumentNullException.ThrowIfNull(name);
        ArgumentNullException.ThrowIfNull(grainContext);
        ArgumentNullException.ThrowIfNull(store);

        _grainContext = grainContext;

        // Get or create shared instance (Flyweight pattern)
        var sharedInstances = ActivatorUtilities.GetServiceOrCreateInstance<StateStorageBridgeSharedMap>(grainContext.ActivationServices);
        _shared = sharedInstances.Get<TState>(name, store);
    }

    // State property (lazy initialization)
    public TState State
    {
        get
        {
            GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);
            if (_grainState is { } grainState)
            {
                return grainState.State;
            }

            return default!;
        }

        set
        {
            GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);
            GrainState.State = value;
        }
    }

    private GrainState<TState> GrainState => _grainState ??= new GrainState<TState>(_shared.Activator.Create());
    internal bool IsStateInitialized { get; private set; }

    internal string Name => _shared.Name;

    // ETag property
    public string? Etag { get => _grainState?.ETag; set => GrainState.ETag = value; }

    // RecordExists property
    public bool RecordExists => IsStateInitialized switch
    {
        true => GrainState.RecordExists,
        _ => throw new InvalidOperationException("State has not yet been loaded")
    };

    // Read state from storage
    public async Task ReadStateAsync()
    {
        try
        {
            GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);

            var sw = ValueStopwatch.StartNew();
            await _shared.Store.ReadStateAsync(_shared.Name, _grainContext.GrainId, GrainState);
            IsStateInitialized = true;
            StorageInstruments.OnStorageRead(sw.Elapsed, _shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
        }
        catch (Exception exc)
        {
            StorageInstruments.OnStorageReadError(_shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
            OnError(exc, ErrorCode.StorageProvider_ReadFailed, nameof(ReadStateAsync));
        }
    }

    // Write state to storage
    public async Task WriteStateAsync()
    {
        try
        {
            GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);

            var sw = ValueStopwatch.StartNew();
            await _shared.Store.WriteStateAsync(_shared.Name, _grainContext.GrainId, GrainState);
            StorageInstruments.OnStorageWrite(sw.Elapsed, _shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
        }
        catch (Exception exc)
        {
            StorageInstruments.OnStorageWriteError(_shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
            OnError(exc, ErrorCode.StorageProvider_WriteFailed, nameof(WriteStateAsync));
        }
    }

    // Clear state from storage
    public async Task ClearStateAsync()
    {
        try
        {
            GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);

            var sw = ValueStopwatch.StartNew();

            // Clear state in external storage
            await _shared.Store.ClearStateAsync(_shared.Name, _grainContext.GrainId, GrainState);
            sw.Stop();

            // Update counters
            StorageInstruments.OnStorageDelete(sw.Elapsed, _shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
        }
        catch (Exception exc)
        {
            StorageInstruments.OnStorageDeleteError(_shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
            OnError(exc, ErrorCode.StorageProvider_DeleteFailed, nameof(ClearStateAsync));
        }
    }

    // Migration support: Dehydrate (save state to migration context)
    public void OnDehydrate(IDehydrationContext dehydrationContext)
    {
        try
        {
            dehydrationContext.TryAddValue(_shared.MigrationContextKey, _grainState);
        }
        catch (Exception exception)
        {
            LogErrorOnDehydrate(_shared.Logger, exception, _shared.Name, _grainContext.GrainId);
            throw;
        }
    }

    // Migration support: Rehydrate (load state from migration context)
    public void OnRehydrate(IRehydrationContext rehydrationContext)
    {
        try
        {
            if (rehydrationContext.TryGetValue<GrainState<TState>>(_shared.MigrationContextKey, out var grainState))
            {
                _grainState = grainState;
                IsStateInitialized = true;
            }
        }
        catch (Exception exception)
        {
            LogErrorOnRehydrate(_shared.Logger, exception, _shared.Name, _grainContext.GrainId);
        }
    }

    [DoesNotReturn]
    private void OnError(Exception exception, ErrorCode id, string operation)
    {
        string? errorCode = null;
        (_shared.Store as IRestExceptionDecoder)?.DecodeException(exception, out _, out errorCode, true);
        var errorString = errorCode is { Length: > 0 } ? $" Error: {errorCode}" : null;

        var grainId = _grainContext.GrainId;

        _shared.Logger.LogError((int)id, exception,
            "Error from storage provider {ProviderName}.{StateName} during {Operation} for grain {GrainId}{ErrorCode}",
            _shared.ProviderTypeName, _shared.Name, operation, grainId, errorString);

        // Wrap non-Orleans exceptions
        if (exception is not OrleansException)
        {
            var errMsg = $"Error from storage provider {_shared.ProviderTypeName}.{_shared.Name} during {operation} for grain {grainId}{errorString}{Environment.NewLine} {LogFormatter.PrintException(exception)}";
            throw new OrleansException(errMsg, exception);
        }

        ExceptionDispatchInfo.Throw(exception);
    }
}
```

**Key Features**:
- **Runtime Context Checking**: Ensures state is only accessed from grain context
- **Lazy Initialization**: `GrainState` created on first access
- **Instrumentation**: Timing and error metrics with provider type name, state name, and state type name dimensions
- **Migration**: Dehydrate/Rehydrate for state transfer
- **Error Wrapping**: Converts storage exceptions to OrleansException
- **Cached Type Names**: `ProviderTypeName` and `StateTypeName` are cached in the shared instance rather than computed via `GetType().Name` on each error

---

### 4. StateStorageBridgeShared (Flyweight)

**File**: `StateStorageBridge.cs:204-233`

Shared state to minimize memory overhead when multiple states use the same provider.

```csharp
internal sealed class StateStorageBridgeSharedMap(ILoggerFactory loggerFactory, IActivatorProvider activatorProvider)
{
    private readonly ConcurrentDictionary<(string Name, IGrainStorage Store, Type StateType), object> _instances = new();
    private readonly ILoggerFactory _loggerFactory = loggerFactory;
    private readonly IActivatorProvider _activatorProvider = activatorProvider;

    public StateStorageBridgeShared<TState> Get<TState>(string name, IGrainStorage store)
        => (StateStorageBridgeShared<TState>)_instances.GetOrAdd(
            (name, store, typeof(TState)),
            static (key, self) => new StateStorageBridgeShared<TState>(
                key.Name,
                key.Store,
                self._loggerFactory.CreateLogger(key.Store.GetType()),
                self._activatorProvider.GetActivator<TState>()),
            this);
}

internal sealed class StateStorageBridgeShared<TState>(string name, IGrainStorage store, ILogger logger, IActivator<TState> activator)
{
    private string? _migrationContextKey;

    public readonly string Name = name;
    public readonly string ProviderTypeName = store.GetType().Name;
    public readonly string StateTypeName = typeof(TState).Name;
    public readonly IGrainStorage Store = store;
    public readonly ILogger Logger = logger;
    public readonly IActivator<TState> Activator = activator;
    public string MigrationContextKey => _migrationContextKey ??= $"state.{Name}";
}
```

**Pattern**: **Flyweight** - Share logger, activator, store instance, and cached type name strings across multiple grains

**Note**: `ProviderTypeName` and `StateTypeName` are computed once at construction time from `store.GetType().Name` and `typeof(TState).Name` respectively, and reused for instrumentation calls and error messages.

---

### 5. IGrainStorage (Provider Interface)

**File**: `IGrainStorage.cs:12-37`

The interface that storage providers must implement.

```csharp
public interface IGrainStorage
{
    /// <summary>Read data function for this storage instance.</summary>
    /// <param name="stateName">Name of the state for this grain</param>
    /// <param name="grainId">Grain ID</param>
    /// <param name="grainState">State data object to be populated for this grain.</param>
    /// <typeparam name="T">The grain state type.</typeparam>
    /// <returns>Completion promise for the Read operation on the specified grain.</returns>
    Task ReadStateAsync<T>(string stateName, GrainId grainId, IGrainState<T> grainState);

    /// <summary>Write data function for this storage instance.</summary>
    /// <param name="stateName">Name of the state for this grain</param>
    /// <param name="grainId">Grain ID</param>
    /// <param name="grainState">State data object to be written for this grain.</param>
    /// <typeparam name="T">The grain state type.</typeparam>
    /// <returns>Completion promise for the Write operation on the specified grain.</returns>
    Task WriteStateAsync<T>(string stateName, GrainId grainId, IGrainState<T> grainState);

    /// <summary>Delete / Clear data function for this storage instance.</summary>
    /// <param name="stateName">Name of the state for this grain</param>
    /// <param name="grainId">Grain ID</param>
    /// <param name="grainState">Copy of last-known state data object for this grain.</param>
    /// <typeparam name="T">The grain state type.</typeparam>
    /// <returns>Completion promise for the Delete operation on the specified grain.</returns>
    Task ClearStateAsync<T>(string stateName, GrainId grainId, IGrainState<T> grainState);
}
```

**IGrainState<T>** (passed to provider):

**File**: `IGrainState.cs:11-25` (interface), `IGrainState.cs:33-78` (default implementation)

```csharp
public interface IGrainState<T>
{
    T State { get; set; }
    string ETag { get; set; }
    bool RecordExists { get; set; }
}

[Serializable]
[GenerateSerializer]
public sealed class GrainState<T> : IGrainState<T>
{
    [Id(0)]
    public T State { get; set; }

    [Id(1)]
    public string ETag { get; set; }

    [Id(2)]
    public bool RecordExists { get; set; }

    public GrainState() { }

    public GrainState(T state) : this(state, null) { }

    public GrainState(T state, string eTag)
    {
        State = state;
        ETag = eTag;
    }
}
```

**Note**: `IGrainState<T>.ETag` is non-nullable `string` (unlike `IStorage.Etag` which is `string?`). `GrainState<T>` is a `sealed class` with `[Serializable]` and `[GenerateSerializer]` attributes, plus `[Id(N)]` attributes on properties for Orleans serialization. It has three constructors: parameterless, single-parameter (`T state`), and two-parameter (`T state, string eTag`).

**Provider Responsibilities**:
- **ReadStateAsync**: Load state, set `State`, `ETag`, `RecordExists`
- **WriteStateAsync**: Save state, check ETag, update ETag
- **ClearStateAsync**: Delete state, check ETag

---

## Optimistic Concurrency (ETags)

### ETag Protocol

**Write Flow**:
1. Grain reads state -> ETag = "abc123"
2. Grain modifies state
3. Grain calls `WriteStateAsync()`
4. Storage provider checks: current ETag in storage == "abc123"?
   - **Yes** -> Write succeeds, new ETag = "xyz789"
   - **No** -> Throw `InconsistentStateException`

**Example Storage Provider** (simplified):

```csharp
public class AzureTableStorageProvider : IGrainStorage
{
    public async Task WriteStateAsync<T>(string stateName, GrainId grainId, IGrainState<T> grainState)
    {
        var entity = new TableEntity(grainId.ToString(), stateName)
        {
            ["State"] = JsonSerializer.Serialize(grainState.State),
            ETag = grainState.ETag != null ? new ETag(grainState.ETag) : ETag.All
        };

        try
        {
            // Azure Table Storage automatically checks ETag
            var response = await tableClient.UpsertEntityAsync(entity, TableUpdateMode.Replace);
            grainState.ETag = response.Headers.ETag.ToString();
            grainState.RecordExists = true;
        }
        catch (RequestFailedException ex) when (ex.Status == 412) // Precondition Failed
        {
            throw new InconsistentStateException(
                $"ETag mismatch for {grainId}",
                grainState.ETag,
                "unknown",  // Azure doesn't tell us the current ETag
                ex);
        }
    }
}
```

### InconsistentStateException

**File**: `IGrainStorage.cs:59-177`

```csharp
[Serializable]
[GenerateSerializer]
public class InconsistentStateException : OrleansException
{
    [Id(0)]
    internal bool IsSourceActivation { get; set; } = true;

    [Id(1)]
    public string StoredEtag { get; private set; }

    [Id(2)]
    public string CurrentEtag { get; private set; }

    public InconsistentStateException() { }
    public InconsistentStateException(string message) : base(message) { }
    public InconsistentStateException(string message, Exception innerException) : base(message, innerException) { }

    public InconsistentStateException(
        string errorMsg,
        string storedEtag,
        string currentEtag,
        Exception storageException) : base(errorMsg, storageException)
    {
        StoredEtag = storedEtag;
        CurrentEtag = currentEtag;
    }

    public InconsistentStateException(
        string errorMsg,
        string storedEtag,
        string currentEtag) : this(errorMsg, storedEtag, currentEtag, null) { }

    public InconsistentStateException(
        string storedEtag,
        string currentEtag,
        Exception storageException) : this(storageException.Message, storedEtag, currentEtag, storageException) { }
}
```

**Note**: The exception class has multiple constructor overloads and also includes `[Obsolete]` serialization support via `SerializationInfo`/`StreamingContext` (not shown above for brevity). The `IsSourceActivation` flag is used to ensure only the originating activation is deactivated, not subsequent activations that receive the exception via RPC.

**Automatic Deactivation**: When `InconsistentStateException` escapes a grain method, Orleans **automatically deactivates the activation**.

**InsideRuntimeClient.cs:304-317**:

```csharp
if (response.Exception is { } invocationException)
{
    LogGrainInvokeException(this.invokeExceptionLogger, message.Direction != Message.Directions.OneWay ? LogLevel.Debug : LogLevel.Warning, invocationException, message);

    // If a grain allowed an inconsistent state exception to escape and the exception originated from
    // this activation, then deactivate it.
    if (invocationException is InconsistentStateException ise && ise.IsSourceActivation)
    {
        // Mark the exception so that it doesn't deactivate any other activations.
        ise.IsSourceActivation = false;

        LogDeactivatingInconsistentState(this.invokeExceptionLogger, target, invocationException);
        target.Deactivate(new DeactivationReason(DeactivationReasonCode.ApplicationError, LogFormatter.PrintException(invocationException)));
    }
}
```

**Note**: The exception is always logged first (at Debug level for request/response calls, Warning level for one-way calls) before the InconsistentStateException check. The `IsSourceActivation` flag is set to `false` before propagating to prevent the receiving activation from also being deactivated.

**Rationale**: If a grain has stale state due to concurrent writes, deactivating it forces a reload on the next activation, ensuring fresh state.

---

## Grain Lifecycle Integration

### Lifecycle Stages

```
┌──────────────────────────────────────────┐
│ Grain Lifecycle Stages                   │
├──────────────────────────────────────────┤
│ 1. Create (ActivationData created)       │
│ 2. SetupState                            │  <- PersistentState.OnStart()
│    - ReadStateAsync()                    │     loads state automatically
│ 3. Activate                              │
│    - Grain.OnActivateAsync()             │  <- Grain code runs here
│ 4. Active                                │     (state already loaded)
│    - Process messages                    │
│ 5. Deactivating                          │
│    - Grain.OnDeactivateAsync()           │
│ 6. Invalid                               │
└──────────────────────────────────────────┘
```

**Key Insight**: State is loaded **before** `OnActivateAsync()`, so grain code can immediately use `State.State` without manual loading.

### Example Grain

```csharp
public interface IUserGrain : IGrainWithStringKey
{
    Task<string> GetName();
    Task SetName(string name);
}

public class UserState
{
    public string Name { get; set; }
    public int Age { get; set; }
}

public class UserGrain : Grain, IUserGrain
{
    [PersistentState("user", "AzureTableStorage")]
    public IPersistentState<UserState> State { get; set; }

    public override Task OnActivateAsync(CancellationToken cancellationToken)
    {
        // State already loaded by PersistentState.OnStart() during SetupState stage
        // No need to call State.ReadStateAsync() here!

        if (!State.RecordExists)
        {
            // First activation - initialize default state
            State.State = new UserState { Name = "Guest", Age = 0 };
        }

        return base.OnActivateAsync(cancellationToken);
    }

    public Task<string> GetName()
    {
        return Task.FromResult(State.State.Name);
    }

    public async Task SetName(string name)
    {
        State.State.Name = name;
        await State.WriteStateAsync();  // Persist to storage
    }
}
```

**No Manual Loading**: The grain doesn't call `ReadStateAsync()` in `OnActivateAsync()`. The state is already loaded.

---

## Migration Support

### Dehydration (Source Silo)

When a grain migrates to another silo:

1. **Deactivation begins** with `DeactivationReason = Migration`
2. **OnDeactivateAsync()** runs
3. **OnDehydrate()** called on all migration participants
4. **StateStorageBridge.OnDehydrate()** saves `_grainState` to `MigrationContext`

**StateStorageBridge.cs:140-151**:

```csharp
public void OnDehydrate(IDehydrationContext dehydrationContext)
{
    try
    {
        dehydrationContext.TryAddValue(_shared.MigrationContextKey, _grainState);
    }
    catch (Exception exception)
    {
        LogErrorOnDehydrate(_shared.Logger, exception, _shared.Name, _grainContext.GrainId);
        throw;
    }
}
```

**MigrationContextKey**: `"state.{StateName}"` (e.g., `"state.user"`)

### Rehydration (Target Silo)

When the grain is reactivated on the new silo:

1. **Activation created**
2. **SetupState stage** -> `PersistentState.OnStart()` checks `IsStateInitialized`
3. **If state was rehydrated**, skip `ReadStateAsync()`
4. **If state not rehydrated**, fall back to `ReadStateAsync()`

**PersistentStateStorageFactory.cs:66-80**:

```csharp
public Task OnStart(CancellationToken cancellationToken = default)
{
    if (cancellationToken.IsCancellationRequested)
    {
        return Task.CompletedTask;
    }

    // No need to load state if it has been loaded already via rehydration.
    if (IsStateInitialized)
    {
        return Task.CompletedTask;
    }

    return ReadStateAsync();
}
```

**StateStorageBridge.cs:154-168**:

```csharp
public void OnRehydrate(IRehydrationContext rehydrationContext)
{
    try
    {
        if (rehydrationContext.TryGetValue<GrainState<TState>>(_shared.MigrationContextKey, out var grainState))
        {
            _grainState = grainState;
            IsStateInitialized = true;  // Mark as initialized to skip ReadStateAsync()
        }
    }
    catch (Exception exception)
    {
        LogErrorOnRehydrate(_shared.Logger, exception, _shared.Name, _grainContext.GrainId);
    }
}
```

**Benefits**:
- **Performance**: Avoid storage read during migration
- **Consistency**: Transfer exact in-memory state, including uncommitted changes
- **Atomicity**: Migration context transferred in single network message

---

## Multi-State Support

A grain can have multiple persistent state properties:

```csharp
public class UserGrain : Grain, IUserGrain
{
    [PersistentState("profile", "AzureTableStorage")]
    public IPersistentState<UserProfile> Profile { get; set; }

    [PersistentState("settings", "AzureTableStorage")]
    public IPersistentState<UserSettings> Settings { get; set; }

    [PersistentState("friendsList", "RedisStorage")]
    public IPersistentState<List<string>> Friends { get; set; }

    public async Task UpdateProfile(UserProfile newProfile)
    {
        Profile.State = newProfile;
        await Profile.WriteStateAsync();  // Write only profile state
    }

    public async Task AddFriend(string friendId)
    {
        Friends.State.Add(friendId);
        await Friends.WriteStateAsync();  // Write only friends state
    }
}
```

**Each state**:
- Has its own name (`"profile"`, `"settings"`, `"friendsList"`)
- Can use a different storage provider
- Has independent lifecycle (read/write/clear)
- Has independent ETag for concurrency control

---

## Factory Pattern

### PersistentStateFactory

**File**: `PersistentStateStorageFactory.cs:18-55`

```csharp
public class PersistentStateFactory : IPersistentStateFactory
{
    public IPersistentState<TState> Create<TState>(IGrainContext context, IPersistentStateConfiguration cfg)
    {
        var storageProvider = !string.IsNullOrWhiteSpace(cfg.StorageName)
            ? context.ActivationServices.GetKeyedService<IGrainStorage>(cfg.StorageName)
            : context.ActivationServices.GetService<IGrainStorage>();
        if (storageProvider == null)
        {
            ThrowMissingProviderException(context, cfg);
        }

        var fullStateName = GetFullStateName(context, cfg);
        return new PersistentState<TState>(fullStateName, context, storageProvider);
    }

    protected virtual string GetFullStateName(IGrainContext context, IPersistentStateConfiguration cfg)
    {
        return cfg.StateName;
    }

    [DoesNotReturn]
    private static void ThrowMissingProviderException(IGrainContext context, IPersistentStateConfiguration cfg)
    {
        string errMsg;
        if (string.IsNullOrEmpty(cfg.StorageName))
        {
            errMsg = $"No default storage provider found loading grain type {context.GrainId.Type}.";
        }
        else
        {
            errMsg = $"No storage provider named \"{cfg.StorageName}\" found loading grain type {context.GrainId.Type}.";
        }

        throw new BadProviderConfigException(errMsg);
    }
}
```

**Factory Workflow**:
1. Extract storage name from `[PersistentState("stateName", "storageName")]`
2. Resolve `IGrainStorage` from DI container (keyed by storage name)
3. Create `PersistentState<TState>` instance
4. Inject into grain property

---

## Error Handling

### Storage Errors

**StateStorageBridge** wraps all storage operations with try-catch:

```csharp
public async Task WriteStateAsync()
{
    try
    {
        GrainRuntime.CheckRuntimeContext(RuntimeContext.Current);

        var sw = ValueStopwatch.StartNew();
        await _shared.Store.WriteStateAsync(_shared.Name, _grainContext.GrainId, GrainState);
        StorageInstruments.OnStorageWrite(sw.Elapsed, _shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
    }
    catch (Exception exc)
    {
        StorageInstruments.OnStorageWriteError(_shared.ProviderTypeName, _shared.Name, _shared.StateTypeName);
        OnError(exc, ErrorCode.StorageProvider_WriteFailed, nameof(WriteStateAsync));
    }
}

[DoesNotReturn]
private void OnError(Exception exception, ErrorCode id, string operation)
{
    string? errorCode = null;
    (_shared.Store as IRestExceptionDecoder)?.DecodeException(exception, out _, out errorCode, true);
    var errorString = errorCode is { Length: > 0 } ? $" Error: {errorCode}" : null;

    var grainId = _grainContext.GrainId;

    // Log error with context
    _shared.Logger.LogError((int)id, exception,
        "Error from storage provider {ProviderName}.{StateName} during {Operation} for grain {GrainId}{ErrorCode}",
        _shared.ProviderTypeName, _shared.Name, operation, grainId, errorString);

    // Wrap non-Orleans exceptions
    if (exception is not OrleansException)
    {
        var errMsg = $"Error from storage provider {_shared.ProviderTypeName}.{_shared.Name} during {operation} for grain {grainId}{errorString}{Environment.NewLine} {LogFormatter.PrintException(exception)}";
        throw new OrleansException(errMsg, exception);
    }

    ExceptionDispatchInfo.Throw(exception);
}
```

**Note**: Instrumentation calls include `ProviderTypeName`, `Name` (state name), and `StateTypeName` dimensions for metrics. Error messages use `_shared.ProviderTypeName` (cached) rather than computing `_shared.Store.GetType().Name` on each error.

**Error Propagation**: Storage exceptions bubble up to grain code, which can handle or propagate them.

### InconsistentStateException Handling

Grains can catch and handle:

```csharp
public async Task UpdateBalance(decimal delta)
{
    try
    {
        State.State.Balance += delta;
        await State.WriteStateAsync();
    }
    catch (InconsistentStateException ex)
    {
        // Concurrent write detected - reload and retry
        await State.ReadStateAsync();
        State.State.Balance += delta;
        await State.WriteStateAsync();
    }
}
```

If not caught, the exception triggers automatic deactivation.

---

## Patterns for Moonpool

### 1. ActorState Trait

```rust
#[async_trait(?Send)]
pub trait ActorState<T: Serialize + DeserializeOwned> {
    fn state(&self) -> &T;
    fn state_mut(&mut self) -> &mut T;

    fn etag(&self) -> Option<&str>;
    fn record_exists(&self) -> bool;

    async fn read_state(&mut self) -> Result<(), StateError>;
    async fn write_state(&mut self) -> Result<(), StateError>;
    async fn clear_state(&mut self) -> Result<(), StateError>;
}

pub struct PersistentState<T> {
    state: T,
    etag: Option<String>,
    record_exists: bool,
    storage_provider: Arc<dyn StorageProvider>,
    actor_id: ActorId,
    state_name: String,
}

impl<T: Serialize + DeserializeOwned> PersistentState<T> {
    pub fn new(
        actor_id: ActorId,
        state_name: String,
        storage_provider: Arc<dyn StorageProvider>,
    ) -> Self {
        Self {
            state: T::default(),
            etag: None,
            record_exists: false,
            storage_provider,
            actor_id,
            state_name,
        }
    }
}

#[async_trait(?Send)]
impl<T: Serialize + DeserializeOwned + Default> ActorState<T> for PersistentState<T> {
    fn state(&self) -> &T {
        &self.state
    }

    fn state_mut(&mut self) -> &mut T {
        &mut self.state
    }

    fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    fn record_exists(&self) -> bool {
        self.record_exists
    }

    async fn read_state(&mut self) -> Result<(), StateError> {
        let state_data = self.storage_provider
            .read_state(&self.actor_id, &self.state_name)
            .await?;

        self.state = serde_json::from_slice(&state_data.data)?;
        self.etag = state_data.etag;
        self.record_exists = state_data.record_exists;

        Ok(())
    }

    async fn write_state(&mut self) -> Result<(), StateError> {
        let data = serde_json::to_vec(&self.state)?;

        let new_etag = self.storage_provider
            .write_state(&self.actor_id, &self.state_name, data, self.etag.clone())
            .await?;

        self.etag = Some(new_etag);
        self.record_exists = true;

        Ok(())
    }

    async fn clear_state(&mut self) -> Result<(), StateError> {
        self.storage_provider
            .clear_state(&self.actor_id, &self.state_name, self.etag.clone())
            .await?;

        self.state = T::default();
        self.etag = None;
        self.record_exists = false;

        Ok(())
    }
}
```

### 2. StorageProvider Trait

```rust
#[async_trait(?Send)]
pub trait StorageProvider: Send + Sync {
    async fn read_state(&self, actor_id: &ActorId, state_name: &str) -> Result<StateData, StorageError>;
    async fn write_state(&self, actor_id: &ActorId, state_name: &str, data: Vec<u8>, etag: Option<String>) -> Result<String, StorageError>;
    async fn clear_state(&self, actor_id: &ActorId, state_name: &str, etag: Option<String>) -> Result<(), StorageError>;
}

pub struct StateData {
    pub data: Vec<u8>,
    pub etag: Option<String>,
    pub record_exists: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("ETag mismatch: expected {expected:?}, found {actual:?}")]
    ETagMismatch { expected: Option<String>, actual: Option<String> },

    #[error("State not found")]
    NotFound,

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Other error: {0}")]
    Other(String),
}
```

### 3. Lifecycle Integration

```rust
pub struct Actor {
    state: PersistentState<MyState>,
    // ...
}

impl Actor {
    pub async fn on_activate(&mut self) -> Result<(), ActivationError> {
        // State already loaded by lifecycle system
        // (Similar to PersistentState.OnStart() in SetupState stage)

        if !self.state.record_exists() {
            // First activation - initialize default state
            *self.state.state_mut() = MyState::default();
        }

        Ok(())
    }

    pub async fn on_deactivate(&mut self) -> Result<(), DeactivationError> {
        // Optionally save state on deactivation
        // (Or rely on explicit WriteStateAsync calls during message processing)
        Ok(())
    }
}
```

### 4. Migration Support

```rust
impl Actor {
    pub fn on_dehydrate(&self, context: &mut DehydrationContext) {
        // Serialize state to migration context
        let state_key = format!("state.{}", self.state_name);
        context.add_value(state_key, &self.state);
    }

    pub fn on_rehydrate(&mut self, context: &RehydrationContext) -> Result<(), RehydrationError> {
        // Deserialize state from migration context
        let state_key = format!("state.{}", self.state_name);
        if let Some(state) = context.get_value(&state_key) {
            self.state = state;
            // Mark as initialized to skip ReadStateAsync
        }
        Ok(())
    }
}
```

### 5. InconsistentStateException Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Inconsistent state: ETag mismatch")]
    InconsistentState {
        stored_etag: Option<String>,
        current_etag: Option<String>,
    },
    // ... other variants
}

// Automatic deactivation on InconsistentState
pub async fn invoke_message(actor: &mut Actor, message: Message) -> Result<Response, InvokeError> {
    match process_message(actor, message).await {
        Ok(response) => Ok(response),
        Err(InvokeError::State(StateError::InconsistentState { .. })) => {
            // Automatically deactivate actor
            actor.deactivate(DeactivationReason::InconsistentState).await?;
            Err(InvokeError::State(StateError::InconsistentState { .. }))
        }
        Err(e) => Err(e),
    }
}
```

---

## References

### Source Files

**Core Interfaces**:
- `IPersistentState.cs:10-12` - Grain-facing interface
- `IStorage.cs:9-94` - Base storage interface with Etag, RecordExists, Read/Write/Clear
- `IStorage.cs:99-105` - Generic storage interface with State property
- `IGrainStorage.cs:12-37` - Provider interface
- `IGrainState.cs:11-25` - Grain state interface (State, ETag, RecordExists)
- `IGrainState.cs:33-78` - GrainState default implementation (sealed, serializable)

**Implementation**:
- `PersistentStateStorageFactory.cs:18-55` - Factory for creating persistent state
- `PersistentStateStorageFactory.cs:57-84` - PersistentState with lifecycle integration
- `StateStorageBridge.cs:22-202` - Storage bridge with error handling and migration
- `StateStorageBridge.cs:204-233` - StateStorageBridgeShared (Flyweight)

**Error Handling**:
- `IGrainStorage.cs:59-177` - InconsistentStateException definition
- `InsideRuntimeClient.cs:304-317` - Automatic deactivation on InconsistentStateException

### Related Analyses

- **activation-lifecycle.md** - Lifecycle stages, SetupState, migration
- **message-system.md** - Request context propagation
- **message-routing.md** - InsideRuntimeClient invocation with exception handling

---

## Summary

Orleans state persistence provides:

1. **Lifecycle Integration** - Automatic loading during SetupState stage
2. **Optimistic Concurrency** - ETag-based conflict detection
3. **Migration Support** - Dehydrate/Rehydrate for state transfer
4. **Multi-State** - Multiple persistent state properties per grain
5. **Provider Abstraction** - Pluggable storage backends
6. **Error Handling** - Automatic deactivation on InconsistentStateException
7. **Instrumentation** - Timing and error metrics with provider/state/type name dimensions

**Key Design Patterns**:
- **Flyweight** - Share logger, activator, storage provider, and cached type name strings
- **Factory** - Centralized creation with provider resolution
- **Bridge** - Separate grain-facing API from storage provider

For Moonpool, implement `PersistentState<T>` with lifecycle integration, ETag-based optimistic concurrency, and migration support for seamless state transfer during actor handoff.
