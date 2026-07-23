---
id: TASK-119
title: 'libSceNet: coherent offline NetState (CtlGetState vs real sockets)'
status: To Do
assignee: []
created_date: '2026-07-15 05:09'
labels:
  - hle
  - tech-debt
dependencies: []
priority: low
ordinal: 123000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Code-review finding (task-113.3). The net layer is incoherent: sceNetCtlGetState reports DISCONNECTED (offline) while sceNetSocket/sceNetSendto are REAL host-backed sockets that actually reach the network, and pool/resolver hand back literal id 1 with no id table (Destroy accepts any id). A guest that ignores the link-state gate reaches the real network, contradicting the offline story. Introduce one NetState (link status + pool/resolver id table + an offline policy flag) consulted by both the control-plane stubs and the socket path, so connectivity is one authoritative decision. Low prio (Celeste runs offline today); revisit when a title's netcode matters.
<!-- SECTION:DESCRIPTION:END -->
