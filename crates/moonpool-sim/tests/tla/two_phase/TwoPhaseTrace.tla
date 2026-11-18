--------------------------- MODULE TwoPhaseTrace ---------------------------
(* The trace spec: TwoPhase, forced to follow the events of one run.       *)
(* `l` is the index of the next event. Every action of the code is logged, *)
(* with the state of the actor after the step as the code reads it back    *)
(* (a projection), and the harness logs each fault it injects, so TLC has  *)
(* nothing to guess. Every step is a step of TwoPhase, so the code can     *)
(* only fail by a rejection: TypeOK and Consistent check the model itself. *)
EXTENDS TwoPhase, Trace, Naturals, Sequences, FiniteSets

VARIABLE l

e == Trace[l]

TraceInit == TPInit /\ l = 1

RMState(r) == rmState'[r] = e.state

(* An event about a resource manager names one of RM: a malformed event is *)
(* a rejection, not a TLC error.                                           *)
RMEvent == "rm" \in DOMAIN e /\ e.rm \in RM

Step ==
  \/ e.event = "rm_prepare"      /\ RMEvent /\ RMPrepare(e.rm)       /\ RMState(e.rm)
  \/ e.event = "rm_choose_abort" /\ RMEvent /\ RMChooseToAbort(e.rm) /\ RMState(e.rm)
  \/ e.event = "rm_rcv_commit"   /\ RMEvent /\ RMRcvCommitMsg(e.rm)  /\ RMState(e.rm)
  \/ e.event = "rm_rcv_abort"    /\ RMEvent /\ RMRcvAbortMsg(e.rm)   /\ RMState(e.rm)
  \* The TM logs how many votes it holds: a TM that loses or double-counts
  \* a vote does not match the model's tmPrepared.
  \/ e.event = "tm_rcv_prepared" /\ RMEvent /\ TMRcvPrepared(e.rm)
                                 /\ Cardinality(tmPrepared') = e.prepared
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
