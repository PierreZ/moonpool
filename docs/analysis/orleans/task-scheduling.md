# Task Scheduling and Execution

## Reference Files
- `WorkItemGroup.cs:1-306` - Work group state machine and execution loop
- `ActivationTaskScheduler.cs:1-157` - Custom TaskScheduler for single-threaded execution (uses source-generated logging)

## Overview

Orleans achieves **single-threaded execution per activation** through a custom scheduling system. Each activation has:
- **WorkItemGroup**: State machine managing task queue and execution
- **ActivationTaskScheduler**: Custom `TaskScheduler` ensuring tasks run in correct context
- **Quantum-based processing**: Execute multiple tasks up to time limit
- **Long-running task detection**: Warn and track slow operations

This design provides:
- ✅ **Deterministic execution**: No concurrent access to activation state
- ✅ **Turn-based processing**: Multiple messages processed per activation turn
- ✅ **Fair scheduling**: Activations yield after quantum expires
- ✅ **Deadlock prevention**: No locks held during task execution

---

## WorkItemGroup: State Machine and Execution

### Purpose
WorkItemGroup is the execution context for a single activation. It:
- Maintains a queue of tasks to execute
- Transitions through states based on work availability
- Executes tasks in quantum-sized batches
- Integrates with ThreadPool for scheduling

**Code**: `WorkItemGroup.cs:17-60` - Core structure

```csharp
internal sealed class WorkItemGroup : IThreadPoolWorkItem, IWorkItemScheduler
{
    private enum WorkGroupStatus : byte
    {
        Waiting = 0,   // No work, not scheduled
        Runnable = 1,  // Has work, scheduled on thread pool
        Running = 2    // Currently executing on thread
    }

    private readonly ILogger _log;
    private readonly object _lockObj = new();
    private readonly Queue<Task> _workItems = new();
    private readonly SchedulingOptions _schedulingOptions;

    private long _totalItemsEnqueued;
    private long _totalItemsProcessed;
    private long _lastLongQueueWarningTimestamp;

    private WorkGroupStatus _state;
    private Task? _currentTask;
    private long _currentTaskStarted;

    internal ActivationTaskScheduler TaskScheduler { get; }
    public IGrainContext GrainContext { get; set; }
}
```

**Additional public API** (lines 299-304):
```csharp
public static void ScheduleExecution(WorkItemGroup workItem)
    => ThreadPool.UnsafeQueueUserWorkItem(workItem, preferLocal: true);

public void QueueAction(Action action) => TaskScheduler.QueueAction(action);
public void QueueAction(Action<object> action, object state) => TaskScheduler.QueueAction(action, state);
public void QueueTask(Task task) => task.Start(TaskScheduler);
```

### State Machine

```
[Waiting] ⇄ [Runnable] ⇄ [Running]
```

**Transitions**:

1. **Waiting → Runnable** (lines 98-114)
   - Triggered by: `EnqueueTask()` when queue was empty
   - Action: Call `ScheduleExecution(this)` to queue on ThreadPool
   ```csharp
   lock (_lockObj)
   {
       _workItems.Enqueue(task);
       if (_state != WorkGroupStatus.Waiting) return;

       _state = WorkGroupStatus.Runnable;
       ScheduleExecution(this);  // ThreadPool.UnsafeQueueUserWorkItem(this, preferLocal: true)
   }
   ```

2. **Runnable → Running** (lines 157-172)
   - Triggered by: ThreadPool starts executing `Execute()`
   - Action: Begin processing tasks
   ```csharp
   lock (_lockObj)
   {
       _state = WorkGroupStatus.Running;
       if (_workItems.Count > 0) {
           _currentTask = task = _workItems.Dequeue();
           _currentTaskStarted = taskStart;
       }
   }
   ```

3. **Running → Runnable** (lines 207-218)
   - Triggered by: Quantum expired but more work remains
   - Action: Re-queue on ThreadPool
   ```csharp
   lock (_lockObj)
   {
       if (_workItems.Count > 0) {
           _state = WorkGroupStatus.Runnable;
           ScheduleExecution(this);
       } else {
           _state = WorkGroupStatus.Waiting;
       }
   }
   ```

4. **Running → Waiting** (lines 207-218)
   - Triggered by: Work queue empty after execution
   - Action: Nothing, wait for new tasks

