//! The user-facing socket (accept loop), the client/session pump, whole-frame hold-back,
//! injection rules, the give-up deadline, session-end triggers and the process-global
//! ghost-eviction token map. Returns a working `SshStdioBridge` value. Does not own ssh
//! argument details.
