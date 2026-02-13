# Orleans Grain References Analysis

**Purpose**: Type-safe remote object references with transparent serialization and proxy generation

**Key insight**: GrainReference provides a type-safe, serializable handle to a virtual actor. Orleans uses the Flyweight pattern (GrainReferenceShared) to minimize memory overhead, surrogate serialization to transmit only GrainId + InterfaceType, and dynamic proxy generation to provide strongly-typed interfaces while routing all calls through a common runtime.

---

## Overview

Grain references solve the problem: **How do you represent a reference to a remote virtual actor with compile-time type safety, minimal memory overhead, and transparent serialization?**

### Key Responsibilities

1. **Identity** - Encapsulate `GrainId` (which virtual actor)
2. **Interface** - Specify `GrainInterfaceType` (which interface view)
3. **Invocation** - Route method calls to the runtime for request-response handling
4. **Serialization** - Compact wire format (GrainId + InterfaceType only)
5. **Equality** - Value semantics based on GrainId
6. **Type Safety** - Compile-time checked method calls
7. **Proxy Generation** - Dynamic generation of typed reference implementations

### Architecture

```
+--------------------------------------------------------------+
| Application Code                                              |
|   IMyGrain grain = grainFactory.GetGrain<IMyGrain>(id);       |
|   var result = await grain.DoSomething(arg);                  |
+--------------------------------------------------------------+
| Generated Proxy: MyGrainReference                             |
|   - Implements IMyGrain                                       |
|   - Extends GrainReference                                    |
|   - Method: DoSomething() -> InvokeAsync(request)             |
+--------------------------------------------------------------+
| GrainReference (Base Class)                                   |
|   - GrainId: type + key                                       |
|   - Shared: runtime, codecs, version                          |
|   - InvokeAsync() -> runtime.InvokeMethodAsync()              |
+--------------------------------------------------------------+
| GrainReferenceShared (Flyweight)                              |
|   - Runtime: IGrainReferenceRuntime                           |
|   - GrainType, InterfaceType, InterfaceVersion                |
|   - CodecProvider, CopyContextPool                            |
|   - Shared by all references of same type+interface           |
+--------------------------------------------------------------+
| IGrainReferenceRuntime -> InsideRuntimeClient                 |
|   - InvokeMethodAsync() -> SendRequest()                      |
+--------------------------------------------------------------+
```

---

## Core Components

### 1. GrainReference (Base Class)

**File**: `GrainReference.cs:249-435`

The base class for all grain references. Provides identity, equality, and invocation infrastructure.

```csharp
[Alias("GrainRef")]
[DefaultInvokableBaseType(typeof(ValueTask<>), typeof(Request<>))]
[DefaultInvokableBaseType(typeof(ValueTask), typeof(Request))]
[DefaultInvokableBaseType(typeof(Task<>), typeof(TaskRequest<>))]
[DefaultInvokableBaseType(typeof(Task), typeof(TaskRequest))]
[DefaultInvokableBaseType(typeof(void), typeof(VoidRequest))]
[DefaultInvokableBaseType(typeof(IAsyncEnumerable<>), typeof(AsyncEnumerableRequest<>))]
public class GrainReference : IAddressable, IEquatable<GrainReference>, ISpanFormattable
{
    // Shared functionality for all references of this type+interface (Flyweight pattern)
    [NonSerialized]
    private readonly GrainReferenceShared _shared;

    // The key portion of the grain id (e.g., user123, guid, compound key)
    [NonSerialized]
    private readonly IdSpan _key;

    // Throws GrainReferenceNotBoundException if _shared is null (unbound reference)
    internal GrainReferenceShared Shared => _shared ?? throw new GrainReferenceNotBoundException(this);

    protected GrainReference(GrainReferenceShared shared, IdSpan key)
    {
        _shared = shared;
        _key = key;
    }

    // Factory method used by UntypedGrainReferenceActivator
    internal static GrainReference FromGrainId(GrainReferenceShared shared, GrainId grainId) => new(shared, grainId.Key);

    // Grain identity: Type + Key
    public GrainId GrainId => GrainId.Create(_shared.GrainType, _key);

    // Interface identity
    public GrainInterfaceType InterfaceType => _shared.InterfaceType;
    public ushort InterfaceVersion => Shared.InterfaceVersion;
    public virtual string InterfaceName => InterfaceType.ToString();

    // Runtime access
    internal IGrainReferenceRuntime Runtime => Shared.Runtime;

    // Serialization helpers exposed to generated proxies
    protected CopyContextPool CopyContextPool => _shared.CopyContextPool;
    protected CodecProvider CodecProvider => _shared.CodecProvider;

    // Equality based on GrainId (value semantics)
    public override bool Equals(object? obj) => Equals(obj as GrainReference);
    public bool Equals(GrainReference? other) => other is not null && this.GrainId.Equals(other.GrainId);
    public override int GetHashCode() => this.GrainId.GetHashCode();

    // Uniform hash for consistent hashing (placement)
    public uint GetUniformHashCode() => GrainId.GetUniformHashCode();

    // Operators
    public static bool operator ==(GrainReference? reference1, GrainReference? reference2)
    {
        if (reference1 is null) return reference2 is null;
        return reference1.Equals(reference2);
    }

    public static bool operator !=(GrainReference? reference1, GrainReference? reference2)
    {
        if (reference1 is null) return !(reference2 is null);
        return !reference1.Equals(reference2);
    }

    // Cast to different interface
    public virtual TGrainInterface Cast<TGrainInterface>()
        where TGrainInterface : IAddressable
        => (TGrainInterface)_shared.Runtime.Cast(this, typeof(TGrainInterface));

    // Request object creation (uses DI to create or retrieve invokable instances)
    protected TInvokable GetInvokable<TInvokable>()
        => ActivatorUtilities.GetServiceOrCreateInstance<TInvokable>(Shared.ServiceProvider);

    // Invocation infrastructure (called by generated proxies)
    protected ValueTask<T> InvokeAsync<T>(IRequest methodDescription)
    {
        return this.Runtime.InvokeMethodAsync<T>(this, methodDescription, methodDescription.Options);
    }

    protected ValueTask InvokeAsync(IRequest methodDescription)
    {
        return this.Runtime.InvokeMethodAsync(this, methodDescription, methodDescription.Options);
    }

    protected void Invoke(IRequest methodDescription)
    {
        this.Runtime.InvokeMethod(this, methodDescription, methodDescription.Options);
    }

    public sealed override string ToString() => $"GrainReference:{GrainId}:{InterfaceType}";

    // ISpanFormattable implementation for efficient string formatting
    string IFormattable.ToString(string? format, IFormatProvider? formatProvider) => ToString();
    bool ISpanFormattable.TryFormat(Span<char> destination, out int charsWritten,
        ReadOnlySpan<char> format, IFormatProvider? provider)
        => destination.TryWrite($"GrainReference:{GrainId}:{InterfaceType}", out charsWritten);
}
```