**Critical Invariants**:
- Only one thread executes `Execute()` at a time (enforced by state machine)
- State transitions happen under lock
- Re-scheduling happens before releasing lock (no lost work)

### Task Execution Loop

**Code**: `WorkItemGroup.cs:143-222`

```csharp
public void Execute()
{
    RuntimeContext.SetExecutionContext(GrainContext, out var originalContext);
    var turnWarningDurationMs = (long)Math.Ceiling(_schedulingOptions.TurnWarningLengthThreshold.TotalMilliseconds);
    var activationSchedulingQuantumMs = (long)_schedulingOptions.ActivationSchedulingQuantum.TotalMilliseconds;

    try
    {
        long loopStart, taskStart, taskEnd;
        loopStart = taskStart = taskEnd = Environment.TickCount64;

        do
        {
            Task task;
            lock (_lockObj)
            {
                _state = WorkGroupStatus.Running;

                if (_workItems.Count > 0)
                {
                    _currentTask = task = _workItems.Dequeue();
                    _currentTaskStarted = taskStart;
                }
                else
                {
                    break;  // Queue empty, done
                }
            }

            try
            {
                TaskScheduler.RunTaskFromWorkItemGroup(task);
            }
            finally
            {
                _totalItemsProcessed++;
                taskEnd = Environment.TickCount64;
                var taskDurationMs = taskEnd - taskStart;
                taskStart = taskEnd;

                if (taskDurationMs > turnWarningDurationMs)
                {
                    SchedulerInstruments.LongRunningTurnsCounter.Add(1);
                    LogLongRunningTurn(task, taskDurationMs);
                }

                _currentTask = null;
            }
        }
        while (activationSchedulingQuantumMs <= 0 || taskEnd - loopStart < activationSchedulingQuantumMs);
    }
    catch (Exception ex)
    {
        LogTaskLoopError(ex);
    }
    finally
    {
        lock (_lockObj)
        {
            if (_workItems.Count > 0)
            {
                _state = WorkGroupStatus.Runnable;
                ScheduleExecution(this);
            }
            else
            {
                _state = WorkGroupStatus.Waiting;
            }
        }

        RuntimeContext.ResetExecutionContext(originalContext);
    }
}
```

**Execution Pattern**:

1. **Set execution context** (line 145)
   - `RuntimeContext.SetExecutionContext(GrainContext, ...)`
   - Makes this activation the "current" grain for thread-local access

2. **Loop until quantum expires** (line 196)
   - Default quantum: configurable via `ActivationSchedulingQuantum`
   - Loop condition: `taskEnd - loopStart < activationSchedulingQuantumMs`
   - If quantum ≤ 0, drain entire queue (no time limit)

3. **Per-task processing** (lines 156-194)
   - Lock → dequeue task → unlock
   - Execute via `TaskScheduler.RunTaskFromWorkItemGroup(task)`
   - Track duration, warn if exceeds `TurnWarningLengthThreshold`
   - Clear `_currentTask` (for diagnostics)

4. **Re-schedule if needed** (lines 207-218)
   - Lock again after loop
   - If work remains, transition to Runnable and re-queue
   - Otherwise transition to Waiting

5. **Restore context** (line 220)
   - `RuntimeContext.ResetExecutionContext(originalContext)`
   - Cleanup for next activation

**Why quantum-based?**
- Prevents starvation: One slow activation can't block others indefinitely
- Fair scheduling: All activations get CPU time
- Configurable: Can tune for latency vs throughput

### Enqueue Task

**Code**: `WorkItemGroup.cs:67-116`

```csharp
public void EnqueueTask(Task task)
{
    lock (_lockObj)
    {
        long thisSequenceNumber = _totalItemsEnqueued++;
        int count = _workItems.Count;

        _workItems.Enqueue(task);

        // Overload detection
        int maxPendingItemsLimit = _schedulingOptions.MaxPendingWorkItemsSoftLimit;
        if (maxPendingItemsLimit > 0 && count > maxPendingItemsLimit)
        {
            var now = Environment.TickCount64;
            if (now > _lastLongQueueWarningTimestamp + 10_000)
            {
                LogTooManyTasksInQueue(count, maxPendingItemsLimit);
            }

            _lastLongQueueWarningTimestamp = now;  // Updated on every overload check, not just when warning
        }

        // Schedule if was waiting
        if (_state != WorkGroupStatus.Waiting) return;

        _state = WorkGroupStatus.Runnable;
        ScheduleExecution(this);
    }
}
```

