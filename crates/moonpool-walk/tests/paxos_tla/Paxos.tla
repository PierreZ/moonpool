------------------------------- MODULE Paxos -------------------------------
(* Single-decree Paxos, after Lamport's Paxos.tla. Every message ever sent *)
(* stays in `msgs`: a lost or repeated message needs no step. Ballots and  *)
(* values are positive integers, and 0 stands for "none".                  *)
EXTENDS Integers, FiniteSets

CONSTANT Acceptor

Quorum == {Q \in SUBSET Acceptor : 2 * Cardinality(Q) > Cardinality(Acceptor)}

VARIABLES maxBal,   \* the highest ballot each acceptor promised or accepted
          maxVBal,  \* the ballot of the value each acceptor accepted last
          maxVal,   \* that value
          msgs      \* every message ever sent

vars == <<maxBal, maxVBal, maxVal, msgs>>

Init == /\ maxBal = [a \in Acceptor |-> 0]
        /\ maxVBal = [a \in Acceptor |-> 0]
        /\ maxVal = [a \in Acceptor |-> 0]
        /\ msgs = {}

Send(m) == msgs' = msgs \cup {m}

(* A proposer opens ballot b. *)
Phase1a(b) ==
  /\ b > 0
  /\ Send([type |-> "1a", bal |-> b])
  /\ UNCHANGED <<maxBal, maxVBal, maxVal>>

(* Acceptor a promises ballot b, and reports what it accepted. *)
Phase1b(a, b) ==
  /\ [type |-> "1a", bal |-> b] \in msgs
  /\ b > maxBal[a]
  /\ maxBal' = [maxBal EXCEPT ![a] = b]
  /\ Send([type |-> "1b", acc |-> a, bal |-> b,
           mbal |-> maxVBal[a], mval |-> maxVal[a]])
  /\ UNCHANGED <<maxVBal, maxVal>>

(* The proposer of ballot b asks to accept v: a quorum promised b, and v   *)
(* is the value of the highest ballot that quorum reported, if any.        *)
Phase2a(b, v) ==
  /\ v > 0
  /\ ~ \E m \in msgs : m.type = "2a" /\ m.bal = b
  /\ \E Q \in Quorum :
       LET Q1b  == {m \in msgs : m.type = "1b" /\ m.acc \in Q /\ m.bal = b}
           Q1bv == {m \in Q1b : m.mbal > 0}
       IN  /\ \A a \in Q : \E m \in Q1b : m.acc = a
           /\ \/ Q1bv = {}
              \/ \E m \in Q1bv : /\ m.mval = v
                                 /\ \A mm \in Q1bv : m.mbal >= mm.mbal
  /\ Send([type |-> "2a", bal |-> b, val |-> v])
  /\ UNCHANGED <<maxBal, maxVBal, maxVal>>

(* Acceptor a accepts v at ballot b, unless it promised a higher ballot. *)
Phase2b(a, b, v) ==
  /\ [type |-> "2a", bal |-> b, val |-> v] \in msgs
  /\ b >= maxBal[a]
  /\ maxBal' = [maxBal EXCEPT ![a] = b]
  /\ maxVBal' = [maxVBal EXCEPT ![a] = b]
  /\ maxVal' = [maxVal EXCEPT ![a] = v]
  /\ Send([type |-> "2b", acc |-> a, bal |-> b, val |-> v])

(* A quorum accepted v at ballot b. *)
ChosenAt(b, v) ==
  \E Q \in Quorum :
    \A a \in Q : [type |-> "2b", acc |-> a, bal |-> b, val |-> v] \in msgs

Chosen == {m.val : m \in {m \in msgs : m.type = "2b" /\ ChosenAt(m.bal, m.val)}}

Agreement == Cardinality(Chosen) <= 1
=============================================================================