**Key Design Decisions**:
- **Flyweight Pattern**: `_shared` contains heavy objects (runtime, codecs) shared across all references of the same type+interface
- **Value Semantics**: Equality based on `GrainId`, not object identity
- **Thin Wrapper**: Only stores `_shared` + `_key` (16-24 bytes per reference)
- **Non-serialized Fields**: `_shared` and `_key` are marked `[NonSerialized]` - serialization uses surrogate pattern
- **Bound Check**: `Shared` property throws `GrainReferenceNotBoundException` if the reference was not constructed by the runtime
- **Invokable Base Types**: Six `[DefaultInvokableBaseType]` attributes map return types to request base classes, including `IAsyncEnumerable<>` support

---

### 2. GrainReferenceShared (Flyweight)

**File**: `GrainReference.cs:21-82`

Contains the heavy, immutable state shared by all grain references of a given `(GrainType, InterfaceType)` pair.

```csharp
public class GrainReferenceShared
{
    public GrainReferenceShared(
        GrainType grainType,
        GrainInterfaceType grainInterfaceType,
        ushort interfaceVersion,
        IGrainReferenceRuntime runtime,
        InvokeMethodOptions invokeMethodOptions,
        CodecProvider codecProvider,
        CopyContextPool copyContextPool,
        IServiceProvider serviceProvider)
    {
        this.GrainType = grainType;
        this.InterfaceType = grainInterfaceType;
        this.Runtime = runtime;
        this.InvokeMethodOptions = invokeMethodOptions;
        this.CodecProvider = codecProvider;
        this.CopyContextPool = copyContextPool;
        this.ServiceProvider = serviceProvider;
        this.InterfaceVersion = interfaceVersion;
    }

    public IGrainReferenceRuntime Runtime { get; }         // InsideRuntimeClient
    public GrainType GrainType { get; }                    // e.g., "myapp.MyGrain"
    public GrainInterfaceType InterfaceType { get; }       // e.g., "myapp.IMyGrain"
    public InvokeMethodOptions InvokeMethodOptions { get; } // e.g., Unordered
    public CodecProvider CodecProvider { get; }            // For serialization
    public CopyContextPool CopyContextPool { get; }        // For deep copying
    public IServiceProvider ServiceProvider { get; }       // DI container
    public ushort InterfaceVersion { get; }                // Interface version number
}
```

**Memory Optimization**: With 10,000 references to the same grain type+interface:
- **Without Flyweight**: 10,000 x 100 bytes = 1 MB
- **With Flyweight**: 1 x 100 bytes + 10,000 x 16 bytes = 160 KB

**Pattern**: **Flyweight** - Share expensive immutable state across many instances

---

### 3. GrainReferenceActivator (Factory)

**File**: `GrainReferenceActivator.cs:24-95`

Factory for creating grain references. Uses a provider chain to find the appropriate activator for each `(GrainType, InterfaceType)` pair.

```csharp
public sealed class GrainReferenceActivator
{
    private readonly object _lockObj = new object();
    private readonly IServiceProvider _serviceProvider;
    private readonly IGrainReferenceActivatorProvider[] _providers;
    private Dictionary<(GrainType, GrainInterfaceType), IGrainReferenceActivator> _activators = new();

    public GrainReferenceActivator(
        IServiceProvider serviceProvider,
        IEnumerable<IGrainReferenceActivatorProvider> providers)
    {
        _serviceProvider = serviceProvider;
        _providers = providers.ToArray();
    }

    public GrainReference CreateReference(GrainId grainId, GrainInterfaceType interfaceType)
    {
        if (!_activators.TryGetValue((grainId.Type, interfaceType), out var entry))
        {
            entry = CreateActivator(grainId.Type, interfaceType);
        }

        var result = entry.CreateReference(grainId);
        return result;
    }

    private IGrainReferenceActivator CreateActivator(GrainType grainType, GrainInterfaceType interfaceType)
    {
        lock (_lockObj)
        {
            if (!_activators.TryGetValue((grainType, interfaceType), out var entry))
            {
                IGrainReferenceActivator activator = null;
                foreach (var provider in _providers)
                {
                    if (provider.TryGet(grainType, interfaceType, out activator))
                        break;
                }

                if (activator is null)
                {
                    throw new InvalidOperationException($"Unable to find an IGrainReferenceActivatorProvider for grain type {grainType}");
                }

                entry = activator;
                _activators = new(_activators) { [(grainType, interfaceType)] = entry };
            }

            return entry;
        }
    }
}
```