**Key Features**:

1. **Queue depth tracking** (line 82)
   - `_totalItemsEnqueued` increments on each enqueue
   - Used for instrumentation and debugging

2. **Overload warning** (lines 86-96)
   - Check if queue exceeds soft limit
   - Warn at most once per 10 seconds (rate limiting)
   - Non-blocking: Just logs warning, doesn't reject

3. **State-based scheduling** (lines 98-114)
   - Only schedule if state is Waiting
   - If already Runnable or Running, task will be picked up

4. **ThreadPool integration** (line 114, line 300)
   - `ScheduleExecution(this)` → `ThreadPool.UnsafeQueueUserWorkItem(this, preferLocal: true)`
   - `preferLocal: true` - prefer current thread's local queue for cache locality

### Long-Running Task Detection

**Code**: `WorkItemGroup.cs:248-265`

```csharp
private void LogLongRunningTurn(Task task, long taskDurationMs)
{
    if (Debugger.IsAttached) return;  // Don't warn during debugging

    var taskDuration = TimeSpan.FromMilliseconds(taskDurationMs);
    _log.LogWarning(
        (int)ErrorCode.SchedulerTurnTooLong3,
        "Task {Task} in WorkGroup {GrainContext} took elapsed time {Duration} for execution, " +
        "which is longer than {TurnWarningLengthThreshold}. Running on thread {Thread}",
        task.AsyncState ?? task,
        GrainContext.ToString(),
        taskDuration.ToString("g"),
        _schedulingOptions.TurnWarningLengthThreshold,
        Environment.CurrentManagedThreadId.ToString());
}
```

**Triggered when**: `taskDurationMs > TurnWarningLengthThreshold` (line 187)

**Purpose**:
- Detect blocking operations in async code (e.g., `.Result`, `Thread.Sleep`)
- Identify performance problems
- Help developers write async-friendly code

**Note**: Suppressed when debugger attached (line 251) to avoid false positives during step-through debugging

### Diagnostic Support

**Code**: `WorkItemGroup.cs:269-297`

```csharp
public string DumpStatus()
{
    lock (_lockObj)
    {
        var sb = new StringBuilder();
        sb.Append(this);
        sb.AppendFormat(". Currently QueuedWorkItems={0}; Total Enqueued={1}; Total processed={2}; ",
            _workItems.Count, _totalItemsEnqueued, _totalItemsProcessed);

        if (_currentTask is Task task)
        {
            sb.AppendFormat(" Executing Task Id={0} Status={1} for {2}.",
                task.Id, task.Status, TimeSpan.FromMilliseconds(Environment.TickCount64 - _currentTaskStarted));
        }

        sb.AppendFormat("TaskRunner={0}; ", TaskScheduler);

        if (GrainContext != null)
        {
            var detailedStatus = GrainContext switch
            {
                ActivationData activationData => activationData.ToDetailedString(includeExtraDetails: true),
                SystemTarget systemTarget => systemTarget.ToDetailedString(),
                object obj => obj.ToString(),
                _ => "None"
            };
            sb.AppendFormat("Detailed context=<{0}>", detailedStatus);
        }

        return sb.ToString();
    }
}
```

**Usage**: Called from workload analysis, diagnostics messages, debugging

**Information provided**:
- Queue depth
- Total enqueued/processed counts
- Currently executing task (if any) with duration
- Detailed grain context state

**Called from**: `ActivationData.AnalyzeWorkload()` (ActivationData.cs:640)

---

## ActivationTaskScheduler: Context Control

### Purpose
Custom `TaskScheduler` that:
- Routes tasks to the correct `WorkItemGroup`
- Enables inline execution optimization
- Ensures tasks run in correct grain context

**Code**: `ActivationTaskScheduler.cs:16-36`

```csharp
internal sealed partial class ActivationTaskScheduler : TaskScheduler
{
    private readonly ILogger logger;
    private readonly long myId;
    private readonly WorkItemGroup workerGroup;

    internal ActivationTaskScheduler(WorkItemGroup workGroup, ILogger<ActivationTaskScheduler> logger)
    {
        this.logger = logger;
        myId = Interlocked.Increment(ref idCounter);
        workerGroup = workGroup;
        LogCreatedTaskScheduler(this, workerGroup.GrainContext);
    }
}
```

