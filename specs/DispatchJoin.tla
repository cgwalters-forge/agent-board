--------------------------- MODULE DispatchJoin ---------------------------
EXTENDS Naturals, FiniteSets

\* A bounded model of the operator-driven preview seam, NOT future admission.
\* Items are open Todo issues with Coordinator turn and complete schema/data.
\* Each of two dispatchers can launch once; its name also identifies its run.
CONSTANTS Items, Dispatchers, Mode
VARIABLES phase, selected, live, recorded
vars == <<phase, selected, live, recorded>>

\* A state is these four values together. Functions map dispatcher/item IDs
\* to values. 0 means no selection/Run; sets hold actual live Actions runs.
Init == /\ phase = [d \in Dispatchers |-> "idle"]
        /\ selected = [d \in Dispatchers |-> 0]
        /\ live = [i \in Items |-> {}]
        /\ recorded = [i \in Items |-> 0]

\* An action relates old values to new (primed) values. /\ means AND;
\* EXCEPT changes one function entry; UNCHANGED keeps other values fixed.
\* command() in dispatch.rs checks the snapshot's empty Run and eligibility.
\* Printing argv does NOT reserve an item or change its project fields.
Prepare(d, i) ==
    /\ phase[d] = "idle"
    /\ recorded[i] = 0
    /\ phase' = [phase EXCEPT ![d] = "prepared"]
    /\ selected' = [selected EXCEPT ![d] = i]
    /\ UNCHANGED <<live, recorded>>

\* External step: the operator approves/executes the printed gh workflow run.
\* This is not a Rust mutation. There is no claim or fresh check in argv.
Start(d) ==
    /\ phase[d] = "prepared"
    /\ phase' = [phase EXCEPT ![d] = "started"]
    /\ live' = [live EXCEPT ![selected[d]] = @ \cup {d}]
    /\ UNCHANGED <<selected, recorded>>

\* proposal() refuses replacing another Run. Grant its external applier an
\* atomic fresh check here (stronger than today's preview-only code).
\* Recording sets In Progress/Worker, abstracted by recorded being nonzero.
\* Even this favorable assumption cannot prevent two runs BEFORE recording.
Record(d) ==
    /\ phase[d] = "started"
    /\ recorded[selected[d]] \in {0, d}
    /\ recorded' = [recorded EXCEPT ![selected[d]] = d]
    /\ phase' = [phase EXCEPT ![d] = "recorded"]
    /\ UNCHANGED <<selected, live>>

\* Negative control: deliberately omit even the snapshot's empty Run check.
\* Combine selection and launch to distinguish this from the stale-preview
\* race above. Two unchecked launches must violate the very same invariant.
BrokenStart(d, i) ==
    /\ phase[d] = "idle"
    /\ phase' = [phase EXCEPT ![d] = "started"]
    /\ selected' = [selected EXCEPT ![d] = i]
    /\ live' = [live EXCEPT ![i] = @ \cup {d}]
    /\ UNCHANGED recorded

\* \/ means OR; \E means 'there exists': TLC tries every enabled choice.
\* Preview is the actual CLI alone: no external workflow execution at all.
Next == \/ /\ Mode \in {"preview", "current"}
           /\ \E d \in Dispatchers, i \in Items : Prepare(d, i)
        \/ /\ Mode = "current"
           /\ \E d \in Dispatchers : Start(d) \/ Record(d)
        \/ /\ Mode = "broken"
           /\ \E d \in Dispatchers, i \in Items : BrokenStart(d, i)

\* [] means 'always'; [Next]_vars permits doing nothing (stuttering).
\* No fairness/liveness: we ask only what can go wrong, not what must finish.
Spec == Init /\ [][Next]_vars

\* An invariant must hold in EVERY reachable state, not just the last one.
OneLiveRun == \A i \in Items : Cardinality(live[i]) <= 1
=============================================================================