**Provider Chain**:
1. **GrainReferenceActivatorProvider** - Handles typed proxy generation (most grains)
2. **UntypedGrainReferenceActivatorProvider** - Handles untyped references (no interface specified)

**Pattern**: **Chain of Responsibility** - Try each provider until one succeeds

---

### 4. RpcProvider (Proxy Type Mapping)

**File**: `GrainReferenceActivator.cs:179-276`

Maps `GrainInterfaceType` to the corresponding generated proxy type.

```csharp
internal class RpcProvider
{
    private readonly TypeConverter _typeConverter;
    private readonly Dictionary<GrainInterfaceType, Type> _mapping;

    public RpcProvider(
        IOptions<TypeManifestOptions> config,
        GrainInterfaceTypeResolver resolver,
        TypeConverter typeConverter)
    {
        _typeConverter = typeConverter;
        var proxyTypes = config.Value.InterfaceProxies;  // Generated proxy types
        _mapping = new Dictionary<GrainInterfaceType, Type>();

        foreach (var proxyType in proxyTypes)
        {
            if (!typeof(IAddressable).IsAssignableFrom(proxyType))
                continue;

            var type = proxyType switch
            {
                { IsGenericType: true } => proxyType.GetGenericTypeDefinition(),
                _ => proxyType
            };

            var grainInterface = GetMainInterface(type);
            var id = resolver.GetGrainInterfaceType(grainInterface);
            _mapping[id] = type;
        }

        // Local static function: finds the most-derived interface implemented by the proxy type
        static Type GetMainInterface(Type t)
        {
            var all = t.GetInterfaces();
            Type result = null;
            foreach (var candidate in all)
            {
                if (result is null)
                    result = candidate;
                else if (result.IsAssignableFrom(candidate))
                    result = candidate;
            }

            return result switch
            {
                { IsGenericType: true } => result.GetGenericTypeDefinition(),
                _ => result
            };
        }
    }

    public bool TryGet(GrainInterfaceType interfaceType, [NotNullWhen(true)] out Type result)
    {
        GrainInterfaceType lookupId;
        Type[] args;

        // Handle generic interfaces
        if (GenericGrainInterfaceType.TryParse(interfaceType, out var genericId))
        {
            lookupId = genericId.GetGenericGrainType().Value;
            args = genericId.GetArguments(_typeConverter);
        }
        else
        {
            lookupId = interfaceType;
            args = default;
        }

        if (!_mapping.TryGetValue(lookupId, out result))
            return false;

        // Construct generic type if needed
        if (args is not null)
            result = result.MakeGenericType(args);

        return true;
    }
}
```

**Example Mapping**:
- `IMyGrain` -> `MyGrainReference` (generated proxy)
- `IMyGenericGrain<T>` -> `MyGenericGrainReference<T>` (generic proxy)

---

### 5. GrainReferenceActivatorProvider (Proxy Factory)

**File**: `GrainReferenceActivator.cs:281-382`

Creates activators that instantiate typed proxies using dynamic code generation.