**Relationship**: One `ActivationTaskScheduler` per `WorkItemGroup` (line 39: `TaskScheduler { get; }`)

### Queue Task

**Code**: `ActivationTaskScheduler.cs:56-62`

```csharp
protected override void QueueTask(Task task)
{
#if DEBUG
    LogTraceQueueTask(myId, task.Id);  // Source-generated logger method
#endif
    workerGroup.EnqueueTask(task);
}
```

**When called**: When code calls `Task.Start(scheduler)` or `Task.Factory.StartNew(..., scheduler)`

**Action**: Delegates to `WorkItemGroup.EnqueueTask()`

### Try Execute Task Inline

**Code**: `ActivationTaskScheduler.cs:72-110`

```csharp
protected override bool TryExecuteTaskInline(Task task, bool taskWasPreviouslyQueued)
{
    var canExecuteInline = !taskWasPreviouslyQueued
        && Equals(RuntimeContext.Current, workerGroup.GrainContext);

#if DEBUG
    LogTraceTryExecuteTaskInline(  // Source-generated logger method
        myId, task.Id, task.Status, taskWasPreviouslyQueued, canExecuteInline,
        workerGroup.ExternalWorkItemCount);
#endif
    if (!canExecuteInline)
    {
#if DEBUG
        LogTraceTryExecuteTaskInlineNotDone(myId, task.Id, task.Status);
#endif
        return false;
    }

    // Try to run the task.
    bool done = TryExecuteTask(task);
#if DEBUG
    if (!done)
    {
        LogWarnTryExecuteTaskNotDone(task.Id, task.Status);
    }
    LogTraceTryExecuteTaskInlineCompleted(myId, task.Id, Environment.CurrentManagedThreadId, done);
#endif
    return done;
}
```

**When called**: When code awaits a task or calls `.Wait()`, .NET may try to execute inline

**Conditions for inline execution**:
1. `!taskWasPreviouslyQueued` - Task wasn't already queued (avoid re-ordering)
2. `RuntimeContext.Current == workerGroup.GrainContext` - Already in correct context

**Why this matters**:
- Optimization: Avoid context switch if already in correct grain
- Safety: Only inline if context is correct (prevents cross-activation contamination)

### Run Task From Work Item Group

**Code**: `ActivationTaskScheduler.cs:42-52`

```csharp
[MethodImpl(MethodImplOptions.AggressiveInlining)]
internal void RunTaskFromWorkItemGroup(Task task)
{
    bool done = TryExecuteTask(task);
    if (!done)
    {
#if DEBUG
        LogWarnTryExecuteTaskNotDone(task.Id, task.Status);
#endif
    }
}
```

**Called by**: `WorkItemGroup.Execute()` (line 179)

**Action**: Executes task using base `TaskScheduler.TryExecuteTask()` which:
1. Transitions task to "Running" state
2. Invokes task's delegate
3. Transitions to "Completed" or "Faulted"
4. Triggers continuations

**Why separate method?**: Called from `WorkItemGroup`, not TaskScheduler infrastructure

---

## Single-Threaded Execution Guarantee

### How Orleans Achieves Determinism

**Key Mechanisms**:

1. **WorkItemGroup State Machine**
   - Only one state allows execution: `Running`
   - State transitions prevent concurrent `Execute()` calls
   - ThreadPool won't re-queue while `Running`

2. **Task Serialization**
   - All tasks enqueued to single `Queue<Task>`
   - Processed one at a time in FIFO order
   - No parallel execution within activation

3. **RuntimeContext Enforcement**
   - `RuntimeContext.SetExecutionContext(GrainContext)` on entry
   - `RuntimeContext.ResetExecutionContext()` on exit
   - Thread-local storage ensures correct context

4. **Custom TaskScheduler**
   - Routes all tasks through `WorkItemGroup`
   - Inline execution only if already in correct context
   - Prevents accidental cross-activation execution

**Proof of Single-Threading**:

