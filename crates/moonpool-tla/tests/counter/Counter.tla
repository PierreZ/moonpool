------------------------------ MODULE Counter ------------------------------
(* A counter that only goes up by one, to at most 5. *)
EXTENDS Naturals

VARIABLE x

Init == x = 0

Inc == x < 5 /\ x' = x + 1

BelowFour == x < 4
=============================================================================