```csharp
internal class GrainReferenceActivatorProvider : IGrainReferenceActivatorProvider
{
    private readonly CopyContextPool _copyContextPool;
    private readonly CodecProvider _codecProvider;
    private readonly IServiceProvider _serviceProvider;
    private readonly GrainPropertiesResolver _propertiesResolver;
    private readonly RpcProvider _rpcProvider;
    private readonly GrainVersionManifest _grainVersionManifest;
    private IGrainReferenceRuntime _grainReferenceRuntime;

    public bool TryGet(GrainType grainType, GrainInterfaceType interfaceType, out IGrainReferenceActivator activator)
    {
        // Find proxy type
        if (!_rpcProvider.TryGet(interfaceType, out var proxyType))
        {
            activator = default;
            return false;
        }

        // Check for unordered execution attribute
        var unordered = false;
        var properties = _propertiesResolver.GetGrainProperties(grainType);
        if (properties.Properties.TryGetValue(WellKnownGrainTypeProperties.Unordered, out var unorderedString)
            && string.Equals("true", unorderedString, StringComparison.OrdinalIgnoreCase))
        {
            unordered = true;
        }

        var interfaceVersion = _grainVersionManifest.GetLocalVersion(interfaceType);
        var invokeMethodOptions = unordered ? InvokeMethodOptions.Unordered : InvokeMethodOptions.None;

        // Lazy-init runtime reference
        var runtime = _grainReferenceRuntime ??= _serviceProvider.GetRequiredService<IGrainReferenceRuntime>();

        // Create shared state
        var shared = new GrainReferenceShared(
            grainType,
            interfaceType,
            interfaceVersion,
            runtime,
            invokeMethodOptions,
            _codecProvider,
            _copyContextPool,
            _serviceProvider);

        activator = new GrainReferenceActivator(proxyType, shared);
        return true;
    }

    // Activator for a specific proxy type
    private sealed class GrainReferenceActivator : IGrainReferenceActivator
    {
        private readonly GrainReferenceShared _shared;
        private readonly Func<GrainReferenceShared, IdSpan, GrainReference> _create;

        public GrainReferenceActivator(Type referenceType, GrainReferenceShared shared)
        {
            _shared = shared;

            // Find constructor: ctor(GrainReferenceShared, IdSpan)
            var ctor = referenceType.GetConstructor(
                BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic,
                new[] { typeof(GrainReferenceShared), typeof(IdSpan) })
                ?? throw new SerializerException("Invalid proxy type: " + referenceType);

            // Generate dynamic method for fast construction
            // arg0 is unused for better delegate performance (avoids argument shuffling thunk)
            var method = new DynamicMethod(referenceType.Name, typeof(GrainReference), new[] { typeof(object), typeof(GrainReferenceShared), typeof(IdSpan) });
            var il = method.GetILGenerator();
            il.Emit(OpCodes.Ldarg_1);  // GrainReferenceShared
            il.Emit(OpCodes.Ldarg_2);  // IdSpan
            il.Emit(OpCodes.Newobj, ctor);
            il.Emit(OpCodes.Ret);

            _create = method.CreateDelegate<Func<GrainReferenceShared, IdSpan, GrainReference>>();
        }

        public GrainReference CreateReference(GrainId grainId) => _create(_shared, grainId.Key);
    }
}
```

**Dynamic Code Generation**: Creates a fast delegate for instantiating proxies. Avoids reflection overhead.

---

## Serialization (Surrogate Pattern)

### Surrogate Structure

**File**: `GrainReference.cs:230-244`

```csharp
[GenerateSerializer]
internal struct GrainReferenceSurrogate
{
    [Id(0)]
    public GrainId GrainId;

    [Id(1)]
    public GrainInterfaceType GrainInterfaceType;
}
```

**Wire Format**: Only 2 fields transmitted:
- `GrainId` - Identity (type + key)
- `GrainInterfaceType` - Interface view

**NOT transmitted**:
- `GrainReferenceShared` (reconstructed on receiver)
- Runtime, codecs, etc. (available on receiver)

### Serialization Codecs

**File**: `GrainReference.cs:84-225`

Orleans provides two codec implementations: a general `GrainReferenceCodec` for the `IAddressable` base type, and a typed `TypedGrainReferenceCodec<T>` for specific grain interface types.

```csharp
// General codec: handles IAddressable
[RegisterSerializer]
internal class GrainReferenceCodec : GeneralizedReferenceTypeSurrogateCodec<IAddressable, GrainReferenceSurrogate>
{
    private readonly IGrainFactory _grainFactory;

    public GrainReferenceCodec(IGrainFactory grainFactory, IValueSerializer<GrainReferenceSurrogate> surrogateSerializer)
        : base(surrogateSerializer)
    {
        _grainFactory = grainFactory;
    }

    public override IAddressable ConvertFromSurrogate(ref GrainReferenceSurrogate surrogate)
    {
        return _grainFactory.GetGrain(surrogate.GrainId, surrogate.GrainInterfaceType);
    }

    public override void ConvertToSurrogate(IAddressable value, ref GrainReferenceSurrogate surrogate)
    {
        var refValue = value.AsReference();
        surrogate.GrainId = refValue.GrainId;
        surrogate.GrainInterfaceType = refValue.InterfaceType;
    }
}

// Typed codec: handles specific grain interface types (e.g., IMyGrain)
internal class TypedGrainReferenceCodec<T> : GeneralizedReferenceTypeSurrogateCodec<T, GrainReferenceSurrogate>
    where T : class, IAddressable
{
    private readonly IGrainFactory _grainFactory;

    public TypedGrainReferenceCodec(IGrainFactory grainFactory, IValueSerializer<GrainReferenceSurrogate> surrogateSerializer)
        : base(surrogateSerializer)
    {
        _grainFactory = grainFactory;
    }

    // Serialize: GrainReference -> Surrogate
    public override void ConvertToSurrogate(T value, ref GrainReferenceSurrogate surrogate)
    {
        if (value is not GrainReference refValue)
        {
            if (value is IGrainObserver observer)
                GrainReferenceCodecProvider.ThrowGrainObserverInvalidException(observer);

            refValue = (GrainReference)(object)value.AsReference<T>();
        }

        surrogate.GrainId = refValue.GrainId;
        surrogate.GrainInterfaceType = refValue.InterfaceType;
    }

    // Deserialize: Surrogate -> GrainReference
    public override T ConvertFromSurrogate(ref GrainReferenceSurrogate surrogate)
    {
        return (T)_grainFactory.GetGrain(surrogate.GrainId, surrogate.GrainInterfaceType);
    }
}
```

Additionally, Orleans provides specialized **copier** and **codec provider** classes for grain references:

