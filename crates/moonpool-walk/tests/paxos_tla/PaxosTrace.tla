----------------------------- MODULE PaxosTrace -----------------------------
(* The trace spec: Paxos, forced to follow the events one walk recorded.   *)
(* `l` is the index of the next event. The model logs every step of the    *)
(* protocol with its arguments, so TLC has nothing to guess. A crash and a *)
(* restart are not steps: an acceptor's state is on its disk.              *)
EXTENDS Paxos, Trace, Naturals, Sequences

VARIABLE l

e == Trace[l]

TraceInit == Init /\ l = 1

(* An event about an acceptor names one: a malformed event is a rejection, *)
(* not a TLC error.                                                        *)
AcceptorEvent == "acc" \in DOMAIN e /\ e.acc \in Acceptor

Step ==
  \/ e.event = "prepare" /\ Phase1a(e.bal)
  \* The promise carries what the acceptor says it accepted: it must be
  \* what the spec's acceptor holds.
  \/ e.event = "promise" /\ AcceptorEvent /\ Phase1b(e.acc, e.bal)
                         /\ e.vbal = maxVBal[e.acc] /\ e.vval = maxVal[e.acc]
  \/ e.event = "propose" /\ Phase2a(e.bal, e.val)
  \/ e.event = "accept"  /\ AcceptorEvent /\ Phase2b(e.acc, e.bal, e.val)
  \/ e.event = "chosen"  /\ e.val \in Chosen /\ UNCHANGED vars

TraceNext == l <= Len(Trace) /\ Step /\ l' = l + 1

TraceSpec == TraceInit /\ [][TraceNext]_<<l, vars>>

(* Violated exactly when a behaviour matched every event: the trace is     *)
(* accepted.                                                               *)
TraceUnfinished == l <= Len(Trace)
=============================================================================