```
Thread T1:
  WorkItemGroup.Execute()
    lock (_lockObj) { _state = Running; }  // T1 sets Running
    ... execute tasks ...
    lock (_lockObj) {
      if (queue not empty) {
        _state = Runnable;
        ScheduleExecution(this);  // Queue for later
      }
    }

Thread T2 (ThreadPool):
  WorkItemGroup.Execute()  // Dequeued from ThreadPool
    lock (_lockObj) {
      _state = Running;  // But _state is already Running!
      // Actually, this can't happen because...
    }
```

**Wait, what prevents double execution?**

The key is: `ScheduleExecution()` only called when transitioning from `Waiting` to `Runnable` (or `Running` to `Runnable`).

Once queued on ThreadPool, no more queueing happens until:
1. ThreadPool picks up work item
2. Executes it
3. Transitions to Waiting (if queue empty) or re-queues (if more work)

**So the guarantee is**: At most one ThreadPool work item queued at a time per `WorkItemGroup`.

### Comparison to Actor Model

| Aspect | Orleans | Erlang | Akka |
|--------|---------|--------|------|
| **Scheduling** | Custom TaskScheduler + WorkItemGroup | VM-level scheduler | Dispatcher pool |
| **Mailbox** | `Queue<Task>` | Process mailbox | `Mailbox` |
| **Execution** | Quantum-based (time limit) | Reduction-based (instruction count) | Quantum-based |
| **Fair scheduling** | Re-queue after quantum | Preemptive scheduler | Dispatcher rotation |
| **Context** | RuntimeContext (thread-local) | Process dictionary | ActorCell |

**Orleans advantage**: Integrates with .NET TaskScheduler, enabling `async/await`

---

## Configuration Options

**Code**: `SchedulingOptions.cs` (in `Orleans.Configuration` namespace)

Key settings:

1. **ActivationSchedulingQuantum** (referenced at WorkItemGroup.cs:147)
   - Default: `100ms` (`DEFAULT_ACTIVATION_SCHEDULING_QUANTUM = TimeSpan.FromMilliseconds(100)`)
   - Controls how long activation can execute before yielding
   - 0 or negative: drain entire queue

2. **TurnWarningLengthThreshold** (referenced at WorkItemGroup.cs:146)
   - Default: `1000ms` (`DEFAULT_TURN_WARNING_THRESHOLD = TimeSpan.FromMilliseconds(1_000)`)
   - Threshold for logging long-running turns
   - Helps detect blocking code

3. **MaxPendingWorkItemsSoftLimit** (referenced at WorkItemGroup.cs:86)
   - Default: `0` (disabled) (`DEFAULT_MAX_PENDING_ITEMS_SOFT_LIMIT = 0`)
   - Warning threshold for queue depth
   - Doesn't reject messages, just logs

4. **DelayWarningThreshold**
   - Default: `10000ms` (`DEFAULT_DELAY_WARNING_THRESHOLD = TimeSpan.FromMilliseconds(10000)`)
   - Threshold for warning about delay between enqueue and execution

5. **StoppedActivationWarningInterval**
   - Default: `1 minute` (`TimeSpan.FromMinutes(1)`)
   - Period after which to log errors for tasks scheduled to stopped activations

**Tuning trade-offs**:
- Shorter quantum: Lower latency, more context switches
- Longer quantum: Higher throughput, risk of starvation
- Lower warning threshold: Earlier detection, more log noise

---

## Patterns for Moonpool

### ✅ Adopt These Patterns

1. **State Machine for Scheduling**: `Waiting → Runnable → Running`
   ```rust
   enum WorkGroupState {
       Waiting,   // No work queued
       Runnable,  // Work queued, scheduled for execution
       Running,   // Currently executing
   }
   ```

2. **Quantum-Based Execution**: Process tasks until time limit
   ```rust
   let quantum_ms = config.scheduling_quantum.as_millis();
   let loop_start = Instant::now();
   while loop_start.elapsed().as_millis() < quantum_ms {
       let Some(task) = tasks.pop_front() else { break };
       task.execute();
   }
   ```

3. **Long-Running Task Detection**: Warn if task exceeds threshold
   ```rust
   let task_start = Instant::now();
   task.execute();
   let duration = task_start.elapsed();
   if duration > config.turn_warning_threshold {
       log::warn!("Task ran for {duration:?}");
   }
   ```

4. **Context Enforcement**: Use Provider traits instead of thread-local
   ```rust
   pub struct ActorContext<'a> {
       actor_id: ActorId,
       time: &'a dyn TimeProvider,
       task: &'a dyn TaskProvider,
       // ...
   }
   ```

