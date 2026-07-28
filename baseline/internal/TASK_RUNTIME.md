# Task Runtime

Version: **1.0.0**

The public Task lifecycle is:

```text
spawn_task
→ poll_task
→ Completed | Yielded | Waiting | Cancelled | Trapped
```

Only `TaskPoll::Waiting` exposes a `HostRequestHandle`. A valid request may be
completed or abandoned once. Resume happens by polling the Task again; callers
never manufacture state-machine events or a Task-complete transition.

Terminal polls are immutable. Polling a terminal Task returns
`RuntimeError::TerminalTask`; cross-Realm and stale Task handles have distinct
errors. Request completion distinguishes stale, cross-Realm, already-completed,
and restart-detached handles.

After every terminal transition:

- continuation, request, scheduler token, completion reservation, and release
  reservation counts are zero;
- one terminal record remains observable;
- Host resources are released exactly once.

The raw poll result and its pending reason are crate-private implementation
details. Product code, examples, integration tests, diagnostics, and benchmark
tools use only the public lifecycle above.

Failure injection returns a probe. A scenario is valid only when the probe was
consumed; otherwise its result is `SCENARIO_NOT_REACHED`.