- `GrainReferenceCopier` (`GrainReference.cs:118-119`) - Shallow copier for `GrainReference` (grain references are immutable, so copying is identity)
- `GrainReferenceCopierProvider` (`GrainReference.cs:124-132`) - Creates specialized copiers for grain interface types
- `GrainReferenceCodecProvider` (`GrainReference.cs:159-181`) - Creates specialized codecs for grain interface types, including `ThrowGrainObserverInvalidException` for invalid observer parameters

**Pattern**: **Surrogate** - Separate representation for serialization

**Deserialization Flow**:
1. Deserialize surrogate (GrainId + InterfaceType)
2. Call `grainFactory.GetGrain(grainId, interfaceType)`
3. GrainFactory calls `GrainReferenceActivator.CreateReference()`
4. Activator finds/creates appropriate proxy type
5. Returns fully reconstructed grain reference

---

## Proxy Generation (Example)

### Grain Interface

```csharp
public interface IMyGrain : IGrainWithGuidKey
{
    Task<string> GetGreeting(string name);
    Task SetCounter(int value);
}
```

### Generated Proxy

```csharp
[GenerateSerializer]
internal sealed class MyGrainReference : GrainReference, IMyGrain
{
    public MyGrainReference(GrainReferenceShared shared, IdSpan key)
        : base(shared, key)
    {
    }

    // Implement IMyGrain.GetGreeting
    public Task<string> GetGreeting(string name)
    {
        var request = GetInvokable<GetGreetingRequest>();
        request.arg0 = name;
        return this.InvokeAsync<string>(request).AsTask();
    }

    // Implement IMyGrain.SetCounter
    public Task SetCounter(int value)
    {
        var request = GetInvokable<SetCounterRequest>();
        request.arg0 = value;
        return this.InvokeAsync(request).AsTask();
    }
}

// Request objects (one per method)
internal sealed class GetGreetingRequest : Request<string>
{
    [Id(0)] public string arg0;

    protected override ValueTask<string> InvokeInner()
    {
        var target = (IMyGrain)this.GetTarget();
        return new ValueTask<string>(target.GetGreeting(arg0));
    }

    public override string GetMethodName() => "GetGreeting";
    public override string GetInterfaceName() => "IMyGrain";
    // ... other metadata methods
}

internal sealed class SetCounterRequest : Request
{
    [Id(0)] public int arg0;

    protected override ValueTask InvokeInner()
    {
        var target = (IMyGrain)this.GetTarget();
        return new ValueTask(target.SetCounter(arg0));
    }

    public override string GetMethodName() => "SetCounter";
    public override string GetInterfaceName() => "IMyGrain";
    // ... other metadata methods
}
```

**Pattern**: **Proxy** - Provide a surrogate that forwards calls to the real subject

**Invocation Flow**:
1. Caller: `await grain.GetGreeting("Alice")`
2. Proxy: Create `GetGreetingRequest` via `GetInvokable<GetGreetingRequest>()` with `arg0 = "Alice"`
3. Proxy: Call `this.InvokeAsync<string>(request)`
4. `GrainReference.InvokeAsync()`: Call `Runtime.InvokeMethodAsync()`
5. `InsideRuntimeClient.InvokeMethodAsync()`: Create message, send request
6. ... (see message-routing.md)

---

## Request Base Types

**File**: `GrainReference.cs:440-821`

Orleans defines the `IRequest` interface and multiple request base types to match different method signatures.

### IRequest Interface

**File**: `GrainReference.cs:440-532`

```csharp
public interface IRequest : IInvokable
{
    InvokeMethodOptions Options { get; }
    void AddInvokeMethodOptions(InvokeMethodOptions options);

    // Static helper methods for formatting request descriptions
    public static string ToString(IRequest request) { /* builds "Interface[(Interface)target].Method(argTypes)" */ }
    public static string ToMethodCallString(IRequest request) { /* builds "Interface.Method(args)" */ }
}
```

### RequestBase

**File**: `GrainReference.cs:539-605`

```csharp
[SuppressReferenceTracking]
[SerializerTransparent]
public abstract class RequestBase : IRequest
{
    [field: NonSerialized]
    public InvokeMethodOptions Options { get; protected set; }

    public void AddInvokeMethodOptions(InvokeMethodOptions options) { Options |= options; }

    public virtual int GetArgumentCount() => 0;
    public virtual object GetArgument(int index) => throw new ArgumentOutOfRangeException(...);
    public virtual void SetArgument(int index, object value) => throw new ArgumentOutOfRangeException(...);

    public abstract ValueTask<Response> Invoke();
    public abstract object GetTarget();
    public abstract void SetTarget(ITargetHolder holder);
    public abstract void Dispose();
    public abstract string GetMethodName();
    public abstract string GetInterfaceName();
    public abstract string GetActivityName();      // For distributed tracing
    public abstract Type GetInterfaceType();        // CLR type of the grain interface
    public abstract MethodInfo GetMethod();

    // Response timeout override (per-method)
    public virtual TimeSpan? GetDefaultResponseTimeout() => null;

    // Cancellation support
    public virtual bool TryCancel() => false;
    public virtual CancellationToken GetCancellationToken() => default;
    public virtual bool IsCancellable => false;

    public override string ToString() => IRequest.ToString(this);
}
```

### Concrete Request Types

