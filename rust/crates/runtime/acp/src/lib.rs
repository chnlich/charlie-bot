//! A generic client for the Agent Client Protocol (ACP): JSON-RPC 2.0 lines over the agent process's stdio.
//!
//! The client writes the handshake (`handshake`) and each live message as one `session/prompt` on the same session
//! (`codec::AcpCodec`), and it reads the agent's output lines into CharlieBot events and the runtime's neutral mid-turn
//! signals. It names no CLI: a backend passes the session it resumes and the model it selects.

pub mod codec;
pub mod handshake;
