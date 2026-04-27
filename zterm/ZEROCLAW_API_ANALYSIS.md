# ZeroClaw REST API Analysis

**Source**: zeroclaw-contrib.git crates/zeroclaw-gateway/src/api.rs  
**Gateway Version**: v0.7.3~beta  
**Date**: April 20, 2026

---

## API Architecture

ZeroClaw gateway provides a **hybrid REST + WebSocket API**:

1. **REST API**: Configuration, session management, memory, status
2. **WebSocket API**: Real-time chat/agent interaction (primary)
3. **Pairing Auth**: Bearer token + optional pairing mechanism

---

## Sessions API (Available)

| Endpoint | Method | Purpose | Status |
|----------|--------|---------|--------|
| `/api/sessions` | GET | List all sessions | ✅ Works |
| `/api/sessions/running` | GET | Get currently running sessions | ✅ Works |
| `/api/sessions/{id}/messages` | GET | Load persisted WebSocket chat transcript | ✅ Works |
| `/api/sessions/{id}` | PUT | Rename session | ✅ Works |
| `/api/sessions/{id}` | DELETE | Delete session | ✅ Works |
| `/api/sessions/{id}/state` | GET | Get session state/info | ✅ Works |
| **`POST /api/sessions`** | **❌ DOES NOT EXIST** | Create session | ❌ Missing |

---

## Key Findings

### ZTerm Expected (REST-based)
```
POST /api/sessions → Create session
POST /api/sessions/{id}/turn + SSE → Stream responses
GET /api/sessions/{id}/messages → Get history
```

### ZeroClaw Provides (WebSocket-based)
```
WebSocket /ws/session/{id} → Real-time interaction
REST /api/sessions/* → Session management only
GET /api/sessions/{id}/messages → Read-only transcript
```

---

## Critical Difference

**Primary interaction protocol**:
- ❌ ZTerm: REST streaming (SSE) — NOT PROVIDED
- ✅ ZeroClaw: WebSocket — PRIMARY INTERFACE

**Session creation**:
- ❌ No REST endpoint to create sessions
- Sessions managed via WebSocket or zeroclaw CLI

**Chat interface**:
- ❌ No `POST /api/sessions/{id}/turn` endpoint
- ✅ Use WebSocket instead

---

## Path Forward

### Option A: Adapt ZTerm to WebSocket (Recommended)
**Effort**: ~500-600 LOC modifications (30-40%)
- Replace SSE streaming with WebSocket handler
- Implement pairing flow for auth
- Use REST for session/config management

### Option B: ZTerm as REST Management-Only Tool
**Effort**: Simplify to ~800 LOC
- Drop chat/streaming
- Focus: sessions, config, memory, cron
- Complementary to `zc agent` CLI

### Option C: Clarify Intended Role
Ask: Is ZTerm meant to be:
- Primary chat client? → WebSocket essential
- Admin/management tool? → REST-only sufficient  
- Demo/learning tool? → Current design acceptable

---

## Conclusion

**ZTerm is production-ready code** but targets zeroclaw's REST streaming interface which **doesn't exist in v0.7.3**. 

The gateway provides a **WebSocket-first architecture** with REST only for management.

**Decision needed**: Proceed with WebSocket redesign or pivot to REST-based management tool?