```csharp
// For methods returning ValueTask
public abstract class Request : RequestBase
{
    public sealed override ValueTask<Response> Invoke()
    {
        try
        {
            var resultTask = InvokeInner();
            if (resultTask.IsCompleted)
            {
                resultTask.GetAwaiter().GetResult();
                return new ValueTask<Response>(Response.Completed);
            }

            return CompleteInvokeAsync(resultTask);
        }
        catch (Exception exception)
        {
            return new ValueTask<Response>(Response.FromException(exception));
        }
    }

    protected abstract ValueTask InvokeInner();
}

// For methods returning ValueTask<T>
public abstract class Request<TResult> : RequestBase
{
    public sealed override ValueTask<Response> Invoke()
    {
        try
        {
            var resultTask = InvokeInner();
            if (resultTask.IsCompleted)
            {
                return new ValueTask<Response>(Response.FromResult(resultTask.Result));
            }

            return CompleteInvokeAsync(resultTask);
        }
        catch (Exception exception)
        {
            return new ValueTask<Response>(Response.FromException(exception));
        }
    }

    protected abstract ValueTask<TResult> InvokeInner();
}

// For methods returning Task<T>
public abstract class TaskRequest<TResult> : RequestBase { /* similar to Request<TResult> but wraps Task<T> */ }

// For methods returning Task
public abstract class TaskRequest : RequestBase { /* similar to Request but wraps Task */ }

// For void methods (OneWay)
public abstract class VoidRequest : RequestBase
{
    protected VoidRequest()
    {
        Options = InvokeMethodOptions.OneWay;  // Always OneWay
    }

    public sealed override ValueTask<Response> Invoke()
    {
        try
        {
            InvokeInner();
            return new ValueTask<Response>(Response.Completed);
        }
        catch (Exception exception)
        {
            return new ValueTask<Response>(Response.FromException(exception));
        }
    }

    protected abstract void InvokeInner();
}

// For methods returning IAsyncEnumerable<T>
public abstract class AsyncEnumerableRequest<T> : RequestBase, IAsyncEnumerable<T>, IAsyncEnumerableRequest<T>
{
    internal GrainReference? TargetGrain { get; private set; }
    [Id(0)] public int MaxBatchSize { get; set; } = 100;

    public IAsyncEnumerator<T> GetAsyncEnumerator(CancellationToken cancellationToken = default)
        => new AsyncEnumeratorProxy<T>(this, cancellationToken);

    // Called upon creation by the grain reference via [ReturnValueProxy] attribute
    public IAsyncEnumerable<T> InitializeRequest(GrainReference targetGrainReference) { ... }

    public override ValueTask<Response> Invoke()
        => throw new NotImplementedException("IAsyncEnumerable requests can not be invoked directly");

    protected abstract IAsyncEnumerable<T> InvokeInner();
}
```

**File for AsyncEnumerableRequest**: `AsyncEnumerableRequest.cs:156-186`

**Design**: Each request type matches a specific method signature pattern, providing type-safe invocation with minimal boxing. The `AsyncEnumerableRequest<T>` is notable because it does not go through the standard `Invoke()` path -- instead it uses a grain extension (`IAsyncEnumerableGrainExtension`) to implement streaming enumeration with batching, heartbeats, and cancellation support.

---

## Interface Versioning

### Version Tracking

**GrainReferenceShared.InterfaceVersion**:
- Stored in `GrainReferenceShared`
- Retrieved from `GrainVersionManifest.GetLocalVersion(interfaceType)`
- Included in message headers

**Message.cs**:
```csharp
public class Message
{
    public GrainInterfaceType InterfaceType { get; set; }
    public ushort InterfaceVersion { get; set; }
    // ...
}
```

### Version Resolution

**InsideRuntimeClient.cs**:
```csharp
public void SendRequest(GrainReference target, IInvokable request, IResponseCompletionSource context, InvokeMethodOptions options)
{
    var message = this.messageFactory.CreateMessage(request, options);
    message.InterfaceType = target.InterfaceType;
    message.InterfaceVersion = target.InterfaceVersion;  // From GrainReferenceShared
    // ...
}
```

**Compatibility Director** (not shown in code snippets) uses version numbers to determine if a silo can handle a message for a specific interface version.

---

## Integration with IGrainFactory

### Creating References

