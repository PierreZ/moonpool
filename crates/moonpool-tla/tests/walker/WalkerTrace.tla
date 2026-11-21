---------------------------- MODULE WalkerTrace ----------------------------
(* The trace spec of Walker: "inc" logs the new value, "flip" logs         *)
(* nothing, "show" logs the hidden bit.                                    *)
EXTENDS Walker, Trace, Naturals, Sequences

VARIABLE l

e == Trace[l]

TraceInit == Init /\ l = 1

Step ==
  \/ e.event = "inc" /\ Inc /\ x' = e.x
  \/ e.event = "flip" /\ Flip
  \/ e.event = "show" /\ Show /\ h = e.h

TraceNext == l <= Len(Trace) /\ Step /\ l' = l + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<l, x, h>>

TraceUnfinished == l <= Len(Trace)
=============================================================================
