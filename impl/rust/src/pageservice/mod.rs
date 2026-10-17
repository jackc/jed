//! The page service (spec/design/page-service.md): one owner process serves pages to client
//! processes over a local endpoint.
//!
//! Phase 1 is the wire codec alone ([`codec`]): framing, the message catalog, and the owner record,
//! hand-written from spec/pageservice/ and tested against its shared byte fixtures. Nothing here
//! opens a socket yet.

pub mod codec;