**GrainFactory.cs**:
```csharp
internal class GrainFactory : IInternalGrainFactory
{
    private readonly GrainReferenceActivator referenceActivator;
    private readonly GrainInterfaceTypeResolver interfaceTypeResolver;
    private readonly GrainInterfaceTypeToGrainTypeResolver interfaceTypeToGrainTypeResolver;
    private readonly IRuntimeClient runtimeClient;

    // Typed access with Guid key
    public TGrainInterface GetGrain<TGrainInterface>(Guid primaryKey, string grainClassNamePrefix = null)
        where TGrainInterface : IGrainWithGuidKey
    {
        var grainKey = GrainIdKeyExtensions.CreateGuidKey(primaryKey);
        return (TGrainInterface)GetGrain(typeof(TGrainInterface), grainKey, grainClassNamePrefix: grainClassNamePrefix);
    }

    // Typed access with string key
    public TGrainInterface GetGrain<TGrainInterface>(string primaryKey, string grainClassNamePrefix = null)
        where TGrainInterface : IGrainWithStringKey
    {
        ArgumentNullException.ThrowIfNullOrWhiteSpace(primaryKey);
        var grainKey = IdSpan.Create(primaryKey);
        return (TGrainInterface)GetGrain(typeof(TGrainInterface), grainKey, grainClassNamePrefix: grainClassNamePrefix);
    }

    // Typed access with long key
    public TGrainInterface GetGrain<TGrainInterface>(long primaryKey, string grainClassNamePrefix = null)
        where TGrainInterface : IGrainWithIntegerKey
    {
        var grainKey = GrainIdKeyExtensions.CreateIntegerKey(primaryKey);
        return (TGrainInterface)GetGrain(typeof(TGrainInterface), grainKey, grainClassNamePrefix: grainClassNamePrefix);
    }

    // Typed access by GrainId
    public TGrainInterface GetGrain<TGrainInterface>(GrainId grainId) where TGrainInterface : IAddressable
    {
        return (TGrainInterface)this.CreateGrainReference(typeof(TGrainInterface), grainId);
    }

    // Deserialization path (GrainId + InterfaceType)
    public IAddressable GetGrain(GrainId grainId, GrainInterfaceType interfaceType)
    {
        return this.referenceActivator.CreateReference(grainId, interfaceType);
    }

    // Untyped access (no interface)
    public IAddressable GetGrain(GrainId grainId)
        => this.referenceActivator.CreateReference(grainId, default);

    // Core resolution: interface type -> grain type -> grain id -> reference
    public IAddressable GetGrain(Type interfaceType, IdSpan grainKey, string grainClassNamePrefix = null)
    {
        ArgumentNullException.ThrowIfNull(interfaceType);
        var grainInterfaceType = this.interfaceTypeResolver.GetGrainInterfaceType(interfaceType);

        GrainType grainType;
        if (!string.IsNullOrWhiteSpace(grainClassNamePrefix))
            grainType = this.interfaceTypeToGrainTypeResolver.GetGrainType(grainInterfaceType, grainClassNamePrefix);
        else
            grainType = this.interfaceTypeToGrainTypeResolver.GetGrainType(grainInterfaceType);

        var grainId = GrainId.Create(grainType, grainKey);
        return this.referenceActivator.CreateReference(grainId, grainInterfaceType);
    }

    private object CreateGrainReference(Type interfaceType, GrainId grainId)
    {
        var grainInterfaceType = this.interfaceTypeResolver.GetGrainInterfaceType(interfaceType);
        return this.referenceActivator.CreateReference(grainId, grainInterfaceType);
    }
}
```

---

## Patterns for Moonpool

### 1. ActorReference Trait

```rust
/// Trait for all actor references
pub trait ActorReference: Send + Sync {
    fn actor_id(&self) -> ActorId;
    fn interface_type(&self) -> &str;

    fn invoke_async(&self, request: Box<dyn Request>) -> BoxFuture<'static, Result<Response, InvokeError>>;
}

/// Concrete implementation
pub struct ActorRef<T: ActorInterface> {
    actor_id: ActorId,
    interface_type: &'static str,
    runtime: Arc<dyn ActorReferenceRuntime>,
    _phantom: PhantomData<T>,
}

impl<T: ActorInterface> ActorRef<T> {
    pub fn new(actor_id: ActorId, runtime: Arc<dyn ActorReferenceRuntime>) -> Self {
        Self {
            actor_id,
            interface_type: T::INTERFACE_TYPE,
            runtime,
            _phantom: PhantomData,
        }
    }

    pub async fn invoke<R: Request>(&self, request: R) -> Result<R::Response, InvokeError> {
        let boxed_request = Box::new(request);
        let response = self.invoke_async(boxed_request).await?;
        // Downcast response to concrete type
        Ok(response.downcast().unwrap())
    }
}

impl<T: ActorInterface> ActorReference for ActorRef<T> {
    fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    fn interface_type(&self) -> &str {
        self.interface_type
    }

    fn invoke_async(&self, request: Box<dyn Request>) -> BoxFuture<'static, Result<Response, InvokeError>> {
        self.runtime.invoke_method_async(self.actor_id, request)
    }
}
```

### 2. Flyweight Pattern

```rust
/// Shared state for all references of a given type+interface
pub struct ActorReferenceShared {
    pub actor_type: String,
    pub interface_type: String,
    pub interface_version: u16,
    pub runtime: Arc<dyn ActorReferenceRuntime>,
    pub invoke_options: InvokeOptions,
}

/// Lazy map of shared instances
pub struct SharedReferenceMap {
    map: DashMap<(String, String), Arc<ActorReferenceShared>>,
}

impl SharedReferenceMap {
    pub fn get_or_create(
        &self,
        actor_type: String,
        interface_type: String,
        runtime: Arc<dyn ActorReferenceRuntime>,
    ) -> Arc<ActorReferenceShared> {
        self.map
            .entry((actor_type.clone(), interface_type.clone()))
            .or_insert_with(|| {
                Arc::new(ActorReferenceShared {
                    actor_type,
                    interface_type,
                    interface_version: 1,
                    runtime,
                    invoke_options: InvokeOptions::default(),
                })
            })
            .clone()
    }
}
```

### 3. Surrogate Serialization

```rust
/// Surrogate for serialization (only ID + interface)
#[derive(Serialize, Deserialize)]
pub struct ActorReferenceSurrogate {
    pub actor_id: ActorId,
    pub interface_type: String,
}

impl<T: ActorInterface> Serialize for ActorRef<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let surrogate = ActorReferenceSurrogate {
            actor_id: self.actor_id,
            interface_type: self.interface_type.to_string(),
        };
        surrogate.serialize(serializer)
    }
}

impl<'de, T: ActorInterface> Deserialize<'de> for ActorRef<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let surrogate = ActorReferenceSurrogate::deserialize(deserializer)?;

        // Reconstruct from factory
        let runtime = get_runtime_from_context()?;  // Thread-local or global
        Ok(ActorRef::new(surrogate.actor_id, runtime))
    }
}
```

