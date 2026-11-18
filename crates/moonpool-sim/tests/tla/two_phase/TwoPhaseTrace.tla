--------------------------- MODULE TwoPhaseTrace ---------------------------
(* The trace spec: TwoPhase, forced to follow the events of one run.       *)
(* `l` is the index of the next event. Every action of the code is logged, *)
(* with the state of the actor after the step (a projection), and the      *)
(* harness logs each fault it injects, so TLC has nothing to guess.        *)
EXTENDS TwoPhase, Trace, Naturals, Sequences

VARIABLE l

e == Trace[l]

TraceInit == TPInit /\ l = 1

RMState(r) == rmState'[r] = e.state

Step ==
  \/ e.event = "rm_prepare"      /\ RMPrepare(e.rm)       /\ RMState(e.rm)
  \/ e.event = "rm_choose_abort" /\ RMChooseToAbort(e.rm) /\ RMState(e.rm)
  \/ e.event = "rm_rcv_commit"   /\ RMRcvCommitMsg(e.rm)  /\ RMState(e.rm)
  \/ e.event = "rm_rcv_abort"    /\ RMRcvAbortMsg(e.rm)   /\ RMState(e.rm)
  \/ e.event = "tm_rcv_prepared" /\ TMRcvPrepared(e.rm)
  \/ e.event = "tm_commit"       /\ TMCommit              /\ tmState' = e.state
  \/ e.event = "tm_abort"        /\ TMAbort               /\ tmState' = e.state
  (* A lost or repeated message: the model keeps every message in a set. *)
  \/ e.event \in {"msg_drop", "msg_duplicate"} /\ UNCHANGED vars

TraceNext == l <= Len(Trace) /\ Step /\ l' = l + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<l, vars>>

(* Violated exactly when a behaviour matched every event: the trace is     *)
(* accepted.                                                               *)
TraceUnfinished == l <= Len(Trace)
=============================================================================
