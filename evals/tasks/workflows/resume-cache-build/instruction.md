Repair the build fixture in this workspace so the pipeline runs to completion.

Scope of this task: the failure below is a failure inside this task's own fixture
script and build library. It is **not** a Mio restart, a crashed provider request,
a network outage, or a tool-runner failure. You do not need to restart anything.

Facts:

- `fixture_build.py` is a deterministic fixture. Against a fresh state directory
  the UNMODIFIED library makes its first attempt fail. After the library is fixed,
  a first attempt against a clean state directory MUST succeed. Do not preserve
  the original bug or introduce a deliberate first-run failure. The fixture must keep its exact current
  contents: do not edit it, and do not delete or rename it.
- Run it with `python3 fixture_build.py build`. It writes its artifacts to `out/`
  and appends one JSON line per attempt to `state/attempts.jsonl`.
- The genuine cause of the failure lives in the `buildlib` package. Diagnose it
  from the real error the fixture prints, fix it there, and run the fixture again
  until it completes and writes the artifacts.
- `data/` holds the inputs. `out/` and `state/` do not exist until the fixture
  creates them.

Acceptance re-runs the pinned fixture against your library from a clean state,
recomputes the expected artifacts itself, and also requires the journal in
`state/attempts.jsonl` to record a real failure before a successful attempt. A
source edit alone, with no executed recovery, is not accepted.

Use only the Python standard library.

The required sequence is: run the original broken library once to record its
error, repair the library, then run again successfully in that same workspace.
The external acceptance separately checks that your repaired library also works
on its FIRST run with a new empty state directory. These requirements do not
conflict: the error record comes from BEFORE the repair. Do not search outside
this workspace for acceptance files or try to reconstruct hidden tests.
