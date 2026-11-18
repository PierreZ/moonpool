---------------------------- MODULE CounterTrace ----------------------------
(* The trace spec of Counter: each "inc" event logs the new value. A       *)
(* "skip" event is a step the model does not take.                         *)
EXTENDS Counter, Trace, Naturals, Sequences

VARIABLE l

e == Trace[l]

TraceInit == Init /\ l = 1

Step == e.event = "inc" /\ Inc /\ x' = e.x

TraceNext == l <= Len(Trace) /\ Step /\ l' = l + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<l, x>>

TraceUnfinished == l <= Len(Trace)
=============================================================================