### 4. Factory Pattern

```rust
pub struct ActorFactory {
    runtime: Arc<dyn ActorReferenceRuntime>,
    shared_map: Arc<SharedReferenceMap>,
}

impl ActorFactory {
    pub fn get_actor<T: ActorInterface>(&self, actor_id: ActorId) -> ActorRef<T> {
        ActorRef::new(actor_id, self.runtime.clone())
    }

    pub fn get_actor_with_guid<T: ActorInterface>(&self, guid: Uuid) -> ActorRef<T> {
        let actor_id = ActorId::from_guid(T::ACTOR_TYPE, guid);
        self.get_actor(actor_id)
    }

    pub fn get_actor_with_string<T: ActorInterface>(&self, key: String) -> ActorRef<T> {
        let actor_id = ActorId::from_string(T::ACTOR_TYPE, key);
        self.get_actor(actor_id)
    }
}
```

### 5. Request Trait

```rust
pub trait Request: Send {
    type Response: Send;

    fn method_name(&self) -> &'static str;
    fn interface_name(&self) -> &'static str;

    async fn invoke(&mut self, target: &dyn Any) -> Result<Self::Response, InvokeError>;
}

// Example generated request
pub struct GetGreetingRequest {
    pub name: String,
}

impl Request for GetGreetingRequest {
    type Response = String;

    fn method_name(&self) -> &'static str {
        "GetGreeting"
    }

    fn interface_name(&self) -> &'static str {
        "IMyActor"
    }

    async fn invoke(&mut self, target: &dyn Any) -> Result<String, InvokeError> {
        let actor = target.downcast_ref::<MyActor>().ok_or(InvokeError::TypeMismatch)?;
        actor.get_greeting(&self.name).await
    }
}
```

---

## References

### Source Files

**GrainReference**:
- `GrainReference.cs:21-82` - GrainReferenceShared (Flyweight)
- `GrainReference.cs:84-116` - GrainReferenceCodec (general IAddressable serialization)
- `GrainReference.cs:118-154` - GrainReferenceCopier, GrainReferenceCopierProvider, TypedGrainReferenceCopier
- `GrainReference.cs:159-181` - GrainReferenceCodecProvider (specialized codec factory)
- `GrainReference.cs:187-225` - TypedGrainReferenceCodec for typed serialization
- `GrainReference.cs:230-244` - GrainReferenceSurrogate for serialization
- `GrainReference.cs:249-435` - Base class with identity, equality, invocation
- `GrainReference.cs:440-532` - IRequest interface with static formatting helpers
- `GrainReference.cs:539-605` - RequestBase with cancellation and tracing support
- `GrainReference.cs:611-821` - Request, Request<T>, TaskRequest<T>, TaskRequest, VoidRequest

**AsyncEnumerable Support**:
- `AsyncEnumerableRequest.cs:156-186` - AsyncEnumerableRequest<T> base type
- `AsyncEnumerableRequest.cs:191-360` - AsyncEnumeratorProxy<T> (client-side batching proxy)
- `AsyncEnumerableRequest.cs:79-130` - IAsyncEnumerableGrainExtension (grain extension interface)

**Activation and Factory**:
- `GrainReferenceActivator.cs:24-95` - Factory with provider chain
- `GrainReferenceActivator.cs:100-174` - UntypedGrainReferenceActivatorProvider
- `GrainReferenceActivator.cs:179-276` - RpcProvider for proxy type mapping
- `GrainReferenceActivator.cs:281-382` - GrainReferenceActivatorProvider with dynamic code gen

**Integration**:
- `GrainFactory.cs` - Creates grain references for application code (implements `IInternalGrainFactory`)
- `InsideRuntimeClient.cs` - Receives InvokeMethodAsync calls from references

### Related Analyses

- **message-routing.md** - How grain references route calls through InsideRuntimeClient
- **message-system.md** - Message structure for grain-to-grain calls
- **activation-lifecycle.md** - Target activations that receive invocations

---

## Summary

Orleans grain references provide:

1. **Type Safety** - Compile-time checked method calls via generated proxies
2. **Flyweight Pattern** - Share `GrainReferenceShared` across all references of same type+interface
3. **Surrogate Serialization** - Compact wire format (GrainId + InterfaceType only)
4. **Factory Pattern** - `GrainReferenceActivator` with provider chain
5. **Dynamic Proxy Generation** - Fast delegates created via `DynamicMethod`
6. **Value Semantics** - Equality based on GrainId
7. **Interface Versioning** - Track and enforce interface compatibility
8. **Async Enumerable Streaming** - `AsyncEnumerableRequest<T>` with batching, heartbeats, and cancellation

**Key Design Patterns**:
- **Flyweight** - Minimize memory overhead
- **Surrogate** - Efficient serialization
- **Proxy** - Transparent method forwarding
- **Factory** - Centralized creation with provider chain

For Moonpool, implement `ActorRef<T>` with similar patterns: flyweight shared state, surrogate serialization, and trait-based request dispatching.