5. **Queue Depth Monitoring**: Track and warn on overload
   ```rust
   if tasks.len() > config.max_pending_soft_limit {
       sometimes_assert!(tasks.len() <= config.max_pending_hard_limit);
   }
   ```

### ⚠️ Adapt These Patterns

1. **ThreadPool Integration**: Orleans uses `IThreadPoolWorkItem`
   - Moonpool: Single-threaded runtime, use `spawn_local` via `TaskProvider`
   - No need for work-stealing or thread pool

2. **Custom TaskScheduler**: Orleans integrates with .NET's `TaskScheduler`
   - Moonpool: No direct equivalent, use `spawn_local` for spawning tasks
   - Context passed explicitly, not thread-local

3. **RuntimeContext Thread-Local**: Orleans uses `AsyncLocal<RuntimeContext>`
   - Moonpool: Pass `ActorContext` explicitly or use tokio task-local
   - More explicit, better for simulation

4. **Inline Execution Optimization**: Orleans checks if can execute inline
   - Moonpool: Less important with single-threaded runtime
   - May still be useful for synchronous fast-path

### ❌ Avoid These Patterns

1. **Multiple Threads per Activation**: Orleans prevents this, moonpool doesn't need it
   - Moonpool: Single-threaded by design

2. **Lock-based State Machine**: Orleans uses locks for state transitions
   - Moonpool: Single-threaded, no locks needed (just mutable borrows)

3. **Separate Enqueue and Schedule**: Orleans splits concerns
   - Moonpool: Can combine since no concurrency

---

## Critical Insights for Phase 12

### WorkItemGroup as Inbox Model

Orleans' `WorkItemGroup` maps cleanly to Phase 12 Step 4 (Actor Inbox):

```
Orleans WorkItemGroup          Moonpool Inbox
├── Queue<Task> _workItems     ├── VecDeque<Message> waiting
├── WorkGroupStatus _state     ├── InboxState state
├── Execute() loop             ├── process_messages() loop
├── EnqueueTask()              ├── enqueue()
└── Quantum-based processing   └── Quantum-based processing
```

**Implementation strategy**:
1. Start with `VecDeque<Message>` for waiting messages
2. Use `enum InboxState { Empty, HasMessages, Paused, Draining }`
3. Implement quantum-based processing (configurable time limit)
4. Add long-running message detection
5. Expose state via `StateRegistry` for invariants

### Single-Threaded Simplification

Moonpool's single-threaded simulation simplifies Orleans' multi-threaded design:

| Orleans (Multi-threaded) | Moonpool (Single-threaded) |
|-------------------------|---------------------------|
| `lock (_lockObj)` | No lock needed |
| `Interlocked.Increment()` | Simple `+= 1` |
| `ConcurrentDictionary` | `HashMap` |
| State machine prevents races | No races possible |
| ThreadPool.UnsafeQueueUserWorkItem | `spawn_local` |

**Advantage**: Simpler code, easier to reason about, deterministic by design

### Testing Strategy

From Orleans' patterns:

1. **Quantum enforcement**: Test that activations yield after quantum
2. **Long-running detection**: Use buggify to inject delays, verify warnings
3. **Queue depth**: Test soft/hard limit warnings
4. **Fair scheduling**: Multiple activations, verify all make progress
5. **Context isolation**: Verify messages don't cross activations

**Invariants to check**:
- Queue state matches queue size (Empty ↔ size == 0)
- Message conservation: enqueued = processed + queued
- No activation processes >1 message concurrently

---

## Summary

Orleans achieves single-threaded execution per activation through:
- **WorkItemGroup**: State machine with quantum-based execution
- **ActivationTaskScheduler**: Custom scheduler for context control
- **RuntimeContext**: Thread-local context enforcement
- **Long-running detection**: Warn on slow tasks
- **Fair scheduling**: Re-queue after quantum expires

Key architectural principles:
- State machine prevents concurrent execution
- Quantum-based processing for fairness
- Inline execution optimization when safe
- Comprehensive diagnostics and monitoring

For Moonpool Phase 12: Simplify by removing multi-threading concerns, use Provider traits for dependencies, implement quantum-based message processing with long-running detection.
