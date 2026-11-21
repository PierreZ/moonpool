------------------------------ MODULE Walker ------------------------------
(* A counter with a hidden bit. `Flip` sets the bit to 0 or 1 and logs    *)
(* nothing of it, so a trace spec must keep both states until `Show`      *)
(* logs the bit.                                                          *)
EXTENDS Naturals

CONSTANT Limit

VARIABLES x, h

Init == x = 0 /\ h = 0

Inc == x' = x + 1 /\ UNCHANGED h

Flip == h' \in {0, 1} /\ UNCHANGED x

Show == UNCHANGED <<x, h>>

Small == x < Limit
=============================================================================
