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
`RealmError::TerminalTask`; cross-Realm and stale handles have distinct errors.

After every terminal transition:

- continuation, request, scheduler token, completion reservation, and release
  reservation counts are zero;
- one terminal record remains observable;
- Host resources are released exactly once.

Failure injection returns a probe. A scenario is valid only when the probe was
consumed; otherwise its result is `SCENARIO_NOT_REACHED`.
