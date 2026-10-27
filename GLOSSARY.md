# Orchestrator Task Context

Manages asynchronous agent tasks, follow-up instructions, time-based schedules, user reminders, and automated knowledge-base maintenance across multiple `Orchestrator` flows for the personal homelab server and Task View UI.

## Language

**Task**:
A tracked unit of background work with a user-visible title, trigger source, live progress summary, and lifecycle status bound to a root session inside a specific Flow.
_Avoid_: Job, Background Process, Thread, Worker

**Flow**:
A named, isolated `Orchestrator` instance configured with its own agent definitions and tool permissions (such as `daily` for interactive assistant work and `wiki` for knowledge-base compilation).
_Avoid_: Pipeline, Engine, Instance, Cluster

**Target Agent**:
A root-capable agent definition within a Flow that can accept a new Task session, exposed with its description so callers or a Voice Agent can select the right specialist.
_Avoid_: Worker Agent, Bot, Handler

**Followup**:
An additional instruction submitted to an existing Task that executes immediately if the Task's session is idle or waits in a FIFO queue if the session is currently running a turn.
_Avoid_: Message, Queued Input, Continuation, Reply

**Schedule**:
A time rule (one-shot timestamp or recurring cron expression) that either emits a Reminder notification or spawns a new Task when due.
_Avoid_: Cronjob, Timer, Alarm

**Reminder**:
A Schedule configured in notification-only mode with a priority level and optional `[[WikiLinks]]` references for user attention rather than autonomous agent execution.
_Avoid_: Alert, Todo, Notification

**Auto-Flow**:
The reactive process that inspects `WikiEngine` for uningested sources or structural lint issues and spawns a first-class Task on the `wiki` Flow to resolve them.
_Avoid_: Sync, Background Loop, Daemon

**Scope**:
The `(owner_id, workspace_id)` identity pair that isolates Tasks and Schedules (defaulting to `("personal", "homelab")` on the personal homelab server).
_Avoid_: Tenant, Account, Namespace

**Task Artifact**:
An immutable, integrity-verified deliverable or intermediate file produced by a Root Agent or Subagent during a Task's execution and owned by that Task's session family.
_Avoid_: Scratch File, Tool Output String, Unverified Attachment

**Artifact Catalog**:
A cross-task read view of committed Task Artifacts enriched with their originating Task context and aggregated counts by semantic kind and role.
_Avoid_: File System Browser, Raw Storage Dump, Media Bucket
